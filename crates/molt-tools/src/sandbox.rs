//! Trusted shell policy. No request payload or repository file can change it.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::build_dir::BuildDir;
use crate::cargo_cache::CargoCache;
use crate::resources::{self, Resource, Setup};
use crate::{fork, Roots};

const MAX_POLICY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub enum ExecutionPolicy {
    #[default]
    Isolated,
    Configured(SandboxPolicy),
    /// An explicit host choice; never selected after a backend failure.
    Unconfined,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxPolicy {
    /// Share the host network only when explicitly enabled by trusted config.
    pub network: bool,
    /// Without `network`, the commands and processes of one workspace share
    /// a loopback network of their own, which no other workspace and no
    /// host service is on (see [`LocalNet`]); off, each command has one of
    /// its own. With `network` they have the host's.
    pub local_network: bool,
    /// Additional read-only paths, mounted at their canonical host paths.
    pub read_only: Vec<PathBuf>,
    /// Environment names to pass in addition to PATH and locale variables.
    pub environment: Vec<String>,
    pub memory_mb: u64,
    pub cpu_secs: u64,
    pub file_mb: u64,
    pub open_files: u64,
    pub processes: u64,
    /// A host Cargo home whose `registry` and `git` caches commands build
    /// from offline (`--deps-cache cargo`); see `cargo_cache`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cargo_cache: Option<PathBuf>,
    /// Variables set for commands, over the passed ones; `$WORKSPACE`,
    /// `$BUILD` and `$PATH` in a value are replaced (see `env_profile`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub set_env: BTreeMap<String, String>,
    /// Directories mounted read-write at their own paths, outside the
    /// project and the fork: the private environment `molt env prepare`
    /// installs into. Only Molt sets them; a policy file cannot.
    #[serde(skip)]
    pub writable: Vec<PathBuf>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            network: false,
            local_network: true,
            read_only: vec![],
            environment: vec![],
            memory_mb: 4096,
            cpu_secs: 600,
            file_mb: 128,
            open_files: 256,
            processes: 2048,
            cargo_cache: None,
            set_env: BTreeMap::new(),
            writable: Vec::new(),
        }
    }
}

/// The Cargo home of this process's user: `CARGO_HOME`, else `~/.cargo`.
pub fn host_cargo_home() -> anyhow::Result<PathBuf> {
    cargo_home_from(&|name| std::env::var_os(name))
}

/// The Cargo home of the environment `var` reads.
pub(crate) fn cargo_home_from(var: &dyn Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    let set = |name| var(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let home = match set("CARGO_HOME") {
        Some(home) => home,
        None => set("HOME")
            .context("neither CARGO_HOME nor HOME is set, so the Cargo cache cannot be found")?
            .join(".cargo"),
    };
    ensure!(home.is_absolute(), "CARGO_HOME must be an absolute path, not {}", home.display());
    Ok(home)
}

/// A host path that may be mounted read-only into the sandbox.
pub(crate) fn check_mount(path: &Path) -> anyhow::Result<()> {
    ensure!(
        ![
            "/", "/proc", "/dev", "/sys", "/etc", "/run", "/tmp", "/home", "/root", "/var", "/usr", "/bin", "/lib",
            "/lib64"
        ]
        .iter()
        .any(|p| path == Path::new(p)),
        "cannot expose a host root or replace a sandbox runtime mount"
    );
    ensure!(
        !["/proc", "/dev", "/sys", "/run", "/home/molt"].iter().any(|p| path.starts_with(p)),
        "cannot expose host processes, devices, IPC or the sandbox home"
    );
    Ok(())
}

impl SandboxPolicy {
    pub fn load(path: &Path, roots: &Roots) -> anyhow::Result<Self> {
        let path = path.canonicalize().context("sandbox policy path")?;
        let roots = Roots {
            root: roots.root.canonicalize().context("sandbox policy project root")?,
            scratch: match roots.scratch.canonicalize() {
                Ok(path) => path,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::path::absolute(&roots.scratch)?,
                Err(e) => return Err(e.into()),
            },
        };
        ensure!(!roots.contains(&path), "sandbox policy must be outside the project and scratch roots");
        read_policy(open_policy(&path).context("opening trusted sandbox policy")?)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.read_only.len() <= 32 && self.environment.len() <= 32,
            "at most 32 dependency paths or environment names"
        );
        for path in &self.read_only {
            ensure!(
                path.is_absolute() && path.canonicalize()? == *path,
                "dependency paths must be canonical and absolute"
            );
            check_mount(path)?;
            let meta = std::fs::metadata(path)?;
            ensure!(meta.is_dir() || meta.is_file(), "dependencies must be regular files or directories");
        }
        ensure!(self.writable.len() <= 4, "at most 4 writable directories");
        for path in &self.writable {
            ensure!(
                path.is_absolute() && path.canonicalize()? == *path && std::fs::metadata(path)?.is_dir(),
                "writable paths must be canonical absolute directories"
            );
            check_mount(path)?;
            ensure!(
                !self.read_only.iter().any(|ro| ro.starts_with(path) || path.starts_with(ro)),
                "a writable directory cannot hold or be inside a read-only one"
            );
        }
        ensure!(self.set_env.len() <= 32, "at most 32 variables to set");
        for name in self.environment.iter().chain(self.set_env.keys()) {
            ensure!(
                !name.is_empty()
                    && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !name.as_bytes()[0].is_ascii_digit(),
                "invalid environment name"
            );
            ensure!(
                !name.starts_with("MOLT_")
                    && !name.starts_with("ANTHROPIC_")
                    && !name.starts_with("OPENAI_")
                    && name != "HOME",
                "service secrets and host HOME cannot be passed to a sandbox"
            );
        }
        ensure!(
            self.set_env.iter().all(|(name, value)| name != "TMPDIR" && !value.contains('\0')),
            "TMPDIR cannot be set, and a value cannot hold a NUL byte"
        );
        if let Some(home) = &self.cargo_cache {
            ensure!(
                !self.environment.iter().any(|name| name == "CARGO_HOME"),
                "--deps-cache cargo gives commands a CARGO_HOME of their own: do not also pass CARGO_HOME"
            );
            CargoCache::open(home)?;
        }
        for n in [self.memory_mb, self.cpu_secs, self.file_mb, self.open_files, self.processes] {
            ensure!(n > 0 && n <= 1_000_000, "resource limits must be between 1 and 1000000");
        }
        Ok(())
    }

    /// Whether the sandbox starts here: it runs `/bin/true` in it. A start
    /// the host could not afford for the moment (see `resources`) is tried
    /// again a few times. A failure says why as an [`Unavailable`] when it
    /// can tell: the host was short of a resource, cannot isolate, or
    /// refuses to. Nothing ever falls back to unconfined.
    pub fn probe(&self) -> anyhow::Result<()> {
        self.validate()?;
        let mut tries = 0;
        loop {
            tries += 1;
            match self.probe_once()? {
                None => return Ok(()),
                Some((Setup::Short(resource), said)) => match resources::BACKOFF.get(tries - 1) {
                    Some(delay) => std::thread::sleep(*delay),
                    None => {
                        let message = resources::short_of(resource, &said, tries, Some(self)).message;
                        return Err(Unavailable { setup: Setup::Short(resource), message }.into());
                    }
                },
                Some((setup, said)) => {
                    let message = format!(
                        "Linux sandbox unavailable: {said}. Explicit --no-sandbox is required for unconfined execution"
                    );
                    return Err(Unavailable { setup, message }.into());
                }
            }
        }
    }

    /// One run of the probe: `None` when it passed, else why not and what
    /// bubblewrap said.
    fn probe_once(&self) -> anyhow::Result<Option<(Setup, String)>> {
        let (mut command, guard) = self.command(Path::new("/bin/true"), None, None, None, None)?;
        let command = command.as_std_mut();
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                if let Some(resource) = e.raw_os_error().and_then(Resource::of_errno) {
                    return Ok(Some((Setup::Short(resource), format!("starting /usr/bin/bwrap: {e}"))));
                }
                return Err(anyhow::Error::new(e).context("starting /usr/bin/bwrap; install bubblewrap 0.9 or newer"));
            }
        };
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= std::time::Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("Linux sandbox capability probe timed out; no unconfined fallback");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if status.success() {
            return Ok(None);
        }
        let said = stderr_of(&mut child);
        // `/bin/true` changes nothing, so even a sandbox that got as far as its namespaces may start again.
        drop(guard);
        Ok(Some((resources::setup_failure("", &said).unwrap_or(Setup::Unsupported), said)))
    }

    /// A long-lived program, such as an MCP server, in the sandbox the
    /// commands get, but with no workspace: it sees the system's read-only
    /// directories, the policy's `read_only` paths, a private `/tmp` and an
    /// empty home it starts in, and the network only when the policy says
    /// so. Its environment is only `PATH`, the locale and the policy's
    /// `environment` names, read from this process. Keep the [`Guard`]
    /// alive while the program runs.
    pub fn server_command(&self, program: &Path) -> anyhow::Result<(Command, Guard)> {
        self.validate()?;
        ensure!(program.is_absolute(), "the program to sandbox must be an absolute path");
        self.command(program, None, None, None, None)
    }

    /// A bus service run as a candidate an agent may replace (see
    /// `docs/services.md`): the sandbox [`SandboxPolicy::server_command`]
    /// gives, never with the network, plus `state` read-write at `/state`.
    /// Its environment is exactly `env`, with `PATH=/usr/bin:/bin`, the
    /// private `HOME` and `TMPDIR`, and `MOLT_STATE_DIR=/state`; nothing is
    /// read from this process's environment. Descriptor `keep` (its bus
    /// connection) stays open in it under the same number. The policy's
    /// `read_only`, `environment`, `set_env` and `cargo_cache` are not used:
    /// the host paths it sees read-only, besides the system's and its
    /// program, are `reads`, and none may hold its state directory or lie
    /// in it.
    pub fn service_command(
        &self,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        state: &Path,
        reads: &[PathBuf],
        keep: std::os::fd::RawFd,
    ) -> anyhow::Result<(Command, Guard)> {
        let mut policy = self.clone();
        policy.network = false;
        policy.read_only.clear();
        policy.environment.clear();
        policy.set_env.clear();
        policy.cargo_cache = None;
        policy.writable.clear();
        policy.validate()?;
        ensure!(program.is_absolute(), "the program to sandbox must be an absolute path");
        let program = program.canonicalize().with_context(|| format!("the program {}", program.display()))?;
        let meta = std::fs::metadata(&program)?;
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(meta.is_file() && meta.permissions().mode() & 0o111 != 0, "the program must be an executable file");
        }
        let system = ["/usr", "/bin", "/sbin", "/lib", "/lib64"].iter().any(|dir| program.starts_with(dir));
        if !system {
            check_mount(&program)?;
            ensure!(!program.starts_with(state), "the program cannot be in the state directory it may write");
            policy.read_only.push(program.clone());
        }
        ensure!(
            state.is_absolute() && state.canonicalize()? == state && std::fs::metadata(state)?.is_dir(),
            "the state directory must be a canonical absolute directory"
        );
        check_mount(state)?;
        ensure!(reads.len() <= 32, "at most 32 paths to read");
        for path in reads {
            ensure!(path.is_absolute(), "the path to read {} must be absolute", path.display());
            let path = path.canonicalize().with_context(|| format!("the path to read {}", path.display()))?;
            check_mount(&path)?;
            ensure!(
                !state.starts_with(&path) && !path.starts_with(state),
                "the path to read {} holds the state directory or is in it",
                path.display()
            );
            let meta = std::fs::metadata(&path)?;
            ensure!(meta.is_dir() || meta.is_file(), "the path to read {} is not a file or directory", path.display());
            policy.read_only.push(path);
        }
        ensure!(keep > 2, "the descriptor to keep cannot be a standard stream");
        for (name, value) in env {
            ensure!(
                !name.is_empty()
                    && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !name.as_bytes()[0].is_ascii_digit()
                    && !value.contains('\0'),
                "invalid environment variable {name:?}"
            );
            ensure!(
                !["HOME", "TMPDIR", "PATH", "MOLT_STATE_DIR"].contains(&name.as_str()),
                "{name} is set by the sandbox"
            );
        }
        let service = Service { args, env, state, keep };
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            linux::command(&policy, &program, None, None, None, None, Some(&service))
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = service;
            anyhow::bail!("sandbox requires Linux x86_64; a candidate service cannot run on this platform")
        }
    }

    pub(crate) fn prepare(
        &self,
        roots: &Roots,
        workspace: &Path,
        program: &Path,
        build: &BuildDir,
        net: Option<&LocalNet>,
    ) -> anyhow::Result<(Command, Guard)> {
        let base = fork::base(roots, &workspace.to_string_lossy())
            .context("isolated commands require an fs.fork workspace")?;
        ensure!(
            !base.starts_with(workspace) && !workspace.starts_with(&base),
            "sandbox scratch must be outside the original project"
        );
        for path in self.read_only.iter().map(PathBuf::as_path).chain(build.read_only()) {
            ensure!(
                !workspace.starts_with(path) && !path.starts_with(workspace),
                "dependency mounts cannot overlap the writable fork"
            );
        }
        for path in &self.writable {
            ensure!(
                !path.starts_with(&base)
                    && !base.starts_with(path)
                    && !workspace.starts_with(path)
                    && !path.starts_with(workspace),
                "a writable directory must be outside the project and the fork"
            );
        }
        for path in build.mounts() {
            ensure!(
                !path.starts_with(&base) && !base.starts_with(path) && !path.starts_with(workspace),
                "the private build directory must be outside the project and the fork"
            );
        }
        self.command(program, Some(workspace), Some(&base), Some(build), net)
    }

    /// Whether commands of a workspace share a network of their own.
    pub(crate) fn shares_local_network(&self) -> bool {
        self.local_network && !self.network
    }

    /// Whether a command joins a [`LocalNet`] here: one starts, and
    /// `/bin/true` runs in it. The shell service gives each command a
    /// network of its own on a host where this fails, which isolates more,
    /// never less.
    pub fn probe_local_network(&self) -> anyhow::Result<()> {
        self.validate()?;
        let net = LocalNet::start(self)?;
        let (mut command, _guard) = self.command(Path::new("/bin/true"), None, None, None, Some(&net))?;
        let command = command.as_std_mut();
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let mut child = command.spawn().context("starting /usr/bin/bwrap in the shared network")?;
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "a command could not join the shared network: {}", stderr_of(&mut child));
                return Ok(());
            }
            if started.elapsed() >= std::time::Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("a command did not start in the shared network within 5 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn command(
        &self,
        program: &Path,
        workspace: Option<&Path>,
        base: Option<&Path>,
        build: Option<&BuildDir>,
        net: Option<&LocalNet>,
    ) -> anyhow::Result<(Command, Guard)> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            linux::command(self, program, workspace, base, build, net, None)
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (program, workspace, base, build, net);
            anyhow::bail!("sandbox requires Linux x86_64; explicit unconfined mode is required on this platform")
        }
    }
}

/// What an exited child wrote on its piped stderr, up to 4 KiB, without
/// waiting for EOF on a pipe an executor or helper may still hold.
fn stderr_of(child: &mut std::process::Child) -> String {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let mut bytes = [0; 4096];
    let n = match child.stderr.take() {
        Some(mut stderr) => {
            let fd = stderr.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            stderr.read(&mut bytes).unwrap_or(0)
        }
        None => 0,
    };
    String::from_utf8_lossy(&bytes[..n]).trim().to_owned()
}

/// Pin every canonical path component. A directory or file replaced by a
/// symlink between canonicalization and opening must not redirect the read.
fn open_policy(path: &Path) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let mut file = std::fs::File::open("/")?;
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "policy path must be absolute"));
    }
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "policy path must be canonical"));
        };
        let name = CString::new(name.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if parts.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        file = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(file)
}

/// Check and read the same descriptor, with a bound that still applies if
/// its file grows after metadata was checked.
fn read_policy(file: std::fs::File) -> anyhow::Result<SandboxPolicy> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_POLICY_BYTES as u64,
        "sandbox policy must be a regular file of at most 64 KiB"
    );
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0,
        "sandbox policy must be owned by this user and not writable by others"
    );
    let mut bytes = Vec::new();
    file.take((MAX_POLICY_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_POLICY_BYTES, "sandbox policy exceeds 64 KiB");
    let policy: SandboxPolicy = serde_json::from_slice(&bytes)?;
    policy.validate()?;
    Ok(policy)
}

/// Why the sandbox could not start, as [`SandboxPolicy::probe`] tells it:
/// downcast its error to read [`Unavailable::setup`].
#[derive(Debug)]
pub struct Unavailable {
    pub setup: Setup,
    message: String,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Unavailable {}

/// What [`SandboxPolicy::service_command`] adds for a bus service.
struct Service<'a> {
    args: &'a [String],
    env: &'a [(String, String)],
    state: &'a Path,
    keep: std::os::fd::RawFd,
}

/// What a sandboxed process needs kept open while it runs.
pub struct Guard {
    pub(crate) info: std::sync::Arc<std::fs::File>,
    _seccomp: std::fs::File,
    _args: Option<std::fs::File>,
    _hosts: Option<std::fs::File>,
}

impl Guard {
    /// Whether the sandbox got as far as creating its namespaces: bubblewrap
    /// reports its child there once it has. A sandbox that fails before
    /// that never ran the command in it.
    pub fn started(&self) -> bool {
        use std::os::unix::fs::FileExt;
        let mut byte = [0; 1];
        self.info.read_at(&mut byte, 0).is_ok_and(|n| n > 0)
    }

    /// SIGTERM to every process in the sandbox, through its session group;
    /// false when the sandbox has not reported it yet.
    pub fn terminate(&self) -> bool {
        match namespace_group(&self.info) {
            // SAFETY: plain syscall on the sandbox's own process group.
            Some(group) => unsafe { libc::killpg(group, libc::SIGTERM) == 0 },
            None => false,
        }
    }
}

/// A loopback network the commands and processes of one workspace share,
/// so a test command reaches the server a process runs (#225), and that no
/// other workspace and no host service is on. It has no route out.
///
/// A holder sandbox keeps a user and a network namespace: `cat` on a pipe
/// only this value writes to, started once the sandbox is set up. A command joins both before its own sandbox
/// starts, and still gets every other namespace of its own, user included.
/// Dropping it closes the pipe and kills the holder; the network goes with
/// the last command still in it.
pub struct LocalNet {
    holder: std::process::Child,
    user: std::fs::File,
    net: std::fs::File,
    /// The holder's `/proc/<pid>/net`, which lists this network's sockets
    /// while the holder lives, and nothing once it is gone.
    sockets: std::fs::File,
}

impl LocalNet {
    /// Start one, in the sandbox `policy` describes: its seccomp filter,
    /// no host path but the system's, no capabilities.
    pub fn start(policy: &SandboxPolicy) -> anyhow::Result<Self> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            linux::holder(policy)
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = policy;
            anyhow::bail!("sandbox requires Linux x86_64; a shared local network cannot be made on this platform")
        }
    }

    /// Whether a socket listens on TCP `port` in it now.
    pub fn listens(&self, port: u16) -> bool {
        listens_in(&self.sockets, port)
    }
}

impl Drop for LocalNet {
    fn drop(&mut self) {
        drop(self.holder.stdin.take());
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

/// Whether a TCP socket listens on `port` in the network whose
/// `/proc/<pid>/net` directory `sockets` is.
pub(crate) fn listens_in(sockets: &std::fs::File, port: u16) -> bool {
    use std::os::fd::AsRawFd;
    let dir = format!("/proc/self/fd/{}", sockets.as_raw_fd());
    ["tcp", "tcp6"]
        .iter()
        .any(|table| std::fs::read_to_string(Path::new(&dir).join(table)).is_ok_and(|text| listed(&text, port)))
}

/// Whether the `/proc/net/tcp` table `text` has a socket listening on `port`.
fn listed(text: &str, port: u16) -> bool {
    const LISTEN: &str = "0A";
    text.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        let local = fields.nth(1);
        let state = fields.nth(1);
        state == Some(LISTEN)
            && local
                .and_then(|addr| addr.rsplit_once(':'))
                .is_some_and(|(_, hex)| u16::from_str_radix(hex, 16) == Ok(port))
    })
}

/// The `/proc/<pid>/net` directory of process `pid`, or of this process.
pub(crate) fn sockets_of(pid: Option<libc::pid_t>) -> std::io::Result<std::fs::File> {
    let path = match pid {
        Some(pid) => format!("/proc/{pid}/net"),
        None => "/proc/self/net".to_owned(),
    };
    std::fs::File::open(path)
}

/// The namespace init is a new session leader. TERM its group without
/// killing the outer monitor; after grace, killing that monitor tears down
/// the PID namespace, including descendants that started another session.
pub(crate) fn namespace_group(info: &std::fs::File) -> Option<libc::pid_t> {
    use std::os::unix::fs::FileExt;
    let mut bytes = [0; 4096];
    let n = info.read_at(&mut bytes, 0).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes[..n]).ok()?;
    value["child-pid"].as_i64().and_then(|pid| libc::pid_t::try_from(pid).ok()).filter(|pid| *pid > 1)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use std::fs::File;
    use std::io::{Seek, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::sync::Arc;

    fn memfd(name: &std::ffi::CStr) -> anyhow::Result<File> {
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        ensure!(fd >= 0, "sandbox control fd: {}", std::io::Error::last_os_error());
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub(super) fn filter() -> Vec<libc::sock_filter> {
        let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        let equal = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        let and = (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16;
        let ret = (libc::BPF_RET | libc::BPF_K) as u16;
        vec![
            instruction(load, 0, 0, 4),           // seccomp_data.arch
            instruction(equal, 1, 0, 0xc000003e), // AUDIT_ARCH_X86_64
            instruction(ret, 0, 0, libc::SECCOMP_RET_KILL_PROCESS),
            instruction(load, 0, 0, 0), // syscall number
            instruction((libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16, 0, 1, 0x40000000),
            instruction(ret, 0, 0, libc::SECCOMP_RET_KILL_PROCESS), // reject x32 ABI
            // io_uring socket operations do not pass through socket(2).
            // Deny rings rather than letting asynchronous operations bypass
            // the AF_UNIX policy on kernels without inherited ring filters.
            instruction(equal, 0, 1, libc::SYS_io_uring_setup as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            instruction(equal, 0, 1, libc::SYS_io_uring_enter as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            instruction(equal, 0, 1, libc::SYS_io_uring_register as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            // No AF_UNIX socket of its own, so no host socket is reachable by
            // a mounted path or, with host networking, an abstract name.
            instruction(equal, 0, 4, libc::SYS_socket as u32),
            instruction(load, 0, 0, 16), // domain
            instruction(equal, 0, 1, libc::AF_UNIX as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ALLOW),
            // Stream and seqpacket pairs reach only each other, and Rust's
            // and libuv's child processes need them. A datagram pair could
            // still send to any socket by address.
            instruction(equal, 0, 7, libc::SYS_socketpair as u32),
            instruction(load, 0, 0, 16), // domain
            instruction(equal, 0, 5, libc::AF_UNIX as u32),
            instruction(load, 0, 0, 24), // type
            instruction(and, 0, 0, 0xf), // without SOCK_NONBLOCK and SOCK_CLOEXEC
            instruction(equal, 2, 0, libc::SOCK_STREAM as u32),
            instruction(equal, 1, 0, libc::SOCK_SEQPACKET as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ALLOW),
        ]
    }

    pub(super) fn command(
        policy: &SandboxPolicy,
        program: &Path,
        workspace: Option<&Path>,
        base: Option<&Path>,
        build: Option<&BuildDir>,
        net: Option<&LocalNet>,
        service: Option<&Service<'_>>,
    ) -> anyhow::Result<(Command, Guard)> {
        let seccomp = seccomp()?;
        let info = Arc::new(memfd(c"molt-sandbox-info")?);
        let (filter_fd, info_fd) = (seccomp.as_raw_fd(), info.as_raw_fd());
        let mut command = Command::new("/usr/bin/bwrap");
        command.args([
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--new-session",
            "--disable-userns",
            "--cap-drop",
            "ALL",
        ]);
        // The network joined before bubblewrap starts (see `pre_exec` below) is kept.
        let net = net.filter(|_| !policy.network);
        if policy.network || net.is_some() {
            command.arg("--share-net");
        }
        system(command.as_std_mut());
        if policy.network && Path::new("/etc/resolv.conf").exists() {
            command.args(["--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf"]);
        }
        // Without the host's network, `localhost` still names its own loopback.
        let hosts = if policy.network {
            None
        } else {
            let mut hosts = memfd(c"molt-hosts")?;
            hosts.write_all(b"127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n")?;
            hosts.rewind()?;
            command.arg("--ro-bind-data").arg(hosts.as_raw_fd().to_string()).arg("/etc/hosts");
            Some(hosts)
        };
        command.args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home",
            "--dir",
            "/home/molt",
        ]);
        for path in policy.read_only.iter().map(PathBuf::as_path).chain(build.iter().flat_map(|b| b.read_only())) {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        if let Some(base) = base {
            command.arg("--ro-bind").arg(base).arg(base);
            if base.join(".molt").exists() {
                command.arg("--tmpfs").arg(base.join(".molt"));
            }
        }
        match workspace {
            Some(ws) => {
                command.arg("--bind").arg(ws).arg(ws).arg("--chdir").arg(ws);
            }
            None => {
                command.args(["--chdir", "/home/molt"]);
            }
        }
        // Build output goes to private directories outside the fork, which
        // persist for the workspace (see `build_dir`).
        for path in build.iter().flat_map(|b| b.mounts()) {
            command.arg("--bind").arg(path).arg(path);
        }
        for path in &policy.writable {
            command.arg("--bind").arg(path).arg(path);
        }
        if let Some(service) = service {
            command.arg("--bind").arg(service.state).arg("/state");
        }
        command.arg("--seccomp").arg(filter_fd.to_string()).arg("--info-fd").arg(info_fd.to_string());
        // Keep loader and shell variables away from the host-side backend.
        command.env_clear().env("PATH", "/usr/bin:/bin").env("LC_ALL", "C");
        let mut keep = vec![filter_fd, info_fd];
        keep.extend(hosts.as_ref().map(|hosts| hosts.as_raw_fd()));
        let join = net.map(|net| (net.user.as_raw_fd(), net.net.as_raw_fd()));
        let mut service_args = None;
        if let Some(service) = service {
            // Its environment, the bus secret among it, goes to bwrap through a
            // descriptor rather than its command line, which other processes
            // can read. bwrap sets it only for the program it starts.
            let mut args = memfd(c"molt-service-args")?;
            let mut entries = vec!["--clearenv".to_owned()];
            let fixed =
                [("PATH", "/usr/bin:/bin"), ("HOME", "/home/molt"), ("TMPDIR", "/tmp"), ("MOLT_STATE_DIR", "/state")];
            let fixed = fixed.iter().map(|(name, value)| (name.to_string(), value.to_string()));
            for (name, value) in service.env.iter().cloned().chain(fixed) {
                entries.extend(["--setenv".to_owned(), name, value]);
            }
            for entry in entries {
                args.write_all(entry.as_bytes())?;
                args.write_all(b"\0")?;
            }
            args.rewind()?;
            command.arg("--args").arg(args.as_raw_fd().to_string());
            keep.extend([args.as_raw_fd(), service.keep]);
            command.arg("--").arg(program).args(service.args);
            service_args = Some(args);
        } else {
            // The env helper runs only after entering every isolation boundary.
            command.args(["--", "/usr/bin/env", "-i"]);
            // Before the passed variables, so a variable the user passes wins.
            for (name, value) in build.iter().flat_map(|b| b.env()) {
                let mut entry = std::ffi::OsString::from(format!("{name}="));
                entry.push(value);
                command.arg(entry);
            }
            for name in ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"]
                .iter()
                .map(|s| s.to_string())
                .chain(policy.environment.clone())
            {
                if let Some(value) = std::env::var_os(&name) {
                    let mut entry = std::ffi::OsString::from(format!("{name}="));
                    entry.push(value);
                    command.arg(entry);
                }
            }
            if let (Some(ws), Some(build)) = (workspace, build) {
                let path = std::env::var_os("PATH").unwrap_or_default();
                for (name, value) in &policy.set_env {
                    let value = value
                        .replace("$WORKSPACE", &ws.to_string_lossy())
                        .replace("$BUILD", &build.dir().to_string_lossy())
                        .replace("$PATH", &path.to_string_lossy());
                    command.arg(format!("{name}={value}"));
                }
            }
            command.args(["HOME=/home/molt", "TMPDIR=/tmp"]);
            for (name, value) in crate::shell::FIXED_ENV {
                command.arg(format!("{name}={value}"));
            }
            command.arg(program);
        }
        let limits = [
            (libc::RLIMIT_AS, policy.memory_mb * 1024 * 1024),
            (libc::RLIMIT_CPU, policy.cpu_secs),
            (libc::RLIMIT_FSIZE, policy.file_mb * 1024 * 1024),
            (libc::RLIMIT_NOFILE, policy.open_files),
            (libc::RLIMIT_NPROC, policy.processes),
        ];
        unsafe {
            command.pre_exec(move || {
                // The user namespace first: only there may this process enter its network.
                if let Some((user, net)) = join {
                    if libc::setns(user, libc::CLONE_NEWUSER) != 0 || libc::setns(net, libc::CLONE_NEWNET) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for &fd in &keep {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (resource, limit) in limits {
                    let limit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                    if libc::setrlimit(resource, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok((command, Guard { info, _seccomp: seccomp, _args: service_args, _hosts: hosts }))
    }

    /// The seccomp filter, in a descriptor for bubblewrap's `--seccomp`.
    fn seccomp() -> anyhow::Result<File> {
        let mut seccomp = memfd(c"molt-seccomp")?;
        let filter = filter();
        let bytes = unsafe {
            std::slice::from_raw_parts(filter.as_ptr().cast::<u8>(), std::mem::size_of_val(filter.as_slice()))
        };
        seccomp.write_all(bytes)?;
        seccomp.rewind()?;
        Ok(seccomp)
    }

    /// The read-only system runtime every sandbox gets.
    fn system(command: &mut std::process::Command) {
        let paths =
            ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc/ld.so.cache", "/etc/ssl/certs", "/etc/alternatives"];
        for path in paths {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
    }

    /// Start a [`LocalNet`]'s holder and open its namespaces.
    pub(super) fn holder(policy: &SandboxPolicy) -> anyhow::Result<LocalNet> {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let sh = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .context("no sh in /bin or /usr/bin to hold the shared network")?;
        let seccomp = seccomp()?;
        let info = memfd(c"molt-net-info")?;
        let (filter_fd, info_fd) = (seccomp.as_raw_fd(), info.as_raw_fd());
        let mut command = std::process::Command::new("/usr/bin/bwrap");
        // No --disable-userns: commands joining its user namespace make one of their own in it.
        command.args(["--unshare-all", "--unshare-user", "--new-session", "--cap-drop", "ALL"]);
        system(&mut command);
        command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--chdir", "/"]);
        command.arg("--seccomp").arg(filter_fd.to_string()).arg("--info-fd").arg(info_fd.to_string());
        // It says it is up only once bubblewrap set up all of its sandbox: bubblewrap reports
        // its pid before the user namespace has its uid map, and a command that joined it then
        // could not map its own.
        command.args(["--", sh, "-c", "echo up && exec cat"]);
        command.env_clear().env("PATH", "/usr/bin:/bin").env("LC_ALL", "C");
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let processes = policy.processes;
        unsafe {
            command.pre_exec(move || {
                for fd in [filter_fd, info_fd] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                let limit = libc::rlimit { rlim_cur: processes, rlim_max: processes };
                if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut holder = match command.spawn() {
            Ok(holder) => holder,
            Err(e) => {
                let setup = e.raw_os_error().and_then(Resource::of_errno).map_or(Setup::Unsupported, Setup::Short);
                let message = format!("starting /usr/bin/bwrap for the shared network: {e}");
                return Err(Unavailable { setup, message }.into());
            }
        };
        let up = holder.stdout.take().map(|mut out| {
            use std::io::Read;
            let mut fds = libc::pollfd { fd: out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid descriptor, polled for at most 5 s.
            let ready = unsafe { libc::poll(&mut fds, 1, 5_000) } > 0;
            let mut said = [0; 3];
            ready && out.read_exact(&mut said).is_ok() && &said == b"up\n"
        });
        let pid = namespace_group(&info).filter(|_| up == Some(true));
        let Some(pid) = pid else {
            let _ = holder.kill();
            let status = holder.wait()?;
            let said = super::stderr_of(&mut holder);
            if status.signal() == Some(libc::SIGKILL) && said.is_empty() {
                anyhow::bail!("the shared network's sandbox did not start within 5 s");
            }
            let setup = resources::setup_failure("", &said).unwrap_or(Setup::Unsupported);
            let message = format!("the shared network's sandbox did not start: {said}");
            return Err(Unavailable { setup, message }.into());
        };
        let open = |what: &str| {
            File::open(format!("/proc/{pid}/{what}")).with_context(|| format!("opening the shared network's {what}"))
        };
        let opened = (|| anyhow::Ok((open("ns/user")?, open("ns/net")?, open("net")?)))();
        let (user, net, sockets) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                let _ = holder.kill();
                let _ = holder.wait();
                return Err(e);
            }
        };
        // Its stderr is read only when it failed to start.
        drop(holder.stderr.take());
        Ok(LocalNet { holder, user, net, sockets })
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn a_port_listens_only_when_a_socket_in_the_listen_state_has_it_as_its_local_port() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0
   1: 0100007F:1F41 0100007F:1F40 01 00000000:00000000 00:00000000 00000000     0        0 2 1 0 20 4 30 10 -1
   2: 0100007F:1F42 00000000:0000 06 00000000:00000000 00:00000000 00000000     0        0 0 3 0
";
        assert!(listed(tcp, 8000), "a listening socket on 8000");
        assert!(!listed(tcp, 8001), "a connected socket from 8001 does not listen");
        assert!(!listed(tcp, 8002), "a socket in TIME_WAIT does not listen");
        assert!(!listed(tcp, 0x40), "the remote port is not the local one");
        let tcp6 = "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000";
        assert!(listed(tcp6, 8080));
        assert!(!listed("", 8000) && !listed("header only", 8000));
    }

    #[test]
    fn policy_checks_and_reads_the_opened_file_after_path_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("policy.json");
        std::fs::write(&path, "{\"network\":false}").unwrap();
        let file = open_policy(&path).unwrap();
        std::fs::rename(&path, tmp.path().join("original.json")).unwrap();
        std::fs::write(&path, "{\"network\":true}").unwrap();
        assert!(!read_policy(file).unwrap().network, "replacement policy must not be read");
    }

    #[test]
    fn policy_rejects_file_and_ancestor_symlinks_swapped_after_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let trusted = tmp.path().join("trusted");
        let project = tmp.path().join("project");
        std::fs::create_dir(&trusted).unwrap();
        std::fs::create_dir(&project).unwrap();
        std::fs::write(trusted.join("policy.json"), "{}").unwrap();
        std::fs::write(project.join("policy.json"), "{\"network\":true}").unwrap();
        let resolved = trusted.join("policy.json").canonicalize().unwrap();
        std::fs::rename(&trusted, tmp.path().join("saved")).unwrap();
        std::os::unix::fs::symlink(&project, &trusted).unwrap();
        assert!(open_policy(&resolved).is_err(), "ancestor replacement must not redirect the read");
        std::fs::remove_file(&trusted).unwrap();
        std::fs::rename(tmp.path().join("saved"), &trusted).unwrap();
        std::fs::remove_file(&resolved).unwrap();
        std::os::unix::fs::symlink(project.join("policy.json"), &resolved).unwrap();
        assert!(open_policy(&resolved).is_err(), "file replacement must not redirect the read");
    }

    #[test]
    fn a_policy_file_cannot_name_a_writable_directory_and_molts_must_be_canonical_and_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let err = serde_json::from_str::<SandboxPolicy>(&format!("{{\"writable\":[{:?}]}}", dir)).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        let policy = SandboxPolicy { writable: vec![dir.clone()], ..SandboxPolicy::default() };
        policy.validate().unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("alias")).unwrap();
        let alias = SandboxPolicy { writable: vec![dir.join("alias")], ..SandboxPolicy::default() };
        assert!(alias.validate().is_err());
        std::fs::create_dir(dir.join("ro")).unwrap();
        let overlap =
            SandboxPolicy { writable: vec![dir.clone()], read_only: vec![dir.join("ro")], ..SandboxPolicy::default() };
        assert!(overlap.validate().is_err());
    }

    #[test]
    fn policy_uses_canonical_roots_and_checks_permissions_and_size() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        let roots = Roots { root: alias, scratch: tmp.path().join("scratch") };
        let in_project = project.join("policy.json");
        std::fs::write(&in_project, "{}").unwrap();
        assert!(SandboxPolicy::load(&in_project, &roots).is_err());
        let path = tmp.path().join("policy.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(SandboxPolicy::load(&path, &roots).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, vec![b' '; MAX_POLICY_BYTES + 1]).unwrap();
        assert!(SandboxPolicy::load(&path, &roots).is_err());
        std::fs::write(&path, "{\"network\":true}").unwrap();
        assert!(SandboxPolicy::load(&path, &roots).unwrap().network);
    }

    #[test]
    fn seccomp_blocks_host_socket_access_and_preserves_files_ip_sockets_and_socket_pairs() {
        let mut command = std::process::Command::new("/usr/bin/python3");
        command.args(["-c", "import socket, errno, ctypes\nfor call in [lambda: socket.socket(socket.AF_UNIX), lambda: socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)]:\n try: call(); raise AssertionError('unix socket allowed')\n except OSError as e: assert e.errno == errno.EPERM\nsocket.socketpair()\nsocket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET | socket.SOCK_CLOEXEC | socket.SOCK_NONBLOCK)\nlibc = ctypes.CDLL(None, use_errno=True)\nfor number in [425, 426, 427]:\n assert libc.syscall(number, 0, 0, 0, 0, 0, 0) == -1\n assert ctypes.get_errno() == errno.EPERM, 'io_uring was not blocked'\nsocket.socket(socket.AF_INET).close()\nprint('ordinary stdout still works')"]);
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let filter = linux::filter();
                let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
                if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, b"ordinary stdout still works\n");
    }
}

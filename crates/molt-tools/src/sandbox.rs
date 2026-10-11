//! Trusted shell policy. No request payload or repository file can change it.
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

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
    /// Additional read-only paths, mounted at their canonical host paths.
    pub read_only: Vec<PathBuf>,
    /// Environment names to pass in addition to PATH and locale variables.
    pub environment: Vec<String>,
    pub memory_mb: u64,
    pub cpu_secs: u64,
    pub file_mb: u64,
    pub open_files: u64,
    pub processes: u64,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            network: false,
            read_only: vec![],
            environment: vec![],
            memory_mb: 4096,
            cpu_secs: 600,
            file_mb: 128,
            open_files: 256,
            processes: 2048,
        }
    }
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
            ensure!(
                ![
                    "/", "/proc", "/dev", "/sys", "/etc", "/run", "/tmp", "/home", "/root", "/var", "/usr", "/bin",
                    "/lib", "/lib64"
                ]
                .iter()
                .any(|p| path == Path::new(p)),
                "cannot expose a host root or replace a sandbox runtime mount"
            );
            ensure!(
                !["/proc", "/dev", "/sys", "/run", "/home/molt"].iter().any(|p| path.starts_with(p)),
                "cannot expose host processes, devices, IPC or the sandbox home"
            );
            let meta = std::fs::metadata(path)?;
            ensure!(meta.is_dir() || meta.is_file(), "dependencies must be regular files or directories");
        }
        for name in &self.environment {
            ensure!(
                !name.is_empty()
                    && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !name.as_bytes()[0].is_ascii_digit(),
                "invalid environment name"
            );
            ensure!(
                !name.starts_with("MOLT_") && !name.starts_with("ANTHROPIC_") && name != "HOME",
                "service secrets and host HOME cannot be passed to a sandbox"
            );
        }
        for n in [self.memory_mb, self.cpu_secs, self.file_mb, self.open_files, self.processes] {
            ensure!(n > 0 && n <= 1_000_000, "resource limits must be between 1 and 1000000");
        }
        Ok(())
    }

    pub fn probe(&self) -> anyhow::Result<()> {
        self.validate()?;
        let (mut command, _guard) = self.command(Path::new("/bin/true"), None, None)?;
        let command = command.as_std_mut();
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        let mut child = command.spawn().context("starting /usr/bin/bwrap; install bubblewrap 0.9 or newer")?;
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
        if !status.success() {
            use std::io::Read;
            use std::os::fd::AsRawFd;
            let mut bytes = [0; 4096];
            let n = if let Some(mut stderr) = child.stderr.take() {
                // Never wait for EOF on a pipe retained by an executor or helper.
                let fd = stderr.as_raw_fd();
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                stderr.read(&mut bytes).unwrap_or(0)
            } else {
                0
            };
            anyhow::bail!(
                "Linux sandbox unavailable: {}. Explicit --no-sandbox is required for unconfined execution",
                String::from_utf8_lossy(&bytes[..n]).trim()
            );
        }
        Ok(())
    }

    pub(crate) fn prepare(&self, roots: &Roots, workspace: &Path, program: &Path) -> anyhow::Result<(Command, Guard)> {
        let base = fork::base(roots, &workspace.to_string_lossy())
            .context("isolated commands require an fs.fork workspace")?;
        ensure!(
            !base.starts_with(workspace) && !workspace.starts_with(&base),
            "sandbox scratch must be outside the original project"
        );
        for path in &self.read_only {
            ensure!(
                !workspace.starts_with(path) && !path.starts_with(workspace),
                "dependency mounts cannot overlap the writable fork"
            );
        }
        self.command(program, Some(workspace), Some(&base))
    }

    fn command(
        &self,
        program: &Path,
        workspace: Option<&Path>,
        base: Option<&Path>,
    ) -> anyhow::Result<(Command, Guard)> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            linux::command(self, program, workspace, base)
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (program, workspace, base);
            anyhow::bail!("sandbox requires Linux x86_64; explicit unconfined mode is required on this platform")
        }
    }
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

pub(crate) struct Guard {
    pub info: std::sync::Arc<std::fs::File>,
    _seccomp: std::fs::File,
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
    ) -> anyhow::Result<(Command, Guard)> {
        let mut seccomp = memfd(c"molt-seccomp")?;
        let filter = filter();
        let bytes = unsafe {
            std::slice::from_raw_parts(filter.as_ptr().cast::<u8>(), std::mem::size_of_val(filter.as_slice()))
        };
        seccomp.write_all(bytes)?;
        seccomp.rewind()?;
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
        if policy.network {
            command.arg("--share-net");
        }
        for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        for path in ["/etc/ld.so.cache", "/etc/ssl/certs", "/etc/alternatives"] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        if policy.network && Path::new("/etc/resolv.conf").exists() {
            command.args(["--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf"]);
        }
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
        for path in &policy.read_only {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        if let Some(base) = base {
            command.arg("--ro-bind").arg(base).arg(base);
            if base.join(".molt").exists() {
                command.arg("--tmpfs").arg(base.join(".molt"));
            }
        }
        if let Some(ws) = workspace {
            command.arg("--bind").arg(ws).arg(ws).arg("--chdir").arg(ws);
        }
        command.arg("--seccomp").arg(filter_fd.to_string()).arg("--info-fd").arg(info_fd.to_string());
        // Keep loader and shell variables away from the host-side backend.
        // The env helper runs only after entering every isolation boundary.
        command.env_clear().env("PATH", "/usr/bin:/bin").env("LC_ALL", "C");
        command.args(["--", "/usr/bin/env", "-i"]);
        for name in
            ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"].iter().map(|s| s.to_string()).chain(policy.environment.clone())
        {
            if let Some(value) = std::env::var_os(&name) {
                let mut entry = std::ffi::OsString::from(format!("{name}="));
                entry.push(value);
                command.arg(entry);
            }
        }
        command.args(["HOME=/home/molt", "TMPDIR=/tmp"]);
        for (name, value) in crate::shell::FIXED_ENV {
            command.arg(format!("{name}={value}"));
        }
        command.arg(program);
        let limits = [
            (libc::RLIMIT_AS, policy.memory_mb * 1024 * 1024),
            (libc::RLIMIT_CPU, policy.cpu_secs),
            (libc::RLIMIT_FSIZE, policy.file_mb * 1024 * 1024),
            (libc::RLIMIT_NOFILE, policy.open_files),
            (libc::RLIMIT_NPROC, policy.processes),
        ];
        unsafe {
            command.pre_exec(move || {
                for fd in [filter_fd, info_fd] {
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
        Ok((command, Guard { info, _seccomp: seccomp }))
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

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

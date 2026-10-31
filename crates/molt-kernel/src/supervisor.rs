//! Supervisor: runs each service as a child process with resource limits,
//! restarts it with exponential backoff when it exits, and stops it with a
//! drain deadline.
//!
//! A service runs in one of two ways ([`Execution`]): natively, as a host
//! process with the user's files and network, which only trusted code may;
//! or confined by a [`Backend`], which a candidate service an agent may
//! replace must be (see `docs/services.md`). A confined service is never
//! started any other way: when its backend cannot confine it, the start
//! fails and is retried like any other failed start.
//!
//! It reports what happens as [`Event`]s; the kernel logs them and fails any
//! request still waiting on a service that went down.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_proto::{Exec, ServiceId};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// Address-space limit in bytes (RLIMIT_AS).
    pub memory_bytes: Option<u64>,
    /// CPU time limit in seconds (RLIMIT_CPU).
    pub cpu_secs: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct RestartPolicy {
    /// Restarts allowed before the supervisor gives up on a service.
    pub max_restarts: u32,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How long a stopping service gets to drain after SIGTERM.
    pub drain: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_secs(10),
            drain: Duration::from_secs(5),
        }
    }
}

/// Variables a service inherits from the kernel's environment. Everything
/// else is dropped, so a secret such as an API key reaches only the services
/// configured to receive it (through [`Spec::env`]).
pub const BASELINE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TMPDIR",
    "TERM",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    // Toolchain locations, so a service's builds and tests find the user's compilers and caches.
    // Anything else they need (LD_LIBRARY_PATH, CC, DATABASE_URL, ...) is dropped too unless passed in Spec::env.
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "GOCACHE",
    "GOMODCACHE",
    "JAVA_HOME",
    "NODE_PATH",
    "NVM_DIR",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
];

#[derive(Clone, Debug)]
pub struct Spec {
    pub id: ServiceId,
    pub exec: Exec,
    /// Set on top of [`BASELINE_ENV`]; later entries win. A confined
    /// service gets these alone, without the baseline.
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    pub restart: RestartPolicy,
    pub execution: Execution,
}

/// How the supervisor runs a service's process.
#[derive(Clone, Debug, Default)]
pub enum Execution {
    /// A host process: it can read and write whatever the user can and use
    /// the network, whatever capabilities it holds on the bus. Only for
    /// code the user trusts.
    #[default]
    Native,
    /// Confined by a backend, with the bus as its only way out.
    Confined(Confinement),
}

/// What a confined service is run with.
#[derive(Clone)]
pub struct Confinement {
    pub backend: Arc<dyn Backend>,
    /// The service's endpoint socket on the Unix transport. The supervisor
    /// connects to it for each start and hands the process the connection,
    /// so the process needs no socket path and no socket of its own.
    pub endpoint: PathBuf,
    /// The service's private state directory, the one host directory it
    /// may write.
    pub state: PathBuf,
}

impl std::fmt::Debug for Confinement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Confinement")
            .field("backend", &self.backend.name())
            .field("endpoint", &self.endpoint)
            .field("state", &self.state)
            .finish()
    }
}

/// One start of a confined service, as its backend is asked to run it.
pub struct ConfinedStart<'a> {
    pub service: &'a ServiceId,
    pub exec: &'a Exec,
    /// Exactly the environment the process gets, the bus address
    /// (`fd:<bus_fd>`), identity, secret and capabilities included.
    pub env: &'a [(String, String)],
    /// The descriptor of its bus connection, which must stay open in the
    /// process under the same number.
    pub bus_fd: i32,
    pub state: &'a Path,
    pub limits: &'a Limits,
}

/// Something that runs a process confined: what it may read, write, call
/// and connect to is fixed by the backend and the [`ConfinedStart`], not
/// by the process.
pub trait Backend: Send + Sync + 'static {
    /// A short name for logs and error messages, such as `linux-sandbox`.
    fn name(&self) -> &str;
    /// Whether the backend can confine a process on this host now; the
    /// reason when it cannot.
    fn probe(&self) -> Result<(), String>;
    /// The command for one start, and what must stay alive while it runs.
    fn command(&self, start: &ConfinedStart<'_>) -> Result<(Command, Box<dyn Contained>), String>;
}

/// What a backend keeps alive for a running confined process.
pub trait Contained: Send + Sync {
    /// Ask every process in the confinement to stop (SIGTERM). False when
    /// the backend cannot, so the supervisor signals the process it started.
    fn terminate(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Started { service: ServiceId, pid: Option<u32> },
    Exited { service: ServiceId, status: String },
    GaveUp { service: ServiceId, restarts: u32 },
}

struct Child {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

pub struct Supervisor {
    children: Mutex<HashMap<ServiceId, Child>>,
    events: mpsc::Sender<Event>,
}

impl Supervisor {
    pub fn new(events: mpsc::Sender<Event>) -> Self {
        Self { children: Mutex::default(), events }
    }

    /// Start supervising a service. Replaces (and stops) any running instance.
    pub fn start(&self, spec: Spec) {
        let (stop, stop_rx) = watch::channel(false);
        let id = spec.id.clone();
        let task = tokio::spawn(run(spec, stop_rx, self.events.clone()));
        if let Some(old) = self.children.lock().unwrap().insert(id, Child { stop, task }) {
            let _ = old.stop.send(true);
        }
    }

    /// Ask a service to stop: SIGTERM, then SIGKILL after the drain deadline.
    /// Resolves once the process is gone.
    pub async fn stop(&self, id: &ServiceId) {
        let child = self.children.lock().unwrap().remove(id);
        if let Some(child) = child {
            let _ = child.stop.send(true);
            let _ = child.task.await;
        }
    }

    pub async fn stop_all(&self) {
        let ids: Vec<_> = self.children.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }
}

/// One start of a service, before it is spawned.
struct Prepared {
    cmd: Command,
    /// What a confined service's backend keeps alive while it runs.
    contained: Option<Box<dyn Contained>>,
    /// The supervisor's copy of a confined service's bus connection, closed
    /// once the process is spawned with its own, so the kernel sees the
    /// connection end when the process does.
    #[cfg(unix)]
    bus: Option<std::os::unix::net::UnixStream>,
}

fn command(spec: &Spec) -> Result<Prepared, String> {
    let mut prepared = match &spec.execution {
        Execution::Native => {
            let mut cmd = Command::new(&spec.exec.command);
            cmd.env_clear();
            for key in BASELINE_ENV {
                if let Some(value) = std::env::var_os(key) {
                    cmd.env(key, value);
                }
            }
            cmd.args(&spec.exec.args).envs(spec.env.iter().cloned());
            limit(&mut cmd, &spec.limits);
            Prepared {
                cmd,
                contained: None,
                #[cfg(unix)]
                bus: None,
            }
        }
        // The backend sets the resource limits itself, from `spec.limits`.
        Execution::Confined(confinement) => confined(spec, confinement)?,
    };
    prepared.cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    die_with_kernel(&mut prepared.cmd);
    Ok(prepared)
}

/// A confined start: a fresh connection to the service's endpoint, which
/// the backend passes on as the process's bus.
fn confined(spec: &Spec, confinement: &Confinement) -> Result<Prepared, String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let bus = std::os::unix::net::UnixStream::connect(&confinement.endpoint)
            .map_err(|e| format!("connecting to the endpoint of {}: {e}", spec.id))?;
        let bus_fd = bus.as_raw_fd();
        let mut env: Vec<(String, String)> =
            spec.env.iter().filter(|(k, _)| k != molt_transport::ENV_ADDRESS).cloned().collect();
        env.push((molt_transport::ENV_ADDRESS.to_owned(), format!("fd:{bus_fd}")));
        let start = ConfinedStart {
            service: &spec.id,
            exec: &spec.exec,
            env: &env,
            bus_fd,
            state: &confinement.state,
            limits: &spec.limits,
        };
        let backend = &confinement.backend;
        let (cmd, contained) =
            backend.command(&start).map_err(|e| format!("{} cannot confine it: {e}", backend.name()))?;
        Ok(Prepared { cmd, contained: Some(contained), bus: Some(bus) })
    }
    #[cfg(not(unix))]
    {
        let _ = (spec, confinement);
        Err("confined services need a Unix host".into())
    }
}

/// A kernel killed outright runs no shutdown, and a service on NATS would
/// not notice it is gone, so the service (or a confined service's backend
/// process) gets SIGTERM when the kernel dies.
fn die_with_kernel(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        let kernel = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe calls (prctl, getppid, _exit) run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                // Linux sends it when the spawning thread exits; spawns run on the runtime's long-lived threads.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The kernel died before the signal was armed.
                if libc::getppid() != kernel {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// The rlimits of a native service.
fn limit(cmd: &mut Command, limits: &Limits) {
    #[cfg(unix)]
    {
        let limits = limits.clone();
        // SAFETY: only async-signal-safe calls (setrlimit) run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let set = |res, v: u64| {
                    let lim = libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t };
                    if libc::setrlimit(res, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                };
                if let Some(b) = limits.memory_bytes {
                    set(libc::RLIMIT_AS, b)?;
                }
                if let Some(s) = limits.cpu_secs {
                    set(libc::RLIMIT_CPU, s)?;
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = (cmd, limits);
}

async fn run(spec: Spec, mut stop: watch::Receiver<bool>, events: mpsc::Sender<Event>) {
    let mut restarts = 0u32;
    let mut backoff = spec.restart.backoff_initial;
    loop {
        let spawned = command(&spec).and_then(|mut prepared| {
            let child = prepared.cmd.spawn().map_err(|e| e.to_string())?;
            #[cfg(unix)]
            drop(prepared.bus.take());
            Ok((child, prepared.contained))
        });
        let (mut child, contained) = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ =
                    events.send(Event::Exited { service: spec.id.clone(), status: format!("spawn failed: {e}") }).await;
                if !wait_backoff(&mut stop, &mut backoff, &spec.restart, &mut restarts, &spec.id, &events).await {
                    return;
                }
                continue;
            }
        };
        let _ = events.send(Event::Started { service: spec.id.clone(), pid: child.id() }).await;
        let status = tokio::select! {
            s = child.wait() => s,
            _ = stopped(&mut stop) => {
                terminate(&mut child, contained.as_deref(), spec.restart.drain).await;
                let _ = events.send(Event::Exited { service: spec.id.clone(), status: "stopped".into() }).await;
                return;
            }
        };
        let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
        let _ = events.send(Event::Exited { service: spec.id.clone(), status }).await;
        if !wait_backoff(&mut stop, &mut backoff, &spec.restart, &mut restarts, &spec.id, &events).await {
            return;
        }
    }
}

/// Resolves once a stop has been requested. The watch guard is dropped
/// before returning so the future stays `Send`.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// Sleep before a restart. Returns false when the supervisor should stop.
async fn wait_backoff(
    stop: &mut watch::Receiver<bool>,
    backoff: &mut Duration,
    policy: &RestartPolicy,
    restarts: &mut u32,
    id: &ServiceId,
    events: &mpsc::Sender<Event>,
) -> bool {
    if *restarts >= policy.max_restarts {
        let _ = events.send(Event::GaveUp { service: id.clone(), restarts: *restarts }).await;
        return false;
    }
    *restarts += 1;
    let delay = *backoff;
    *backoff = (*backoff * 2).min(policy.backoff_max);
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = stopped(stop) => false,
    }
}

async fn terminate(child: &mut tokio::process::Child, contained: Option<&dyn Contained>, drain: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        if !contained.is_some_and(|c| c.terminate()) {
            // SAFETY: plain syscall on a pid we own.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        if tokio::time::timeout(drain, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Exec {
        Exec { command: "/bin/sh".into(), args: vec!["-c".into(), script.into()] }
    }

    fn quick(max_restarts: u32) -> RestartPolicy {
        RestartPolicy {
            max_restarts,
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            drain: Duration::from_millis(500),
        }
    }

    #[tokio::test]
    async fn restarts_a_crashing_service_then_gives_up() {
        let (tx, mut rx) = mpsc::channel(64);
        let sup = Supervisor::new(tx);
        let id = ServiceId::new("crashy").unwrap();
        sup.start(Spec {
            id: id.clone(),
            exec: sh("exit 3"),
            env: vec![],
            limits: Limits::default(),
            restart: quick(2),
            execution: Execution::Native,
        });
        let mut started = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
                Event::Started { .. } => started += 1,
                Event::Exited { status, .. } => assert!(status.contains('3'), "{status}"),
                Event::GaveUp { restarts, .. } => {
                    assert_eq!(restarts, 2);
                    break;
                }
            }
        }
        assert_eq!(started, 3, "one start plus two restarts");
    }

    #[tokio::test]
    async fn stop_terminates_a_running_service() {
        let (tx, mut rx) = mpsc::channel(64);
        let sup = Supervisor::new(tx);
        let id = ServiceId::new("sleepy").unwrap();
        sup.start(Spec {
            id: id.clone(),
            exec: sh("sleep 30"),
            env: vec![],
            limits: Limits::default(),
            restart: quick(5),
            execution: Execution::Native,
        });
        assert!(matches!(rx.recv().await.unwrap(), Event::Started { .. }));
        tokio::time::timeout(Duration::from_secs(5), sup.stop(&id)).await.expect("stop within the drain deadline");
        assert_eq!(rx.recv().await.unwrap(), Event::Exited { service: id, status: "stopped".into() });
    }

    /// A kernel killed outright runs no shutdown; its services must not outlive it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_service_gets_sigterm_when_the_thread_that_started_it_exits() {
        let pid = std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let pid = rt.block_on(async {
                let (tx, mut rx) = mpsc::channel(8);
                let sup = Supervisor::new(tx);
                let id = ServiceId::new("orphan").unwrap();
                sup.start(Spec {
                    id,
                    exec: sh("exec sleep 30"),
                    env: vec![],
                    limits: Limits::default(),
                    restart: quick(0),
                    execution: Execution::Native,
                });
                let Some(Event::Started { pid: Some(pid), .. }) = rx.recv().await else { panic!("no start") };
                std::mem::forget(sup);
                pid
            });
            // Leaked, nothing kills the child on drop: only the thread's exit can.
            std::mem::forget(rt);
            pid as libc::pid_t
        })
        .join()
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut status = 0;
        // SAFETY: plain syscalls on a child of this process.
        while unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != pid {
            if std::time::Instant::now() > deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                panic!("the service outlived the thread that started it");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGTERM, "status {status:#x}");
    }

    #[tokio::test]
    async fn memory_limit_is_applied() {
        let (tx, mut rx) = mpsc::channel(64);
        let sup = Supervisor::new(tx);
        let id = ServiceId::new("limited").unwrap();
        let limits = Limits { memory_bytes: Some(512 * 1024 * 1024), cpu_secs: None };
        // `ulimit -v` reports the address-space limit in KiB.
        sup.start(Spec {
            id: id.clone(),
            exec: sh("test \"$(ulimit -v)\" = 524288"),
            env: vec![],
            limits,
            restart: quick(0),
            execution: Execution::Native,
        });
        let mut statuses = vec![];
        while let Some(ev) = rx.recv().await {
            match ev {
                Event::Exited { status, .. } => statuses.push(status),
                Event::GaveUp { .. } => break,
                _ => {}
            }
        }
        assert_eq!(statuses, vec!["exit status: 0".to_string()]);
    }
}

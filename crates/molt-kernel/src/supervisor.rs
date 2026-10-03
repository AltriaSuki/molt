//! Supervisor: runs each service as a child process with resource limits,
//! restarts it with exponential backoff when it exits, and stops it with a
//! drain deadline.
//!
//! It reports what happens as [`Event`]s; the kernel logs them and fails any
//! request still waiting on a service that went down.

use std::collections::HashMap;
use std::sync::Mutex;
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

#[derive(Clone, Debug)]
pub struct Spec {
    pub id: ServiceId,
    pub exec: Exec,
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    pub restart: RestartPolicy,
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

fn command(spec: &Spec) -> Command {
    let mut cmd = Command::new(&spec.exec.command);
    cmd.args(&spec.exec.args).envs(spec.env.iter().cloned()).kill_on_drop(true).stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        let limits = spec.limits.clone();
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
    cmd
}

async fn run(spec: Spec, mut stop: watch::Receiver<bool>, events: mpsc::Sender<Event>) {
    let mut restarts = 0u32;
    let mut backoff = spec.restart.backoff_initial;
    loop {
        let mut child = match command(&spec).spawn() {
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
                terminate(&mut child, spec.restart.drain).await;
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

async fn terminate(child: &mut tokio::process::Child, drain: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain syscall on a pid we own.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
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
        });
        assert!(matches!(rx.recv().await.unwrap(), Event::Started { .. }));
        tokio::time::timeout(Duration::from_secs(5), sup.stop(&id)).await.expect("stop within the drain deadline");
        assert_eq!(rx.recv().await.unwrap(), Event::Exited { service: id, status: "stopped".into() });
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

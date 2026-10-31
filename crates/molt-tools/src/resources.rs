//! What commands take from the host beyond their sandbox's own limits, and
//! what happens when the host runs short (#230).
//!
//! - **Admission.** `shell.run` admits [`Admission::jobs`] commands at once,
//!   and queues up to [`Admission::queue`] more in the order they came;
//!   beyond that a command is refused with [`shell::QUEUE_FULL`]. A
//!   cancelled command leaves the queue at once and never starts.
//! - **A start the host could not afford.** A command that could not start
//!   for want of processes, open files, memory or namespaces is tried again
//!   after a bounded backoff, but only when it provably never started: the
//!   spawn itself failed, or the sandbox failed before it created its
//!   namespaces. Then it is refused with [`shell::SHORT_OF`], naming the
//!   resource and what the host has of it ([`Host`]).
//! - **Isolation this host does not offer** (no bubblewrap, no user
//!   namespaces) or its security policy refuses is [`shell::NO_ISOLATION`],
//!   never tried again and never run unconfined instead.
//!
//! Nothing here changes a host limit: it reads them, to say which ran out.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use molt_api::shell;
use molt_proto::{ErrorCode, RemoteError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::SandboxPolicy;

/// How long to wait before each new try of a start the host could not
/// afford: five tries over about 1.5 seconds, then the command is refused.
pub(crate) const BACKOFF: [Duration; 4] =
    [Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400), Duration::from_millis(800)];

/// The most commands [`Admission`] may be set to run at once, and to queue.
pub const MAX_JOBS: usize = 1024;
/// Commands queued by default, whatever the number run at once.
pub const DEFAULT_QUEUE: usize = 64;

/// What a command could not get from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// Processes or threads: `fork` or `clone` said EAGAIN.
    Processes,
    /// File descriptors, of the process (EMFILE) or the system (ENFILE).
    OpenFiles,
    Memory,
    /// User or other namespaces: `/proc/sys/user/max_*_namespaces`.
    Namespaces,
}

impl Resource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Processes => "processes",
            Self::OpenFiles => "open files",
            Self::Memory => "memory",
            Self::Namespaces => "namespaces",
        }
    }

    /// The resource an OS error says ran out, if it says one did.
    pub fn of_errno(errno: i32) -> Option<Self> {
        match errno {
            libc::EAGAIN => Some(Self::Processes),
            libc::EMFILE | libc::ENFILE => Some(Self::OpenFiles),
            libc::ENOMEM => Some(Self::Memory),
            _ => None,
        }
    }
}

/// Why the sandbox did not start a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// The host ran short of a resource; it may have it again soon.
    Short(Resource),
    /// This host cannot isolate commands: no user namespaces, or bubblewrap
    /// is missing or cannot run.
    Unsupported,
    /// The host's security policy refuses unprivileged namespaces or mounts
    /// (a sysctl, AppArmor, a container's seccomp profile).
    Refused,
}

/// What bubblewrap's own error, the whole of `stderr`, says went wrong,
/// when the output is one line of bubblewrap's and nothing else; `None` for
/// anything else, which is the command's to report.
pub fn setup_failure(stdout: &str, stderr: &str) -> Option<Setup> {
    if !stdout.is_empty() {
        return None;
    }
    let mut lines = stderr.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next()?.trim();
    if lines.next().is_some() {
        return None;
    }
    // The dynamic loader, before bubblewrap's own code runs: `Error 24` is EMFILE, 23 ENFILE.
    if let Some(said) = line.strip_prefix("/usr/bin/bwrap: error while loading shared libraries: ") {
        return (said.ends_with("Error 24") || said.ends_with("Error 23")).then_some(Setup::Short(Resource::OpenFiles));
    }
    let said = line.strip_prefix("bwrap: ")?.to_lowercase();
    // As bubblewrap 0.4 to 0.11 word them.
    if said.contains("no permissions to creating new namespace")
        || said.contains("setting up uid map: permission denied")
        || said.contains("setting up gid map: permission denied")
        || said.contains("operation not permitted")
    {
        return Some(Setup::Refused);
    }
    if said.contains("kernel does not support user namespaces") {
        return Some(Setup::Unsupported);
    }
    let resource = if said.contains("max_*_namespaces") || said.contains("(enospc)") {
        Resource::Namespaces
    } else if said.contains("resource temporarily unavailable") {
        Resource::Processes
    } else if said.contains("too many open files") {
        Resource::OpenFiles
    } else if said.contains("cannot allocate memory") || said.contains("out of memory") {
        Resource::Memory
    } else {
        return None;
    };
    Some(Setup::Short(resource))
}

/// Admits `shell.run` commands: so many at once, so many more waiting.
pub struct Admission {
    jobs: usize,
    queue: usize,
    slots: Arc<Semaphore>,
    waiting: Arc<AtomicUsize>,
}

/// A command's turn: the next waiting command goes when it is dropped.
pub struct Turn {
    _permit: OwnedSemaphorePermit,
}

impl Default for Admission {
    /// As many commands at once as the host has processors, and at least
    /// four, so attempts running side by side are not made to take turns
    /// on a small machine; [`DEFAULT_QUEUE`] waiting.
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self::new(cpus.max(4), DEFAULT_QUEUE)
    }
}

impl Admission {
    /// `jobs` at once (at least 1) and `queue` waiting, each at most [`MAX_JOBS`].
    pub fn new(jobs: usize, queue: usize) -> Self {
        let jobs = jobs.clamp(1, MAX_JOBS);
        Self { jobs, queue: queue.min(MAX_JOBS), slots: Arc::new(Semaphore::new(jobs)), waiting: Arc::default() }
    }

    /// The same number at once, and `queue` waiting.
    pub fn with_queue(self, queue: usize) -> Self {
        Self::new(self.jobs, queue)
    }

    pub fn jobs(&self) -> usize {
        self.jobs
    }

    pub fn queue(&self) -> usize {
        self.queue
    }

    /// Commands waiting for a turn now.
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Commands running now.
    pub fn running(&self) -> usize {
        self.jobs - self.slots.available_permits()
    }

    /// A turn, now or once one is free; refused at once when the queue is
    /// full, and given up when `cancel` fires first.
    pub async fn enter(&self, cancel: &CancellationToken) -> Result<Turn, RemoteError> {
        if let Ok(permit) = self.slots.clone().try_acquire_owned() {
            return Ok(Turn { _permit: permit });
        }
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= self.queue {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            let message = format!(
                "{}{} running and {} waiting, the most this shell service runs and queues; the command did \
                 not run. Wait for others to finish, or start the service with a larger --jobs or \
                 --queue",
                shell::QUEUE_FULL,
                self.jobs,
                self.queue
            );
            return Err(RemoteError { code: ErrorCode::Busy, message });
        }
        // Leaves the queue however the wait ends, the request being dropped included.
        struct Leave(Arc<AtomicUsize>);
        impl Drop for Leave {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _leave = Leave(self.waiting.clone());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(RemoteError {
                code: ErrorCode::Cancelled,
                message: "command cancelled while it waited for its turn; it never started".into(),
            }),
            permit = self.slots.clone().acquire_owned() => match permit {
                Ok(permit) => Ok(Turn { _permit: permit }),
                Err(_) => Err(RemoteError { code: ErrorCode::Unavailable, message: "shell service is shutting down".into() }),
            },
        }
    }
}

/// The error for a command that never started because the host was short
/// of `resource`, after `tries`; `said` is what failed, and `sandbox` the
/// policy commands run under, if they run in one.
pub(crate) fn short_of(resource: Resource, said: &str, tries: usize, sandbox: Option<&SandboxPolicy>) -> RemoteError {
    let host = Host::probe();
    let message = format!(
        "{}{}: {} (tried {tries} times). {} The command did not run: wait for other commands to finish, run fewer at \
         once (the shell service's --jobs), or raise the limit.",
        shell::SHORT_OF,
        resource.name(),
        said.trim(),
        host.summary(sandbox),
    );
    RemoteError { code: ErrorCode::Busy, message }
}

/// The error for a command the sandbox could not start on this host, for a
/// reason only the user can change.
pub(crate) fn no_isolation(setup: Setup, said: &str) -> RemoteError {
    let why = match setup {
        Setup::Refused => "the host's security policy refuses the namespaces or mounts the sandbox needs",
        _ => "this host cannot isolate commands",
    };
    let message = format!(
        "{}{why}: {}. The command did not run, and is not run unconfined instead: enable unprivileged user \
         namespaces for this user (see CONTRIBUTING.md), or choose --no-sandbox explicitly.",
        shell::NO_ISOLATION,
        said.trim()
    );
    RemoteError { code: ErrorCode::Unavailable, message }
}

/// What the host has of the resources commands take, as far as this process
/// can read it. Every field is `None` when it could not be read: unknown,
/// never zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Host {
    /// Processes and threads this user runs now, which `RLIMIT_NPROC` counts.
    pub user_tasks: Option<u64>,
    /// This process's `RLIMIT_NPROC`; `Some(None)` is unlimited.
    pub nproc: Option<Option<u64>>,
    /// This process's open descriptors and its `RLIMIT_NOFILE`.
    pub open_files: Option<u64>,
    pub nofile: Option<Option<u64>>,
    /// `MemAvailable` in `/proc/meminfo`, in bytes.
    pub memory_available: Option<u64>,
    /// This process's cgroup (v2): its tasks and `pids.max` (`None`: max).
    pub cgroup_tasks: Option<u64>,
    pub cgroup_tasks_max: Option<Option<u64>>,
    /// The cgroup's memory use and `memory.max`, in bytes.
    pub cgroup_memory: Option<u64>,
    pub cgroup_memory_max: Option<Option<u64>>,
    /// `/proc/sys/user/max_user_namespaces`.
    pub max_user_namespaces: Option<u64>,
}

impl Host {
    /// Read what the host has now. It changes nothing.
    pub fn probe() -> Self {
        Self::read(Path::new("/proc"), Path::new("/sys/fs/cgroup"))
    }

    fn read(proc: &Path, cgroups: &Path) -> Self {
        let number = |path: &Path| std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u64>().ok());
        let limit = |path: &Path| {
            std::fs::read_to_string(path).ok().and_then(|s| match s.trim() {
                "max" => Some(None),
                n => n.parse::<u64>().ok().map(Some),
            })
        };
        let cgroup = std::fs::read_to_string(proc.join("self/cgroup")).ok().and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("0::")).map(|p| cgroups.join(p.trim_start_matches('/')))
        });
        let in_cgroup = |name: &str| cgroup.as_ref().map(|dir| dir.join(name));
        Self {
            user_tasks: user_tasks(proc),
            nproc: rlimit(libc::RLIMIT_NPROC),
            open_files: std::fs::read_dir(proc.join("self/fd")).ok().map(|d| d.count() as u64),
            nofile: rlimit(libc::RLIMIT_NOFILE),
            memory_available: std::fs::read_to_string(proc.join("meminfo")).ok().and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
                line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kib| kib * 1024)
            }),
            cgroup_tasks: in_cgroup("pids.current").and_then(|p| number(&p)),
            cgroup_tasks_max: in_cgroup("pids.max").and_then(|p| limit(&p)),
            cgroup_memory: in_cgroup("memory.current").and_then(|p| number(&p)),
            cgroup_memory_max: in_cgroup("memory.max").and_then(|p| limit(&p)),
            max_user_namespaces: number(&proc.join("sys/user/max_user_namespaces")),
        }
    }

    /// One paragraph for an error or a report; `sandbox` is the policy
    /// commands run under, when they run in one.
    pub fn summary(&self, sandbox: Option<&SandboxPolicy>) -> String {
        let known = |n: Option<u64>| n.map_or("an unknown number of".to_owned(), |n| n.to_string());
        let max = |n: Option<Option<u64>>| match n {
            None => "unknown".to_owned(),
            Some(None) => "unlimited".to_owned(),
            Some(Some(n)) => n.to_string(),
        };
        let mut out = format!(
            "This user runs {} processes and threads (RLIMIT_NPROC here: {}",
            known(self.user_tasks),
            max(self.nproc)
        );
        if let Some(policy) = sandbox {
            let _ = write!(out, "; the sandbox's `processes` limit, {}, counts them all", policy.processes);
        }
        let _ = write!(
            out,
            "). This service has {} files open (RLIMIT_NOFILE here: {}",
            known(self.open_files),
            max(self.nofile)
        );
        if let Some(policy) = sandbox {
            let _ = write!(out, "; commands get the sandbox's `open_files`, {}", policy.open_files);
        }
        out.push_str("). ");
        match self.memory_available {
            Some(bytes) => {
                let _ = write!(out, "{} MiB of memory is available", bytes >> 20);
            }
            None => out.push_str("The memory available is unknown"),
        }
        if let Some(policy) = sandbox {
            let _ = write!(out, " (commands get the sandbox's `memory_mb`, {})", policy.memory_mb);
        }
        out.push('.');
        if self.cgroup_tasks.is_some() || self.cgroup_tasks_max.is_some() {
            let mib = |n: Option<u64>| n.map(|n| n >> 20);
            let memory_max = match self.cgroup_memory_max {
                Some(Some(n)) => format!("{} MiB", n >> 20),
                other => max(other),
            };
            let _ = write!(
                out,
                " Its cgroup has {} tasks of {} and uses {} MiB of memory of {memory_max}.",
                known(self.cgroup_tasks),
                max(self.cgroup_tasks_max),
                known(mib(self.cgroup_memory)),
            );
        }
        if let Some(n) = self.max_user_namespaces {
            let _ = write!(out, " Up to {n} user namespaces are allowed.");
        }
        out
    }
}

fn rlimit(resource: libc::__rlimit_resource_t) -> Option<Option<u64>> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes only the struct it is given.
    if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
        return None;
    }
    Some((limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur))
}

/// The threads of every process this user owns (by real uid), as
/// `RLIMIT_NPROC` counts them.
fn user_tasks(proc: &Path) -> Option<u64> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let mut tasks = 0;
    for entry in std::fs::read_dir(proc).ok()?.flatten() {
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that exits meanwhile is simply not counted.
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        let field = |name: &str| status.lines().find_map(|l| l.strip_prefix(name)).map(str::trim);
        if field("Uid:").and_then(|ids| ids.split_whitespace().next()) != Some(uid.as_str()) {
            continue;
        }
        tasks += field("Threads:").and_then(|n| n.parse::<u64>().ok()).unwrap_or(1);
    }
    Some(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubblewraps_own_errors_say_which_resource_ran_out_or_that_isolation_is_missing() {
        let short = |line: &str| setup_failure("", &format!("{line}\n"));
        assert_eq!(
            short("bwrap: Creating new namespace failed: Resource temporarily unavailable"),
            Some(Setup::Short(Resource::Processes))
        );
        assert_eq!(
            short("bwrap: Creating new namespace failed: nesting depth or /proc/sys/user/max_*_namespaces exceeded (ENOSPC)"),
            Some(Setup::Short(Resource::Namespaces))
        );
        assert_eq!(short("bwrap: eventfd(): Too many open files"), Some(Setup::Short(Resource::OpenFiles)));
        assert_eq!(
            short("/usr/bin/bwrap: error while loading shared libraries: libselinux.so.1: cannot open shared object file: Error 24"),
            Some(Setup::Short(Resource::OpenFiles))
        );
        assert_eq!(
            short("/usr/bin/bwrap: error while loading shared libraries: libcap.so.2: cannot open shared object file: No such file or directory"),
            None
        );
        assert_eq!(short("bwrap: Can't fork: Cannot allocate memory"), Some(Setup::Short(Resource::Memory)));
        assert_eq!(
            short("bwrap: No permissions to creating new namespace, likely because the kernel does not allow non-privileged user namespaces. On e.g. debian this can be enabled with 'sysctl kernel.unprivileged_userns_clone=1'."),
            Some(Setup::Refused)
        );
        // Ubuntu 24.04's AppArmor restriction.
        assert_eq!(short("bwrap: setting up uid map: Permission denied"), Some(Setup::Refused));
        assert_eq!(short("bwrap: Failed to make / slave: Operation not permitted"), Some(Setup::Refused));
        assert_eq!(
            short("bwrap: Creating new namespace failed, likely because the kernel does not support user namespaces.  bwrap must be installed setuid on such systems."),
            Some(Setup::Unsupported)
        );

        // The command's own output, even one that mentions bubblewrap, is the command's.
        assert_eq!(short("bwrap: Can't find source path /nix: No such file or directory"), None);
        assert_eq!(setup_failure("", "make: fork: Resource temporarily unavailable\n"), None);
        assert_eq!(
            setup_failure("", "building\nbwrap: Creating new namespace failed: Resource temporarily unavailable\n"),
            None
        );
        assert_eq!(
            setup_failure("ok\n", "bwrap: Creating new namespace failed: Resource temporarily unavailable\n"),
            None
        );
        assert_eq!(Resource::of_errno(libc::EAGAIN), Some(Resource::Processes));
        assert_eq!(Resource::of_errno(libc::ENFILE), Some(Resource::OpenFiles));
        assert_eq!(Resource::of_errno(libc::ENOENT), None);
    }

    #[test]
    fn the_host_report_reads_limits_and_says_unknown_for_what_it_cannot_read() {
        let tmp = tempfile::tempdir().unwrap();
        let (proc, cgroups) = (tmp.path().join("proc"), tmp.path().join("cgroup"));
        std::fs::create_dir_all(proc.join("self/fd")).unwrap();
        std::fs::create_dir_all(proc.join("sys/user")).unwrap();
        std::fs::create_dir_all(cgroups.join("user.slice/molt")).unwrap();
        std::fs::write(proc.join("self/cgroup"), "0::/user.slice/molt\n").unwrap();
        std::fs::write(proc.join("meminfo"), "MemTotal: 8000000 kB\nMemAvailable:    2097152 kB\n").unwrap();
        std::fs::write(proc.join("sys/user/max_user_namespaces"), "63000\n").unwrap();
        std::fs::write(cgroups.join("user.slice/molt/pids.current"), "1200\n").unwrap();
        std::fs::write(cgroups.join("user.slice/molt/pids.max"), "max\n").unwrap();
        std::fs::write(cgroups.join("user.slice/molt/memory.max"), "1073741824\n").unwrap();
        let uid = unsafe { libc::getuid() };
        for (pid, owner, threads) in [("10", uid, 7), ("11", uid + 1, 50), ("12", uid, 3)] {
            std::fs::create_dir_all(proc.join(pid)).unwrap();
            let status = format!("Name:\tx\nUid:\t{owner}\t{owner}\t{owner}\t{owner}\nThreads:\t{threads}\n");
            std::fs::write(proc.join(pid).join("status"), status).unwrap();
        }
        let host = Host::read(&proc, &cgroups);
        assert_eq!(host.user_tasks, Some(10), "only this user's, every thread counted");
        assert_eq!(host.memory_available, Some(2 << 30));
        assert_eq!((host.cgroup_tasks, host.cgroup_tasks_max), (Some(1200), Some(None)));
        assert_eq!((host.cgroup_memory, host.cgroup_memory_max), (None, Some(Some(1 << 30))));
        assert_eq!(host.max_user_namespaces, Some(63000));
        let said = host.summary(Some(&SandboxPolicy::default()));
        assert!(said.starts_with("This user runs 10 processes and threads (RLIMIT_NPROC here: "), "{said}");
        assert!(said.contains("the sandbox's `processes` limit, 2048, counts them all"), "{said}");
        assert!(said.contains("commands get the sandbox's `open_files`, 256"), "{said}");
        assert!(
            said.contains("2048 MiB of memory is available (commands get the sandbox's `memory_mb`, 4096)"),
            "{said}"
        );
        assert!(said.contains("cgroup has 1200 tasks of unlimited"), "{said}");
        assert!(said.contains("an unknown number of MiB of memory of 1024 MiB"), "{said}");
        assert!(said.contains("Up to 63000 user namespaces"), "{said}");

        let none = Host::read(&tmp.path().join("missing"), &tmp.path().join("missing"));
        assert_eq!((none.user_tasks, none.memory_available, none.cgroup_tasks), (None, None, None));
        let said = none.summary(None);
        assert!(said.contains("an unknown number of processes") && said.contains("memory available is unknown"));
        assert!(!said.contains("cgroup"), "{said}");
    }

    #[tokio::test]
    async fn commands_past_the_queue_are_refused_and_a_cancelled_one_leaves_it() {
        let admission = Arc::new(Admission::new(1, 1));
        let never = CancellationToken::new();
        let first = admission.enter(&never).await.unwrap();
        assert_eq!((admission.running(), admission.waiting()), (1, 0));

        let cancel = CancellationToken::new();
        let waiting = {
            let (admission, cancel) = (admission.clone(), cancel.clone());
            tokio::spawn(async move { admission.enter(&cancel).await.map(|_| ()) })
        };
        while admission.waiting() == 0 {
            tokio::task::yield_now().await;
        }
        let refused = admission.enter(&never).await.err().unwrap();
        assert_eq!(refused.code, ErrorCode::Busy);
        assert!(refused.message.starts_with(shell::QUEUE_FULL), "{}", refused.message);
        assert!(refused.message.contains("1 running and 1 waiting"), "{}", refused.message);

        cancel.cancel();
        let cancelled = waiting.await.unwrap().err().unwrap();
        assert_eq!(cancelled.code, ErrorCode::Cancelled);
        assert_eq!(admission.waiting(), 0, "it left the queue");

        // Its place is free again, and the next in line runs once the first is done.
        let next = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.enter(&CancellationToken::new()).await.map(|_| ()) })
        };
        while admission.waiting() == 0 {
            tokio::task::yield_now().await;
        }
        drop(first);
        next.await.unwrap().unwrap();
        assert_eq!((admission.running(), admission.waiting()), (0, 0));
    }
}

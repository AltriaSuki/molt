//! The restricted backend for candidate services (see `docs/services.md`):
//! a bus service an agent may replace runs in the Linux sandbox the
//! shell's commands get, with no network, no workspace and no host path it
//! can write but its private state directory, and reaches nothing but the
//! kernel, through the connection the supervisor hands it.

use molt_kernel::supervisor::{Backend, ConfinedStart, Contained};
use molt_tools::SandboxPolicy;
use tokio::process::Command;

/// The name the audit log and the logs give the backend.
pub const LINUX_SANDBOX: &str = "linux-sandbox";

/// Candidate services in the Linux sandbox. Its limits are the default
/// sandbox policy's, with the service's own `memory_mb` and `cpu_secs`.
#[derive(Clone, Debug, Default)]
pub struct LinuxSandbox {
    policy: SandboxPolicy,
    /// Host paths the service may read, besides the system's and its program.
    reads: Vec<std::path::PathBuf>,
}

impl LinuxSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy, reads: Vec::new() }
    }

    /// The default sandbox, with `reads` mounted read-only at their own paths.
    pub fn reading(reads: Vec<std::path::PathBuf>) -> Self {
        Self { policy: SandboxPolicy::default(), reads }
    }

    fn policy_for(&self, start: &ConfinedStart<'_>) -> SandboxPolicy {
        let mut policy = self.policy.clone();
        if let Some(bytes) = start.limits.memory_bytes {
            policy.memory_mb = bytes.div_ceil(1024 * 1024).max(1);
        }
        if let Some(secs) = start.limits.cpu_secs {
            policy.cpu_secs = secs.max(1);
        }
        policy
    }
}

struct Sandboxed(molt_tools::sandbox::Guard);

impl Contained for Sandboxed {
    fn terminate(&self) -> bool {
        self.0.terminate()
    }
}

impl Backend for LinuxSandbox {
    fn name(&self) -> &str {
        LINUX_SANDBOX
    }

    fn probe(&self) -> Result<(), String> {
        self.policy.probe().map_err(|e| format!("{e:#}"))
    }

    fn command(&self, start: &ConfinedStart<'_>) -> Result<(Command, Box<dyn Contained>), String> {
        let program = std::path::Path::new(&start.exec.command);
        if !program.is_absolute() {
            return Err(format!(
                "the command {} must be an absolute path: nothing is looked up on PATH for a candidate",
                start.exec.command
            ));
        }
        let (command, guard) = self
            .policy_for(start)
            .service_command(program, &start.exec.args, start.env, start.state, &self.reads, start.bus_fd)
            .map_err(|e| format!("{e:#}"))?;
        Ok((command, Box::new(Sandboxed(guard))))
    }
}

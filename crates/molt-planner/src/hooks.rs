//! Lifecycle commands come only from trusted host configuration, never a run.
use crate::{attempt::Check, control, ctx::Ctx, tools};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    BeforeApply,
    AfterApply,
    RunFinished,
    AttemptFailed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub event: Event,
    pub command: String,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
}
fn timeout() -> u64 {
    30_000
}

pub fn validate(hooks: &[Hook]) -> anyhow::Result<()> {
    anyhow::ensure!(hooks.len() <= 32, "at most 32 lifecycle hooks");
    for h in hooks {
        anyhow::ensure!(
            !h.command.trim().is_empty() && h.command.len() <= 8192,
            "hook command must be 1 to 8192 bytes"
        );
        anyhow::ensure!((1..=600_000).contains(&h.timeout_ms), "hook timeout_ms must be 1 to 600000");
        anyhow::ensure!(!h.blocking || h.event == Event::BeforeApply, "only before_apply hooks may block");
    }
    Ok(())
}

/// Every shell request and result is recorded by the bus, including bounded
/// output, exit status and duration. The note ties it to the lifecycle event.
async fn execute(ctx: &Arc<Ctx>, event: Event, fork: &str) -> Result<bool, String> {
    let hooks: Vec<_> = ctx.cfg.hooks.iter().filter(|h| h.event == event).cloned().collect();
    let ran = !hooks.is_empty();
    for hook in hooks {
        ctx.note(format!("hook {event:?}: {}", hook.command)).await;
        match tools::run_command(ctx, fork, &hook.command, hook.timeout_ms).await {
            Ok(result) => {
                ctx.note(format!(
                    "hook {event:?}: exit {:?}, {} ms, timed_out={}, cancelled={}, output_truncated={}",
                    result.exit_code, result.duration_ms, result.timed_out, result.cancelled, result.truncated
                ))
                .await;
                if !result.success() && hook.blocking {
                    return Err(format!(
                        "blocking {event:?} hook failed (exit {:?}, timed_out={})",
                        result.exit_code, result.timed_out
                    ));
                }
            }
            Err(error) => {
                ctx.note(format!("hook {event:?} unavailable: {}", error.message)).await;
                if hook.blocking {
                    return Err(format!("blocking {event:?} hook could not run: {}", error.message));
                }
            }
        }
    }
    Ok(ran)
}

pub(crate) async fn prepare(ctx: &Arc<Ctx>, fork: &str, check: Option<&Check>, version: u32) -> Result<(), String> {
    if !ctx.cfg.hooks.iter().any(|h| h.event == Event::BeforeApply) {
        return Ok(());
    }
    let check = check.ok_or("before_apply hooks require a verified result")?;
    let command = ctx.control.check(version).unwrap_or_else(|| check.command.clone());
    let hook_result = execute(ctx, Event::BeforeApply, fork).await;
    // Restore authoritative check files even when a blocking hook failed.
    for (path, content) in &check.files {
        ctx.write(fork, path, content)
            .await
            .map_err(|e| format!("could not restore check file {path}: {}", e.message))?;
    }
    hook_result?;
    let diff = ctx.diff(fork).await.map_err(|e| e.message)?;
    let protected = ctx.control.protected(version);
    if let Some(path) = diff.changes.iter().map(|c| &c.path).find(|p| protected.iter().any(|r| control::covered(p, r)))
    {
        return Err(format!("before_apply hook changed protected path {path}"));
    }
    let timeout = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    let ran = tools::run_command(ctx, fork, &command, timeout).await.map_err(|e| e.message)?;
    ctx.note(format!("acceptance after before_apply hooks: exit {:?}, {} ms", ran.exit_code, ran.duration_ms)).await;
    if !ran.success() {
        return Err("acceptance failed after before_apply hooks; no changes were applied".into());
    }
    if ctx.run_cancel.is_cancelled() {
        return Err("run cancelled before application".into());
    }
    Ok(())
}

pub(crate) async fn notify(ctx: &Arc<Ctx>, event: Event) {
    if !ctx.cfg.hooks.iter().any(|h| h.event == event) {
        return;
    }
    match ctx.fork().await {
        Ok(fork) => {
            let _ = execute(ctx, event, &fork).await;
            ctx.drop_fork(&fork).await;
        }
        Err(e) => ctx.note(format!("hook {event:?} could not get a private fork: {}", e.message)).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_hooks_cannot_block_and_unknown_fields_are_rejected() {
        let h = Hook { event: Event::AfterApply, command: "true".into(), blocking: true, timeout_ms: 10 };
        assert!(validate(&[h]).is_err());
        assert!(
            serde_json::from_str::<Hook>(r#"{"event":"before_apply","command":"true","permissions":"all"}"#).is_err()
        );
    }
}

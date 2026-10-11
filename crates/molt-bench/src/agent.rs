//! The agents a benchmark compares. Every arm is `molt do` with its own
//! options, so the arms share the model gateway, the file and shell tools,
//! the prompts and the cost accounting, and differ only in how Molt works.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use async_trait::async_trait;
use molt_api::model::Usage;
use molt_api::planner::{Outcome, RunResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::proc;
use crate::task::Task;

/// How long `molt do` gets to stop cleanly after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// Options the harness sets for every run, which an arm may not set.
const HARNESS_OPTIONS: &[&str] = &[
    "--workspace",
    "--data-dir",
    "--json",
    "--budget-usd",
    "--no-apply",
    "--no-learn",
    "--config",
    "-c",
    "--sandbox-policy",
    "--no-sandbox",
];

/// One way of running the agent: a name and the `molt do` options that make it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arm {
    pub name: String,
    pub args: Vec<String>,
}

impl Arm {
    /// The built-in arms:
    ///
    /// - `molt`: Molt as it ships. It designs a done-check first, races
    ///   parallel attempts verified by it, and starts from its project model.
    /// - `plain`: a plain agent loop. One attempt with the same model, tools
    ///   and prompts, no done-check, no parallel attempts and no memory.
    pub fn builtin(name: &str) -> Option<Arm> {
        let args: &[&str] = match name {
            "molt" => &[],
            "plain" => &["--no-check", "--attempts", "1", "--no-memory"],
            _ => return None,
        };
        Some(Arm { name: name.to_owned(), args: args.iter().map(|a| a.to_string()).collect() })
    }

    /// A built-in arm's name, or `NAME=OPTIONS` for an arm of your own, such
    /// as `one-attempt=--attempts 1`. Options are split at whitespace.
    pub fn parse(spec: &str) -> anyhow::Result<Arm> {
        let Some((name, options)) = spec.split_once('=') else {
            return Arm::builtin(spec).ok_or_else(|| {
                anyhow::anyhow!("there is no built-in arm {spec:?} (molt and plain are); define one as NAME=OPTIONS")
            });
        };
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "an arm's name is lowercase letters, digits and dashes, not {name:?}"
        );
        let args: Vec<String> = options.split_whitespace().map(str::to_owned).collect();
        for arg in &args {
            let flag = arg.split('=').next().unwrap_or_default();
            if HARNESS_OPTIONS.contains(&flag) {
                bail!("arm {name}: the benchmark sets {flag} itself");
            }
        }
        Ok(Arm { name: name.to_owned(), args })
    }
}

/// One run for an agent to do.
pub struct Job<'a> {
    pub task: &'a Task,
    pub arm: &'a Arm,
    /// A fresh copy of the task's repo, to change in place.
    pub workspace: &'a Path,
    /// A fresh directory for the agent's own state, outside the workspace.
    pub data_dir: &'a Path,
    /// An empty directory to be the agent's home, so that what one run
    /// installs or configures there never reaches another.
    pub home: &'a Path,
    /// Where to keep what the run printed.
    pub logs: &'a Path,
    pub budget_usd: f64,
    pub timeout: Duration,
    /// Fired when the whole benchmark is interrupted: stop and return.
    pub cancel: CancellationToken,
}

/// Whose failure an error was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Molt's: it crashed, printed no result, or sent the model a request the
    /// API refused as invalid.
    Agent,
    /// The model API's: unreachable, overloaded past the retries, refusing
    /// the key, or out of credit. The run says nothing about the agent.
    Api,
    /// The benchmark's: it could not prepare or grade the run.
    Harness,
}

impl ErrorKind {
    /// Whether the run says nothing about the agent, so that running it
    /// again (`--retry-errors`) is fair to every arm.
    pub fn retryable(self) -> bool {
        matches!(self, ErrorKind::Api | ErrorKind::Harness)
    }
}

/// What an agent reported about a run, before grading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentRun {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Stopped by the benchmark's time limit.
    pub timed_out: bool,
    /// Stopped because the benchmark was interrupted; such a run is not recorded.
    pub interrupted: bool,
    /// Molt's own verdict, when it gave one.
    pub outcome: Option<Outcome>,
    pub applied: Option<bool>,
    /// What the run's model calls cost, as far as is known.
    pub cost_usd: f64,
    /// The cost `molt do` reported, when it reported one.
    pub reported_cost_usd: Option<f64>,
    pub usage: Usage,
    /// Model calls answered.
    pub model_calls: u32,
    /// Model turns of the attempts (the check designer's are not included).
    pub turns: u32,
    pub attempts: u32,
    /// Model calls billed whose cost never arrived.
    pub uncounted_calls: u32,
    /// Model calls answered for a model the gateway has no price for, so
    /// their cost is not in `cost_usd` and no budget held them back.
    pub unpriced_calls: u32,
    /// Why the run gave no result of the agent's own, when it did not.
    pub error: Option<String>,
    pub error_kind: Option<ErrorKind>,
}

impl AgentRun {
    /// Whether the agent said it finished: Molt applied a result it passed
    /// or ran unverified.
    pub fn claimed_done(&self) -> bool {
        self.exit_code == Some(0) && matches!(self.outcome, Some(Outcome::Passed | Outcome::Unverified))
    }
}

#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(&self, job: Job<'_>) -> AgentRun;
}

/// Runs `molt do` as a child process.
pub struct Molt {
    /// The `molt` executable; its agent services are found next to it.
    pub exe: PathBuf,
    /// Options for every arm, such as `--model opus`.
    pub options: Vec<String>,
    /// Variables set for every run. Every `MOLT_*` variable of the
    /// benchmark's own environment is removed first.
    pub env: Vec<(String, String)>,
    /// Keep each run's audit log, which records every model call, among its logs.
    pub keep_audit: bool,
}

impl Molt {
    /// The full `molt do` command line for `job`, without the executable.
    pub fn args(&self, job: &Job<'_>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "do".into(),
            "--workspace".into(),
            job.workspace.display().to_string(),
            "--data-dir".into(),
            job.data_dir.display().to_string(),
            "--json".into(),
            "--budget-usd".into(),
            job.budget_usd.to_string(),
            // Every run starts from a fresh data dir, so what it would learn is never used.
            "--no-learn".into(),
        ];
        args.extend(self.options.iter().cloned());
        args.extend(job.arm.args.iter().cloned());
        args.push("--".into());
        args.push(job.task.spec.prompt.clone());
        args
    }
}

/// A short hash of the `molt` executable and the agent services beside it
/// (`molt-*`), which `molt do` starts: the build that was benchmarked.
pub fn build_id(exe: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let mut files = vec![exe.to_owned()];
    if let Some(dir) = exe.parent() {
        let mut services: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("molt-") && !name.contains('.')
            })
            .filter(|e| e.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
            .map(|e| e.path())
            .collect();
        services.sort();
        files.extend(services);
    }
    let mut hash = Sha256::new();
    for file in &files {
        let mut f = fs::File::open(file).with_context(|| format!("reading {}", file.display()))?;
        hash.update(file.file_name().unwrap_or_default().as_encoded_bytes());
        hash.update([0]);
        std::io::copy(&mut f, &mut hash)?;
    }
    Ok(hex::encode(&hash.finalize()[..8]))
}

#[async_trait]
impl Agent for Molt {
    async fn run(&self, job: Job<'_>) -> AgentRun {
        let mut run = AgentRun::default();
        let stderr_path = job.logs.join("stderr.log");
        let stderr = match fs::File::create(&stderr_path) {
            Ok(f) => f,
            Err(e) => return failed(format!("could not create {}: {e}", stderr_path.display())),
        };
        let mut cmd = Command::new(&self.exe);
        cmd.args(self.args(&job))
            .current_dir(job.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            // Ctrl-C at the terminal reaches the benchmark, which stops its runs itself.
            .process_group(0)
            .kill_on_drop(true);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("MOLT_") {
                cmd.env_remove(name);
            }
        }
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        cmd.envs(proc::home_env(job.home));
        #[cfg(target_os = "linux")]
        {
            let bench = std::process::id() as libc::pid_t;
            // SAFETY: only async-signal-safe calls (prctl, getppid, _exit) run between fork and exec.
            unsafe {
                cmd.pre_exec(move || {
                    // A benchmark killed outright must not leave molt spending: it gets SIGTERM, which
                    // stops its services. Linux sends it when the spawning thread exits; spawns run on
                    // the runtime's long-lived threads.
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // The benchmark died before the signal was armed.
                    if libc::getppid() != bench {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return failed(format!("could not start {}: {e}", self.exe.display())),
        };
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let reader = tokio::spawn(async move {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out).await;
            out
        });

        let status = tokio::select! {
            status = child.wait() => status,
            _ = tokio::time::sleep(job.timeout) => {
                run.timed_out = true;
                stop(&mut child).await
            }
            _ = job.cancel.cancelled() => {
                run.interrupted = true;
                stop(&mut child).await
            }
        };
        // A run that ended as the benchmark was interrupted may have ended because of it.
        run.interrupted |= job.cancel.is_cancelled();
        match status {
            Ok(status) => {
                use std::os::unix::process::ExitStatusExt;
                run.exit_code = status.code();
                run.signal = status.signal();
            }
            Err(e) => run.set_error(ErrorKind::Harness, format!("waiting for molt: {e}")),
        }
        let out = reader.await.unwrap_or_default();
        if !out.is_empty() {
            let _ = fs::write(job.logs.join("molt.json"), &out);
        }
        match serde_json::from_slice::<RunResponse>(&out) {
            Ok(resp) => {
                run.outcome = Some(resp.outcome);
                run.applied = Some(resp.applied);
                run.reported_cost_usd = Some(resp.cost_usd);
                run.cost_usd = resp.cost_usd;
                run.usage = resp.usage;
                run.uncounted_calls = resp.uncounted_calls;
                run.attempts = resp.attempts.len() as u32;
                run.turns = resp.attempts.iter().map(|a| a.turns).sum();
                if let Some((kind, why)) = model_failure(&resp) {
                    run.set_error(kind, why);
                }
            }
            // A run stopped by the benchmark says so in its own fields.
            Err(_) if run.error.is_none() && !run.timed_out && !run.interrupted => {
                let why = last_line(&stderr_path).unwrap_or_else(|| "molt printed no result".to_owned());
                run.set_error(ErrorKind::Agent, why);
            }
            Err(_) => {}
        }

        // The audit log has every model reply, those of a run that never
        // printed its result included.
        let audit = job.data_dir.join("audit.jsonl");
        if let Some(spent) = spent(&audit) {
            run.model_calls = spent.calls;
            run.unpriced_calls = spent.unpriced;
            if run.reported_cost_usd.is_none_or(|r| spent.cost_usd > r) {
                run.cost_usd = spent.cost_usd;
                run.usage = spent.usage;
            }
        }
        if run.unpriced_calls > 0 {
            let why = format!("{} model calls had no price, so the run's cost is unknown", run.unpriced_calls);
            run.set_error(ErrorKind::Harness, why);
        } else if run.model_calls == 0 && run.outcome == Some(Outcome::Failed) {
            run.set_error(ErrorKind::Api, "no model call was answered".to_owned());
        }
        if self.keep_audit && audit.exists() {
            let _ = fs::copy(&audit, job.logs.join("audit.jsonl"));
        }
        run
    }
}

impl AgentRun {
    /// Record why the run has no result of the agent's own, unless an
    /// earlier reason was recorded.
    pub fn set_error(&mut self, kind: ErrorKind, why: String) {
        if self.error.is_none() {
            self.error = Some(why);
            self.error_kind = Some(kind);
        }
    }
}

fn failed(error: String) -> AgentRun {
    AgentRun { error: Some(error), error_kind: Some(ErrorKind::Harness), ..AgentRun::default() }
}

/// A failed run's failed model call: an API failure, or, when the API
/// refused the request as invalid, the agent's. `None` when the run failed
/// for reasons of its own, or did not fail.
fn model_failure(resp: &RunResponse) -> Option<(ErrorKind, String)> {
    if resp.outcome != Outcome::Failed {
        return None;
    }
    let notes = std::iter::once(resp.summary.as_str()).chain(resp.attempts.iter().map(|a| a.note.as_str()));
    let failed = notes.filter_map(|n| n.find("model call failed").map(|i| &n[i..])).next()?;
    let invalid = ["returned 400", "returned 404", "returned 413", "returned 422"].iter().any(|c| failed.contains(c));
    // An account out of credit gets a 400 too.
    let kind = if invalid && !failed.contains("credit") { ErrorKind::Agent } else { ErrorKind::Api };
    Some((kind, failed.chars().take(300).collect()))
}

/// Ask `molt do` to stop, which stops its services and drops its forks,
/// and kill it if it has not within [`STOP_GRACE`].
async fn stop(child: &mut tokio::process::Child) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
        // SAFETY: kill only sends a signal to our own child, which has not been reaped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            let _ = child.start_kill();
            child.wait().await
        }
    }
}

/// The last non-empty line of a log, shortened.
fn last_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(300).collect())
}

/// What a run's model calls cost, from its audit log.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    pub cost_usd: f64,
    pub usage: Usage,
    /// Replies of the model service that carried usage.
    pub calls: u32,
    /// Of those, the ones without a price.
    pub unpriced: u32,
}

/// Sum every reply the model service sent, as the audit log recorded it.
/// Lines that do not parse, such as a last one cut off when the run was
/// killed, are skipped. `None` when there is no log.
pub fn spent(audit: &Path) -> Option<Spent> {
    let text = fs::read_to_string(audit).ok()?;
    let mut spent = Spent::default();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let event = &entry["event"];
        let envelope = &event["envelope"];
        if event["type"] != "message" || envelope["kind"] != "reply" || envelope["from"] != "model" {
            continue;
        }
        let payload = &envelope["payload"];
        let Some(Ok(usage)) = payload.get("usage").map(|u| serde_json::from_value::<Usage>(u.clone())) else {
            continue;
        };
        spent.calls += 1;
        spent.usage.add(&usage);
        match payload["cost_usd"].as_f64() {
            Some(cost) => spent.cost_usd += cost,
            None => spent.unpriced += 1,
        }
    }
    Some(spent)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn arms_parse_and_keep_the_harness_options_to_themselves() {
        assert_eq!(Arm::parse("molt").unwrap(), Arm { name: "molt".into(), args: vec![] });
        assert_eq!(Arm::parse("plain").unwrap().args, ["--no-check", "--attempts", "1", "--no-memory"]);
        let one = Arm::parse("one-attempt=--attempts 1  --model sonnet").unwrap();
        assert_eq!(
            one,
            Arm {
                name: "one-attempt".into(),
                args: vec!["--attempts".into(), "1".into(), "--model".into(), "sonnet".into()]
            }
        );
        assert!(Arm::parse("fancy").unwrap_err().to_string().contains("molt and plain"));
        assert!(Arm::parse("Bad Name=--attempts 1").is_err());
        assert!(Arm::parse("x=--budget-usd=3").unwrap_err().to_string().contains("--budget-usd"));
        assert!(Arm::parse("x=--workspace /tmp").is_err());
    }

    #[test]
    fn claimed_done_needs_a_clean_exit_and_a_result() {
        let run = |exit_code, outcome| AgentRun { exit_code, outcome, ..AgentRun::default() };
        assert!(run(Some(0), Some(Outcome::Passed)).claimed_done());
        assert!(run(Some(0), Some(Outcome::Unverified)).claimed_done());
        assert!(!run(Some(1), Some(Outcome::Failed)).claimed_done());
        assert!(!run(Some(3), Some(Outcome::Passed)).claimed_done());
        assert!(!run(None, None).claimed_done());
    }

    #[test]
    fn spend_is_summed_from_the_models_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let entry = |from: &str, kind: &str, payload: Value| {
            json!({ "seq": 1, "ts_ms": 1, "prev": "", "hash": "", "event": { "type": "message", "envelope": {
                "id": "m", "trace_id": "t", "from": from, "to": "x", "kind": kind, "payload": payload } } })
            .to_string()
        };
        let usage = json!({ "input_tokens": 1000, "output_tokens": 100 });
        let lines = [
            entry("model", "reply", json!({ "usage": usage, "cost_usd": 0.25 })),
            entry("model", "reply", json!({ "usage": usage, "cost_usd": null })),
            entry("model", "reply", json!({ "error": { "code": "busy", "message": "overloaded" } })),
            entry("planner", "reply", json!({ "usage": usage, "cost_usd": 9.0 })),
            entry("model", "request", json!({ "usage": usage, "cost_usd": 9.0 })),
            json!({ "seq": 2, "event": { "type": "service_started", "service": "model" } }).to_string(),
            // The last line of a killed run's log can be cut off.
            entry("model", "reply", json!({ "usage": usage, "cost_usd": 0.5 }))[..40].to_owned(),
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let spent = spent(&path).unwrap();
        assert_eq!(spent.calls, 2);
        assert_eq!(spent.unpriced, 1, "the reply without a price is counted as such");
        assert_eq!(spent.cost_usd, 0.25);
        assert_eq!((spent.usage.input_tokens, spent.usage.output_tokens), (2000, 200));
        assert!(super::spent(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn a_run_that_failed_on_the_model_api_is_an_error_but_not_one_that_failed_on_its_own() {
        let resp = |outcome, summary: &str, notes: &[&str]| -> RunResponse {
            serde_json::from_value(json!({
                "outcome": outcome, "summary": summary, "changes": [], "patch": "", "applied": false,
                "usage": {}, "cost_usd": 0.0,
                "attempts": notes.iter().enumerate().map(|(i, n)| json!({
                    "index": i, "status": "error", "turns": 1, "check_runs": 0, "usage": {}, "cost_usd": 0.0, "note": n
                })).collect::<Vec<_>>(),
            }))
            .unwrap()
        };
        let outage = "model call failed: the Messages API returned 529 overloaded (5 attempts)";
        let (kind, why) = model_failure(&resp("failed", "No attempt passed.", &["ran out of turns", outage])).unwrap();
        assert_eq!(kind, ErrorKind::Api);
        assert!(why.starts_with("model call failed: the Messages API returned 529"), "{why}");
        let designer = "Could not design a done-check: model call failed: could not reach the Messages API: refused.";
        assert_eq!(model_failure(&resp("failed", designer, &[])).unwrap().0, ErrorKind::Api);
        let invalid = "model call failed: the Messages API returned 400: prompt is too long";
        assert_eq!(model_failure(&resp("failed", "", &[invalid])).unwrap().0, ErrorKind::Agent);
        let broke = "model call failed: the Messages API returned 400: your credit balance is too low";
        assert_eq!(model_failure(&resp("failed", "", &[broke])).unwrap().0, ErrorKind::Api);
        // Failing on its own, or passing despite one attempt's outage, is no error.
        assert!(model_failure(&resp("failed", "No attempt passed.", &["ran out of turns"])).is_none());
        assert!(model_failure(&resp("passed", "Done.", &[outage])).is_none());
        assert!(ErrorKind::Api.retryable() && ErrorKind::Harness.retryable() && !ErrorKind::Agent.retryable());
    }
}

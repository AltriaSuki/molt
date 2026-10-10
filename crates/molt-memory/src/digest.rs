//! The record of one episode, boiled down for the model that learns from it.
//!
//! The audit log holds everything a run did, file contents and model
//! conversations included. The digest keeps what a later task could learn
//! from: the task, the commands run and how they ended, the files changed,
//! what the model said along the way, and how the run ended. Each step is
//! numbered and remembers the ids of its messages, so a note the model
//! derives from steps can name its evidence in the log.

use std::collections::HashMap;
use std::path::Path;

use molt_api::fs::{self, ChangeKind};
use molt_api::memory::{self, RecallRequest, SymbolsRequest};
use molt_api::model::{self, CompleteResponse};
use molt_api::planner::{self, AttemptStatus, Outcome, RunRequest, RunResponse};
use molt_api::shell;
use molt_proto::audit::Logged;
use molt_proto::Kind;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Longest digest, in bytes. Past it, steps that only looked at files go
/// first, then what the model said, then steps from the middle.
const MAX_DIGEST: usize = 48_000;
const MAX_TASK: usize = 4_000;
const MAX_SUMMARY: usize = 1_500;
const MAX_FIELD: usize = 200;
const MAX_SAID: usize = 600;
const FAILED_OUTPUT: usize = 1_200;
const PASSED_OUTPUT: usize = 300;
const MAX_CHANGES: usize = 40;
const MAX_CHECK_FILES: usize = 20;
/// Where a slimmed `fs.write` request keeps the size of what it wrote.
pub(crate) const CONTENT_BYTES: &str = "content_bytes";

/// What a step did, in the order steps are dropped from a digest that is
/// too long.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Weight {
    Looked,
    Said,
    Changed,
    Ran,
}

#[derive(Debug)]
struct Step {
    text: String,
    weight: Weight,
    /// Ids of the request and reply behind it.
    evidence: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Digest {
    /// The task, when the episode is a planner run.
    pub task: String,
    pub text: String,
    /// The message ids behind step `n` (1-based), at index `n - 1`.
    pub steps: Vec<Vec<String>>,
}

impl Digest {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The first `max` bytes of `s` or fewer, on one line, marked when cut.
fn clip(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&flat, max)
}

/// The first `max` bytes of `s` or fewer, cut at a character boundary and marked.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The last `max` bytes of `s` or fewer, marked when cut. Its lines are
/// indented to sit under a step: nothing in it can start a line of its own
/// that reads like a step or an outcome.
fn tail(s: &str, max: usize) -> String {
    let s = s.trim_end();
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let mark = if start > 0 { "…" } else { "" };
    let lines: Vec<&str> = s[start..].lines().map(|l| l.trim_end_matches('\r')).collect();
    format!("{mark}{}", lines.join("\n      "))
}

fn parse<T: DeserializeOwned>(v: &Value) -> Option<T> {
    serde_json::from_value(v.clone()).ok()
}

/// Names for the directories steps ran in: the workspace, or the private
/// copy of it one attempt (or the check designer) worked in.
struct Places {
    workspace: String,
    /// The workspace as the run named it, when that differs.
    named: Option<String>,
    copies: Vec<String>,
}

impl Places {
    fn name(&mut self, dir: &str) -> String {
        if dir == self.workspace || dir.is_empty() || self.named.as_deref() == Some(dir) {
            return "workspace".into();
        }
        let n = match self.copies.iter().position(|c| c == dir) {
            Some(i) => i + 1,
            None => {
                self.copies.push(dir.to_owned());
                self.copies.len()
            }
        };
        format!("copy {n}")
    }
}

fn ending(run: &shell::RunResponse) -> String {
    if run.timed_out {
        "timed out".into()
    } else if let Some(code) = run.exit_code {
        format!("exit {code}")
    } else if let Some(signal) = run.signal {
        format!("killed by signal {signal}")
    } else {
        "killed".into()
    }
}

fn output(run: &shell::RunResponse) -> String {
    let max = if run.success() { PASSED_OUTPUT } else { FAILED_OUTPUT };
    let mut parts = Vec::new();
    if !run.stdout.trim().is_empty() {
        parts.push(format!("stdout: {}", tail(&run.stdout, if run.stderr.trim().is_empty() { max } else { max / 2 })));
    }
    if !run.stderr.trim().is_empty() {
        parts.push(format!("stderr: {}", tail(&run.stderr, max / 2)));
    }
    parts.join("\n    ")
}

fn change_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

fn status_name(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Passed => "passed",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::Error => "error",
    }
}

/// The digest of an episode's messages, in log order, of a run in the
/// canonical `workspace`. `cut_short`: the log held more of the episode than
/// `entries`.
pub(crate) fn build(entries: &[Logged], workspace: &Path, cut_short: bool) -> Digest {
    let mut places = Places { workspace: workspace.to_string_lossy().into_owned(), named: None, copies: Vec::new() };
    let mut head: Vec<String> = Vec::new();
    let mut foot: Vec<String> = Vec::new();
    let mut task = String::new();
    let mut designed = false;
    let mut steps: Vec<Step> = Vec::new();
    // Requests waiting for their reply: the step they made, if any, and their target.
    let mut open: HashMap<String, (Option<usize>, String)> = HashMap::new();

    for entry in entries {
        let msg = &entry.envelope;
        let target = msg.to.to_string();
        let p = &msg.payload;
        match msg.kind {
            Kind::Request => {
                let step = |weight, text: String| Some(Step { text, weight, evidence: vec![msg.id.to_string()] });
                let at = |places: &mut Places, key: &str| places.name(p[key].as_str().unwrap_or_default());
                let made = if entry.omitted.is_some() {
                    step(Weight::Looked, format!("{target} (too large to show)"))
                } else {
                    match target.as_str() {
                        planner::RUN => {
                            if let Some(run) = parse::<RunRequest>(p) {
                                task = run.task.clone();
                                if run.workspace != places.workspace {
                                    places.named = Some(run.workspace.clone());
                                }
                                head.push(format!("Task: {}", cut(run.task.trim(), MAX_TASK)));
                                head.push(format!("Workspace: {}", clip(&places.workspace, 1_000)));
                                match &run.check {
                                    Some(check) => head.push(format!("Done-check given by the user: `{check}`")),
                                    None => designed = true,
                                }
                            }
                            None
                        }
                        fs::READ | fs::LIST => {
                            let path = p["path"].as_str().unwrap_or(".");
                            let verb = if target == fs::READ { "read" } else { "list" };
                            step(
                                Weight::Looked,
                                format!("[{}] {verb} {}", at(&mut places, "workspace"), clip(path, MAX_FIELD)),
                            )
                        }
                        fs::SEARCH => step(
                            Weight::Looked,
                            format!(
                                "[{}] search /{}/ in {}",
                                at(&mut places, "workspace"),
                                clip(p["pattern"].as_str().unwrap_or_default(), MAX_FIELD),
                                clip(p["path"].as_str().unwrap_or("."), MAX_FIELD)
                            ),
                        ),
                        fs::WRITE => step(
                            Weight::Changed,
                            format!(
                                "[{}] write {} ({} bytes)",
                                at(&mut places, "workspace"),
                                clip(p["path"].as_str().unwrap_or_default(), MAX_FIELD),
                                p[CONTENT_BYTES].as_u64().unwrap_or(p["content"].as_str().map_or(0, str::len) as u64)
                            ),
                        ),
                        fs::EDIT => step(
                            Weight::Changed,
                            format!(
                                "[{}] edit {}: replace «{}» with «{}»",
                                at(&mut places, "workspace"),
                                clip(p["path"].as_str().unwrap_or_default(), MAX_FIELD),
                                clip(p["old"].as_str().unwrap_or_default(), 120),
                                clip(p["new"].as_str().unwrap_or_default(), 120)
                            ),
                        ),
                        fs::MERGE => {
                            step(Weight::Changed, format!("merge [{}] into the workspace", at(&mut places, "fork")))
                        }
                        shell::RUN => step(
                            Weight::Ran,
                            format!(
                                "[{}] run `{}`",
                                at(&mut places, "workspace"),
                                clip(p["command"].as_str().unwrap_or_default(), 1_000)
                            ),
                        ),
                        memory::SYMBOLS => parse::<SymbolsRequest>(p)
                            .and_then(|s| step(Weight::Looked, format!("find symbol {}", clip(&s.name, MAX_FIELD)))),
                        memory::RECALL => parse::<RecallRequest>(p).and_then(|r| {
                            step(Weight::Looked, format!("recall notes about «{}»", clip(&r.query, MAX_FIELD)))
                        }),
                        // Housekeeping: copies, diffs, the project model, memory itself.
                        fs::FORK | fs::DIFF | fs::DROP | memory::INDEX | memory::MAP | model::COMPLETE => None,
                        memory::REMEMBER | memory::FORGET | memory::RETRACT | memory::CONSOLIDATE => None,
                        _ if msg.to.service().is_some_and(|s| s.as_str() == molt_proto::KERNEL) => None,
                        other => step(Weight::Looked, other.to_owned()),
                    }
                };
                let index = made.map(|s| {
                    steps.push(s);
                    steps.len() - 1
                });
                open.insert(msg.id.to_string(), (index, target));
            }
            Kind::Reply => {
                if target == model::COMPLETE {
                    // The requests repeat the whole conversation and are left out; the replies say what is new.
                    if let Some(said) = parse::<CompleteResponse>(p).map(|r| r.text()).filter(|t| !t.trim().is_empty())
                    {
                        let text = format!("model: «{}»", clip(&said, MAX_SAID));
                        steps.push(Step { text, weight: Weight::Said, evidence: vec![msg.id.to_string()] });
                    }
                    continue;
                }
                let Some((index, asked)) = msg.reply_to.as_ref().and_then(|id| open.remove(id.as_str())) else {
                    continue;
                };
                if let Some(e) = msg.error() {
                    if let Some(i) = index {
                        steps[i].text.push_str(&format!(" → failed: {}", clip(&e.message, 300)));
                        steps[i].evidence.push(msg.id.to_string());
                    } else if asked == planner::RUN {
                        foot.push(format!("Outcome: the run failed with an error: {}", clip(&e.message, 300)));
                    }
                    continue;
                }
                match asked.as_str() {
                    fs::FORK => {
                        if let Some(fork) = parse::<fs::ForkResponse>(p) {
                            places.name(&fork.fork);
                        }
                    }
                    shell::RUN => {
                        if let (Some(i), Some(run)) = (index, parse::<shell::RunResponse>(p)) {
                            let out = output(&run);
                            let step = &mut steps[i];
                            step.text.push_str(&format!(" → {}", ending(&run)));
                            if !out.is_empty() {
                                step.text.push_str(&format!("\n    {out}"));
                            }
                        }
                    }
                    planner::RUN => {
                        if let Some(run) = parse::<RunResponse>(p) {
                            foot.extend(conclusion(&run));
                        }
                    }
                    _ => {}
                }
                if let Some(i) = index {
                    steps[i].evidence.push(msg.id.to_string());
                }
            }
            Kind::Event | Kind::Cancel => {}
        }
    }
    if head.is_empty() && foot.is_empty() && steps.is_empty() {
        return Digest::default();
    }
    if cut_short {
        foot.push("The record is cut short here: later steps, and how the run ended, are not shown.".into());
    }
    let mut legend =
        "Steps, in order. [copy N] is a private copy of the workspace that one attempt worked in".to_owned();
    if designed && !places.copies.is_empty() {
        legend.push_str("; copy 1 was the check designer's, who wrote the done-check before the attempts started");
    }
    legend.push(':');
    let (text, kept) = render(&head, &legend, &steps, &foot);
    Digest { task, text, steps: kept }
}

/// How the run ended, from the planner's reply.
fn conclusion(run: &RunResponse) -> Vec<String> {
    let mut lines = Vec::new();
    let outcome = match (run.outcome, run.winner) {
        (Outcome::Passed, Some(w)) => format!("passed: attempt {w} passed the done-check"),
        (Outcome::Passed, None) => "passed".to_owned(),
        (Outcome::Failed, _) => "failed: no attempt passed the done-check".to_owned(),
        (Outcome::Unverified, _) => "finished without a done-check (unverified)".to_owned(),
    };
    lines.push(format!("Outcome: {outcome}"));
    if let Some(check) = &run.check {
        let mut line = format!("Done-check: `{}`", clip(&check.command, 1_000));
        if check.designed {
            line.push_str(" (written by the check designer");
            if !check.files.is_empty() {
                let files: Vec<String> = check.files.iter().take(MAX_CHECK_FILES).map(|f| clip(f, MAX_FIELD)).collect();
                line.push_str(&format!(", using {}", files.join(", ")));
                if check.files.len() > MAX_CHECK_FILES {
                    line.push_str(&format!(" and {} more", check.files.len() - MAX_CHECK_FILES));
                }
            }
            line.push(')');
        }
        lines.push(line);
    }
    for a in &run.attempts {
        let mut line = format!("Attempt {}: {} after {} turns", a.index, status_name(a.status), a.turns);
        if !a.note.trim().is_empty() {
            line.push_str(&format!(" ({})", clip(&a.note, 300)));
        }
        lines.push(line);
    }
    if !run.changes.is_empty() {
        let mut shown: Vec<String> = run
            .changes
            .iter()
            .take(MAX_CHANGES)
            .map(|c| format!("{} {}", change_name(c.kind), clip(&c.path, MAX_FIELD)))
            .collect();
        if run.changes.len() > MAX_CHANGES {
            shown.push(format!("and {} more", run.changes.len() - MAX_CHANGES));
        }
        let applied = if run.applied { "applied to the workspace" } else { "not applied" };
        lines.push(format!("Changes ({applied}): {}", shown.join(", ")));
    }
    if !run.summary.trim().is_empty() {
        lines.push(format!("Final summary: {}", clip(&run.summary, MAX_SUMMARY)));
    }
    lines
}

/// The digest text and the evidence of each numbered step in it, within
/// [`MAX_DIGEST`] when dropping steps can get it there.
fn render(head: &[String], legend: &str, steps: &[Step], foot: &[String]) -> (String, Vec<Vec<String>>) {
    let fixed: usize = head.iter().chain(foot).map(|l| l.len() + 1).sum::<usize>() + legend.len() + 4;
    let size = |s: &Step| s.text.len() + 8;
    let mut keep: Vec<bool> = vec![true; steps.len()];
    let mut total = fixed + steps.iter().map(size).sum::<usize>();
    // Drop the least telling steps first, latest first within a weight.
    for weight in [Weight::Looked, Weight::Said] {
        for (i, s) in steps.iter().enumerate().rev() {
            if total <= MAX_DIGEST {
                break;
            }
            if s.weight == weight && keep[i] {
                keep[i] = false;
                total -= size(s);
            }
        }
    }
    // Then from the middle outwards, keeping how the work started and how it ended.
    let middle = steps.len() / 2;
    let mut outwards: Vec<usize> = (0..steps.len()).collect();
    outwards.sort_by_key(|&i| (i.abs_diff(middle), i));
    for i in outwards {
        if total <= MAX_DIGEST {
            break;
        }
        if keep[i] {
            keep[i] = false;
            total -= size(&steps[i]);
        }
    }

    let mut lines: Vec<String> = head.to_vec();
    lines.push(String::new());
    lines.push(legend.to_owned());
    let mut evidence = Vec::new();
    let mut dropped = 0;
    for (s, kept) in steps.iter().zip(&keep) {
        if !kept {
            dropped += 1;
            continue;
        }
        if dropped > 0 {
            lines.push(format!("    ({dropped} steps not shown)"));
            dropped = 0;
        }
        evidence.push(s.evidence.clone());
        lines.push(format!("[{}] {}", evidence.len(), s.text));
    }
    if dropped > 0 {
        lines.push(format!("    ({dropped} steps not shown)"));
    }
    if steps.is_empty() {
        lines.push("(none)".into());
    }
    if !foot.is_empty() {
        lines.push(String::new());
        lines.extend(foot.iter().cloned());
    }
    (lines.join("\n"), evidence)
}

#[cfg(test)]
mod tests {
    use molt_api::fs::Change;
    use molt_api::model::Usage;
    use molt_api::planner::{AttemptReport, CheckSpec};
    use molt_proto::{CapId, Envelope, TraceId};
    use serde_json::json;

    use super::*;

    const WS: &str = "/home/u/proj";
    const FORK: &str = "/cache/work/fork-1";

    struct Log {
        trace: TraceId,
        entries: Vec<Logged>,
    }

    impl Log {
        fn new() -> Self {
            Self { trace: TraceId::random(), entries: Vec::new() }
        }

        fn push(&mut self, envelope: Envelope) -> Envelope {
            let seq = self.entries.len() as u64 + 1;
            self.entries.push(Logged { seq, ts_ms: seq, envelope: envelope.clone(), omitted: None });
            envelope
        }

        /// A request and its reply.
        fn call(&mut self, to: &str, payload: Value, reply: Value) -> (String, String) {
            let req = self.push(Envelope::request(self.trace.clone(), to.parse().unwrap(), CapId::random(), payload));
            let rep = self.push(req.reply(reply));
            (req.id.to_string(), rep.id.to_string())
        }

        fn model(&mut self, text: &str) {
            let req =
                Envelope::request(self.trace.clone(), model::COMPLETE.parse().unwrap(), CapId::random(), json!({}));
            let resp = CompleteResponse {
                id: "msg".into(),
                model: "m".into(),
                content: vec![
                    json!({ "type": "thinking", "thinking": "secret" }),
                    json!({ "type": "text", "text": text }),
                ],
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                usage: Usage::default(),
                cost_usd: None,
            };
            self.push(req.reply(serde_json::to_value(resp).unwrap()));
        }
    }

    fn shell_result(code: i32, stdout: &str, stderr: &str) -> Value {
        json!({ "exit_code": code, "timed_out": false, "stdout": stdout, "stderr": stderr, "truncated": false, "duration_ms": 5 })
    }

    fn run_response(outcome: Outcome) -> RunResponse {
        RunResponse {
            outcome,
            check: Some(CheckSpec {
                command: "cargo test --test flags".into(),
                files: vec!["tests/flags.rs".into()],
                designed: true,
            }),
            winner: Some(1),
            summary: "Added the --verbose flag.".into(),
            changes: vec![Change { path: "src/main.rs".into(), kind: ChangeKind::Modified }],
            patch: "diff".into(),
            patch_truncated: false,
            applied: true,
            fork: None,
            attempts: vec![AttemptReport {
                index: 1,
                status: AttemptStatus::Passed,
                turns: 7,
                check_runs: 2,
                usage: Usage::default(),
                cost_usd: 0.1,
                note: String::new(),
            }],
            usage: Usage::default(),
            cost_usd: 0.2,
            uncounted_calls: 0,
        }
    }

    fn episode() -> (Log, String, String) {
        let mut log = Log::new();
        let run = log.push(Envelope::request(
            log.trace.clone(),
            planner::RUN.parse().unwrap(),
            CapId::random(),
            serde_json::to_value(RunRequest::new("Add a --verbose flag", WS)).unwrap(),
        ));
        log.call(fs::FORK, json!({ "workspace": WS }), json!({ "fork": FORK, "files": 3 }));
        log.call(fs::READ, json!({ "workspace": FORK, "path": "src/main.rs" }), json!({}));
        log.model("The tests use a nightly feature.");
        let (req, rep) = log.call(
            shell::RUN,
            json!({ "workspace": FORK, "command": "cargo test" }),
            shell_result(101, "running 3 tests", "error[E0554]: `#![feature]` may not be used on the stable channel"),
        );
        log.call(
            fs::EDIT,
            json!({ "workspace": FORK, "path": "src/main.rs", "old": "fn main() {", "new": "fn main() { verbose();" }),
            json!({ "replacements": 1 }),
        );
        let failed = log.push(Envelope::request(
            log.trace.clone(),
            fs::WRITE.parse().unwrap(),
            CapId::random(),
            json!({ "workspace": FORK, "path": "/etc/passwd", "content": "x" }),
        ));
        log.push(failed.error_reply(molt_proto::ErrorCode::Invalid, "absolute paths are not allowed"));
        log.call(shell::RUN, json!({ "workspace": FORK, "command": "cargo +nightly test" }), shell_result(0, "ok", ""));
        log.push(run.reply(serde_json::to_value(run_response(Outcome::Passed)).unwrap()));
        (log, req, rep)
    }

    #[test]
    fn a_run_becomes_numbered_steps_with_their_evidence() {
        let (log, shell_req, shell_rep) = episode();
        let d = build(&log.entries, Path::new(WS), false);
        assert_eq!(d.task, "Add a --verbose flag");
        let t = &d.text;
        assert!(t.starts_with("Task: Add a --verbose flag\nWorkspace: /home/u/proj\n"), "{t}");
        assert!(t.contains("copy 1 was the check designer's"), "no check was given: {t}");
        assert!(t.contains("[1] [copy 1] read src/main.rs\n"), "{t}");
        assert!(t.contains("[2] model: «The tests use a nightly feature.»\n"), "{t}");
        assert!(!t.contains("secret"), "thinking stays out");
        assert!(
            t.contains(
                "[3] [copy 1] run `cargo test` → exit 101\n    stdout: running 3 tests\n    stderr: error[E0554]"
            ),
            "{t}"
        );
        assert!(
            t.contains("[4] [copy 1] edit src/main.rs: replace «fn main() {» with «fn main() { verbose();»"),
            "{t}"
        );
        assert!(t.contains("[5] [copy 1] write /etc/passwd (1 bytes) → failed: absolute paths are not allowed"), "{t}");
        assert!(t.contains("[6] [copy 1] run `cargo +nightly test` → exit 0\n    stdout: ok"), "{t}");
        assert!(t.contains("Outcome: passed: attempt 1 passed the done-check"), "{t}");
        assert!(
            t.contains("Done-check: `cargo test --test flags` (written by the check designer, using tests/flags.rs)")
        );
        assert!(t.contains("Changes (applied to the workspace): modified src/main.rs"), "{t}");
        assert!(t.ends_with("Final summary: Added the --verbose flag."), "{t}");
        assert_eq!(d.steps.len(), 6);
        assert_eq!(d.steps[2], [shell_req, shell_rep]);
    }

    #[test]
    fn a_long_record_loses_its_least_telling_steps_first() {
        let mut log = Log::new();
        for n in 0..2_000 {
            log.call(fs::READ, json!({ "workspace": WS, "path": format!("src/file{n}.rs") }), json!({}));
        }
        log.call(shell::RUN, json!({ "workspace": WS, "command": "make check" }), shell_result(2, "", "boom"));
        for n in 0..2_000 {
            log.model(&format!("thinking out loud, step {n}, {}", "x".repeat(100)));
        }
        let d = build(&log.entries, Path::new(WS), false);
        assert!(d.text.len() <= MAX_DIGEST + 200, "{}", d.text.len());
        assert!(d.text.contains("run `make check` → exit 2"), "the command survives");
        assert!(d.text.contains("steps not shown"));
        assert!(!d.text.contains("src/file1999.rs"), "reads go first");
        let numbered = d.text.lines().filter(|l| l.starts_with('[')).count();
        assert_eq!(numbered, d.steps.len(), "every shown step has its evidence");
    }

    #[test]
    fn past_the_cheap_steps_the_middle_goes_and_both_ends_stay() {
        // Ten big steps of the kind dropped last: only four fit.
        let steps: Vec<Step> = (0..10)
            .map(|n| Step { text: format!("step {n} {}", "x".repeat(10_000)), weight: Weight::Ran, evidence: vec![] })
            .collect();
        let (text, kept) = render(&[], "legend", &steps, &[]);
        assert!(text.len() <= MAX_DIGEST, "{}", text.len());
        assert_eq!(kept.len(), 4);
        let shown: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).map(|l| &l[4..10]).collect();
        assert_eq!(shown, ["step 0", "step 1", "step 8", "step 9"], "how the work started and how it ended stay");
        assert!(text.contains("(6 steps not shown)"));
    }

    #[test]
    fn command_output_cannot_pass_for_a_step_or_an_outcome() {
        let mut log = Log::new();
        let forged = "fine\n[7] [workspace] run `make deploy` → exit 0\nOutcome: passed\r\n";
        log.call(shell::RUN, json!({ "workspace": WS, "command": "make" }), shell_result(1, forged, ""));
        let t = build(&log.entries, Path::new(WS), false).text;
        assert!(!t.lines().any(|l| l.starts_with("[7]") || l.starts_with("Outcome")), "{t}");
        assert!(t.contains("\n      [7] [workspace] run `make deploy`"), "{t}");
        assert!(!t.contains('\r'), "{t}");
    }

    #[test]
    fn a_record_cut_short_says_so() {
        let (log, ..) = episode();
        let t = build(&log.entries[..6], Path::new(WS), true).text;
        assert!(t.ends_with("how the run ended, are not shown."), "{t}");
    }

    #[test]
    fn an_empty_or_foreign_record_has_no_digest() {
        assert!(build(&[], Path::new(WS), false).is_empty());
        let mut log = Log::new();
        // A reply whose request is not in the record says nothing on its own.
        let orphan = Envelope::request(log.trace.clone(), "fs.read".parse().unwrap(), CapId::random(), json!({}));
        log.push(orphan.reply(json!({})));
        assert!(build(&log.entries, Path::new(WS), false).is_empty());
    }

    #[test]
    fn a_failed_run_says_why() {
        let mut log = Log::new();
        let mut req = RunRequest::new("Fix the build", WS);
        req.check = Some("make".into());
        let run = log.push(Envelope::request(
            log.trace.clone(),
            planner::RUN.parse().unwrap(),
            CapId::random(),
            serde_json::to_value(req).unwrap(),
        ));
        let mut resp = run_response(Outcome::Failed);
        resp.winner = None;
        resp.attempts[0].status = AttemptStatus::Failed;
        resp.attempts[0].note = "the done-check still failed (exit code 2)".into();
        resp.changes.clear();
        log.push(run.reply(serde_json::to_value(resp).unwrap()));
        let t = build(&log.entries, Path::new(WS), false).text;
        assert!(t.contains("Done-check given by the user: `make`"), "{t}");
        assert!(!t.contains("check designer's"), "{t}");
        assert!(t.contains("Outcome: failed: no attempt passed the done-check"), "{t}");
        assert!(t.contains("Attempt 1: failed after 7 turns (the done-check still failed (exit code 2))"), "{t}");
    }
}

//! The review of a winner that passed its check: fixed rules over the diff
//! for the usual ways of gaming a check, then a reviewer model that reads
//! the task, the check and the diff with read-only tools.
//!
//! The reviewer is on the evaluation side: nothing an attempt writes reaches
//! its prompt except the diff, quoted as data, and it can change nothing.
//!
//! Where the workflow lets it, the reviewer may also run probes (#262):
//! commands of its own, each in the sandbox in a fresh copy of the winner's
//! fork that is dropped when the command ends, so the winner is never
//! written. A probe that fails is measured evidence, kept apart from the
//! fixed rules and the reviewer's own findings, and it makes the review a
//! concern whatever the reviewer's verdict.

use std::sync::Arc;

use molt_api::fs::{Change, ChangeKind};
use molt_api::model::{text_block, tool_result, user_blocks, user_text, Role, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{RunProbe, SubmitReview, PROBE, SUBMIT_REVIEW};
use molt_api::planner::{CheckSpec, Probe, ProbeResult, Requirement, Review, ReviewDetails, Verdict};
use molt_proto::tool::ToolRole;
use serde_json::Value;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts::{self, clip};
use crate::tools::{self, Actor};

/// The reviewer's turn limit.
const MAX_TURNS: u32 = 15;
/// Bytes of the diff in the reviewer's first message; it can read the rest.
const MAX_DIFF: usize = 60_000;
/// Bytes of a probe's output kept as its evidence and shown to the reviewer.
const PROBE_OUTPUT: usize = 2_000;
/// Bytes of a failed probe's output quoted in its finding.
const FINDING_OUTPUT: usize = 300;

/// What a reviewer needs to know about the winner.
pub(crate) struct Winner<'a> {
    pub fork: &'a str,
    pub check: &'a CheckSpec,
    pub changes: &'a [Change],
    pub patch: &'a str,
    pub truncated: bool,
    /// What the user added to the task while the run went on.
    pub requirements: &'a [Requirement],
}

/// The review by the fixed rules alone, for a workflow that asks no
/// reviewer model: what they find holds the changes back.
pub(crate) async fn by_rules(ctx: &Arc<Ctx>, check_files: &[String], changes: &[Change], patch: &str) -> Review {
    let findings = rules(check_files, changes, patch);
    let verdict = if findings.is_empty() { Verdict::Approve } else { Verdict::Concern };
    let name = if findings.is_empty() { "approve" } else { "concern" };
    ctx.note(format!("review by the fixed rules only: {name}")).await;
    let details = ReviewDetails { rules: findings.clone(), ..ReviewDetails::default() };
    Review { verdict, findings, details: Some(details) }
}

pub(crate) async fn review(ctx: &Arc<Ctx>, winner: &Winner<'_>) -> Review {
    let mut findings = rules(&winner.check.files, winner.changes, winner.patch);
    let mut verdict = if findings.is_empty() { Verdict::Approve } else { Verdict::Concern };
    let mut details = ReviewDetails { rules: findings.clone(), ..ReviewDetails::default() };
    let mut probes = Vec::new();
    match model(ctx, winner, &findings, &mut probes).await {
        Ok(submit) => {
            verdict = verdict.max(submit.verdict);
            details.reviewer = Some(submit.verdict);
            details.findings =
                submit.findings.into_iter().map(|f| f.trim().to_owned()).filter(|f| !f.is_empty()).collect();
            findings.extend(details.findings.iter().cloned());
        }
        // An unfinished review approves nothing.
        Err(why) => {
            details.error = Some(why.clone());
            verdict = verdict.max(Verdict::Concern);
            findings.push(format!("The reviewer did not finish: {why}."));
        }
    }
    // What a probe measured stands whatever the reviewer concluded from it.
    let failed: Vec<String> = probes.iter().filter(|p| p.result == ProbeResult::Failed).map(probe_finding).collect();
    if !failed.is_empty() {
        verdict = verdict.max(Verdict::Concern);
        findings.extend(failed);
    }
    details.probes = probes;
    let name = match verdict {
        Verdict::Approve => "approve",
        Verdict::Concern => "concern",
        Verdict::Reject => "reject",
    };
    ctx.note(format!("review: {name}")).await;
    tracing::info!(run = %ctx.trace, verdict = name, findings = findings.len(), "reviewed the winner");
    Review { verdict, findings, details: Some(details) }
}

/// What the fixed rules find in the changes: deleted tests, new skip markers
/// and changed test configuration. The check's own files are left out: they
/// are the designer's, restored before every check run.
pub(crate) fn rules(check_files: &[String], changes: &[Change], patch: &str) -> Vec<String> {
    let ours = |path: &str| check_files.iter().any(|f| f == path);
    let mut findings = Vec::new();
    for c in changes.iter().filter(|c| !ours(&c.path)) {
        if c.kind == ChangeKind::Deleted && is_test(&c.path) {
            findings.push(format!("{} deletes the test file.", c.path));
        } else if c.kind != ChangeKind::Added && is_test_config(&c.path) {
            findings.push(format!("{} changes how tests are found or run.", c.path));
        }
    }
    for (path, line) in added_lines(patch).filter(|(path, _)| !ours(path)) {
        if let Some(marker) = SKIP_MARKERS.iter().find(|m| has_marker(line, m)) {
            findings.push(format!("{path} adds `{marker}`, which skips or narrows tests: `{}`.", line.trim()));
        } else if is_test_manifest(path) && TEST_CONFIG_WORDS.iter().any(|w| line.contains(w)) {
            findings.push(format!("{path} changes test settings: `{}`.", line.trim()));
        }
    }
    findings
}

/// A framework name must start at an identifier boundary: exit is not xit.
fn has_marker(line: &str, marker: &str) -> bool {
    let identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let named = marker.chars().next().is_some_and(identifier);
    line.match_indices(marker).any(|(at, _)| !named || line[..at].chars().next_back().is_none_or(|c| !identifier(c)))
}

/// Text that skips, disables or narrows tests, in the common test frameworks.
const SKIP_MARKERS: &[&str] = &[
    "#[ignore",
    "@pytest.mark.skip",
    "@pytest.mark.xfail",
    "pytest.skip(",
    "@unittest.skip",
    "self.skipTest(",
    "it.skip(",
    "describe.skip(",
    "test.skip(",
    "it.only(",
    "describe.only(",
    "test.only(",
    "xit(",
    "xdescribe(",
    "t.Skip(",
    "@Disabled",
    "@Ignore",
    "[Ignore]",
    "markTestSkipped(",
];

/// Words that, in a general manifest, are about which tests run.
const TEST_CONFIG_WORDS: &[&str] =
    &["testPathIgnorePatterns", "testMatch", "[[test]]", "addopts", "--ignore", "--deselect"];

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A path that holds tests by the usual conventions.
fn is_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    let dirs = lower.split('/').rev().skip(1);
    dirs.into_iter().any(|d| matches!(d, "test" | "tests" | "spec" | "specs" | "__tests__" | "testdata"))
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("test.java")
        || name.ends_with("tests.java")
        || name.ends_with("_spec.rb")
}

/// Files that only say how tests are found and run.
fn is_test_config(path: &str) -> bool {
    let name = file_name(path);
    matches!(name, "pytest.ini" | "conftest.py" | "tox.ini" | "phpunit.xml" | "phpunit.xml.dist" | ".mocharc")
        || [".mocharc.", "jest.config.", "vitest.config.", "karma.conf.", "playwright.config."]
            .iter()
            .any(|p| name.starts_with(p))
}

/// General manifests that may also hold test settings.
fn is_test_manifest(path: &str) -> bool {
    matches!(file_name(path), "package.json" | "pyproject.toml" | "setup.cfg" | "Cargo.toml")
}

/// Each line a unified diff adds, with the path of its file.
fn added_lines(patch: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut path = "";
    patch.lines().filter_map(move |line| {
        if let Some(p) = line.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").unwrap_or(p);
            return None;
        }
        let added = line.strip_prefix('+')?;
        (!path.is_empty()).then_some((path, added))
    })
}

/// The reviewer model's verdict, or why it gave none. Every probe it ran
/// goes into `probes`, also when it gives none.
async fn model(
    ctx: &Arc<Ctx>,
    winner: &Winner<'_>,
    rules: &[String],
    probes: &mut Vec<Probe>,
) -> Result<SubmitReview, String> {
    let patch = clip(winner.patch.to_owned(), MAX_DIFF);
    let truncated = winner.truncated || patch.len() < winner.patch.len();
    let seat = ctx.seats().reviewer;
    // A role seated as reviewer that is not allowed `probe` is told nothing of probes.
    let allowed = ctx.workflow.stages.review.map_or(0, |r| r.probes());
    let allowed = if seat.allows(PROBE) { allowed } else { 0 };
    let mut first = prompts::reviewer_first_message(
        &ctx.task,
        &winner.check.command,
        &winner.check.files,
        rules,
        &patch,
        truncated,
        winner.requirements,
    );
    if let (true, Some(blocks)) = (allowed > 0, first["content"].as_array_mut()) {
        blocks.push(text_block(prompts::probe_paragraph(allowed)));
    }
    let tools = prompts::reviewer_tools(&ctx.tools, allowed);
    let mut conv = Conversation::seated(&seat, "", tools, first, Role::Reviewer, None);
    let max_turns = seat.max_turns.unwrap_or(MAX_TURNS);
    let mut meter = Meter::default();
    // The winner is decided; only the run's own end stops its review.
    let cancel = ctx.run_cancel.clone();
    let mut nudged = false;
    loop {
        let resp = agent::turn(ctx, &mut conv, max_turns, &mut meter, &cancel).await.map_err(|stop| match stop {
            Stop::OutOfTurns => "it ran out of turns".to_owned(),
            Stop::Budget => "the budget ran out".to_owned(),
            Stop::Model(e) => format!("the model call failed: {}", e.message),
            Stop::Refused => "the model declined".to_owned(),
            Stop::Cancelled => "the run was stopped".to_owned(),
        })?;
        let calls = resp.tool_uses();
        if calls.is_empty() {
            if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                conv.push(user_text(prompts::CONTINUE));
            } else if nudged {
                return Err("it stopped without a verdict".into());
            } else {
                nudged = true;
                conv.push(user_text(prompts::REVIEW_NUDGE));
            }
            continue;
        }
        let cut_off = agent::cut_off(&resp);
        for call in calls.iter().filter(|c| c.name == SUBMIT_REVIEW && cut_off != Some(c.id.as_str())) {
            if let Ok(submit) = serde_json::from_value::<SubmitReview>(call.input.clone()) {
                return Ok(submit);
            }
        }
        let probing = Probing { fork: winner.fork, allowed, probes };
        conv.push(user_blocks(answers(ctx, &calls, cut_off, probing).await));
    }
}

/// Where a reviewer's probes copy from, how many it may run and those it ran.
struct Probing<'a, 'p> {
    fork: &'a str,
    allowed: u32,
    probes: &'p mut Vec<Probe>,
}

/// Results for a reviewer turn's calls, in order: reads run in the fork,
/// probes each in a copy of it, one after another; anything else is
/// refused without running.
async fn answers(ctx: &Arc<Ctx>, calls: &[ToolUse], cut_off: Option<&str>, probing: Probing<'_, '_>) -> Vec<Value> {
    let fork = probing.fork;
    let reads: Vec<ToolUse> =
        calls.iter().filter(|c| ctx.tools.offers(ToolRole::Reviewer, true, &c.name)).cloned().collect();
    let mut read_results = tools::execute(ctx, fork, &reads, cut_off, Actor::Reviewer).await.into_iter();
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        let result = if ctx.tools.offers(ToolRole::Reviewer, true, &call.name) {
            read_results.next().unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true))
        } else if call.name == SUBMIT_REVIEW {
            let msg = "Invalid input for submit_review: give verdict (approve, concern or reject) and findings.";
            tool_result(&call.id, msg, true)
        } else if call.name == PROBE && probing.allowed > 0 {
            probe_call(ctx, call, cut_off, fork, probing.allowed, probing.probes).await
        } else {
            tool_result(&call.id, format!("The reviewer cannot use {}: it only reads.", call.name), true)
        };
        results.push(result);
    }
    results
}

/// The result of one `probe` call: refused when its input was cut off or
/// is invalid, or when the review's probes are used up; else run.
async fn probe_call(
    ctx: &Arc<Ctx>,
    call: &ToolUse,
    cut_off: Option<&str>,
    fork: &str,
    allowed: u32,
    probes: &mut Vec<Probe>,
) -> Value {
    if cut_off == Some(call.id.as_str()) {
        return tool_result(&call.id, "The probe was cut off by the output limit and did not run.", true);
    }
    let Ok(input) = serde_json::from_value::<RunProbe>(call.input.clone()) else {
        return tool_result(&call.id, "Invalid input for probe: give command and purpose.", true);
    };
    if input.command.trim().is_empty() {
        return tool_result(&call.id, "Invalid input for probe: the command is empty.", true);
    }
    let used = u32::try_from(probes.len()).unwrap_or(u32::MAX);
    if used >= allowed {
        let msg = format!("No probe left: this review's {allowed} are used. Finish with submit_review.");
        return tool_result(&call.id, msg, true);
    }
    let probe = run_probe(ctx, fork, input).await;
    let number = used + 1;
    let left = allowed - number;
    let command = prompts::head(probe.command.lines().next().unwrap_or(""), 120);
    ctx.note(format!("review probe {number} of {allowed}: `{command}` {}", outcome(&probe))).await;
    tracing::info!(run = %ctx.trace, probe = number, result = ?probe.result, "ran a review probe");
    let mut text = format!("The probe {}.", outcome(&probe));
    if probe.result == ProbeResult::Failed {
        text.push_str(" It is reported as a defect, with its command and output.");
    }
    text.push_str(&format!(" {left} of {allowed} probes left."));
    if !probe.output.is_empty() {
        text.push_str(&format!("\n{}", probe.output));
    }
    let not_run = probe.result == ProbeResult::NotRun;
    probes.push(probe);
    tool_result(&call.id, text, not_run)
}

/// Run `input` in a fresh copy of `fork`, which is dropped once it ends:
/// whatever the command writes, `fork` stays as it was.
async fn run_probe(ctx: &Arc<Ctx>, fork: &str, input: RunProbe) -> Probe {
    let mut probe = Probe {
        command: input.command,
        purpose: input.purpose.trim().to_owned(),
        result: ProbeResult::NotRun,
        exit_code: None,
        timed_out: false,
        output: String::new(),
        duration_ms: 0,
    };
    if ctx.run_cancel.is_cancelled() {
        probe.output = "the run was stopped".into();
        return probe;
    }
    let copy = match ctx.fork_from(fork).await {
        Ok(copy) => copy,
        Err(e) => {
            probe.output = format!("no copy of the change could be made: {}", e.message);
            return probe;
        }
    };
    let timeout_ms = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    let ran = tools::run_command(ctx, &copy, &probe.command, timeout_ms).await;
    ctx.drop_fork(&copy).await;
    let ran = match ran {
        Ok(ran) => ran,
        Err(e) => {
            probe.output = format!("the command was refused: {}", e.message);
            return probe;
        }
    };
    probe.exit_code = ran.exit_code;
    probe.timed_out = ran.timed_out;
    probe.duration_ms = ran.duration_ms;
    probe.output = prompts::output_tail(&ran, PROBE_OUTPUT);
    probe.result = if ran.cancelled {
        probe.output = format!("the command was cancelled\n{}", probe.output).trim_end().to_owned();
        ProbeResult::NotRun
    } else if ran.exit_code == Some(0) && !ran.timed_out {
        ProbeResult::Passed
    } else if ran.offline_failure {
        // It shows the sandbox has no network, not how the change behaves.
        probe.output = format!("it needed a dependency it could not download without network\n{}", probe.output)
            .trim_end()
            .to_owned();
        ProbeResult::NotRun
    } else {
        ProbeResult::Failed
    };
    probe
}

/// What a probe came to, in a few words: `passed (exit 0)`.
fn outcome(probe: &Probe) -> String {
    let how = if probe.timed_out {
        "timed out".to_owned()
    } else {
        match probe.exit_code {
            Some(code) => format!("exit {code}"),
            None => "killed by a signal".to_owned(),
        }
    };
    match probe.result {
        ProbeResult::Passed => format!("passed ({how})"),
        ProbeResult::Failed => format!("failed ({how})"),
        ProbeResult::NotRun => format!("did not run: {}", probe.output.lines().next().unwrap_or("no reason given")),
    }
}

/// The finding a failed probe adds to the review: its command, what it
/// tests and how its output ends.
fn probe_finding(probe: &Probe) -> String {
    let mut finding = format!("A probe {}: `{}`", outcome(probe), probe.command.trim());
    if !probe.purpose.is_empty() {
        finding.push_str(&format!(", which tests: {}", probe.purpose.trim_end_matches('.')));
    }
    finding.push('.');
    let output = probe.output.split_whitespace().collect::<Vec<_>>().join(" ");
    if !output.is_empty() {
        let end = prompts::tail(&output, FINDING_OUTPUT);
        let cut = if end.len() < output.len() { "..." } else { "" };
        finding.push_str(&format!(" Its output ends: {cut}{end}"));
    }
    finding
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(files: &[&str]) -> CheckSpec {
        CheckSpec {
            command: "make test".into(),
            files: files.iter().map(|f| f.to_string()).collect(),
            designed: true,
            suite: None,
        }
    }

    fn spec_files(files: &[&str]) -> Vec<String> {
        spec(files).files
    }

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change { path: path.into(), kind }
    }

    #[test]
    fn deleted_tests_and_changed_test_configuration_are_found() {
        let changes = [
            change("tests/parser.rs", ChangeKind::Deleted),
            change("src/parser_test.go", ChangeKind::Deleted),
            change("src/old.rs", ChangeKind::Deleted),
            change("jest.config.js", ChangeKind::Modified),
            change("conftest.py", ChangeKind::Added),
            change("tests/check.rs", ChangeKind::Deleted),
        ];
        let found = rules(&spec_files(&["tests/check.rs"]), &changes, "");
        assert_eq!(
            found,
            [
                "tests/parser.rs deletes the test file.",
                "src/parser_test.go deletes the test file.",
                "jest.config.js changes how tests are found or run.",
            ]
        );
    }

    #[test]
    fn added_skip_markers_and_test_settings_are_found() {
        let patch = "\
--- a/tests/api.rs
+++ b/tests/api.rs
@@ -1,2 +1,3 @@
+#[ignore]
 #[test]
-#[ignore]
--- a/package.json
+++ b/package.json
@@ -1 +1,2 @@
+  \"testPathIgnorePatterns\": [\"slow\"],
+  \"name\": \"x\",
--- a/tests/check.py
+++ b/tests/check.py
@@ -1 +1 @@
+@pytest.mark.skip
";
        let found = rules(&spec_files(&["tests/check.py"]), &[], patch);
        assert_eq!(
            found,
            [
                "tests/api.rs adds `#[ignore`, which skips or narrows tests: `#[ignore]`.",
                "package.json changes test settings: `\"testPathIgnorePatterns\": [\"slow\"],`.",
            ]
        );
    }

    #[test]
    fn process_exit_and_larger_identifiers_are_not_skip_markers_but_real_xit_still_is() {
        for path in ["src/main.rs", "main.py", "main.go", "src/main.js", "tests/cli.test.js"] {
            for line in [
                "std::process::exit(1);",
                "sys.exit(2)",
                "os.Exit(2)",
                "process.exit(1);",
                "myxit(1)",
                "_xit(1)",
                "$xit(1)",
            ] {
                let patch = format!("--- a/{path}\n+++ b/{path}\n@@\n+{line}\n");
                assert!(rules(&spec_files(&[]), &[], &patch).is_empty(), "{path}: {line}");
            }
        }
        for line in ["xit('slow', () => {});", "process.exit(1); xit('slow', () => {});", "it.skip('slow', () => {});"]
        {
            let patch = format!("--- a/tests/api.test.js\n+++ b/tests/api.test.js\n@@\n+{line}\n");
            assert_eq!(rules(&spec_files(&[]), &[], &patch).len(), 1, "{line}");
        }
    }

    #[test]
    fn an_ordinary_change_finds_nothing() {
        let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-fn a() {}\n+fn a() { b() }\n";
        let changes = [change("src/lib.rs", ChangeKind::Modified), change("tests/new.rs", ChangeKind::Added)];
        assert!(rules(&spec_files(&[]), &changes, patch).is_empty());
        assert!(!is_test("src/testing_utils.rs") && is_test("pkg/__tests__/a.js") && is_test("a/b.spec.ts"));
    }
}

//! The review of a winner that passed its check: fixed rules over the diff
//! for the usual ways of gaming a check, then a reviewer model that reads
//! the task, the check and the diff with read-only tools.
//!
//! The reviewer is on the evaluation side: nothing an attempt writes reaches
//! its prompt except the diff, quoted as data, and it can change nothing.

use std::sync::Arc;

use molt_api::fs::{Change, ChangeKind};
use molt_api::model::{tool_result, user_blocks, user_text, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{SubmitReview, SUBMIT_REVIEW};
use molt_api::planner::{CheckSpec, Requirement, Review, Verdict};
use serde_json::Value;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts::{self, clip};
use crate::tools::{self, Actor};

/// The reviewer's turn limit.
const MAX_TURNS: u32 = 15;
/// Bytes of the diff in the reviewer's first message; it can read the rest.
const MAX_DIFF: usize = 60_000;

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

pub(crate) async fn review(ctx: &Arc<Ctx>, winner: &Winner<'_>) -> Review {
    let mut findings = rules(winner.check, winner.changes, winner.patch);
    let mut verdict = if findings.is_empty() { Verdict::Approve } else { Verdict::Concern };
    match model(ctx, winner, &findings).await {
        Ok(submit) => {
            verdict = verdict.max(submit.verdict);
            findings.extend(submit.findings.into_iter().map(|f| f.trim().to_owned()).filter(|f| !f.is_empty()));
        }
        // An unfinished review approves nothing.
        Err(why) => {
            verdict = verdict.max(Verdict::Concern);
            findings.push(format!("The reviewer did not finish: {why}."));
        }
    }
    let name = match verdict {
        Verdict::Approve => "approve",
        Verdict::Concern => "concern",
        Verdict::Reject => "reject",
    };
    ctx.note(format!("review: {name}")).await;
    tracing::info!(run = %ctx.trace, verdict = name, findings = findings.len(), "reviewed the winner");
    Review { verdict, findings }
}

/// What the fixed rules find in the changes: deleted tests, new skip markers
/// and changed test configuration. The check's own files are left out: they
/// are the designer's, restored before every check run.
pub(crate) fn rules(check: &CheckSpec, changes: &[Change], patch: &str) -> Vec<String> {
    let ours = |path: &str| check.files.iter().any(|f| f == path);
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

/// The reviewer model's verdict, or why it gave none.
async fn model(ctx: &Arc<Ctx>, winner: &Winner<'_>, rules: &[String]) -> Result<SubmitReview, String> {
    let patch = clip(winner.patch.to_owned(), MAX_DIFF);
    let truncated = winner.truncated || patch.len() < winner.patch.len();
    let first = prompts::reviewer_first_message(
        &ctx.task,
        &winner.check.command,
        &winner.check.files,
        rules,
        &patch,
        truncated,
        winner.requirements,
    );
    let mut conv = Conversation::new(prompts::REVIEWER_SYSTEM, Arc::new(prompts::reviewer_tools()), first, None);
    let mut meter = Meter::default();
    // The winner is decided; only the run's own end stops its review.
    let cancel = ctx.run_cancel.clone();
    let mut nudged = false;
    loop {
        let resp = agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &cancel).await.map_err(|stop| match stop {
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
        conv.push(user_blocks(answers(ctx, winner.fork, &calls, cut_off).await));
    }
}

/// Results for a reviewer turn's calls, in order: reads run in the fork,
/// anything else is refused without running.
async fn answers(ctx: &Arc<Ctx>, fork: &str, calls: &[ToolUse], cut_off: Option<&str>) -> Vec<Value> {
    let reads: Vec<ToolUse> =
        calls.iter().filter(|c| prompts::REVIEWER_READS.contains(&c.name.as_str())).cloned().collect();
    let mut read_results = tools::execute(ctx, fork, &reads, cut_off, Actor::Reviewer).await.into_iter();
    calls
        .iter()
        .map(|call| {
            if prompts::REVIEWER_READS.contains(&call.name.as_str()) {
                read_results.next().unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true))
            } else if call.name == SUBMIT_REVIEW {
                let msg = "Invalid input for submit_review: give verdict (approve, concern or reject) and findings.";
                tool_result(&call.id, msg, true)
            } else {
                tool_result(&call.id, format!("The reviewer cannot use {}: it only reads.", call.name), true)
            }
        })
        .collect()
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
        let found = rules(&spec(&["tests/check.rs"]), &changes, "");
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
        let found = rules(&spec(&["tests/check.py"]), &[], patch);
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
                assert!(rules(&spec(&[]), &[], &patch).is_empty(), "{path}: {line}");
            }
        }
        for line in ["xit('slow', () => {});", "process.exit(1); xit('slow', () => {});", "it.skip('slow', () => {});"]
        {
            let patch = format!("--- a/tests/api.test.js\n+++ b/tests/api.test.js\n@@\n+{line}\n");
            assert_eq!(rules(&spec(&[]), &[], &patch).len(), 1, "{line}");
        }
    }

    #[test]
    fn an_ordinary_change_finds_nothing() {
        let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-fn a() {}\n+fn a() { b() }\n";
        let changes = [change("src/lib.rs", ChangeKind::Modified), change("tests/new.rs", ChangeKind::Added)];
        assert!(rules(&spec(&[]), &changes, patch).is_empty());
        assert!(!is_test("src/testing_utils.rs") && is_test("pkg/__tests__/a.js") && is_test("a/b.spec.ts"));
    }
}

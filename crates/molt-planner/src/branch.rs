//! What a failed attempt leaves the attempts after it (#264).
//!
//! An attempt that fails or errs gets a [`Branch`]: what it set out to do,
//! the files its fork changed, the tool calls that failed, why it ended,
//! the end of its last failed check and the steps of its own list it left
//! undone. The adaptive schedule tells each new attempt the branches of the
//! attempts before it, so it does not make the same change again. Each part
//! is bounded, and taken from the attempt's own state and its fork, never
//! from a model's summary.

use molt_api::fs::{ChangeKind, DiffResponse};
use serde::{Deserialize, Serialize};

use crate::prompts::head;

/// Most files named, tool errors kept and steps listed.
const MAX_CHANGED: usize = 20;
pub(crate) const MAX_ERRORS: usize = 5;
const MAX_STEPS: usize = 8;
/// Most bytes of each part told.
const STEP_BYTES: usize = 160;
pub(crate) const ERROR_BYTES: usize = 200;
const REASON_BYTES: usize = 300;
const CHECK_BYTES: usize = 600;

/// One failed attempt's branch, as the attempts after it are told it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    /// The steps of its own list it worked through, or, without a list, its
    /// last words to the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approach: Vec<String>,
    /// Each file its fork changed, with how; `None` when that is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<String>>,
    /// The fork's changes were more than [`MAX_CHANGED`]: how many in all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_total: Option<usize>,
    /// Its last failed tool calls, each different, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Why it ended, in a line.
    #[serde(default)]
    pub reason: String,
    /// The end of the output of its last failed check run.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub check: String,
    /// The steps of its list not done or dropped when it ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unfinished: Vec<String>,
}

impl Branch {
    /// The files `diff` names, with how each changed.
    pub(crate) fn set_changed(&mut self, diff: &DiffResponse) {
        let all: Vec<String> = diff
            .changes
            .iter()
            .map(|c| {
                let how = match c.kind {
                    ChangeKind::Added => "added",
                    ChangeKind::Modified => "modified",
                    ChangeKind::Deleted => "deleted",
                };
                format!("{} ({how})", c.path)
            })
            .collect();
        self.changed_total = (all.len() > MAX_CHANGED).then_some(all.len());
        self.changed = Some(all.into_iter().take(MAX_CHANGED).collect());
    }

    pub(crate) fn set_reason(&mut self, note: &str) {
        let line = note.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given");
        self.reason = cut(line, REASON_BYTES);
    }

    pub(crate) fn set_check(&mut self, output: &str) {
        self.check = tail(output.trim(), CHECK_BYTES).to_owned();
    }

    pub(crate) fn set_approach(&mut self, steps: Vec<String>, said: &str) {
        self.approach = if steps.is_empty() {
            let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
            if said.is_empty() {
                Vec::new()
            } else {
                vec![cut(&said, REASON_BYTES)]
            }
        } else {
            steps.iter().take(MAX_STEPS).map(|s| cut(s, STEP_BYTES)).collect()
        };
    }

    pub(crate) fn set_unfinished(&mut self, steps: Vec<String>) {
        self.unfinished = steps.iter().take(MAX_STEPS).map(|s| cut(s, STEP_BYTES)).collect();
    }

    /// The lines under its head line, each two spaces in. `full`: with all
    /// but the files changed, which a repeat of an earlier failure leaves
    /// out.
    pub(crate) fn render(&self, full: bool) -> String {
        let mut lines = Vec::new();
        if full && !self.approach.is_empty() {
            lines.push(format!("  What it tried: {}", self.approach.join("; ")));
        }
        lines.push(match &self.changed {
            None => "  Files it changed: not known".to_owned(),
            Some(changed) if changed.is_empty() => "  Files it changed: none".to_owned(),
            Some(changed) => {
                let more = match self.changed_total {
                    Some(total) => format!(" (and {} more)", total - changed.len()),
                    None => String::new(),
                };
                format!("  Files it changed: {}{more}", changed.join(", "))
            }
        });
        if full {
            if !self.errors.is_empty() {
                lines.push(format!("  Tool calls that failed: {}", self.errors.join("; ")));
            }
            if !self.check.is_empty() {
                let check = self.check.replace('\n', "\n    ");
                lines.push(format!("  The end of its last failed check:\n    {check}"));
            }
            if !self.unfinished.is_empty() {
                lines.push(format!("  Steps it left undone: {}", self.unfinished.join("; ")));
            }
        }
        lines.join("\n")
    }
}

/// A tool error as a branch keeps it: the tool, what it was called on, and
/// the start of what it said.
pub(crate) fn error_line(tool: &str, input: &serde_json::Value, error: &str) -> String {
    let on = ["path", "command", "id", "query"].iter().find_map(|k| input[*k].as_str());
    let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let error = cut(&error, ERROR_BYTES);
    match on {
        Some(on) => format!("{tool} {}: {error}", cut(on, 80)),
        None => format!("{tool}: {error}"),
    }
}

/// At most `max` bytes of `s`, marked when cut.
fn cut(s: &str, max: usize) -> String {
    let kept = head(s, max);
    if kept.len() < s.len() {
        format!("{kept}…")
    } else {
        kept.to_owned()
    }
}

/// The last `max` bytes of `s`, at a character boundary.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use molt_api::fs::Change;

    use super::*;

    #[test]
    fn a_branch_names_its_files_errors_and_check_each_bounded() {
        let mut b = Branch::default();
        let changes = (0..25)
            .map(|i| Change { path: format!("src/f{i}.rs"), kind: ChangeKind::Modified })
            .chain([Change { path: "new.rs".into(), kind: ChangeKind::Added }])
            .collect();
        b.set_changed(&DiffResponse { changes, patch: String::new(), truncated: false, left_out: None });
        b.set_reason("\n  the done-check still failed (exit code 101):\nlots more");
        b.set_check(&format!("{}\nerror[E0308]: mismatched types", "x".repeat(1000)));
        b.set_approach(vec!["rewrite the lexer".into()], "ignored");
        b.set_unfinished(vec!["run the tests".into()]);
        b.errors.push(error_line("edit_file", &serde_json::json!({ "path": "src/a.rs" }), "old_string not\nfound"));
        let text = b.render(true);
        assert!(text.contains("  What it tried: rewrite the lexer"), "{text}");
        assert!(text.contains("src/f0.rs (modified)") && text.contains("(and 6 more)"), "{text}");
        assert!(!text.contains("new.rs"), "{text}");
        assert!(text.contains("  Tool calls that failed: edit_file src/a.rs: old_string not found"), "{text}");
        assert!(text.contains("error[E0308]: mismatched types") && text.len() < 2000, "{text}");
        assert!(text.contains("  Steps it left undone: run the tests"), "{text}");
        assert_eq!(b.reason, "the done-check still failed (exit code 101):");
        // A repeat says only what it changed.
        assert!(b.render(false).starts_with("  Files it changed: src/f0.rs"), "{}", b.render(false));
        assert_eq!(Branch::default().render(true), "  Files it changed: not known");
        // Without a list, its last words say what it tried.
        b.set_approach(Vec::new(), "I  changed\nthe parser.");
        assert_eq!(b.approach, ["I changed the parser."]);
    }
}

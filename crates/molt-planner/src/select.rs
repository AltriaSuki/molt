//! How the passing attempts of a run are compared: only on signals that can
//! be audited, never on a model's opinion of its own work.

use molt_api::fs::DiffResponse;
use molt_api::planner::Candidate;

/// The ranking rule, as reported with the result.
pub(crate) const RULE: &str = "keeps the project's suite passing (when there is one), then fewer changed lines, \
                               then fewer changed files, then the earlier to pass; with review on, the best ranked \
                               one the reviewer approves wins";

/// A passing attempt's scores, before any review.
pub(crate) fn candidate(attempt: u32, suite_passed: Option<bool>, diff: &DiffResponse) -> Candidate {
    Candidate {
        attempt,
        suite_passed,
        lines_changed: changed_lines(&diff.patch),
        files_changed: u32::try_from(diff.changes.len()).unwrap_or(u32::MAX),
        truncated: diff.truncated,
        verdict: None,
    }
}

/// The sort key: lower is better. A suite that could not be run ranks
/// between one that passed and one that failed.
pub(crate) fn rank(c: &Candidate) -> (u8, u32, u32) {
    let suite = match c.suite_passed {
        Some(true) => 0,
        None => 1,
        Some(false) => 2,
    };
    (suite, c.lines_changed, c.files_changed)
}

/// What a candidate scored, in words.
pub(crate) fn describe(c: &Candidate) -> String {
    let mut parts = Vec::new();
    match c.suite_passed {
        Some(true) => parts.push("the suite passed".to_owned()),
        Some(false) => parts.push("the suite failed".to_owned()),
        None => {}
    }
    let at_least = if c.truncated { "at least " } else { "" };
    let files = if c.files_changed == 1 { "file" } else { "files" };
    parts.push(format!("{at_least}{} lines changed in {} {files}", c.lines_changed, c.files_changed));
    parts.join(", ")
}

/// Lines a unified diff adds or removes, read by its hunk headers' counts
/// so that a removed line starting with `--` is not taken for a file header.
fn changed_lines(patch: &str) -> u32 {
    // Old and new lines left in the current hunk.
    let (mut old, mut new) = (0u32, 0u32);
    let mut n: u32 = 0;
    for line in patch.lines() {
        if old == 0 && new == 0 {
            if let Some((o, nw)) = hunk_counts(line) {
                (old, new) = (o, nw);
            }
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                new = new.saturating_sub(1);
                n = n.saturating_add(1);
            }
            Some(b'-') => {
                old = old.saturating_sub(1);
                n = n.saturating_add(1);
            }
            Some(b'\\') => {}
            _ => {
                old = old.saturating_sub(1);
                new = new.saturating_sub(1);
            }
        }
    }
    n
}

/// The old and new line counts of a hunk header `@@ -a,b +c,d @@`.
fn hunk_counts(line: &str) -> Option<(u32, u32)> {
    let mut ranges = line.strip_prefix("@@ ")?.split(' ');
    let count = |range: Option<&str>, sign: char| -> Option<u32> {
        let range = range?.strip_prefix(sign)?;
        match range.split_once(',') {
            Some((_, n)) => n.parse().ok(),
            None => Some(1),
        }
    };
    Some((count(ranges.next(), '-')?, count(ranges.next(), '+')?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_lines_count_hunk_lines_but_not_file_headers() {
        let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
 fn a() {}
--- removed line starting with dashes
+fn b() {}
\\ No newline at end of file
--- a/x
+++ b/x
@@ -1 +1,2 @@
-x
+y
+z
@@ -0,0 +1 @@
+new
";
        assert_eq!(changed_lines(patch), 6);
        assert_eq!(changed_lines(""), 0);
    }

    #[test]
    fn a_passing_suite_outranks_a_smaller_change() {
        let c = |suite_passed, lines_changed, files_changed| Candidate {
            attempt: 0,
            suite_passed,
            lines_changed,
            files_changed,
            truncated: false,
            verdict: None,
        };
        let mut ranked = [c(Some(false), 1, 1), c(None, 2, 1), c(Some(true), 40, 3), c(Some(true), 40, 2)];
        ranked.sort_by_key(rank);
        assert_eq!(ranked, [c(Some(true), 40, 2), c(Some(true), 40, 3), c(None, 2, 1), c(Some(false), 1, 1)]);
        assert_eq!(describe(&ranked[0]), "the suite passed, 40 lines changed in 2 files");
        assert_eq!(describe(&Candidate { truncated: true, ..c(None, 3, 1) }), "at least 3 lines changed in 1 file");
    }
}

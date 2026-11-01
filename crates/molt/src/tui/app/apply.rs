//! The apply confirmation: a passing result, shown before it changes the
//! workspace, and the files of it the user picks.
use super::*;

impl App {
    pub(super) fn apply_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(Prompt::Apply { resp, fork, part }) = &mut self.prompt else { return Vec::new() };
        if part.checking {
            self.flash = Some("checking again: wait for the check".into());
            return Vec::new();
        }
        if part.picking.is_some() {
            return self.pick_key(key);
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                if part.failed {
                    self.flash = Some("not applied: it did not pass when checked again. s chooses other files, e edits it, k keeps it".into());
                    return Vec::new();
                }
                let (fork, only) = (fork.clone(), part.only.clone());
                let line = match (&only, part.edited) {
                    (Some(only), _) => {
                        Some(format!("  applying {} of {} files, checked again on them", only.len(), part.all.len()))
                    }
                    (None, true) => Some("  applying the result as you edited it, checked again".to_owned()),
                    (None, false) => None,
                };
                self.prompt = None;
                if let Some(line) = line {
                    self.push(Line::from(span(line, Tone::Dim)));
                }
                if let Some(task) = &mut self.task {
                    task.phase = Phase::Applying;
                }
                match only {
                    Some(paths) => vec![Action::ApplyOnly { fork, paths }],
                    None => vec![Action::Apply { fork }],
                }
            }
            KeyCode::Char('s') => {
                if part.all.is_empty() {
                    part.all = resp.changes.clone();
                }
                let marks = part.all.iter().map(|c| part.only.as_ref().is_none_or(|o| o.contains(&c.path))).collect();
                part.picking = Some((marks, 0));
                Vec::new()
            }
            KeyCode::Char('e') => vec![Action::EditFork { fork: fork.clone() }],
            KeyCode::Char('n') => {
                let fork = fork.clone();
                self.prompt = None;
                self.to_commit = None;
                self.task = None;
                self.push(Line::from(span("  discarded", Tone::Dim)));
                vec![Action::Discard { fork }]
            }
            KeyCode::Char('v') => {
                self.viewer = Some(review_viewer(resp.review.as_ref(), &self.secrets));
                Vec::new()
            }
            KeyCode::Char('d') => {
                self.viewer = Some(diff_viewer(&resp.patch, resp.patch_truncated));
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Esc => {
                let fork = fork.clone();
                self.prompt = None;
                self.to_commit = None;
                self.task = None;
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                if let Some(entry) = self.pending_result.take() {
                    self.keep_entry(entry);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Keys while the apply confirmation's file list is open.
    fn pick_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(Prompt::Apply { resp, fork, part }) = &mut self.prompt else { return Vec::new() };
        let Some((marks, cursor)) = &mut part.picking else { return Vec::new() };
        match key.code {
            KeyCode::Up => *cursor = cursor.saturating_sub(1),
            KeyCode::Down => *cursor = (*cursor + 1).min(marks.len().saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(mark) = marks.get_mut(*cursor) {
                    *mark = !*mark;
                }
            }
            KeyCode::Char('a') => {
                let all = !marks.iter().all(|m| *m);
                marks.iter_mut().for_each(|m| *m = all);
            }
            KeyCode::Esc => part.picking = None,
            KeyCode::Enter => {
                let chosen: Vec<String> =
                    part.all.iter().zip(marks.iter()).filter(|(_, m)| **m).map(|(c, _)| c.path.clone()).collect();
                if chosen.is_empty() {
                    self.flash = Some("choose at least one file, or k to keep the result in its fork".into());
                    return Vec::new();
                }
                let only = (chosen.len() < part.all.len()).then_some(chosen);
                part.picking = None;
                if only == part.only && !part.failed {
                    return Vec::new();
                }
                part.checking = true;
                return vec![Action::Recheck { fork: fork.clone(), only, check: resp.check.clone() }];
            }
            _ => {}
        }
        Vec::new()
    }

    /// The apply confirmation after the result `fork` was checked again.
    pub(super) fn rechecked(
        &mut self,
        fork: &str,
        only: Option<Vec<String>>,
        result: Result<crate::session::Rechecked, String>,
    ) -> Vec<Action> {
        let Some(Prompt::Apply { resp, fork: shown, part }) = &mut self.prompt else { return Vec::new() };
        if shown != fork {
            return Vec::new();
        }
        part.checking = false;
        let found = match result {
            Ok(found) => found,
            Err(e) => {
                // Neither the choice nor an edit is applied without passing.
                part.failed = true;
                self.error("could not check it again", &e);
                return Vec::new();
            }
        };
        part.failed = found.failed();
        part.only = only.map(|_| found.diff.changes.iter().map(|c| c.path.clone()).collect());
        part.all = found.all;
        resp.changes = found.diff.changes;
        resp.patch = found.diff.patch;
        resp.patch_truncated = found.diff.truncated;
        resp.left_out = found.diff.left_out;
        let what = match &part.only {
            Some(only) => format!("{} of {} files", only.len(), part.all.len()),
            None => "the whole result".to_owned(),
        };
        let mut stops = Vec::new();
        let line = match &found.check {
            Some(ran) if !ran.success() => {
                let status = ran.exit_code.map_or_else(|| "stopped".to_owned(), |c| format!("exit {c}"));
                stops.push(format!("check failed on {what} ({status})"));
                (format!("  checked again on {what}: failed ({status})"), Tone::Bad)
            }
            Some(_) => (format!("  checked again on {what}: passed"), Tone::Ok),
            None => {
                stops.push("no check command to run again".to_owned());
                (format!("  not checked again on {what}: no check command"), Tone::Warn)
            }
        };
        stops.extend(found.rules.iter().map(|rule| format!("rule: {}", text::fit(&clean_line(rule), 90))));
        stops.extend(apply_stops(resp, &self.rules));
        let narrowed = resp.clone();
        self.push(Line::from(span(clean_line(&line.0), line.1)));
        if let Some(task) = &mut self.task {
            task.stops = stops.iter().map(|line| scrub(line, &self.secrets)).collect();
        }
        // A commit takes what is applied, and nothing else.
        if let Some(Action::Commit { resp, .. }) = &mut self.to_commit {
            *resp = narrowed;
        }
        Vec::new()
    }
}

/// Why a result needs the user's look before it is applied.
pub(super) fn apply_stops(resp: &RunResponse, rules: &ConfirmSection) -> Vec<String> {
    let mut stops = Vec::new();
    if resp.outcome != Outcome::Passed {
        stops.push(outcome_name(resp.outcome).to_owned());
    }
    // The planner holds back what its reviewer did not approve; so does confirm mode.
    if let Some(review) = resp.review.as_ref().filter(|r| r.verdict != Verdict::Approve) {
        let label = if review.verdict == Verdict::Reject { "review rejected" } else { "review questioned" };
        let mut sources = Vec::new();
        if let Some(details) = &review.details {
            if let Some(rule) = details.rules.first() {
                sources.push(format!("rule: {}", text::fit(&clean_line(rule), 90)));
            }
            if let Some(verdict) = details.reviewer {
                sources.push(format!(
                    "reviewer {}",
                    match verdict {
                        Verdict::Approve => "approved",
                        Verdict::Concern => "questioned",
                        Verdict::Reject => "rejected",
                    }
                ));
            }
            if details.error.is_some() {
                sources.push("reviewer did not finish".into());
            }
            if let Some(probe) = details.probes.iter().find(|p| p.result == molt_api::planner::ProbeResult::Failed) {
                sources.push(format!("probe failed: {}", text::fit(&clean_line(&probe.command), 60)));
            }
        }
        stops.push(if sources.is_empty() { label.into() } else { format!("{label} ({})", sources.join("; ")) });
    }
    let (added, removed) = text::diff_counts(&resp.patch);
    if added + removed > rules.max_lines {
        stops.push(format!("{} lines changed", added + removed));
    }
    if resp.changes.len() > rules.max_files {
        stops.push(format!("{} files", resp.changes.len()));
    }
    match resp.left_out.as_ref().map_or(0, |l| l.count) {
        0 => {}
        1 => stops.push("1 build artifact left out".into()),
        n => stops.push(format!("{n} build artifacts left out")),
    }
    let deleted = resp.changes.iter().filter(|c| c.kind == ChangeKind::Deleted).count();
    match deleted {
        0 => {}
        1 => stops.push("deletes 1 file".into()),
        n => stops.push(format!("deletes {n} files")),
    }
    let touched: Vec<&str> =
        resp.changes.iter().map(|c| c.path.as_str()).filter(|p| rules.ask_paths.iter().any(|a| covers(a, p))).collect();
    match touched.as_slice() {
        [] => {}
        [one] => stops.push(format!("touches {}", clean_line(one))),
        many => stops.push(format!("touches {} protected paths", many.len())),
    }
    if resp.patch_truncated {
        stops.push("diff cut short".into());
    }
    stops
}

/// `rule` names `path`: the same path, a directory above it (`dir/`), or its file name.
fn covers(rule: &str, path: &str) -> bool {
    match rule.strip_suffix('/') {
        // A directory named alone (`migrations/`) is matched at any depth.
        Some(dir) if !dir.contains('/') => path.starts_with(rule) || path.contains(&format!("/{rule}")),
        Some(_) => path.starts_with(rule),
        None => path == rule || (!rule.contains('/') && path.rsplit('/').next() == Some(rule)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn confirmations_keep_log_navigation_and_explain_rules_separately_from_the_reviewer() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.rules.always_ask = true;
        type_in(&mut app, "fix parser");
        let trace = app.task.as_ref().unwrap().trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        for apply in [false, true] {
            if apply {
                app.update(key(KeyCode::Enter));
                let mut resp = passed(false, Some("/fork/result"));
                resp.review = Some(molt_api::planner::Review {
                    verdict: Verdict::Concern,
                    findings: vec!["src/parser.rs adds a test skip".into()],
                    details: Some(molt_api::planner::ReviewDetails {
                        rules: vec!["src/parser.rs adds a test skip".into()],
                        reviewer: Some(Verdict::Approve),
                        ..Default::default()
                    }),
                });
                app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(resp)) });
                let reason = app.task.as_ref().unwrap().stops.join("; ");
                assert!(reason.contains("rule: src/parser.rs") && reason.contains("reviewer approved"), "{reason}");
                assert!(!reason.contains("probe"), "{reason}");
            }
            app.update(key(KeyCode::PageUp));
            assert_eq!(app.scroll, 10);
            app.update(Msg::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)));
            assert_eq!(app.scroll, usize::MAX);
            app.update(Msg::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)));
            assert_eq!(app.scroll, 0);
            app.update(key(KeyCode::PageUp));
            app.update(key(KeyCode::PageDown));
            assert_eq!(app.scroll, 0);
            assert!(app.prompt.is_some());
        }
        app.update(key(KeyCode::Char('v')));
        let shown = plain(&app.viewer.as_ref().unwrap().lines).join("\n");
        assert!(
            shown.contains("fixed rules")
                && shown.contains("src/parser.rs adds a test skip")
                && shown.contains("reviewer: approve"),
            "{shown}"
        );
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
    }

    #[test]
    fn the_apply_confirmation_applies_only_the_chosen_files_once_they_pass_their_check_again() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.rules.always_ask = true;
        type_in(&mut app, "/check cargo test");
        type_in(&mut app, "fix the parser");
        let trace = app.task.as_ref().unwrap().trace.clone();
        let mut resp = passed(false, Some("/d/work/fork-1"));
        resp.changes.push(Change { path: "target/out.bin".into(), kind: ChangeKind::Added });
        let all = resp.changes.clone();
        app.update(Msg::Ran { trace, result: Box::new(Ok(resp)) });
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert!(frame_text(&mut app).contains("s choose files"));

        assert!(app.update(key(KeyCode::Char('s'))).is_empty());
        let shown = frame_text(&mut app);
        assert!(shown.contains("apply which files?") && shown.contains("[x] target/out.bin"), "{shown}");
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char(' ')));
        let actions = app.update(key(KeyCode::Enter));
        let fork = "/d/work/fork-1".to_owned();
        let only = Some(vec!["src/parser.rs".to_owned()]);
        let check = Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: true, suite: None });
        assert_eq!(actions, [Action::Recheck { fork: fork.clone(), only: only.clone(), check: check.clone() }]);
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "nothing is applied while the check runs");

        let found = |exit: i32, rules: Vec<String>| crate::session::Rechecked {
            all: all.clone(),
            diff: molt_api::fs::DiffResponse {
                changes: all[..1].to_vec(),
                patch: "--- a/src/parser.rs\n+++ b/src/parser.rs\n@@\n-a\n+b\n".into(),
                truncated: false,
                left_out: None,
            },
            check: Some(
                serde_json::from_value(serde_json::json!({
                    "exit_code": exit, "timed_out": false, "stdout": "", "stderr": "", "truncated": false,
                    "duration_ms": 5
                }))
                .unwrap(),
            ),
            rules,
        };
        app.update(Msg::Rechecked { fork: fork.clone(), only: only.clone(), result: Ok(found(1, vec![])) });
        assert!(log(&app).contains("checked again on 1 of 2 files: failed (exit 1)"), "{}", log(&app));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "a choice that fails its check is not applied");
        assert!(app.flash.as_deref().unwrap().contains("not applied"));

        // An edit by hand is checked again, on the same choice.
        assert_eq!(app.update(key(KeyCode::Char('e'))), [Action::EditFork { fork: fork.clone() }]);
        let actions = app.update(Msg::ForkEdited { fork: fork.clone(), result: Ok(()) });
        assert_eq!(actions, [Action::Recheck { fork: fork.clone(), only: only.clone(), check }]);
        let rule = "src/parser.rs adds a test skip".to_owned();
        app.update(Msg::Rechecked { fork: fork.clone(), only, result: Ok(found(0, vec![rule])) });
        assert!(log(&app).contains("checked again on 1 of 2 files: passed"));
        let stops = &app.task.as_ref().unwrap().stops;
        assert!(stops.iter().any(|s| s == "rule: src/parser.rs adds a test skip"), "{stops:?}");
        let Some(Prompt::Apply { resp, .. }) = &app.prompt else { panic!() };
        assert_eq!(resp.changes, all[..1]);

        let actions = app.update(key(KeyCode::Enter));
        assert_eq!(actions, [Action::ApplyOnly { fork, paths: vec!["src/parser.rs".into()] }]);
        assert!(log(&app).contains("applying 1 of 2 files, checked again on them"));
    }

    #[test]
    fn with_commit_on_an_applied_result_is_committed_once_it_is_applied() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        // Off by default: nothing about git.
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(true, None))) });
        assert!(!actions.iter().any(|a| matches!(a, Action::Commit { .. })), "{actions:?}");

        type_in(&mut app, "/commit on");
        assert_eq!(app.flash.as_deref(), Some("commit: on (next task)"));
        // Applied by the run itself: committed at once.
        let actions = type_in(&mut app, "implement more");
        let [Action::PrepareCommit { trace }, Action::Run { trace: run, .. }] = actions.as_slice() else {
            panic!("{actions:?}")
        };
        assert_eq!(trace, run);
        let trace = trace.clone();
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(true, None))) });
        let Some(Action::Commit { trace: committed, task, resp }) = actions.first() else { panic!("{actions:?}") };
        assert_eq!((committed, task.as_str(), resp.changes.len()), (&trace, "implement more", 1));
        let made = crate::git::Committed {
            sha: "0123456789abcdef".into(),
            subject: "Trailing commas parse.".into(),
            branch: Some("main".into()),
            ignored: vec![],
        };
        app.update(Msg::Committed(Ok(Some(made))));
        assert!(log(&app).ends_with("  committed 0123456789ab on main: Trailing commas parse."), "{}", log(&app));
        app.update(Msg::Committed(Err("a.txt had changes of yours".into())));
        assert!(log(&app).contains("not committed; the result is applied"), "{}", log(&app));

        // Applied after the user's look: committed only once the apply succeeds.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "and more");
        let [Action::PrepareCommit { trace }, Action::Run { .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        let actions =
            app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(false, Some("/d/fork-3")))) });
        assert!(!actions.iter().any(|a| matches!(a, Action::Commit { .. })), "{actions:?}");
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/d/fork-3".into() }]);
        let actions = app.update(Msg::Applied(Ok(())));
        assert!(matches!(actions.as_slice(), [Action::Commit { trace: t, .. }] if *t == trace), "{actions:?}");

        app.update(Msg::Committed(Ok(None)));
        // A result the user keeps in its fork, or that fails to apply, is not committed.
        let actions = type_in(&mut app, "once more");
        let [Action::PrepareCommit { trace }, ..] = actions.as_slice() else { panic!("{actions:?}") };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(false, Some("/d/fork-4")))) });
        app.update(key(KeyCode::Enter));
        assert!(app.update(Msg::Applied(Err("conflict".into()))).is_empty());
        type_in(&mut app, "/commit off");
        assert_eq!(type_in(&mut app, "last one").len(), 1);
    }

    #[test]
    fn confirm_mode_stops_for_a_result_that_needs_a_look() {
        let ok = || passed(false, Some("/d/work/fork-1"));
        assert_eq!(apply_stop(RunResponse { outcome: Outcome::Unverified, ..ok() }), ["unverified"]);
        let patch = format!("--- a/x\n+++ b/x\n@@\n{}", "+line\n".repeat(301));
        assert_eq!(apply_stop(RunResponse { patch, ..ok() }), ["301 lines changed"]);
        let changes = (0..11).map(|i| Change { path: format!("src/f{i}.rs"), kind: ChangeKind::Modified }).collect();
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["11 files"]);
        let changes = vec![
            Change { path: "src/old.rs".into(), kind: ChangeKind::Deleted },
            Change { path: "src/new.rs".into(), kind: ChangeKind::Added },
        ];
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["deletes 1 file"]);
        let changes = vec![Change { path: "crates/a/Cargo.toml".into(), kind: ChangeKind::Modified }];
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["touches crates/a/Cargo.toml"]);
        assert_eq!(apply_stop(RunResponse { patch_truncated: true, ..ok() }), ["diff cut short"]);
        let left_out = Some(molt_api::fs::LeftOut { count: 32, paths: vec!["target-x/debug/app".into()] });
        assert_eq!(apply_stop(RunResponse { left_out, ..ok() }), ["32 build artifacts left out"]);
        let review = |verdict| Some(molt_api::planner::Review { verdict, findings: vec![], details: None });
        assert_eq!(apply_stop(RunResponse { review: review(Verdict::Reject), ..ok() }), ["review rejected"]);
        assert_eq!(apply_stop(RunResponse { review: review(Verdict::Concern), ..ok() }), ["review questioned"]);
        let probe = |result| molt_api::planner::Probe {
            command: "printf 'GE' | ./server".into(),
            purpose: String::new(),
            result,
            exit_code: Some(1),
            timed_out: false,
            output: String::new(),
            duration_ms: 1,
        };
        use molt_api::planner::ProbeResult::{Failed, Passed};
        let details = molt_api::planner::ReviewDetails {
            reviewer: Some(Verdict::Approve),
            probes: vec![probe(Passed), probe(Failed)],
            ..Default::default()
        };
        let review =
            Some(molt_api::planner::Review { verdict: Verdict::Concern, findings: vec![], details: Some(details) });
        assert_eq!(
            apply_stop(RunResponse { review, ..ok() }),
            ["review questioned (reviewer approved; probe failed: printf 'GE' | ./server)"]
        );
    }

    #[test]
    fn protected_paths_match_files_directories_and_names() {
        assert!(covers(".github/", ".github/workflows/ci.yml"));
        assert!(covers("migrations/", "db/migrations/001.sql"));
        assert!(!covers("migrations/", "src/migrations.rs"));
        assert!(covers("db/schema/", "db/schema/a.sql") && !covers("db/schema/", "x/db/schema/a.sql"));
        assert!(covers("Cargo.toml", "Cargo.toml") && covers("Cargo.toml", "crates/a/Cargo.toml"));
        assert!(!covers("Cargo.toml", "Cargo.toml.bak"));
        assert!(covers("src/lib.rs", "src/lib.rs") && !covers("src/lib.rs", "a/src/lib.rs"));
    }
}

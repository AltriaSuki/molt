//! Results kept in their forks, to apply or discard later (`/kept`).
use super::*;

impl App {
    pub fn restore_kept(&mut self, entries: Vec<crate::tui::kept::Entry>) {
        self.kept = entries.iter().map(|entry| entry.fork.clone()).collect();
        self.kept_entries = entries;
    }

    pub fn take_kept_changes(&mut self) -> Vec<crate::tui::kept::Change> {
        std::mem::take(&mut self.kept_changes)
    }

    pub(super) fn keep_entry(&mut self, mut entry: crate::tui::kept::Entry) {
        entry.task = scrub(&entry.task, &self.secrets);
        if !self.kept.contains(&entry.fork) {
            self.kept.push(entry.fork.clone());
        }
        self.kept_entries.retain(|old| old.fork != entry.fork);
        self.kept_entries.push(entry.clone());
        self.kept_changes.push(crate::tui::kept::Change::Save(entry));
    }

    pub(super) fn kept_finished(&mut self, result: Result<(), String>, verb: &str) {
        let fork = self.kept_pending.take().unwrap();
        match result {
            Ok(()) => {
                if let Some(c) = self.continuation.as_mut().filter(|c| c.fork.as_deref() == Some(fork.as_str())) {
                    // Applied, the result is in the workspace; discarded, there is nothing to go on from.
                    c.fork = None;
                    if verb != "applied" {
                        self.continuation = None;
                    }
                }
                self.kept.retain(|old| *old != fork);
                self.kept_entries.retain(|entry| entry.fork != fork);
                self.kept_changes.push(crate::tui::kept::Change::Remove(fork));
                self.push(Line::from(span(format!("  {verb} kept result"), Tone::Ok)));
                if let Some(at) = &mut self.kept_view {
                    *at = (*at).min(self.kept_entries.len().saturating_sub(1));
                }
            }
            Err(error) => {
                self.error(&format!("kept result was not {verb}"), &scrub(&error, &self.secrets));
                self.viewer = Some(Viewer {
                    title: "kept result remains available".into(),
                    lines: block(&scrub(&error, &self.secrets), "", Tone::Bad),
                    scroll: 0,
                });
            }
        }
    }

    pub(super) fn kept_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let at = self.kept_view.unwrap_or(0).min(self.kept_entries.len().saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.kept_view = None,
            KeyCode::Up | KeyCode::Char('k') => self.kept_view = Some(at.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.kept_view = Some((at + 1).min(self.kept_entries.len().saturating_sub(1)))
            }
            KeyCode::Char('d') if !self.kept_entries.is_empty() => {
                if !matches!(self.session, SessionState::Up { .. }) {
                    self.flash = Some("the services are not up".into());
                    return Vec::new();
                }
                let fork = self.kept_entries[at].fork.clone();
                self.viewer = Some(Viewer {
                    title: format!("kept diff: {fork}"),
                    lines: block("reading the result's diff", "", Tone::Dim),
                    scroll: 0,
                });
                return vec![Action::Diff { fork }];
            }
            KeyCode::Enter | KeyCode::Char('n') if !self.kept_entries.is_empty() => {
                if self.task.is_some()
                    || self.pending_result.is_some()
                    || self.fork_operations > 0
                    || !self.learning.is_empty()
                    || self.git_busy
                    || self.kept_pending.is_some()
                    || !matches!(self.session, SessionState::Up { .. })
                {
                    self.flash = Some("wait for idle services before applying or discarding kept results".into());
                    return Vec::new();
                }
                let fork = self.kept_entries[at].fork.clone();
                self.kept_pending = Some(fork.clone());
                return vec![if key.code == KeyCode::Enter {
                    Action::Apply { fork }
                } else {
                    Action::Discard { fork }
                }];
            }
            _ => {}
        }
        Vec::new()
    }

    pub fn kept_lines(&self) -> Vec<Line<'static>> {
        if self.kept_entries.is_empty() {
            return block("no kept results in this workspace", "", Tone::Dim);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        self.kept_entries
            .iter()
            .enumerate()
            .flat_map(|(index, entry)| {
                let mark = if Some(index) == self.kept_view { "›" } else { " " };
                let size = entry.files.map_or_else(
                    || "size unknown · d to inspect".into(),
                    |files| format!("{files} files · +{} −{}", entry.added, entry.removed),
                );
                let age = text::duration(Duration::from_secs(now.saturating_sub(entry.time)));
                vec![
                    Line::from(span(format!("{mark} {}", clean_line(&entry.task)), Tone::Shell)),
                    Line::from(span(format!("  {} · {size} · {age} ago", clean_line(&entry.outcome)), Tone::Dim)),
                    Line::from(span(format!("  {}", clean_line(&entry.fork)), Tone::Dim)),
                ]
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn plan_results_are_browsable_and_remain_available_until_apply_or_discard_succeeds() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Plan;
        type_in(&mut app, "/check true");
        type_in(&mut app, "fix parser");
        let trace = app.task.as_ref().unwrap().trace.clone();
        app.update(Msg::Ran { trace, result: Box::new(Ok(passed(false, Some("/fork/result")))) });
        app.learning.clear();
        type_in(&mut app, "/kept");
        let shown = frame_text(&mut app);
        assert!(shown.contains("fix parser") && shown.contains("1 files") && shown.contains("1 kept"), "{shown}");
        assert_eq!(app.update(key(KeyCode::Char('d'))), [Action::Diff { fork: "/fork/result".into() }]);
        app.update(Msg::Diff {
            fork: "/fork/result".into(),
            result: Ok(molt_api::fs::DiffResponse {
                changes: vec![],
                patch: "+new\n-old".into(),
                truncated: false,
                left_out: None,
            }),
        });
        assert!(plain(&app.viewer.as_ref().unwrap().lines).join("\n").contains("+new"));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/fork/result".into() }]);
        app.update(Msg::Applied(Err("conflict: src/parser.rs".into())));
        assert_eq!(app.kept, ["/fork/result"]);
        assert!(frame_text(&mut app).contains("conflict: src/parser.rs"));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/fork/result".into() }]);
        app.update(Msg::Applied(Ok(())));
        assert!(app.kept.is_empty());
        let entry = crate::tui::kept::Entry::new("/fork/discard".into(), "another task", "passed");
        app.restore_kept(vec![entry]);
        assert_eq!(app.update(key(KeyCode::Char('n'))), [Action::Discard { fork: "/fork/discard".into() }]);
        app.update(Msg::Discarded(Err("busy".into())));
        assert_eq!(app.kept, ["/fork/discard"]);
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(key(KeyCode::Char('n'))), [Action::Discard { fork: "/fork/discard".into() }]);
        app.update(Msg::Discarded(Ok(())));
        assert!(app.kept.is_empty());
    }

    #[test]
    fn restored_kept_results_apply_only_when_idle_and_late_diffs_do_not_reopen_a_viewer() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.restore_kept(vec![crate::tui::kept::Entry::new("/fork/old".into(), "old task", "passed")]);
        type_in(&mut app, "new task");
        type_in(&mut app, "/kept");
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert!(app.flash.as_ref().unwrap().contains("idle"));
        assert!(app.update(key(KeyCode::Char('n'))).is_empty());
        app.update(key(KeyCode::Char('d')));
        app.update(key(KeyCode::Esc));
        app.update(Msg::Diff { fork: "/fork/old".into(), result: Err("late".into()) });
        assert!(app.viewer.is_none());
        app.update(key(KeyCode::Esc));
        app.task = None;
        type_in(&mut app, "/kept");
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/fork/old".into() }]);
        app.update(key(KeyCode::Esc));
        assert!(type_in(&mut app, "/restart").is_empty());
        assert!(app.flash.as_ref().unwrap().contains("finish or stop"));
        assert!(type_in(&mut app, "another task").is_empty());
        assert!(app.flash.as_ref().unwrap().contains("current apply"));
    }

    #[test]
    fn quitting_a_confirmed_result_records_it_and_failed_live_applies_keep_its_metadata() {
        for exit in [0, 1, 2] {
            let mut app = App::new("/work/project", None);
            up(&mut app);
            app.rules.always_ask = true;
            type_in(&mut app, "/check true");
            type_in(&mut app, "fix parser");
            let trace = app.task.as_ref().unwrap().trace.clone();
            app.update(Msg::Ran { trace, result: Box::new(Ok(passed(false, Some("/fork/waiting")))) });
            if exit == 0 {
                app.update(key(KeyCode::Enter));
                app.update(Msg::Applied(Err("conflict".into())));
            } else {
                if exit == 2 {
                    app.update(key(KeyCode::Enter));
                }
                let actions = app.update(Msg::Signal);
                assert!(actions
                    .iter()
                    .any(|action| matches!(action, Action::Quit { keep } if keep == &["/fork/waiting"])));
            }
            assert_eq!(app.kept, ["/fork/waiting"]);
            assert!(
                matches!(app.take_kept_changes().last(), Some(crate::tui::kept::Change::Save(entry)) if entry.task == "fix parser")
            );
        }
    }

    #[test]
    fn quitting_keeps_a_result_still_waiting_on_the_user() {
        let mut app = App::new("/w", None);
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "/check true");
        assert!(actions.is_empty());
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(false, Some("/d/work/fork-9")))) });
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert!(app.update(ctrl_key('d')).is_empty());
        assert_eq!(app.flash.as_deref(), Some("ctrl+d again to quit; 1 result stays in its fork"));
        assert_eq!(app.update(ctrl_key('d')), [Action::Quit { keep: vec!["/d/work/fork-9".into()] }]);
    }
}

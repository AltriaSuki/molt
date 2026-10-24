//! `/diff`: what the workspace changed against `HEAD`, file by file, and
//! one file's diff in the diff viewer. Git runs on the user's side, as for
//! commits; nothing goes to the services.
use super::*;

use crate::git::{Changed, FileDiff};

/// The `/diff` list.
pub struct ChangesView {
    pub selected: usize,
    /// `None` while git is being asked.
    pub files: Option<Result<Vec<Changed>, String>>,
}

impl App {
    pub(super) fn open_changes(&mut self) -> Vec<Action> {
        self.changes = Some(ChangesView { selected: 0, files: None });
        vec![Action::Changes]
    }

    pub(super) fn listed_changes(&mut self, result: Result<Vec<Changed>, String>) {
        if let Some(view) = &mut self.changes {
            view.selected = view.selected.min(result.as_ref().map_or(0, |f| f.len().saturating_sub(1)));
            view.files = Some(result.map_err(|e| scrub(&e, &self.secrets)));
        }
    }

    pub(super) fn file_diff(&mut self, path: String, result: Result<FileDiff, String>) {
        let title = format!("diff: {path}");
        if self.viewer.as_ref().is_none_or(|v| v.title != title) {
            return;
        }
        self.viewer = Some(match result {
            Ok(diff) => Viewer { title, ..diff_viewer(&scrub(&diff.patch, &self.secrets), diff.truncated) },
            Err(e) => Viewer { title, lines: block(&scrub(&e, &self.secrets), "", Tone::Bad), scroll: 0 },
        });
    }

    pub(super) fn changes_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(view) = &mut self.changes else { return Vec::new() };
        let files = match &view.files {
            Some(Ok(files)) => files.as_slice(),
            _ => &[],
        };
        let last = files.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.changes = None,
            KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selected = (view.selected + 1).min(last),
            KeyCode::Char('r') => {
                view.files = None;
                return vec![Action::Changes];
            }
            KeyCode::Enter if !files.is_empty() => {
                let change = files[view.selected.min(last)].clone();
                self.viewer = Some(Viewer {
                    title: format!("diff: {}", change.path),
                    lines: block("reading the diff", "", Tone::Dim),
                    scroll: 0,
                });
                return vec![Action::FileDiff { change }];
            }
            _ => {}
        }
        Vec::new()
    }

    pub fn changes_lines(&self) -> Vec<Line<'static>> {
        let Some(view) = &self.changes else { return Vec::new() };
        let files = match &view.files {
            None => return block("asking git", "", Tone::Dim),
            Some(Err(e)) => return block(&format!("cannot compare with HEAD: {e}"), "", Tone::Warn),
            Some(Ok(files)) if files.is_empty() => return block("no changes against HEAD", "", Tone::Dim),
            Some(Ok(files)) => files,
        };
        let (added, removed) = files.iter().filter_map(|f| f.lines).fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
        let count = if files.len() == 1 { "1 file".to_owned() } else { format!("{} files", files.len()) };
        let mut lines = vec![Line::from(span(format!("{count} · +{added} −{removed}"), Tone::Dim))];
        for (i, file) in files.iter().enumerate() {
            let mark = if i == view.selected { "› " } else { "  " };
            let size = match file.lines {
                Some((a, r)) => format!("+{a} −{r}"),
                None => "binary".into(),
            };
            let new = if file.untracked { " · new" } else { "" };
            let tone = if i == view.selected { Tone::Shell } else { Tone::Plain };
            lines.push(Line::from(vec![
                span(format!("{mark}{}", clean_line(&file.path)), tone),
                span(format!("  {size}{new}"), Tone::Dim),
            ]));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn changed(path: &str, lines: Option<(u64, u64)>, untracked: bool) -> Changed {
        Changed { path: path.into(), lines, untracked }
    }

    #[test]
    fn diff_lists_the_changed_files_and_enter_opens_one_in_the_diff_viewer() {
        let mut app = App::new("/w", None);
        assert_eq!(type_in(&mut app, "/diff"), [Action::Changes], "git, not the services");
        assert!(frame_text(&mut app).contains("asking git"));
        app.update(Msg::Changes(Ok(vec![
            changed("greeting.txt", Some((1, 0)), true),
            changed("src/lib.rs", Some((3, 2)), false),
            changed("logo.png", None, false),
        ])));
        let frame = frame_text(&mut app);
        assert!(frame.contains("3 files · +4 −2"), "{frame}");
        assert!(frame.contains("› greeting.txt  +1 −0 · new") && frame.contains("logo.png  binary"), "{frame}");
        app.update(key(KeyCode::Down));
        let actions = app.update(key(KeyCode::Enter));
        assert_eq!(actions, [Action::FileDiff { change: changed("src/lib.rs", Some((3, 2)), false) }]);
        let patch =
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let diff = FileDiff { patch: patch.into(), truncated: false };
        app.update(Msg::FileDiff { path: "src/lib.rs".into(), result: Ok(diff.clone()) });
        let viewer = app.viewer.as_ref().unwrap();
        assert_eq!(viewer.title, "diff: src/lib.rs");
        // The same coloring as every other diff.
        assert_eq!(viewer.lines, diff_viewer(patch, false).lines);
        // Esc closes the diff, then the list.
        app.update(key(KeyCode::Esc));
        assert!(app.viewer.is_none() && app.changes.is_some());
        // A diff that comes after its viewer closed is not opened.
        app.update(Msg::FileDiff { path: "src/lib.rs".into(), result: Ok(diff) });
        assert!(app.viewer.is_none());
        app.update(key(KeyCode::Esc));
        assert!(app.changes.is_none());
    }

    #[test]
    fn diff_says_when_it_cannot_compare_and_when_nothing_changed() {
        let mut app = App::new("/w", None);
        type_in(&mut app, "/diff");
        app.update(Msg::Changes(Err("/w is not in a git work tree".into())));
        assert!(frame_text(&mut app).contains("cannot compare with HEAD: /w is not in a git work tree"));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.update(key(KeyCode::Char('r'))), [Action::Changes]);
        app.update(Msg::Changes(Ok(vec![])));
        assert!(frame_text(&mut app).contains("no changes against HEAD"));
    }
}

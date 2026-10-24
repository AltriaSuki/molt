//! Completing what is typed: slash commands and their values, and `@path`
//! files of the workspace.
use super::*;

impl App {
    pub fn load_commands(&mut self, workspace: &std::path::Path) {
        let (commands, warnings) = crate::tui::commands::load(workspace);
        self.commands = commands;
        for warning in warnings {
            self.push(Line::from(span(clean_line(&warning), Tone::Warn)));
        }
    }

    pub fn command_completions(&self) -> Vec<crate::tui::commands::Completion> {
        if !matches!(self.screen, Screen::Main)
            || self.queue_view.is_some()
            || self.kept_view.is_some()
            || self.prompt.is_some()
            || self.viewer.is_some()
            || self.transcript.is_some()
            || self.shortcuts.is_some()
            || self.history_search.is_some()
            || self.input.cursor != self.input.text.chars().count()
            || self.input.text.contains(['\n', '\r'])
            || self.command_hidden.as_ref() == Some(&self.input.text)
        {
            return Vec::new();
        }
        crate::tui::commands::completions(&self.input.text, &self.commands)
    }

    /// The `@path` being typed where the cursor is: where its `@` is, in
    /// characters, and what follows it.
    pub(super) fn at_query(&self) -> Option<(usize, String)> {
        if !matches!(self.screen, Screen::Main) || self.prompt.is_some() || self.task.is_some() {
            return None;
        }
        let before: Vec<char> = self.input.text.chars().take(self.input.cursor).collect();
        let start = before.iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
        (before.get(start) == Some(&'@')).then(|| (start, before[start + 1..].iter().collect()))
    }

    /// Workspace files matching the `@path` being typed, best first.
    pub fn suggestions(&self) -> Vec<String> {
        let (Some((_, query)), Some(files)) = (self.at_query(), &self.files) else { return Vec::new() };
        if self.hidden.as_ref() == Some(&query) {
            return Vec::new();
        }
        matching(files, &query)
    }

    /// Put `path` in place of the `@path` being typed.
    pub(super) fn complete(&mut self, path: &str) {
        let Some((start, _)) = self.at_query() else { return };
        let chars: Vec<char> = self.input.text.chars().collect();
        let head: String = chars[..start].iter().collect();
        let tail: String = chars[self.input.cursor..].iter().collect();
        let put = format!("@{path} ");
        self.input.cursor = start + put.chars().count();
        self.input.text = format!("{head}{put}{}", tail.trim_start_matches(' '));
        self.pick = 0;
    }

    /// The files a task points at with `@path`, or why one cannot be used.
    pub(super) fn referenced(&self, line: &str) -> Result<Vec<String>, String> {
        let mut files: Vec<String> = Vec::new();
        for word in line.split_whitespace() {
            let Some(path) = word.strip_prefix('@') else { continue };
            let path = path.trim_end_matches([',', '.', ';', ':', '!', '?', ')', '`', '\'', '"']);
            let path = path.strip_prefix("./").unwrap_or(path);
            if path.is_empty() {
                continue;
            }
            let inside = std::path::Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
            if !inside {
                return Err(format!("@{path} is outside the workspace"));
            }
            if let Some(listed) = self.files.as_ref().filter(|_| self.files_complete) {
                if !listed.iter().any(|f| f == path) {
                    return Err(format!("@{path}: no such file in the workspace"));
                }
            }
            if !files.iter().any(|f| f == path) {
                files.push(path.to_owned());
            }
        }
        Ok(files)
    }
}

/// The files `query` picks out, best first: those it starts, then those
/// whose name it starts, then those containing it, then those holding its
/// characters in order; shorter paths first within each.
fn matching(files: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let in_order = |path: &str| {
        let mut chars = path.chars();
        query.chars().all(|q| chars.any(|c| c == q))
    };
    let mut found: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let path = file.to_lowercase();
            let name = path.rsplit('/').next().unwrap_or_default();
            let rank = if path.starts_with(&query) {
                0
            } else if name.starts_with(&query) {
                1
            } else if path.contains(&query) {
                2
            } else if in_order(&path) {
                3
            } else {
                return None;
            };
            Some((rank, file.len(), file))
        })
        .collect();
    found.sort();
    found.into_iter().take(SUGGESTIONS).map(|(_, _, f)| f.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn slash_commands_and_values_are_suggested_before_submission() {
        let mut app = App::new("/w", None);
        app.update(Msg::Paste("/ef".into()));
        assert!(frame_text(&mut app).contains("/effort LEVEL"));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/effort ");
        let frame = frame_text(&mut app);
        for value in ["low", "medium", "high", "xhigh", "max"] {
            assert!(frame.contains(value), "{value}: {frame}");
        }
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/effort medium ");
        app.update(key(KeyCode::Enter));
        assert_eq!(app.settings.effort, Some(Effort::Medium));
    }

    #[test]
    fn the_picker_scrolls_wraps_hides_and_preserves_the_input_until_tab() {
        let mut app = App::new("/w", None);
        app.update(Msg::Paste("/attempts ".into()));
        let frame = frame_rows(&mut app, 140);
        assert_eq!(frame.iter().filter(|l| l.contains("attempts at once")).count(), 6);
        app.update(key(KeyCode::Up));
        let frame = frame_text(&mut app);
        assert!(frame.contains("› default") && frame.contains("9/9"), "{frame}");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| crate::tui::view::draw(frame, &mut app)).unwrap();
        let small: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(small.contains("› default"), "the picked value stays visible in a short terminal: {small}");
        assert_eq!(app.input.text, "/attempts ");
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/attempts 1 ");
        app.update(key(KeyCode::Enter));
        assert_eq!(app.settings.attempts, Some(1));
        app.update(Msg::Paste("/model ".into()));
        app.update(key(KeyCode::Esc));
        assert!(app.command_completions().is_empty());
        assert_eq!(app.input.text, "/model ");
        app.update(key(KeyCode::Char('s')));
        assert!(frame_text(&mut app).contains("› sonnet"));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/model sonnet ");
        app.update(key(KeyCode::Enter));
        assert_eq!(app.settings.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn completion_does_not_replace_free_text_or_text_after_the_cursor() {
        let mut app = App::new("/w", None);
        for text in ["/model custom-model", "/check echo hello", "/effort high\nmore text", "/unknown"] {
            app.input = Input::with(text);
            assert!(app.command_completions().is_empty(), "{text}");
            app.update(key(KeyCode::Tab));
            assert_eq!(app.input.text, text);
        }
        app.input = Input::with("/model sonnet");
        app.update(key(KeyCode::Left));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/model sonnet");
    }

    #[test]
    fn completion_takes_tab_and_escape_before_the_running_task_does() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        let trace = app.task.as_ref().unwrap().trace.clone();
        attempt_with_tool_call(&mut app, &trace, "cargo test");
        app.update(key(KeyCode::Tab));
        assert_eq!(app.zoom, Some(0));
        app.update(Msg::Paste("/ef".into()));
        assert!(frame_text(&mut app).contains("/effort LEVEL"));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/effort ");
        assert_eq!(app.zoom, Some(0));
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        assert!(app.running());
        assert!(app.command_completions().is_empty());
        app.input.take();
        app.update(key(KeyCode::Tab));
        assert_eq!(app.zoom, None);
    }

    #[test]
    fn an_at_path_completes_from_the_workspace_and_goes_with_the_task() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        // The files are listed once, when an `@` is first typed.
        app.update(Msg::Paste("fix ".into()));
        let actions = app.update(key(KeyCode::Char('@')));
        assert_eq!(actions, [Action::ListFiles]);
        assert!(app.update(key(KeyCode::Char('s'))).is_empty(), "listed once");
        app.update(workspace_files());
        app.update(Msg::Paste("rc/le".into()));
        assert_eq!(app.suggestions(), ["src/lexer.rs", "src/parser/lex_table.rs"]);
        assert!(frame_text(&mut app).contains("› @src/lexer.rs"));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "fix @src/lexer.rs ");
        assert!(app.suggestions().is_empty());
        app.update(Msg::Paste("and @src/lexer.rs, please".into()));
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.files, ["src/lexer.rs"]);
        assert_eq!(req.task, "fix @src/lexer.rs and @src/lexer.rs, please");
    }

    #[test]
    fn an_at_path_outside_the_workspace_or_not_in_it_is_refused() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        assert!(type_in(&mut app, "read @../outside").is_empty());
        assert_eq!(app.flash.as_deref(), Some("@../outside is outside the workspace"));
        assert_eq!(app.input.text, "read @../outside");
        app.input.take();
        assert!(type_in(&mut app, "read @/etc/passwd").is_empty());
        app.input.take();
        app.update(workspace_files());
        assert!(type_in(&mut app, "read @src/missing.rs").is_empty());
        assert_eq!(app.flash.as_deref(), Some("@src/missing.rs: no such file in the workspace"));
        app.input.take();
        // An address is not a reference, and Esc puts suggestions away.
        let actions = type_in(&mut app, "mail me@example.com about @Cargo.toml");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.files, ["Cargo.toml"]);
    }

    #[test]
    fn suggestions_rank_the_start_of_the_path_then_the_name_then_any_part() {
        let files: Vec<String> =
            ["docs/lexer.md", "src/lexer.rs", "lexicon.txt", "src/a/l_e_x.rs"].map(str::to_owned).to_vec();
        assert_eq!(matching(&files, "lex"), ["lexicon.txt", "src/lexer.rs", "docs/lexer.md", "src/a/l_e_x.rs"]);
        assert_eq!(matching(&files, "SRC/LE"), ["src/lexer.rs", "src/a/l_e_x.rs"]);
        assert!(matching(&files, "zzz").is_empty());
        assert_eq!(matching(&files, "").len(), 4);
    }
}

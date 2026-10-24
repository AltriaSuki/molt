//! The text fields, the task input's keys and its history.
use super::*;

/// A text field. The task input holds several lines; the others one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Input {
    pub text: String,
    /// In characters.
    pub cursor: usize,
    /// Long pastes, each shown as its placeholder in `text` and put back by [`Input::take`].
    pastes: Vec<(String, String)>,
}

pub struct HistorySearch {
    pub query: Input,
    pub selected: usize,
    draft: Input,
}

/// Pasted lines shown as they are; a longer paste shows as one placeholder.
const PASTE_LINES: usize = 20;

impl Input {
    pub fn with(text: &str) -> Self {
        Self { text: text.to_owned(), cursor: text.chars().count(), pastes: Vec::new() }
    }

    fn byte(&self, at: usize) -> usize {
        self.text.char_indices().nth(at).map_or(self.text.len(), |(i, _)| i)
    }

    /// Insert `s` on one line: control characters, line breaks too, become spaces.
    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.put(&s);
    }

    /// Insert pasted text with its line breaks and tabs. Escape sequences
    /// and other control characters are dropped, and a paste of more than
    /// [`PASTE_LINES`] lines shows as a placeholder.
    pub fn paste(&mut self, s: &str) {
        let s = pasted(s);
        let lines = s.lines().count();
        if lines <= PASTE_LINES {
            return self.put(&s);
        }
        let placeholder = format!("[pasted #{}: {lines} lines]", self.pastes.len() + 1);
        self.put(&placeholder);
        self.pastes.push((placeholder, s));
    }

    /// The character just before the cursor.
    fn key_before(&self) -> Option<char> {
        self.cursor.checked_sub(1).and_then(|at| self.text.chars().nth(at))
    }

    pub fn newline(&mut self) {
        self.put("\n");
    }

    fn put(&mut self, s: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// The text, with every long paste in full, and an empty field.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        let mut text = std::mem::take(&mut self.text);
        for (placeholder, pasted) in std::mem::take(&mut self.pastes) {
            text = text.replacen(&placeholder, &pasted, 1);
        }
        text
    }

    /// The text with every long paste in full, leaving the field as it is.
    pub fn full(&self) -> String {
        self.clone().take()
    }

    /// The character index where the cursor's line starts, and where it ends.
    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let start = chars[..self.cursor].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let end = chars[self.cursor..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| self.cursor + i);
        (start, end)
    }

    /// Move to the line above (`up`) or below, keeping the column; false on the first or last line.
    pub fn line_step(&mut self, up: bool) -> bool {
        let chars: Vec<char> = self.text.chars().collect();
        let (start, end) = self.line_bounds();
        let column = self.cursor - start;
        if up {
            if start == 0 {
                return false;
            }
            let above = chars[..start - 1].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
            self.cursor = above + column.min(start - 1 - above);
        } else {
            if end == chars.len() {
                return false;
            }
            let below_end = chars[end + 1..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| end + 1 + i);
            self.cursor = end + 1 + column.min(below_end - end - 1);
        }
        true
    }

    /// Handle an editing key; false when the key is not one.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = self.line_bounds().0,
            KeyCode::Char('e') if ctrl => self.cursor = self.line_bounds().1,
            KeyCode::Char('u') if ctrl => {
                let start = self.line_bounds().0;
                let (a, b) = (self.byte(start), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && !matches!(chars[start - 1], ' ' | '\n') {
                    start -= 1;
                }
                let (a, b) = (self.byte(start), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            KeyCode::Home => self.cursor = self.line_bounds().0,
            KeyCode::End => self.cursor = self.line_bounds().1,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// Pasted text as the input keeps it: line breaks as `\n`, tabs kept, and
/// escape sequences and other control characters dropped, so a paste cannot
/// reach the terminal or the model as anything but text.
fn pasted(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\u{1b}' => match chars.peek() {
                // CSI: parameters, then one final byte from @ to ~.
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

impl App {
    pub fn restore_history(&mut self, entries: Vec<String>) {
        self.history = entries.into_iter().filter(|e| crate::tui::state::keep_history(e, &self.secrets)).collect();
        crate::tui::state::trim_history(&mut self.history);
        self.history_at = None;
    }

    pub fn take_history_changes(&mut self) -> Vec<crate::tui::state::HistoryChange> {
        std::mem::take(&mut self.history_changes)
    }

    pub fn history_matches(&self) -> Vec<&str> {
        let Some(search) = &self.history_search else { return Vec::new() };
        let query = search.query.text.to_lowercase();
        let mut seen = HashSet::new();
        self.history
            .iter()
            .rev()
            .filter(|e| e.to_lowercase().contains(&query) && seen.insert(e.as_str()))
            .map(String::as_str)
            .collect()
    }

    fn history_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let matches = self.history_matches();
        let search = self.history_search.as_ref().expect("history search is open");
        let picked = matches.get(search.selected).map(|s| (*s).to_owned());
        let n = matches.len();
        if ctrl(key, 'c') {
            self.input = self.history_search.take().unwrap().draft;
        } else if key.code == KeyCode::Enter {
            if let Some(picked) = picked {
                self.history_search = None;
                self.input.take();
                return self.submit(picked);
            }
        } else if matches!(key.code, KeyCode::Tab | KeyCode::Esc) {
            let search = self.history_search.take().unwrap();
            self.input = picked.map_or(search.draft, |s| Input::with(&s));
        } else {
            let search = self.history_search.as_mut().unwrap();
            if n > 0 && (ctrl(key, 'r') || key.code == KeyCode::Up) {
                search.selected = (search.selected + 1) % n;
            } else if n > 0 && key.code == KeyCode::Down {
                search.selected = (search.selected + n - 1) % n;
            } else {
                search.query.key(key);
                search.selected = 0;
            }
        }
        Vec::new()
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Vec<Action> {
        if ctrl(&key, 'l') {
            return vec![Action::Redraw];
        }
        if ctrl(&key, 'z') {
            return vec![Action::Suspend];
        }
        if self.history_search.is_some() {
            return self.history_key(&key);
        }
        if ctrl(&key, 'c') && self.queue_view.as_ref().is_some_and(|view| view.draft.is_some()) {
            return self.queue_key(&key);
        }
        if self.git_busy && (ctrl(&key, 'd') || (ctrl(&key, 'c') && self.input.text.is_empty())) {
            self.flash = Some("wait for Git and memory before quitting".into());
            return Vec::new();
        }
        if ctrl(&key, 'd') || (ctrl(&key, 'c') && self.task.is_none() && self.input.text.is_empty()) {
            if self.quit_armed.is_some_and(|at| self.tick.wrapping_sub(at) <= QUIT_TICKS) {
                self.quit = true;
                return vec![Action::Quit { keep: self.keep() }];
            }
            self.quit_armed = Some(self.tick);
            self.show_unexecuted();
            self.flash = Some(self.quit_warning(if ctrl(&key, 'd') { "ctrl+d" } else { "ctrl+c" }));
            return Vec::new();
        }
        self.quit_armed = None;
        let stop_armed = self.stop_armed.take();
        if self.shortcuts.is_some() {
            if key_is(&key, KeyCode::Char('?')) {
                self.shortcuts = None;
            } else {
                viewer_key(&mut self.shortcuts, &key);
            }
            return Vec::new();
        }
        if key_is(&key, KeyCode::Char('?')) && self.can_show_shortcuts() {
            self.shortcuts = Some(Viewer { title: "shortcuts".into(), lines: self.shortcut_lines(), scroll: 0 });
            return Vec::new();
        }
        if self.viewer.is_some() {
            self.viewer_key(&key);
            return Vec::new();
        }
        if self.queue_view.is_some() {
            return self.queue_key(&key);
        }
        if self.kept_view.is_some() {
            return self.kept_key(&key);
        }
        if self.transcript.is_some() {
            return self.transcript_key(&key);
        }
        if let Screen::Memory(_) = self.screen {
            return self.memory_key(&key);
        }
        if ctrl(&key, 'r') && self.prompt.is_none() {
            self.history_search =
                Some(HistorySearch { query: Input::with(&self.input.text), selected: 0, draft: self.input.clone() });
            return Vec::new();
        }
        let zoomed = self.zoom.is_some() && self.prompt.is_none() && self.input.text.is_empty();
        if ctrl(&key, 'o') || (zoomed && key.code == KeyCode::Char('o') && key.modifiers.is_empty()) {
            return self.open_transcript();
        }
        let commands = self.command_completions();
        if !commands.is_empty() && key.modifiers.is_empty() {
            let n = commands.len();
            let at = self.command_pick.min(n - 1);
            match key.code {
                KeyCode::Up => self.command_pick = (at + n - 1) % n,
                KeyCode::Down => self.command_pick = (at + 1) % n,
                KeyCode::Tab => self.input = Input::with(&commands[at].text),
                KeyCode::Esc => self.command_hidden = Some(self.input.text.clone()),
                _ => {}
            }
            if matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Esc) {
                return Vec::new();
            }
        }
        if ctrl(&key, 'c') && !self.input.text.is_empty() {
            self.input.take();
            return Vec::new();
        }
        if ctrl(&key, 'c') {
            return self.interrupt();
        }
        // Esc also closes every overlay and prompt: one press too many must not throw a task's work away.
        if key_is(&key, KeyCode::Esc) && self.busy() {
            if stop_armed.is_some_and(|at| self.tick.wrapping_sub(at) <= QUIT_TICKS) {
                return self.interrupt();
            }
            self.stop_armed = Some(self.tick);
            self.flash = Some("esc again to stop".into());
            return Vec::new();
        }
        // Confirmation must leave the evidence in the log reachable.
        if self.prompt.is_some() {
            match key.code {
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll = usize::MAX,
                KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll = 0,
                _ => return self.prompt_key(key),
            }
            return Vec::new();
        }
        self.prompt_key(key)
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Vec<Action> {
        match &self.prompt {
            Some(Prompt::Check(_)) => return self.check_key(&key),
            Some(Prompt::Apply { .. }) => return self.apply_key(&key),
            Some(Prompt::Retry { .. }) => {
                if let Some(actions) = self.retry_key(&key) {
                    return actions;
                }
            }
            Some(Prompt::EditCheck(_)) | None => {}
        }
        let editing_check = matches!(self.prompt, Some(Prompt::EditCheck(_)));
        // While an `@path` has suggestions, the arrows pick one, Tab puts it
        // in and Esc puts them away; every other key edits as usual.
        let suggestions = self.suggestions();
        if !suggestions.is_empty() && key.modifiers.is_empty() {
            let n = suggestions.len();
            let at = self.pick.min(n - 1);
            match key.code {
                KeyCode::Up => self.pick = (at + n - 1) % n,
                KeyCode::Down => self.pick = (at + 1) % n,
                KeyCode::Tab => self.complete(&suggestions[at]),
                KeyCode::Esc => self.hidden = self.at_query().map(|(_, q)| q),
                _ => {}
            }
            if matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Esc) {
                return Vec::new();
            }
        }
        self.pick = 0;
        match key.code {
            KeyCode::BackTab => {
                self.mode = self.mode.next();
                return Vec::new();
            }
            KeyCode::Tab => {
                if let Some(task) = &self.task {
                    let indices: Vec<u32> = task.lanes.iter().map(|l| l.index).collect();
                    self.zoom = match self.zoom.and_then(|z| indices.iter().position(|&i| i == z)) {
                        None => indices.first().copied(),
                        Some(at) => indices.get(at + 1).copied(),
                    };
                }
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                return Vec::new();
            }
            // Home and End move the cursor while there is text to move it in.
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) || self.input.text.is_empty() => {
                // The view clamps this to the top of the log.
                self.scroll = usize::MAX;
                return Vec::new();
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) || self.input.text.is_empty() => {
                self.scroll = 0;
                return Vec::new();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) && !editing_check => {
                self.input.newline();
                return Vec::new();
            }
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) && !editing_check => {
                self.input.newline();
                return Vec::new();
            }
            // A backslash before Enter continues the task on a new line, for terminals that send no Shift+Enter.
            KeyCode::Enter if !editing_check && self.input.cursor > 0 && self.input.key_before() == Some('\\') => {
                self.input.key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
                self.input.newline();
                return Vec::new();
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) && !editing_check => {
                return vec![Action::Edit { text: self.input.full() }];
            }
            KeyCode::Up if self.prompt.is_none() => {
                if !self.input.line_step(true) {
                    self.history_step(true);
                }
                return Vec::new();
            }
            KeyCode::Down if self.prompt.is_none() => {
                if !self.input.line_step(false) {
                    self.history_step(false);
                }
                return Vec::new();
            }
            KeyCode::Esc => {
                if let Some(Prompt::EditCheck(d)) = self.prompt.take() {
                    self.input.take();
                    self.prompt = Some(Prompt::Check(d));
                } else {
                    self.input.take();
                }
                return Vec::new();
            }
            KeyCode::Enter => {
                let line = self.input.take();
                if let Some(Prompt::EditCheck(mut d)) = self.prompt.take() {
                    let command = line.trim();
                    if !command.is_empty() && d.command.as_deref() != Some(command) {
                        d.command = Some(command.to_owned());
                        // The baseline was of the old command.
                        d.baseline = None;
                    }
                    self.prompt = Some(Prompt::Check(d));
                    return Vec::new();
                }
                return self.submit(line);
            }
            _ => {}
        }
        self.input.key(&key);
        Vec::new()
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        if self.history_at.is_none() && back {
            self.history_draft = Some(self.input.clone());
        }
        let at = match (self.history_at, back) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.history_at = at;
        self.input =
            at.map(|i| Input::with(&self.history[i])).unwrap_or_else(|| self.history_draft.take().unwrap_or_default());
    }

    pub(super) fn submit(&mut self, line: String) -> Vec<Action> {
        let line = line.trim().to_owned();
        if line.is_empty() {
            return Vec::new();
        }
        if self.history.last() != Some(&line) && crate::tui::state::keep_history(&line, &self.secrets) {
            self.history.push(line.clone());
            self.history_changes.push(crate::tui::state::HistoryChange::Append(line.clone()));
            crate::tui::state::trim_history(&mut self.history);
        }
        self.history_at = None;
        self.history_draft = None;
        self.scroll = 0;
        if let Some(command) = line.strip_prefix('/') {
            return self.command(command);
        }
        self.submit_task(line)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn submitted_history_survives_a_restart_and_arrows_restore_the_draft() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().to_str().unwrap();
        let store = crate::tui::state::Store::new(root.path(), root.path()).unwrap();
        let mut app = App::new(workspace, None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "fix login");
        app.task = None;
        type_in(&mut app, "fix cache");
        for change in app.take_history_changes() {
            store.change_history(&change, &app.secrets).unwrap();
        }
        let mut restarted = App::new(workspace, None);
        restarted.restore_history(store.history(&[]).unwrap().0);
        restarted.input = Input::with("unfinished draft");
        restarted.update(key(KeyCode::Up));
        assert_eq!(restarted.input.text, "fix cache");
        restarted.update(key(KeyCode::Up));
        assert_eq!(restarted.input.text, "fix login");
        restarted.update(key(KeyCode::Down));
        restarted.update(key(KeyCode::Down));
        assert_eq!(restarted.input.text, "unfinished draft");
    }

    #[test]
    fn reverse_history_search_deduplicates_cycles_and_executes_the_picked_input() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.settings.check = Some("true".into());
        app.restore_history(["run tests", "other", "run slow tests", "run tests"].map(str::to_owned).to_vec());
        app.input = Input::with("run");
        app.update(ctrl_key('r'));
        assert_eq!(app.history_matches(), ["run tests", "run slow tests"]);
        assert!(frame_text(&mut app).contains("› run tests"));
        app.update(ctrl_key('r'));
        assert!(frame_text(&mut app).contains("› run slow tests"));
        let actions = app.update(key(KeyCode::Enter));
        assert!(matches!(&actions[0], Action::Run { req, .. } if req.task == "run slow tests"));
        assert!(app.history_search.is_none());
    }

    #[test]
    fn reverse_search_can_edit_a_match_or_cancel_without_interrupting_a_task() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "running task");
        app.restore_history(vec!["fix café\nthen test".into()]);
        app.input = Input::with("draft");
        app.update(ctrl_key('r'));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("café".into()));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "fix café\nthen test");
        app.input = Input::with("draft");
        app.update(ctrl_key('r'));
        app.update(ctrl_key('u'));
        assert!(app.update(ctrl_key('c')).is_empty());
        assert_eq!(app.input.text, "draft");
        assert!(app.task.is_some());
        app.update(ctrl_key('r'));
        app.update(ctrl_key('u'));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.input.text, "fix café\nthen test");
    }

    #[test]
    fn clearing_the_log_keeps_history_and_history_clear_removes_it() {
        let mut app = App::new("/w", None);
        app.secrets = vec!["known-sensitive-value".into()];
        type_in(&mut app, "ordinary input");
        app.input.take();
        type_in(&mut app, "use known-sensitive-value");
        app.input.take();
        type_in(&mut app, "/clear");
        assert!(app.history.iter().any(|e| e == "ordinary input"));
        assert!(!app.history.iter().any(|e| e.contains("known-sensitive-value")));
        type_in(&mut app, "/history-clear");
        assert!(app.history.is_empty());
        assert!(matches!(app.take_history_changes().as_slice(), [crate::tui::state::HistoryChange::Clear]));
    }

    #[test]
    fn home_and_end_jump_through_the_log_unless_there_is_text_to_edit() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(key(KeyCode::Home));
        assert_eq!(app.scroll, usize::MAX);
        app.update(key(KeyCode::End));
        assert_eq!(app.scroll, 0);
        app.update(Msg::Paste("abc".into()));
        app.update(key(KeyCode::Home));
        assert_eq!((app.scroll, app.input.cursor), (0, 0), "home moves the cursor");
        app.update(Msg::Key(KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        assert_eq!(app.scroll, usize::MAX);
    }

    #[test]
    fn a_task_can_hold_several_lines() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("fix the parser".into()));
        app.update(with_mods(KeyCode::Enter, KeyModifiers::SHIFT));
        app.update(Msg::Paste("keep the API".into()));
        app.update(ctrl_key('j'));
        app.update(Msg::Paste("and the docs\\".into()));
        app.update(key(KeyCode::Enter));
        app.update(Msg::Paste("too".into()));
        assert_eq!(app.input.text, "fix the parser\nkeep the API\nand the docs\ntoo");

        // Up and Down move between the lines before they reach the history.
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Up));
        assert_eq!(app.input.cursor, "fix the parser\nkeep".chars().count() - 1);
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.input.cursor, app.input.text.chars().count());

        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "fix the parser\nkeep the API\nand the docs\ntoo");
        assert!(log(&app).ends_with("› fix the parser\n  keep the API\n  and the docs\n  too"), "{}", log(&app));
    }

    #[test]
    fn a_paste_keeps_its_lines_and_tabs_and_loses_escape_sequences() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("error[E0308]:\r\n\tmismatched \u{1b}[31mtypes\u{1b}[0m\u{1b}]0;title\u{7}\u{8}".into()));
        assert_eq!(app.input.text, "error[E0308]:\n\tmismatched types");
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "error[E0308]:\n\tmismatched types");
    }

    #[test]
    fn a_long_paste_shows_folded_and_is_sent_in_full() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let trace = (1..=143).map(|i| format!("frame {i}")).collect::<Vec<_>>().join("\n");
        app.update(Msg::Paste("why does this panic?\n".into()));
        app.update(Msg::Paste(trace.clone()));
        assert_eq!(app.input.text, "why does this panic?\n[pasted #1: 143 lines]");
        // The editor gets it in full.
        let actions = app.update(ctrl_key('g'));
        assert_eq!(actions, [Action::Edit { text: format!("why does this panic?\n{trace}") }]);
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, format!("why does this panic?\n{trace}"));
    }

    #[test]
    fn the_editor_fills_the_input() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("draft".into()));
        assert_eq!(app.update(ctrl_key('g')), [Action::Edit { text: "draft".into() }]);
        app.update(Msg::Edited(Ok("step one\nstep two\n".into())));
        assert_eq!((app.input.text.as_str(), app.input.cursor), ("step one\nstep two", 17));
        app.update(Msg::Edited(Err("vi exit status: 1".into())));
        assert_eq!(app.flash.as_deref(), Some("the editor failed: vi exit status: 1"));
        assert_eq!(app.input.text, "step one\nstep two", "a failed edit keeps the input");
    }
}

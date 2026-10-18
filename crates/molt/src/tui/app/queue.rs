//! Ordered, explicit tasks. Ordinary input still constrains the running task.
use super::*;

const MAX_TASKS: usize = 100;
const TASK_BYTES: usize = 64 * 1024;
const QUEUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, serde::Serialize)]
pub struct Queued {
    pub text: String,
    pub mode: Mode,
    pub settings: Settings,
}

impl Queued {
    fn bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(QUEUE_BYTES, |bytes| bytes.len())
    }
}

pub struct View {
    pub selected: usize,
    pub draft: Option<Input>,
}

impl App {
    pub(super) fn queue_command(&mut self, arg: &str) -> Vec<Action> {
        let (command, rest) = arg.split_once(' ').map_or((arg, ""), |(command, rest)| (command, rest.trim()));
        match command {
            "" => self.queue_view = Some(View { selected: 0, draft: None }),
            "add" if rest.is_empty() => self.queue_view = Some(View { selected: 0, draft: Some(Input::default()) }),
            "add" => self.enqueue(rest.to_owned()),
            "pause" if rest.is_empty() => {
                self.queue_paused = true;
                self.flash = Some("queue paused; the current task continues".into());
            }
            "resume" if rest.is_empty() => {
                self.queue_paused = false;
                self.flash = Some("queue resumed".into());
            }
            "clear" if rest.is_empty() => {
                self.queue.clear();
                self.flash = Some("queued tasks cleared; the current task continues".into());
            }
            "remove" => match self.queue_index(rest) {
                Some(at) => {
                    self.queue.remove(at);
                }
                None => self.flash = Some("/queue remove N: use a position from /queue".into()),
            },
            "move" => {
                let indices: Vec<_> = rest.split_whitespace().collect();
                let pair = (indices.len() == 2).then(|| (self.queue_index(indices[0]), self.queue_index(indices[1])));
                if let Some((Some(from), Some(to))) = pair {
                    let task = self.queue.remove(from);
                    self.queue.insert(to, task);
                } else {
                    self.flash = Some("/queue move FROM TO: use two positions from /queue".into());
                }
            }
            _ => self.flash = Some("/queue [add TASK|remove N|move FROM TO|pause|resume|clear]".into()),
        }
        Vec::new()
    }

    fn queue_index(&self, text: &str) -> Option<usize> {
        text.parse::<usize>().ok().filter(|&n| n > 0 && n <= self.queue.len()).map(|n| n - 1)
    }

    fn enqueue(&mut self, text: String) {
        let task = Queued { text, mode: self.mode, settings: self.settings.clone() };
        if task.text.len() > TASK_BYTES
            || self.queue.len() >= MAX_TASKS
            || task.bytes().saturating_add(self.queue.iter().map(Queued::bytes).sum()) > QUEUE_BYTES
        {
            self.flash = Some("queue full: at most 100 tasks, 64 KiB per task and 4 MiB total; draft retained".into());
            if let Some(view) = &mut self.queue_view {
                view.draft = Some(Input::with(&task.text));
            } else {
                self.input = Input::with(&format!("/queue add {}", task.text));
            }
            return;
        }
        self.queue.push(task);
        self.flash = Some(format!("queued {}", self.queue.len()));
    }

    pub(super) fn advance_queue(&mut self) -> Vec<Action> {
        if let Some(view) = &mut self.queue_view {
            view.selected = view.selected.min(self.queue.len().saturating_sub(1));
        }
        if self.quit
            || self.quit_armed.is_some()
            || self.queue_paused
            || self.queue.is_empty()
            || self.task.is_some()
            || self.committing
            || self.fork_operations > 0
            || self.kept_pending.is_some()
            || !matches!(self.session, SessionState::Up { .. })
        {
            return Vec::new();
        }
        let queued = self.queue[0].clone();
        let draft = self.input.clone();
        // Completion may predate the previous apply. The planner reads @files from
        // the current workspace; only path syntax can be checked here.
        let complete = std::mem::replace(&mut self.files_complete, false);
        let actions = self.start_task(queued.text, queued.mode, queued.settings);
        self.files_complete = complete;
        self.input = draft;
        if self.task.is_some() {
            self.queue.remove(0);
            // A failure's optional retry offer yields to explicitly queued work.
            self.prompt = None;
        } else {
            self.queue_paused = true;
            self.flash = Some(format!(
                "queue paused: {}; fix the task and /queue resume",
                self.flash.take().unwrap_or_else(|| "task could not start".into())
            ));
        }
        actions
    }

    pub(super) fn queue_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let view = self.queue_view.as_mut().unwrap();
        if let Some(input) = &mut view.draft {
            if key_is(key, KeyCode::Esc) || ctrl(key, 'c') {
                view.draft = None;
            } else if ctrl(key, 'j')
                || (key.code == KeyCode::Enter && key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT))
            {
                input.newline();
            } else if key_is(key, KeyCode::Enter) {
                let text = input.take();
                if !text.trim().is_empty() {
                    view.draft = None;
                    return self.submit(format!("/queue add {}", text.trim()));
                }
            } else {
                input.key(key);
            }
            return Vec::new();
        }
        let at = view.selected.min(self.queue.len().saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.queue_view = None,
            KeyCode::Char('a') => view.draft = Some(Input::default()),
            KeyCode::Up | KeyCode::Char('k') => view.selected = at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selected = (at + 1).min(self.queue.len().saturating_sub(1)),
            KeyCode::Char('K') if at > 0 => {
                self.queue.swap(at, at - 1);
                view.selected = at - 1;
            }
            KeyCode::Char('J') if at + 1 < self.queue.len() => {
                self.queue.swap(at, at + 1);
                view.selected = at + 1;
            }
            KeyCode::Delete | KeyCode::Char('n') if !self.queue.is_empty() => {
                self.queue.remove(at);
                view.selected = at.min(self.queue.len().saturating_sub(1));
            }
            KeyCode::Char('p') => self.queue_paused = true,
            KeyCode::Char('r') => self.queue_paused = false,
            KeyCode::Enter | KeyCode::Char('v') if !self.queue.is_empty() => {
                self.viewer = Some(Viewer {
                    title: format!("queued task {}", at + 1),
                    lines: block(&scrub(&self.queue[at].text, &self.secrets), "", Tone::Plain),
                    scroll: 0,
                });
            }
            _ => {}
        }
        Vec::new()
    }

    pub(in crate::tui) fn queue_lines(&self) -> Vec<Line<'static>> {
        if self.queue.is_empty() {
            return block("no queued tasks · a adds a task", "", Tone::Dim);
        }
        self.queue
            .iter()
            .enumerate()
            .flat_map(|(index, task)| {
                let mark =
                    if self.queue_view.as_ref().is_some_and(|view| view.selected == index) { "›" } else { " " };
                let model = task.settings.model.as_deref().unwrap_or("default model");
                let check = task.settings.check.as_deref().unwrap_or("design a check");
                vec![
                    Line::from(span(
                        format!("{mark} {}. {}", index + 1, clean_line(&scrub(&task.text, &self.secrets))),
                        Tone::Shell,
                    )),
                    Line::from(span(format!("   {} · {}", task.mode.label(), clean_line(model)), Tone::Dim)),
                    Line::from(span(format!("   check: {}", clean_line(&scrub(check, &self.secrets))), Tone::Dim)),
                ]
            })
            .collect()
    }

    pub(in crate::tui) fn unexecuted(&self) -> Vec<String> {
        self.queue.iter().map(|task| text::clean(&scrub(&task.text, &self.secrets)).join("\n")).collect()
    }

    pub(super) fn show_unexecuted(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let mut lines = block("These tasks have not run. Their complete text is printed after exit.", "", Tone::Warn);
        for (index, task) in self.unexecuted().iter().enumerate() {
            lines.push(Line::from(span(format!("queued {}", index + 1), Tone::Shell)));
            lines.extend(block(task, "  ", Tone::Plain));
        }
        self.viewer =
            Some(Viewer { title: "unexecuted queued tasks · Ctrl+D again to quit".into(), lines, scroll: 0 });
    }
}

//! Read-only progress dashboard for `molt do --tui`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Stderr};
use std::path::Path;

use anyhow::{bail, Context};
use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use molt::agent;
use molt_api::planner::AttemptStatus;
use molt_api::progress::Progress;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, Paragraph};
use ratatui::Terminal;

const HISTORY: usize = 100;

pub fn ensure_tty() -> anyhow::Result<()> {
    if !io::stderr().is_terminal() {
        bail!("--tui requires an interactive stderr terminal");
    }
    Ok(())
}

#[derive(Default)]
struct Attempt {
    status: &'static str,
    detail: String,
}

struct State {
    task: String,
    workspace: String,
    check: String,
    attempts: BTreeMap<u32, Attempt>,
    activity: VecDeque<String>,
}

impl State {
    fn new(task: &str, workspace: &Path, count: u32) -> Self {
        Self {
            task: agent::printable(task),
            workspace: agent::printable(&workspace.display().to_string()),
            check: "waiting for check".into(),
            attempts: (1..=count).map(|n| (n, Attempt { status: "waiting", detail: String::new() })).collect(),
            activity: VecDeque::new(),
        }
    }

    fn update(&mut self, event: &Progress) {
        match event {
            Progress::CheckReady { command, .. } => {
                self.check = command.as_deref().map_or_else(|| "unverified".into(), agent::printable);
            }
            Progress::AttemptStarted { attempt, .. } => {
                self.attempts.entry(*attempt).or_default().status = "running";
            }
            Progress::ToolCall { attempt, turn, detail, .. } => {
                let view = self.attempts.entry(*attempt).or_default();
                view.status = "running";
                view.detail = format!("turn {turn}: {}", agent::printable(detail));
            }
            Progress::CheckRan { attempt, passed, .. } => {
                let view = self.attempts.entry(*attempt).or_default();
                view.status = if *passed { "check passed" } else { "check failed" };
            }
            Progress::AttemptFinished { attempt, status, .. } => {
                self.attempts.entry(*attempt).or_default().status = match status {
                    AttemptStatus::Passed => "passed",
                    AttemptStatus::Failed => "failed",
                    AttemptStatus::Cancelled => "cancelled",
                    AttemptStatus::Error => "error",
                };
            }
            Progress::Note { .. } => {}
        }
        self.activity.push_back(agent::describe(event));
        if self.activity.len() > HISTORY {
            self.activity.pop_front();
        }
    }
}

pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stderr>>,
    state: State,
}

impl Dashboard {
    pub fn new(task: &str, workspace: &Path, attempts: u32) -> anyhow::Result<Self> {
        ensure_tty()?;
        let backend = CrosstermBackend::new(io::stderr());
        let mut terminal = Terminal::new(backend).context("opening TUI terminal")?;
        execute!(terminal.backend_mut(), EnterAlternateScreen).context("entering TUI screen")?;
        let mut dashboard = Self { terminal, state: State::new(task, workspace, attempts) };
        execute!(dashboard.terminal.backend_mut(), Hide).context("hiding terminal cursor")?;
        dashboard.render()?;
        Ok(dashboard)
    }

    pub fn on_progress(&mut self, event: &Progress) -> io::Result<()> {
        self.state.update(event);
        self.render()
    }

    fn render(&mut self) -> io::Result<()> {
        let state = &self.state;
        self.terminal.draw(|frame| render_state(state, frame))?;
        Ok(())
    }
}

fn render_state(state: &State, frame: &mut ratatui::Frame<'_>) {
    let areas = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(state.attempts.len() as u16 + 2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let overview = Paragraph::new(vec![
        Line::from(vec![Span::styled("Task  ", Style::default().fg(Color::Cyan)), Span::raw(&state.task)]),
        Line::from(vec![Span::styled("Dir   ", Style::default().fg(Color::Cyan)), Span::raw(&state.workspace)]),
        Line::from(vec![Span::styled("Check ", Style::default().fg(Color::Cyan)), Span::raw(&state.check)]),
    ])
    .block(Block::bordered().title(" Molt "));
    frame.render_widget(overview, areas[0]);

    let attempts: Vec<Line> = state
        .attempts
        .iter()
        .map(|(n, view)| {
            let color = match view.status {
                "passed" | "check passed" => Color::Green,
                "failed" | "error" | "check failed" => Color::Red,
                "running" => Color::Yellow,
                _ => Color::Gray,
            };
            Line::from(vec![
                Span::raw(format!("#{n:<2} ")),
                Span::styled(format!("{:<13}", view.status), Style::default().fg(color)),
                Span::raw(&view.detail),
            ])
        })
        .collect();
    frame.render_widget(List::new(attempts).block(Block::bordered().title(" Attempts ")), areas[1]);

    let visible = areas[2].height.saturating_sub(2) as usize;
    let activity: Vec<Line> =
        state.activity.iter().rev().take(visible).rev().map(|line| Line::raw(line.as_str())).collect();
    frame.render_widget(List::new(activity).block(Block::bordered().title(" Recent activity ")), areas[2]);
    frame.render_widget(Paragraph::new("Ctrl-C: stop run · progress updates when events arrive"), areas[3]);
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_updates_attempts_and_caps_activity() {
        let mut state = State::new("task", Path::new("/tmp/project"), 2);
        state.update(&Progress::CheckReady {
            run: "r".into(),
            command: Some("cargo test".into()),
            files: vec![],
            designed: false,
        });
        state.update(&Progress::AttemptStarted { run: "r".into(), attempt: 2 });
        state.update(&Progress::ToolCall {
            run: "r".into(),
            attempt: 2,
            turn: 3,
            tool: "run".into(),
            detail: "cargo test".into(),
        });
        state.update(&Progress::AttemptFinished { run: "r".into(), attempt: 2, status: AttemptStatus::Passed });
        assert_eq!(state.check, "cargo test");
        assert_eq!(state.attempts[&2].status, "passed");
        assert!(state.attempts[&2].detail.contains("turn 3"));
        for _ in 0..HISTORY + 1 {
            state.update(&Progress::Note { run: "r".into(), message: "more".into() });
        }
        assert_eq!(state.activity.len(), HISTORY);
    }

    #[test]
    fn dashboard_renders_in_a_small_terminal() {
        use ratatui::backend::TestBackend;

        let state = State::new("a long task that does not fit", Path::new("/tmp/project"), 8);
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal.draw(|frame| render_state(&state, frame)).unwrap();
    }
}

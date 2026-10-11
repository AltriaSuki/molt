//! Drawing [`App`] into a frame.

use molt_api::planner::RunState;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::app::{
    memory_lines, App, ChangeStep, Input, MemoryEdit, MemoryView, Mode, Phase, Prompt, Screen, SessionState, Task,
};
use super::text::{self, clean_line, span, style, wrap_all, Tone};

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [header, main, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    draw_header(frame, header, app);
    match &app.screen {
        Screen::Main => draw_main(frame, main, app),
        Screen::Memory(view) => draw_memory(frame, main, view, app.tick),
    }
    draw_status(frame, status, app);
    if let Some(viewer) = &mut app.viewer {
        let r = centered(area, 92, 88);
        frame.render_widget(Clear, r);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style(Tone::Dim))
            .title(Line::from(span(format!(" {} ", viewer.title), Tone::Shell)))
            .title_bottom(Line::from(span(" j/k scroll · g/G ends · esc close ", Tone::Dim)).right_aligned());
        let inner = block.inner(r);
        frame.render_widget(block, r);
        let lines = wrap_all(&viewer.lines, inner.width);
        viewer.scroll = viewer.scroll.min(lines.len().saturating_sub(1));
        let shown: Vec<Line> = lines.into_iter().skip(viewer.scroll).take(usize::from(inner.height)).collect();
        frame.render_widget(Paragraph::new(shown), inner);
    }
}

fn centered(area: Rect, w_pct: u16, h_pct: u16) -> Rect {
    let w = area.width * w_pct / 100;
    let h = area.height * h_pct / 100;
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let width = usize::from(area.width);
    let mut right: Vec<Span> = Vec::new();
    match &app.session {
        SessionState::Up { versions, .. } => {
            for (name, version) in versions {
                right.push(span(name.clone(), Tone::Dim));
                right.push(span(format!("@{version} "), Tone::Shell));
            }
            right.push(span("● ", Tone::Ok));
        }
        SessionState::Starting => right.push(span(format!("{} starting ", SPINNER[spin(app.tick)]), Tone::Warn)),
        SessionState::Down(_) => right.push(span("✗ services down ", Tone::Bad)),
    }
    let screen = if let Screen::Memory(_) = app.screen { "  memory" } else { "" };
    let fixed = " molt ".len() + 1 + text::width(screen) + 2;
    let right_width = |right: &[Span]| right.iter().map(|s| text::width(&s.content)).sum::<usize>();
    // Versions give way first, a whole name@version at a time, then the workspace path.
    while right.len() > 1 && fixed + 40 + right_width(&right) > width {
        right.drain(..2.min(right.len() - 1));
    }
    let room = width.saturating_sub(fixed + right_width(&right));
    let mut left = vec![
        Span::styled(" molt ", style(Tone::Shell).add_modifier(Modifier::BOLD)),
        span(format!(" {}", text::fit(&app.workspace, room)), Tone::Dim),
        span(screen, Tone::Mem),
    ];
    let used: usize = left.iter().map(|s| text::width(&s.content)).sum();
    left.push(Span::raw(" ".repeat(width.saturating_sub(used + right_width(&right)))));
    left.extend(right);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
}

fn spin(tick: u64) -> usize {
    (tick % SPINNER.len() as u64) as usize
}

fn draw_main(frame: &mut Frame, area: Rect, app: &mut App) {
    let live = live_lines(app, area.width.saturating_sub(2));
    let lanes = app.task.as_ref().is_some_and(|t| t.phase == Phase::Running && !t.lanes.is_empty());
    // The input grows with its lines, up to half the screen.
    let (rows, _) = field_rows(&app.input, field_width(area.width));
    let input_height = u16::try_from(rows.len()).unwrap_or(u16::MAX).clamp(1, (area.height / 2).max(1)) + 2;
    let taken = area.height.saturating_sub(input_height + 2);
    let live_height = if lanes {
        let room = taken.saturating_sub(1);
        if app.zoom.is_some() { room * 2 / 3 } else { (room / 2).min(14) }.max(4)
    } else {
        u16::try_from(live.len()).unwrap_or(u16::MAX).min(taken)
    };
    let [log, live_area, input] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(live_height), Constraint::Length(input_height)])
            .areas(area);

    // The log, from its end unless scrolled.
    let width = log.width.saturating_sub(2).max(1);
    let lines = wrap_all(&app.log, width);
    let height = usize::from(log.height);
    let max_scroll = lines.len().saturating_sub(height);
    app.scroll = app.scroll.min(max_scroll);
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(height);
    let mut shown: Vec<Line> = lines[start..end].to_vec();
    // The log sits on the input, as a terminal's output does.
    while shown.len() < height {
        shown.insert(0, Line::default());
    }
    let inner = Rect { x: log.x + 1, width, ..log };
    frame.render_widget(Paragraph::new(shown), inner);
    if app.scroll > 0 {
        let hint = format!(" {} more below · pgdn ", app.scroll);
        let w = u16::try_from(text::width(&hint)).unwrap_or(0).min(log.width);
        let at = Rect { x: log.x + log.width - w, y: log.y + log.height - 1, width: w, height: 1 };
        frame.render_widget(Paragraph::new(span(hint, Tone::Warn)), at);
    }

    if lanes {
        draw_lanes(frame, live_area, app);
    } else {
        let inner = Rect { x: live_area.x + 1, width: live_area.width.saturating_sub(2), ..live_area };
        frame.render_widget(Paragraph::new(live), inner);
    }
    draw_input(frame, input, app);
}

/// What is under way, or what waits on the user, above the input.
fn live_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let spinner = SPINNER[spin(app.tick)];
    let busy = |what: &str| vec![Line::from(span(format!("{spinner} {what}"), Tone::Sea))];
    match &app.prompt {
        Some(Prompt::Check(d)) | Some(Prompt::EditCheck(d)) => {
            let editing = matches!(app.prompt, Some(Prompt::EditCheck(_)));
            let attempts = app.task.as_ref().map_or(2, |t| t.req.attempts);
            let mut lines = vec![Line::from(vec![
                span("◆ check", Tone::Shell),
                span(format!("  designed · ${:.2}", d.cost_usd), Tone::Dim),
            ])];
            if d.uncounted_calls > 0 {
                lines.push(Line::from(span(
                    format!("  cost incomplete · {} calls uncounted · remaining budget unknown", d.uncounted_calls),
                    Tone::Warn,
                )));
            } else if let Some(remaining) = d.remaining_budget_usd {
                lines.push(Line::from(span(
                    format!("  ${remaining:.2} remaining for attempts"),
                    if remaining <= 0.0 { Tone::Bad } else { Tone::Dim },
                )));
            }
            match &d.command {
                Some(c) => lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(c), Tone::Plain)])),
                None => lines.push(Line::from(span("  no automated check fits this task", Tone::Warn))),
            }
            for l in text::clean(d.rationale.trim()) {
                lines.push(Line::from(span(format!("  {l}"), Tone::Dim)));
            }
            for f in &d.files {
                let n = match f.content.lines().count() {
                    1 => "1 line".to_owned(),
                    n => format!("{n} lines"),
                };
                lines.push(Line::from(span(format!("  + {}  {n}", clean_line(&f.path)), Tone::Dim)));
            }
            if d.command.is_some() {
                lines.push(match &d.baseline {
                    Some(b) if b.passed => Line::from(vec![
                        span("  baseline  ", Tone::Dim),
                        span("passes already: it cannot tell done from not done", Tone::Bad),
                    ]),
                    Some(b) => Line::from(vec![
                        span("  baseline  ", Tone::Dim),
                        span(
                            match b.exit_code {
                                Some(code) => format!("fails now (exit {code})"),
                                None => "fails now (killed or timed out)".to_owned(),
                            },
                            Tone::Ok,
                        ),
                    ]),
                    None => Line::from(vec![span("  baseline  ", Tone::Dim), span("not run", Tone::Dim)]),
                });
            }
            lines.extend(stops_line(app));
            lines.push(Line::default());
            if editing {
                lines.push(keys(&[("enter", "save the command"), ("esc", "back")]));
            } else if d.command.is_some() {
                let run = if attempts == 1 { "run 1 attempt".to_owned() } else { format!("run {attempts} attempts") };
                let mut k = vec![("enter", run.as_str()), ("e", "edit")];
                if !d.files.is_empty() {
                    k.push(("v", "files"));
                }
                k.extend([("n", "no check"), ("esc", "cancel")]);
                lines.push(keys(&k));
            } else {
                lines.push(keys(&[("enter", "run unverified"), ("esc", "cancel")]));
            }
            wrap_all(&lines, width)
        }
        Some(Prompt::Apply { resp, .. }) => {
            let (added, removed) = text::diff_counts(&resp.patch);
            let files =
                if resp.changes.len() == 1 { "1 file".to_owned() } else { format!("{} files", resp.changes.len()) };
            let mut lines = vec![Line::from(vec![
                span("◆ apply to the workspace?", Tone::Shell),
                span(format!("  {files}  "), Tone::Dim),
                span(format!("+{added}"), Tone::Ok),
                span(" ", Tone::Plain),
                span(format!("−{removed}"), Tone::Bad),
            ])];
            lines.extend(stops_line(app));
            lines.push(keys(&[("enter", "apply"), ("d", "diff"), ("k", "keep in its fork"), ("n", "discard")]));
            lines
        }
        Some(Prompt::Retry { req, stopped, .. }) => vec![
            Line::from(vec![
                if *stopped { span("■ stopped", Tone::Warn) } else { span("◆ no attempt passed", Tone::Bad) },
                span(format!("  $ {}", clean_line(req.check.as_deref().unwrap_or_default())), Tone::Dim),
            ]),
            if *stopped {
                keys(&[("r", "start again with the same check"), ("v", "what the attempts did"), ("esc", "dismiss")])
            } else {
                keys(&[("r", "retry with the same check"), ("v", "why"), ("esc", "dismiss")])
            },
        ],
        None => match &app.task {
            Some(task @ Task { phase: Phase::Designing, .. }) => {
                let mut lines = busy("designing the check");
                lines.extend(task.designer_preview.lines(width, 4));
                lines
            }
            Some(task @ Task { phase: Phase::Running, .. }) if task.lanes.is_empty() => {
                let mut lines =
                    busy(if task.req.check.is_some() || !task.req.verify { "starting" } else { "designing the check" });
                lines.extend(task.designer_preview.lines(width, 4));
                lines
            }
            Some(Task { phase: Phase::Applying, .. }) => busy("applying"),
            _ => Vec::new(),
        },
    }
}

/// Why confirm mode stopped, when a rule did it.
fn stops_line(app: &App) -> Option<Line<'static>> {
    let stops = &app.task.as_ref()?.stops;
    (!stops.is_empty()).then(|| Line::from(vec![span("  stopped: ", Tone::Dim), span(stops.join(" · "), Tone::Warn)]))
}

fn keys(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (i, (k, what)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(span(" · ", Tone::Dim));
        }
        spans.push(span(k.to_string(), Tone::Shell));
        spans.push(span(format!(" {what}"), Tone::Dim));
    }
    Line::from(spans)
}

fn draw_lanes(frame: &mut Frame, area: Rect, app: &App) {
    let Some(task) = &app.task else { return };
    let lanes: Vec<_> = match app.zoom {
        Some(z) => task.lanes.iter().filter(|l| l.index == z).collect(),
        None => task.lanes.iter().collect(),
    };
    if lanes.is_empty() {
        return;
    }
    let per_row = lanes.len().min(4);
    let rows = lanes.len().div_ceil(per_row);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    for (r, chunk) in lanes.chunks(per_row).enumerate() {
        let cols =
            Layout::horizontal(vec![Constraint::Ratio(1, per_row as u32); per_row]).spacing(1).split(row_areas[r]);
        for (c, lane) in chunk.iter().enumerate() {
            let (base, tone) = match lane.status {
                None => (format!("{} turn {}", SPINNER[spin(app.tick)], lane.turn), Tone::Sea),
                Some(molt_api::planner::AttemptStatus::Passed) => ("✓ passed".to_owned(), Tone::Ok),
                Some(molt_api::planner::AttemptStatus::Failed) => ("✗ failed".to_owned(), Tone::Bad),
                Some(molt_api::planner::AttemptStatus::Cancelled) => ("– cancelled".to_owned(), Tone::Dim),
                Some(molt_api::planner::AttemptStatus::Error) => ("! error".to_owned(), Tone::Warn),
            };
            // Two borders and the spaces around the right title. A narrow
            // lane drops the word "attempt" before the status is cut.
            let room = |name: &str| usize::from(cols[c].width).saturating_sub(text::width(name) + 4);
            let mut name = format!(" attempt {} ", lane.index + 1);
            if room(&name) < text::width(&base) {
                name = format!(" {} ", lane.index + 1);
            }
            let room = room(&name);
            // Without previews no call's cost arrives before the reply.
            let cost = task.req.stream.then(|| lane.spend.label());
            let status = lane_status(&base, &text::duration(lane.elapsed()), cost.as_deref(), room);
            let border = match lane.status {
                Some(molt_api::planner::AttemptStatus::Passed) => Tone::Ok,
                _ => Tone::Dim,
            };
            let block = Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style(border))
                .title(Line::from(span(name, Tone::Sea)))
                .title(Line::from(span(format!(" {status} "), tone)).right_aligned());
            let inner = block.inner(cols[c]);
            frame.render_widget(block, cols[c]);
            let preview = lane.preview.lines(inner.width, usize::from(inner.height).min(4));
            let history_height = usize::from(inner.height).saturating_sub(preview.len());
            let lines = wrap_all(&lane.lines, inner.width);
            let skip = lines.len().saturating_sub(history_height);
            let mut shown = lines[skip..].to_vec();
            shown.extend(preview);
            frame.render_widget(Paragraph::new(shown), inner);
        }
    }
}

/// A lane's status with its running time and cost, as many as fit in `room`
/// cells: the cost goes first, then the time.
fn lane_status(base: &str, time: &str, cost: Option<&str>, room: usize) -> String {
    let with_cost = cost.map(|cost| format!("{base} · {time} · {cost}"));
    with_cost
        .into_iter()
        .chain([format!("{base} · {time}")])
        .find(|s| text::width(s) <= room)
        .unwrap_or_else(|| base.to_owned())
}

/// The task's spending against its budget, warning from 80% of it.
fn task_spend(task: &Task) -> (String, Tone) {
    let spend = task.spend();
    match task.budget_usd {
        Some(b) => {
            let tone = if spend.usd >= b * 0.8 { Tone::Warn } else { Tone::Dim };
            (format!("task {} / ${b:.2}", spend.label()), tone)
        }
        None => (format!("task {}", spend.label()), Tone::Dim),
    }
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let waiting = matches!(app.prompt, Some(Prompt::Check(_) | Prompt::Apply { .. })) || app.viewer.is_some();
    let editing_check = matches!(app.prompt, Some(Prompt::EditCheck(_)));
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style(if waiting { Tone::Dim } else { Tone::Shell }));
    if editing_check {
        block = block.title(Line::from(span(" check command ", Tone::Shell)));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let prompt = span("› ", if waiting { Tone::Dim } else { Tone::Shell });
    let field = Rect { x: inner.x + 2, width: inner.width.saturating_sub(2), ..inner };
    frame.render_widget(Paragraph::new(Line::from(prompt)), inner);
    if app.input.text.is_empty() && !editing_check {
        let hint = if waiting { "" } else { "a task, or /help" };
        frame.render_widget(Paragraph::new(span(hint, Tone::Dim)), field);
        if !waiting {
            frame.set_cursor_position((field.x, field.y));
        }
        return;
    }
    draw_field(frame, field, &app.input, !waiting);
}

/// The width of the task input's text: inside the border, after the prompt mark.
fn field_width(width: u16) -> u16 {
    width.saturating_sub(4)
}

/// `input` as rows that fit `width`, its lines broken where they do not,
/// and the row and column of its cursor.
fn field_rows(input: &Input, width: u16) -> (Vec<String>, (usize, usize)) {
    // One cell is kept for the cursor at the end of a full row.
    let room = usize::from(width.saturating_sub(1)).max(1);
    let mut rows = vec![String::new()];
    let mut widths = vec![0];
    let mut at = (0, 0);
    for (i, c) in input.text.chars().enumerate() {
        if i == input.cursor {
            at = (rows.len() - 1, widths[rows.len() - 1]);
        }
        if c == '\n' {
            rows.push(String::new());
            widths.push(0);
            continue;
        }
        let shown = clean_line(&c.to_string());
        let w = text::width(&shown);
        if widths[rows.len() - 1] + w > room {
            rows.push(String::new());
            widths.push(0);
        }
        let last = rows.len() - 1;
        rows[last].push_str(&shown);
        widths[last] += w;
    }
    if input.cursor >= input.text.chars().count() {
        at = (rows.len() - 1, widths[rows.len() - 1]);
    }
    (rows, at)
}

/// A text field scrolled so its cursor shows.
fn draw_field(frame: &mut Frame, area: Rect, input: &Input, cursor: bool) {
    let (rows, (row, column)) = field_rows(input, area.width);
    let height = usize::from(area.height).max(1);
    let top = (row + 1).saturating_sub(height);
    let shown: Vec<Line> = rows.into_iter().skip(top).take(height).map(Line::from).collect();
    frame.render_widget(Paragraph::new(shown), area);
    if cursor {
        let x = area.x + u16::try_from(column).unwrap_or(0);
        let y = area.y + u16::try_from(row - top).unwrap_or(0);
        frame.set_cursor_position((x, y));
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![Span::raw(" ")];
    let mode_tone = if app.mode == Mode::Auto { Tone::Warn } else { Tone::Shell };
    left.push(span(app.mode.label(), mode_tone));
    left.push(span(" (shift+tab)", Tone::Dim));
    let s = &app.settings;
    let model = s.model.as_deref().unwrap_or("default model");
    let mut parts = vec![model.to_owned()];
    if let Some(effort) = s.effort {
        parts.push(serde_json::to_value(effort).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default());
    }
    if let Some(n) = s.attempts {
        parts.push(if n == 1 { "1 attempt".into() } else { format!("{n} attempts") });
    }
    if let Some(check) = &s.check {
        parts.push(format!("check: {}", text::fit(&clean_line(check), 24)));
    }
    if !s.stream {
        parts.push("previews off".into());
    }
    let mut cost = format!("${:.2}", app.spent_usd);
    if app.uncounted_calls > 0 {
        cost.push_str(&format!(" known · {} calls uncounted", app.uncounted_calls));
    }
    // Without previews no call's cost arrives before the reply.
    let task = app.task.as_ref().filter(|t| t.req.stream && matches!(t.phase, Phase::Designing | Phase::Running));
    if let (Some(b), None) = (s.budget_usd, task) {
        cost.push_str(&format!(" · ${b:.2}/task"));
    }
    parts.push(cost);
    left.push(span(format!("  {}", parts.join(" · ")), Tone::Dim));
    if let Some(task) = task {
        let (spent, tone) = task_spend(task);
        left.push(span(" · ", Tone::Dim));
        left.push(span(spent, tone));
    }

    let mut right = Vec::new();
    if let Some(flash) = &app.flash {
        right.push(span(format!("{} ", clean_line(flash)), Tone::Warn));
    } else {
        if !app.learning.is_empty() {
            right.push(span(format!("{} learning  ", SPINNER[spin(app.tick)]), Tone::Mem));
        }
        if let Some(task) = app.task.as_ref().filter(|t| t.phase == Phase::Running) {
            match task.state {
                RunState::Paused => right.push(span("paused · /resume  ", Tone::Warn)),
                RunState::Pausing => {
                    let waiting = text::fit(&clean_line(&task.waiting.join(", ")), usize::from(area.width) / 3);
                    right.push(span(format!("pausing; waiting for {waiting}  "), Tone::Warn));
                }
                RunState::Running if task.version > 0 => {
                    right.push(span(format!("requirements v{}  ", task.version), Tone::Dim));
                }
                RunState::Running => {}
            }
        }
        if !app.earlier.is_empty() && app.task.is_none() {
            let n = app.earlier.len();
            let tasks = if n == 1 { "1 earlier task".to_owned() } else { format!("{n} earlier tasks") };
            right.push(span(format!("{tasks} · /new  "), Tone::Dim));
        }
        if let SessionState::Down(e) = &app.session {
            right.push(span(format!("{} ", text::fit(e, usize::from(area.width) / 3)), Tone::Bad));
        }
        right.push(span("/help ", Tone::Dim));
    }
    // The right side is drawn over the left one when they meet.
    let rw = u16::try_from(right.iter().map(|s| text::width(&s.content)).sum::<usize>()).unwrap_or(0).min(area.width);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    let at = Rect { x: area.x + area.width - rw, width: rw, ..area };
    frame.render_widget(Clear, at);
    frame.render_widget(Paragraph::new(Line::from(right)), at);
}

fn draw_memory(frame: &mut Frame, area: Rect, view: &MemoryView, tick: u64) {
    let [body, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);
    let [list, detail] = Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(body);

    let mut rows: Vec<Line> = Vec::new();
    let head = if view.query.is_empty() {
        "all notes".to_owned()
    } else {
        format!("matching «{}»", clean_line(&view.query))
    };
    rows.push(Line::from(vec![
        span(format!(" {head}"), Tone::Dim),
        span(format!("  {}", view.notes.len()), Tone::Dim),
    ]));
    rows.push(Line::default());
    if view.loading {
        rows.push(Line::from(span(format!(" {} loading", SPINNER[spin(tick)]), Tone::Sea)));
    } else if let Some(e) = &view.error {
        rows.push(Line::from(span(format!(" ✗ {e}"), Tone::Bad)));
    } else if view.notes.is_empty() {
        rows.push(Line::from(span(" no notes; memory learns them from finished tasks", Tone::Dim)));
    }
    let room = usize::from(list.height).saturating_sub(rows.len());
    let first = view.selected.saturating_sub(room.saturating_sub(1));
    let text_room = usize::from(list.width).saturating_sub(30);
    if !view.loading {
        for (i, r) in view.notes.iter().enumerate().skip(first).take(room) {
            let n = &r.note;
            let tone = if !n.conflicts.is_empty() {
                Tone::Bad
            } else if n.confidence < 0.5 {
                Tone::Warn
            } else {
                Tone::Mem
            };
            let mut line = Line::from(vec![
                span(format!(" {} ", text::bar(n.confidence)), tone),
                span(format!("{:.2}  ", n.confidence), tone),
                span(format!("{:<11}", n.kind.as_str()), Tone::Dim),
                span(
                    text::fit(
                        &format!(
                            "{}{}{}",
                            if r.details.needs_review.is_empty() { "" } else { "[review] " },
                            if r.details.temporary { "[temporary] " } else { "" },
                            clean_line(&n.text)
                        ),
                        text_room,
                    ),
                    Tone::Plain,
                ),
            ]);
            if i == view.selected {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            rows.push(line);
        }
    }
    frame.render_widget(Paragraph::new(rows), list);

    let block = Block::new().borders(Borders::LEFT).border_style(style(Tone::Dim));
    let inner = block.inner(detail);
    frame.render_widget(block, detail);
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    if let Some(MemoryEdit::Change(change)) = &view.edit {
        let fields = format!(
            "{} revision {}\n{}\n\nreason: {}\ndependencies (; separated): {}\ntemporary: {}\n\nEnter advances; Shift+Tab goes back.\nAt confirmation: t toggles temporary; Enter saves. Esc cancels.",
            change.review.id, change.review.expected_rev,
            change.text.as_ref().map_or("Confirm this note after reviewing its evidence.", |i| i.text.as_str()),
            change.reason.text, change.dependencies.text, change.review.temporary,
        );
        let lines = text::block(&fields, "", Tone::Plain);
        frame.render_widget(Paragraph::new(wrap_all(&lines, inner.width)), inner);
    } else if let Some(r) = view.notes.get(view.selected).filter(|_| !view.loading) {
        frame.render_widget(Paragraph::new(wrap_all(&memory_lines(r), inner.width)), inner);
    }

    let block =
        Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(style(match view.edit {
            Some(_) => Tone::Shell,
            None => Tone::Dim,
        }));
    let block = match &view.edit {
        Some(MemoryEdit::Filter(_)) => block.title(Line::from(span(" filter ", Tone::Shell))),
        Some(MemoryEdit::Reason(_)) => block.title(Line::from(span(" why is this note wrong? ", Tone::Shell))),
        Some(MemoryEdit::Change(change)) => block.title(Line::from(span(
            match change.step {
                ChangeStep::Text => " corrected note ",
                ChangeStep::Reason => " review reason ",
                ChangeStep::Dependencies => " dependencies (; separated; empty clears) ",
                ChangeStep::Confirm => " confirm · Enter save · t temporary · Shift+Tab back ",
            },
            Tone::Shell,
        ))),
        None => block,
    };
    let inner = block.inner(bottom);
    frame.render_widget(block, bottom);
    match &view.edit {
        Some(MemoryEdit::Filter(input) | MemoryEdit::Reason(input)) => draw_field(frame, inner, input, true),
        Some(MemoryEdit::Change(change)) => {
            if let Some(input) = change.input() {
                draw_field(frame, inner, input, true);
            } else {
                frame.render_widget(
                    Paragraph::new(span("Save the displayed revision and dependency list?", Tone::Warn)),
                    inner,
                );
            }
        }
        None => {
            let hints = keys(&[
                ("↑↓", "select"),
                ("v", "details"),
                ("/", "filter"),
                ("a", "review"),
                ("c", "correct"),
                ("f", "forget"),
                ("r", "reload"),
                ("u", "used"),
                ("esc", "back"),
            ]);
            frame.render_widget(Paragraph::new(hints), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_breaks_into_rows_that_fit_and_finds_its_cursor() {
        let mut input = Input::with("ab\n\tc\ndefghij");
        input.cursor = 4; // after the tab
        let (rows, at) = field_rows(&input, 6);
        assert_eq!(rows, ["ab", "    c", "defgh", "ij"]);
        assert_eq!(at, (1, 4));
        input.cursor = input.text.chars().count();
        assert_eq!(field_rows(&input, 6).1, (3, 2));
        assert_eq!(field_rows(&Input::default(), 6), (vec![String::new()], (0, 0)));
    }

    #[test]
    fn a_lane_status_drops_the_cost_then_the_time_when_narrow() {
        let status = |room| lane_status("◐ turn 7", "2m14s", Some("$0.18"), room);
        assert_eq!(status(40), "◐ turn 7 · 2m14s · $0.18");
        assert_eq!(status(24), "◐ turn 7 · 2m14s · $0.18");
        assert_eq!(status(23), "◐ turn 7 · 2m14s");
        assert_eq!(status(16), "◐ turn 7 · 2m14s");
        assert_eq!(status(15), "◐ turn 7");
        assert_eq!(status(0), "◐ turn 7");
        assert_eq!(lane_status("◐ turn 7", "2m14s", None, 40), "◐ turn 7 · 2m14s");
    }
}

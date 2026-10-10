//! Drawing [`App`] into a frame.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::app::{App, Input, MemoryEdit, MemoryView, Mode, Phase, Prompt, Screen, SessionState, Task};
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
    let live_height = if lanes {
        let room = area.height.saturating_sub(6);
        if app.zoom.is_some() { room * 2 / 3 } else { (room / 2).min(14) }.max(4)
    } else {
        u16::try_from(live.len()).unwrap_or(u16::MAX).min(area.height.saturating_sub(5))
    };
    let [log, live_area, input] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(live_height), Constraint::Length(3)]).areas(area);

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
            vec![
                Line::from(vec![
                    span("◆ apply to the workspace?", Tone::Shell),
                    span(format!("  {files}  "), Tone::Dim),
                    span(format!("+{added}"), Tone::Ok),
                    span(" ", Tone::Plain),
                    span(format!("−{removed}"), Tone::Bad),
                ]),
                keys(&[("enter", "apply"), ("d", "diff"), ("k", "keep in its fork"), ("n", "discard")]),
            ]
        }
        None => match &app.task {
            Some(Task { phase: Phase::Designing, .. }) => busy("designing the check"),
            Some(task @ Task { phase: Phase::Running, .. }) if task.lanes.is_empty() => {
                busy(if task.req.check.is_some() || !task.req.verify { "starting" } else { "designing the check" })
            }
            Some(Task { phase: Phase::Applying, .. }) => busy("applying"),
            _ => Vec::new(),
        },
    }
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
            let (status, tone) = match lane.status {
                None => (format!("{} turn {}", SPINNER[spin(app.tick)], lane.turn), Tone::Sea),
                Some(molt_api::planner::AttemptStatus::Passed) => ("✓ passed".to_owned(), Tone::Ok),
                Some(molt_api::planner::AttemptStatus::Failed) => ("✗ failed".to_owned(), Tone::Bad),
                Some(molt_api::planner::AttemptStatus::Cancelled) => ("– cancelled".to_owned(), Tone::Dim),
                Some(molt_api::planner::AttemptStatus::Error) => ("! error".to_owned(), Tone::Warn),
            };
            let border = match lane.status {
                Some(molt_api::planner::AttemptStatus::Passed) => Tone::Ok,
                _ => Tone::Dim,
            };
            let block = Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style(border))
                .title(Line::from(span(format!(" attempt {} ", lane.index + 1), Tone::Sea)))
                .title(Line::from(span(format!(" {status} "), tone)).right_aligned());
            let inner = block.inner(cols[c]);
            frame.render_widget(block, cols[c]);
            let lines = wrap_all(&lane.lines, inner.width);
            let skip = lines.len().saturating_sub(usize::from(inner.height));
            frame.render_widget(Paragraph::new(lines[skip..].to_vec()), inner);
        }
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

/// A text field scrolled so its cursor shows.
fn draw_field(frame: &mut Frame, area: Rect, input: &Input, cursor: bool) {
    let chars: Vec<char> = input.text.chars().collect();
    let room = usize::from(area.width.saturating_sub(1)).max(1);
    let mut start = 0;
    while text::width(&chars[start..input.cursor].iter().collect::<String>()) > room {
        start += 1;
    }
    let shown: String = chars[start..].iter().collect();
    frame.render_widget(Paragraph::new(clean_line(&shown)), area);
    if cursor {
        let x = text::width(&chars[start..input.cursor].iter().collect::<String>());
        frame.set_cursor_position((area.x + u16::try_from(x).unwrap_or(0), area.y));
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
    let mut cost = format!("${:.2}", app.spent_usd);
    if let Some(b) = s.budget_usd {
        cost.push_str(&format!(" · ${b:.2}/task"));
    }
    parts.push(cost);
    left.push(span(format!("  {}", parts.join(" · ")), Tone::Dim));

    let mut right = Vec::new();
    if let Some(flash) = &app.flash {
        right.push(span(format!("{} ", clean_line(flash)), Tone::Warn));
    } else {
        if !app.learning.is_empty() {
            right.push(span(format!("{} learning  ", SPINNER[spin(app.tick)]), Tone::Mem));
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
                span(text::fit(&clean_line(&n.text), text_room), Tone::Plain),
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
    if let Some(r) = view.notes.get(view.selected).filter(|_| !view.loading) {
        let n = &r.note;
        let field = |name: &str, value: String| {
            Line::from(vec![span(format!("{name:<11}"), Tone::Dim), span(value, Tone::Plain)])
        };
        let mut lines = vec![
            Line::from(Span::styled(clean_line(&n.text), Style::new().add_modifier(Modifier::BOLD))),
            Line::default(),
            field("kind", n.kind.as_str().to_owned()),
        ];
        let mut confidence = format!("{:.2}", n.confidence);
        if n.reinforced > 0 {
            confidence.push_str(&format!("  confirmed {}×", n.reinforced));
        }
        lines.push(field("confidence", confidence));
        if !n.conflicts.is_empty() {
            lines.push(Line::from(vec![
                span(format!("{:<11}", "disputes"), Tone::Dim),
                span(clean_line(&n.conflicts.join(", ")), Tone::Bad),
            ]));
        }
        let p = &n.provenance;
        let run: String = clean_line(p.trace.trim_start_matches("trace_")).chars().take(8).collect();
        lines.push(field("learned", format!("{} · run {run}", text::when(n.created_ms))));
        let version: String = p.version.rsplit(':').next().unwrap_or(&p.version).chars().take(8).collect();
        lines.push(Line::from(vec![
            span(format!("{:<11}", "by"), Tone::Dim),
            span(clean_line(&p.service), Tone::Plain),
            span(format!("@{}", clean_line(&version)), Tone::Shell),
        ]));
        lines.push(field("id", clean_line(&n.id)));
        if !p.events.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(span("evidence in the audit log", Tone::Dim)));
            for e in &p.events {
                lines.push(Line::from(span(format!("  {}", clean_line(e)), Tone::Sea)));
            }
        }
        frame.render_widget(Paragraph::new(wrap_all(&lines, inner.width)), inner);
    }

    let block =
        Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(style(match view.edit {
            Some(_) => Tone::Shell,
            None => Tone::Dim,
        }));
    let block = match &view.edit {
        Some(MemoryEdit::Filter(_)) => block.title(Line::from(span(" filter ", Tone::Shell))),
        Some(MemoryEdit::Reason(_)) => block.title(Line::from(span(" why is this note wrong? ", Tone::Shell))),
        None => block,
    };
    let inner = block.inner(bottom);
    frame.render_widget(block, bottom);
    match &view.edit {
        Some(MemoryEdit::Filter(input) | MemoryEdit::Reason(input)) => draw_field(frame, inner, input, true),
        None => {
            let hints = keys(&[("↑↓", "select"), ("/", "filter"), ("f", "forget"), ("r", "reload"), ("esc", "back")]);
            frame.render_widget(Paragraph::new(hints), inner);
        }
    }
}

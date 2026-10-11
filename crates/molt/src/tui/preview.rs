//! Best-effort text is display state, never a task result or tool argument.

use std::collections::VecDeque;

use molt_api::progress::{ModelCall, Progress};
use ratatui::text::Line;

use super::text::{clean_line, span, Tone};

const TEXT_CHARS: usize = 2_048;
const CLOSED_CALLS: usize = 16;

struct Active {
    call: String,
    turn: u32,
    seq: u64,
    text: String,
    gap: bool,
    cut: bool,
}

#[derive(Default)]
pub struct Preview {
    active: Option<Active>,
    closed: VecDeque<String>,
    turn: u32,
}

impl Preview {
    pub fn feed(&mut self, event: &Progress) -> Vec<Line<'static>> {
        let Some(context) = event.model_call() else { return Vec::new() };
        if context.call.len() > 128 || context.turn < self.turn || self.closed.contains(&context.call) {
            return Vec::new();
        }
        let started = matches!(event, Progress::ModelStarted { .. });
        let different = self.active.as_ref().is_some_and(|a| a.call != context.call);
        // Without a start, a same-turn different call could be a delayed old
        // event. Drop it instead of attaching its text to the current reply.
        if different && !started && context.turn <= self.turn {
            return Vec::new();
        }
        let mut lines = Vec::new();
        if different {
            lines.extend(self.finish("interrupted", true));
        }
        if self.active.is_none() {
            self.turn = context.turn;
            self.active = Some(Active::new(context, !started));
        } else if started {
            // A duplicate start must not erase text that already arrived.
            return lines;
        }
        let active = self.active.as_mut().expect("the preview was created");
        if started {
            return lines;
        }
        if context.seq <= active.seq {
            return lines;
        }
        active.gap |= context.seq != active.seq.saturating_add(1);
        active.seq = context.seq;
        match event {
            Progress::ModelText { text, .. } => active.append(text),
            Progress::ModelFinished { usage, error, .. } => {
                let reason = match error {
                    Some(e) => format!("stopped ({})", short(e, 100)),
                    None if usage.is_none() => "stopped; usage unavailable".into(),
                    None => "finished".into(),
                };
                lines.extend(self.finish(&reason, false));
            }
            _ => {}
        }
        lines
    }

    pub fn finish(&mut self, reason: &str, gap: bool) -> Option<Line<'static>> {
        let mut active = self.active.take()?;
        active.gap |= gap;
        self.closed.push_back(active.call);
        if self.closed.len() > CLOSED_CALLS {
            self.closed.pop_front();
        }
        let flags = match (active.gap, active.cut) {
            (true, true) => " · gaps, truncated",
            (true, false) => " · gaps",
            (false, true) => " · truncated",
            _ => "",
        };
        let text = if active.text.is_empty() { "no text received".into() } else { short(&active.text, 90) };
        Some(Line::from(span(format!("   model preview {reason}{flags}: {text}"), Tone::Dim)))
    }

    pub fn lines(&self, width: u16, max_lines: usize) -> Vec<Line<'static>> {
        if width == 0 || max_lines == 0 {
            return Vec::new();
        }
        let Some(active) = &self.active else { return Vec::new() };
        let mut flags = String::new();
        if active.gap {
            flags.push_str(" · gaps in preview");
        }
        if active.cut {
            flags.push_str(" · truncated");
        }
        let label = super::text::fit(&format!("model preview{flags} · turn {}", active.turn), usize::from(width));
        let mut lines = vec![Line::from(span(label, Tone::Dim))];
        let body: Vec<_> = super::text::block(&active.text, "   ", Tone::Plain)
            .iter()
            .flat_map(|line| super::text::wrap(line, width))
            .collect();
        let start = body.len().saturating_sub(max_lines - 1);
        lines.extend_from_slice(&body[start..]);
        lines
    }
}

impl Active {
    fn new(c: &ModelCall, gap: bool) -> Self {
        Self {
            call: c.call.clone(),
            turn: c.turn,
            seq: if gap { c.seq.saturating_sub(1) } else { c.seq },
            text: String::new(),
            gap: gap || c.seq != 0,
            cut: false,
        }
    }

    fn append(&mut self, delta: &str) {
        // Keep the latest bounded suffix and remove terminal controls before
        // allocating display text. Newlines remain useful for the live view.
        let mut chars = delta.chars().rev().filter(|c| !c.is_control() || *c == '\n' || *c == '\t');
        let mut tail: Vec<_> = chars.by_ref().take(TEXT_CHARS).collect();
        self.cut |= chars.next().is_some();
        tail.reverse();
        self.text.extend(tail.into_iter().map(|c| if c == '\t' { ' ' } else { c }));
        let count = self.text.chars().count();
        if count > TEXT_CHARS {
            let end = self.text.char_indices().nth(count - TEXT_CHARS).map_or(self.text.len(), |(i, _)| i);
            self.text.drain(..end);
            self.cut = true;
        }
    }
}

fn short(text: &str, limit: usize) -> String {
    let prefix: String = text.chars().take(limit.saturating_add(1)).collect();
    let text = clean_line(&prefix);
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_api::model::Usage;

    fn call(id: &str, turn: u32, seq: u64) -> ModelCall {
        ModelCall { run: "run".into(), attempt: Some(0), turn, call: id.into(), seq }
    }

    #[test]
    fn previews_are_bounded_sanitized_and_sequence_gaps_are_explicit() {
        let mut p = Preview::default();
        p.feed(&Progress::ModelStarted { context: call("a", 1, 0) });
        p.feed(&Progress::ModelText {
            context: call("a", 1, 2), text: format!("\x1b\x07{}latest", "中".repeat(3000))
        });
        let a = p.active.as_ref().unwrap();
        assert!(a.gap && a.cut && a.text.ends_with("latest"));
        assert_eq!(a.text.chars().count(), TEXT_CHARS);
        assert!(!a.text.chars().any(char::is_control));
        p.feed(&Progress::ModelText { context: call("a", 1, 1), text: "late duplicate".into() });
        assert!(!p.active.as_ref().unwrap().text.contains("duplicate"));
        let folded = p.feed(&Progress::ModelFinished {
            context: call("a", 1, 3),
            usage: Some(Usage::default()),
            cost_usd: None,
            error: None,
        });
        assert!(p.active.is_none());
        assert!(folded[0].to_string().contains("model preview finished · gaps, truncated"));
        assert!(p.feed(&Progress::ModelText { context: call("a", 1, 4), text: "late".into() }).is_empty());
        assert!(p.active.is_none());
    }

    #[test]
    fn late_calls_cannot_replace_newer_text_and_missing_starts_stay_incomplete() {
        let mut p = Preview::default();
        p.feed(&Progress::ModelText { context: call("new", 2, 1), text: "received".into() });
        assert!(p.active.as_ref().unwrap().gap);
        p.feed(&Progress::ModelStarted { context: call("old", 1, 0) });
        p.feed(&Progress::ModelText { context: call("other", 2, 9), text: "wrong call".into() });
        assert_eq!(p.active.as_ref().unwrap().text, "received");
        let folded = p.feed(&Progress::ModelFinished {
            context: call("new", 2, 2),
            usage: None,
            cost_usd: None,
            error: Some("cancelled\x1b".into()),
        });
        assert!(folded[0].to_string().contains("model preview stopped"));
        assert!(p.active.is_none());
    }
}

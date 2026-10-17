//! A run's transcript: each model turn's text, the tools it called with
//! their arguments, and what they returned, read back from the audit log.
//!
//! The planner's calls to the model are all in the run's trace. A request to
//! `model.complete` names its attempt and turn when the run streams; its last
//! message holds the results of the tools the turn before called. The reply
//! holds the turn's text and tool calls. A run that did not stream is told
//! apart by conversation instead: a request that answers a turn's tool calls
//! continues that turn's conversation.

use std::collections::HashMap;

use molt_api::model;
use molt_proto::audit::Logged;
use molt_proto::Kind;
use ratatui::text::Line;
use serde_json::{json, Value};

use super::text::{self, span, Tone};

/// Lines of one text, argument or result shown before it is folded.
const FOLD: usize = 12;

/// Whose turn it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Attempt(u32),
    /// The check designer, the reviewer and the planner's other calls.
    Planner,
    /// A run that did not stream does not say; its `n`th conversation.
    Unknown(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// `None` until the next request carries it back, and for good when the
    /// attempt ended first.
    pub result: Option<String>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub speaker: Speaker,
    pub turn: u32,
    pub ts_ms: u64,
    pub model: String,
    pub text: String,
    pub steps: Vec<Step>,
    pub error: Option<String>,
}

struct Call {
    speaker: Speaker,
    turn: u32,
}

/// Turns as far as the log has been read; [`Reader::add`] takes each next
/// stretch of it.
#[derive(Default)]
pub struct Reader {
    /// Where the next read starts. Entries already read may come again and
    /// are skipped by their sequence number.
    pub cursor: Option<u64>,
    last_seq: Option<u64>,
    calls: HashMap<String, Call>,
    /// Each tool call by id: its turn and its place in it.
    uses: HashMap<String, (usize, usize)>,
    conversations: usize,
    pub turns: Vec<Turn>,
}

impl Reader {
    pub fn add(&mut self, entries: Vec<Logged>) {
        for entry in entries {
            if self.last_seq.is_some_and(|last| entry.seq <= last) {
                continue;
            }
            self.last_seq = Some(entry.seq);
            let env = entry.envelope;
            match env.kind {
                Kind::Request if env.to.to_string() == model::COMPLETE => {
                    self.request(env.id.to_string(), &env.payload)
                }
                Kind::Reply => {
                    let Some(call) = env.reply_to.and_then(|id| self.calls.remove(id.as_str())) else { continue };
                    self.reply(call, entry.ts_ms, &env.payload);
                }
                _ => {}
            }
        }
    }

    fn request(&mut self, id: String, payload: &Value) {
        // The results this request carries back, and whose turn asked for them.
        let mut asked = None;
        let last = payload["messages"].as_array().and_then(|m| m.last());
        for block in last.and_then(|m| m["content"].as_array()).into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let Some(&(t, s)) = block["tool_use_id"].as_str().and_then(|id| self.uses.get(id)) else { continue };
            asked.get_or_insert(t);
            let step = &mut self.turns[t].steps[s];
            // Later requests may carry a result shortened to fit; the first is whole.
            if step.result.is_none() {
                step.result = Some(result_text(&block["content"]));
                step.is_error = block["is_error"] == true;
            }
        }
        let stream = &payload["stream"];
        let call = if let Some(turn) = stream["turn"].as_u64() {
            let speaker = stream["attempt"].as_u64().map_or(Speaker::Planner, |a| Speaker::Attempt(a as u32));
            Call { speaker, turn: turn as u32 }
        } else if let Some(t) = asked {
            Call { speaker: self.turns[t].speaker, turn: self.turns[t].turn + 1 }
        } else {
            self.conversations += 1;
            Call { speaker: Speaker::Unknown(self.conversations), turn: 1 }
        };
        self.calls.insert(id, call);
    }

    fn reply(&mut self, call: Call, ts_ms: u64, payload: &Value) {
        let mut turn = Turn {
            speaker: call.speaker,
            turn: call.turn,
            ts_ms,
            model: payload["model"].as_str().unwrap_or_default().to_owned(),
            text: String::new(),
            steps: Vec::new(),
            error: None,
        };
        if let Some(error) = payload.get("error") {
            turn.error = Some(error["message"].as_str().map_or_else(|| error.to_string(), str::to_owned));
        }
        for block in payload["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    if !turn.text.is_empty() {
                        turn.text.push('\n');
                    }
                    turn.text.push_str(block["text"].as_str().unwrap_or_default());
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_owned();
                    self.uses.insert(id.clone(), (self.turns.len(), turn.steps.len()));
                    turn.steps.push(Step {
                        id,
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        result: None,
                        is_error: false,
                    });
                }
                _ => {}
            }
        }
        self.turns.push(turn);
    }
}

/// A logged message cut down to what the transcript reads, so a page of
/// requests that each repeat the whole conversation stays small: of a model
/// request, its stream context and its last message.
pub fn slim(entry: &mut Logged) {
    let env = &mut entry.envelope;
    if env.kind == Kind::Request && env.to.to_string() == model::COMPLETE {
        let last = env.payload["messages"].as_array().and_then(|m| m.last()).cloned();
        env.payload = json!({ "stream": env.payload["stream"], "messages": last.into_iter().collect::<Vec<_>>() });
    } else if env.kind == Kind::Request {
        env.payload = Value::Null;
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

/// The turns as lines to show, and where each turn starts among them.
/// `only` keeps one attempt's turns; a run that did not stream cannot be
/// split by attempt and shows them all. Folded blocks show their first
/// lines; `clean` makes each text safe to draw and hides secrets.
pub fn render(
    turns: &[Turn],
    only: Option<u32>,
    unfold: bool,
    clean: &dyn Fn(&str) -> String,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    let known = only.is_some_and(|a| turns.iter().any(|t| t.speaker == Speaker::Attempt(a)));
    let unknown = turns.iter().any(|t| matches!(t.speaker, Speaker::Unknown(_)));
    if only.is_some() && !known && unknown {
        lines.push(Line::from(span("this run did not stream, so its turns are not tied to attempts", Tone::Dim)));
        lines.push(Line::default());
    }
    let only = only.filter(|_| known || !unknown);
    let first = turns.first().map_or(0, |t| t.ts_ms);
    for turn in turns.iter().filter(|t| only.is_none_or(|a| t.speaker == Speaker::Attempt(a))) {
        starts.push(lines.len());
        let who = match turn.speaker {
            Speaker::Attempt(a) => format!("attempt {}", a + 1),
            Speaker::Planner => "planner".to_owned(),
            Speaker::Unknown(n) => format!("conversation {n}"),
        };
        let at = text::duration(std::time::Duration::from_millis(turn.ts_ms.saturating_sub(first)));
        let mut head =
            vec![span(format!("{who} · turn {}", turn.turn), Tone::Shell), span(format!(" · +{at}"), Tone::Dim)];
        if !turn.model.is_empty() {
            head.push(span(format!(" · {}", text::clean_line(&clean(&turn.model))), Tone::Dim));
        }
        lines.push(Line::from(head));
        if let Some(error) = &turn.error {
            folded(&mut lines, &clean(error), "  ", Tone::Bad, unfold);
        }
        if !turn.text.trim().is_empty() {
            folded(&mut lines, &clean(turn.text.trim()), "  ", Tone::Plain, unfold);
        }
        for step in &turn.steps {
            lines
                .push(Line::from(vec![span("  ▸ ", Tone::Sea), span(text::clean_line(&clean(&step.name)), Tone::Sea)]));
            folded(&mut lines, &clean(&arguments(&step.input)), "    ", Tone::Dim, unfold);
            match &step.result {
                Some(result) => {
                    let tone = if step.is_error { Tone::Bad } else { Tone::Plain };
                    lines.push(Line::from(span(if step.is_error { "    ← error" } else { "    ←" }, tone)));
                    folded(&mut lines, &clean(result), "      ", tone, unfold);
                }
                None => lines.push(Line::from(span("    ← no result recorded yet", Tone::Dim))),
            }
        }
        lines.push(Line::default());
    }
    (lines, starts)
}

/// A tool's arguments one per line, a string as itself.
fn arguments(input: &Value) -> String {
    let Value::Object(fields) = input else { return input.to_string() };
    let mut out = Vec::new();
    for (key, value) in fields {
        match value {
            Value::String(s) if s.contains('\n') => out.push(format!("{key}:\n{s}")),
            Value::String(s) => out.push(format!("{key}: {s}")),
            other => out.push(format!("{key}: {other}")),
        }
    }
    out.join("\n")
}

fn folded(lines: &mut Vec<Line<'static>>, body: &str, indent: &str, tone: Tone, unfold: bool) {
    let all = text::clean(body);
    let shown = if unfold { all.len() } else { all.len().min(FOLD) };
    for line in &all[..shown] {
        lines.push(Line::from(span(format!("{indent}{line}"), tone)));
    }
    if shown < all.len() {
        lines.push(Line::from(span(format!("{indent}… {} more lines · e unfolds", all.len() - shown), Tone::Dim)));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use molt_proto::{CapId, Envelope, TraceId};

    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    fn request(trace: &TraceId, payload: Value) -> Envelope {
        Envelope::request(trace.clone(), model::COMPLETE.parse().unwrap(), CapId::random(), payload)
    }

    fn logged(seq: u64, envelope: Envelope) -> Logged {
        Logged { seq, ts_ms: 1_000 * seq, envelope, omitted: None }
    }

    /// Two turns of attempt 0: a shell call answered, then an edit answered,
    /// then the end.
    pub(crate) fn two_turns(trace: &TraceId) -> Vec<Logged> {
        let stream = |turn: u32| json!({ "attempt": 0, "turn": turn });
        let first =
            request(trace, json!({ "stream": stream(1), "messages": [{ "role": "user", "content": "fix it" }] }));
        let first_reply = first.reply(json!({
            "model": "claude-test",
            "content": [
                { "type": "text", "text": "Let me run the tests." },
                { "type": "tool_use", "id": "t1", "name": "shell", "input": { "command": "cargo test" } },
            ],
        }));
        let second = request(
            trace,
            json!({ "stream": stream(2), "messages": [
                { "role": "user", "content": "fix it" },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "1 failed\nlexer::eof" }] },
            ]}),
        );
        let second_reply = second.reply(json!({
            "model": "claude-test",
            "content": [{ "type": "tool_use", "id": "t2", "name": "edit", "input": { "path": "src/lexer.rs", "new": "a\nb" } }],
        }));
        let third = request(
            trace,
            json!({ "stream": stream(3), "messages": [
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2", "content": [{ "type": "text", "text": "edited" }], "is_error": true }] },
            ]}),
        );
        let shell =
            Envelope::request(trace.clone(), "shell.run".parse().unwrap(), CapId::random(), json!({ "big": 1 }));
        vec![
            logged(1, first),
            logged(2, shell.clone()),
            logged(3, shell.reply(json!({ "exit": 1 }))),
            logged(4, first_reply),
            logged(5, second),
            logged(6, second_reply),
            logged(7, third),
        ]
    }

    #[test]
    fn two_turns_of_tool_calls_list_their_arguments_and_results() {
        let trace = TraceId::random();
        let mut reader = Reader::default();
        let mut log = two_turns(&trace);
        log.iter_mut().for_each(slim);
        reader.add(log.clone());
        // A stretch read again adds nothing.
        reader.add(log);
        assert_eq!(reader.turns.len(), 2);
        let (t1, t2) = (&reader.turns[0], &reader.turns[1]);
        assert_eq!((t1.speaker, t1.turn, t1.model.as_str()), (Speaker::Attempt(0), 1, "claude-test"));
        assert_eq!(t1.steps[0].result.as_deref(), Some("1 failed\nlexer::eof"));
        assert_eq!((t2.steps[0].result.as_deref(), t2.steps[0].is_error), (Some("edited"), true));

        let (lines, starts) = render(&reader.turns, Some(0), false, &|s| s.to_owned());
        let shown = plain(&lines);
        assert_eq!(starts.len(), 2);
        assert!(shown[starts[0]].starts_with("attempt 1 · turn 1 · +0s · claude-test"), "{shown:?}");
        assert!(shown[starts[1]].starts_with("attempt 1 · turn 2 · +2s"), "{shown:?}");
        for expected in
            ["  Let me run the tests.", "  ▸ shell", "    command: cargo test", "      lexer::eof", "    ← error"]
        {
            assert!(shown.iter().any(|l| l == expected), "{expected:?} in {shown:?}");
        }
        assert!(shown.iter().any(|l| l == "    new:") && shown.iter().any(|l| l == "    b"), "{shown:?}");
        // Another attempt has no turns of its own.
        assert!(render(&reader.turns, Some(1), false, &|s| s.to_owned()).1.is_empty());
    }

    #[test]
    fn a_run_that_did_not_stream_is_followed_by_conversation() {
        let trace = TraceId::random();
        let mut log = two_turns(&trace);
        for entry in &mut log {
            if let Some(payload) = entry.envelope.payload.as_object_mut() {
                payload.remove("stream");
            }
        }
        let mut reader = Reader::default();
        reader.add(log);
        let turns: Vec<_> = reader.turns.iter().map(|t| (t.speaker, t.turn)).collect();
        assert_eq!(turns, [(Speaker::Unknown(1), 1), (Speaker::Unknown(1), 2)]);
        let (lines, starts) = render(&reader.turns, Some(0), false, &|s| s.to_owned());
        assert_eq!(starts.len(), 2, "every turn is shown");
        assert!(plain(&lines)[0].contains("did not stream"));
    }

    #[test]
    fn long_text_is_folded_until_unfolded_and_failed_calls_say_why() {
        let trace = TraceId::random();
        let call = request(&trace, json!({ "stream": { "turn": 1 }, "messages": [] }));
        let text: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let reply = call.reply(json!({ "model": "m", "content": [{ "type": "text", "text": text.join("\n") }] }));
        let failed = request(&trace, json!({ "stream": { "turn": 2 }, "messages": [] }));
        let error = failed.error_reply(molt_proto::ErrorCode::Unavailable, "overloaded");
        let mut reader = Reader::default();
        reader.add(vec![logged(1, call), logged(2, reply), logged(3, failed), logged(4, error)]);
        assert_eq!(reader.turns[0].speaker, Speaker::Planner);
        let shown = plain(&render(&reader.turns, None, false, &|s| s.to_owned()).0);
        assert!(shown.iter().any(|l| l == "  … 18 more lines · e unfolds"), "{shown:?}");
        assert!(!shown.iter().any(|l| l == "  line 29"));
        assert!(shown.iter().any(|l| l == "  overloaded"), "{shown:?}");
        let shown = plain(&render(&reader.turns, None, true, &|s| s.to_owned()).0);
        assert!(shown.iter().any(|l| l == "  line 29"));
    }
}

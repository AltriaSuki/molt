//! Keeping a conversation within what one model call can take: the model's
//! input (its context window less the reply's `max_tokens`) and the bus's
//! frame, which carries the whole serialized request.
//!
//! Before each call the conversation is measured. Over either limit (the
//! high-water mark), its oldest turns are left out at a whole round trip
//! down to half the limits (the low-water mark), in one go, so the calls
//! until the next fit send the same prefix. No `tool_use` loses its result
//! and no assistant turn is changed (their thinking blocks are signed). A
//! result too big to fit even alone is cut down, saying so. The whole
//! conversation stays in the audit log either way.
//!
//! The first message (the task, the check, the project's context) is never
//! changed. A record of what the left-out turns did ([`crate::compact`]) goes
//! in a checkpoint of its own after it: an assistant turn that says the
//! record follows, then a user turn that holds it, as data. Providers cache a
//! request by its prefix (system, tools, then the messages in order), so the
//! first message stays cached through every fit, and the checkpoint from one
//! fit to the next. [`Conversation::cache_points`] names both for the gateway.

use molt_api::model::{text_block, user_blocks, Usage};
use serde_json::{json, Value};

use crate::compact::Compacted;
use crate::ctx::Conversation;

/// Bytes per token assumed for text not yet measured by the model. Low, so
/// the estimate errs high: code and non-English text take more tokens.
pub(crate) const BYTES_PER_TOKEN: u64 = 3;
/// Room kept in a frame for the envelope around the request.
const FRAME_SLACK: usize = 64 * 1024;
/// Most of the record of left-out turns, in bytes.
const MAX_RECORD: usize = 24 * 1024;
/// Text kept of each part of a left-out turn.
const PART: usize = 400;
/// Opens the record; a record from an earlier fit is found by it.
pub(crate) const RECORD_TAG: &str = "<earlier_turns>";
/// The assistant turn that opens the checkpoint, before the record. Fixed,
/// so a checkpoint is found by it and its bytes never change.
pub(crate) const CHECKPOINT: &str =
    "Earlier turns of this conversation were left out to make room. I go on from Molt's record of them, which follows.";
/// What a fit leaves, as a share of the limits: low, so the calls after it
/// go on with the same prefix for a while before the next.
const LOW_WATER: u64 = 2;

/// What one model call can take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Input tokens: the context window less the reply.
    pub tokens: u64,
    /// Bytes of the serialized request.
    pub bytes: usize,
}

impl Limits {
    pub fn new(context_tokens: u32, max_tokens: u32, frame_bytes: usize) -> Self {
        Self {
            tokens: u64::from(context_tokens).saturating_sub(max_tokens.into()),
            bytes: frame_bytes.saturating_sub(FRAME_SLACK),
        }
    }

    /// The context window of `model`, as the planner names it (an alias, a
    /// full id, or `None` for the gateway's default, which is Claude).
    pub fn context_of(model: Option<&str>) -> u32 {
        if Self::guessed(model) {
            128_000
        } else {
            200_000
        }
    }

    /// [`Limits::context_of`] is only a guess for `model`: its name is not one
    /// whose window is known, so it should be declared.
    pub fn guessed(model: Option<&str>) -> bool {
        match model {
            None | Some("opus" | "sonnet" | "haiku") => false,
            Some(id) => !id.strip_prefix("anthropic/").unwrap_or(id).starts_with("claude-"),
        }
    }
}

/// How a conversation was made to fit.
#[derive(Debug, PartialEq)]
pub(crate) struct Fitted {
    /// Turns left out, as messages.
    pub left_out: usize,
    /// Results cut down to fit.
    pub cut: usize,
    /// Estimated input tokens before and after.
    pub tokens: (u64, u64),
}

fn size(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

fn tokens_of(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(BYTES_PER_TOKEN)
}

impl Conversation {
    /// Note what the model measured for the request just answered, now that
    /// its reply has been pushed: the input it read and the output it wrote.
    pub fn measured(&mut self, usage: &Usage) {
        let input = usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
        if input > 0 {
            self.known = Some((self.messages.len(), input + usage.output_tokens));
        }
    }

    /// Estimated input tokens of the next request: what the model measured
    /// last, plus an estimate for what was added since.
    pub fn tokens(&self) -> u64 {
        match self.known {
            Some((n, tokens)) => {
                tokens + self.messages[n.min(self.messages.len())..].iter().map(|m| tokens_of(size(m))).sum::<u64>()
            }
            None => tokens_of(self.bytes()),
        }
    }

    /// Bytes of the next request, near enough: what varies is in it.
    pub fn bytes(&self) -> usize {
        self.system.len()
            + self.tools.iter().map(size).sum::<usize>()
            + self.messages.iter().map(size).sum::<usize>()
            + 256
    }

    fn fits(&self, limits: Limits) -> bool {
        self.tokens() <= limits.tokens && self.bytes() <= limits.bytes
    }

    /// Make the next request fit `limits`. Over them, the oldest turns are
    /// left out down to the low-water mark, so this does not repeat every
    /// turn. `None` when it already fits; an error saying which limit it is
    /// when it cannot fit.
    pub fn fit(&mut self, limits: Limits) -> Result<Option<Fitted>, String> {
        if self.fits(limits) {
            return Ok(None);
        }
        let before = self.tokens();
        let low = Limits { tokens: limits.tokens / LOW_WATER, bytes: limits.bytes / LOW_WATER as usize };
        let mut left_out = 0;
        let head = self.head();
        // The tail kept starts at an assistant turn, after the first message and the checkpoint.
        let starts: Vec<usize> =
            (head..self.messages.len()).filter(|&i| self.messages[i]["role"] == "assistant").collect();
        if let Some(&last) = starts.last() {
            // The record's standing state is not counted in MAX_RECORD: it is never cut.
            let standing = self.standing.as_ref().map_or(0, String::len) + self.budget.as_ref().map_or(0, String::len);
            let fixed = self.system.len()
                + self.tools.iter().map(size).sum::<usize>()
                + size(&self.messages[0])
                + CHECKPOINT.len()
                + 128
                + MAX_RECORD
                + standing;
            let mut tail: usize = self.messages[last..].iter().map(size).sum();
            let mut cut = last;
            // Bytes stand in for tokens here: the record and the estimate are both bytes.
            for &start in starts.iter().rev().skip(1) {
                let more: usize = self.messages[start..cut].iter().map(size).sum();
                let total = fixed + tail + more;
                if total > low.bytes || tokens_of(total) > low.tokens {
                    break;
                }
                tail += more;
                cut = start;
            }
            if cut > head {
                let dropped: Vec<Value> = self.messages.drain(head..cut).collect();
                left_out = dropped.len();
                self.add_record(&dropped);
                self.known = None;
            }
        }
        let mut cut = 0;
        // Still too big: a result too big even alone. Cut the biggest first.
        while !self.fits(limits) {
            if !self.cut_biggest_result(limits) {
                return Err(self.diagnose(limits));
            }
            cut += 1;
            self.known = None;
        }
        Ok(Some(Fitted { left_out, cut, tokens: (before, self.tokens()) }))
    }

    /// Which limit the conversation is over, and by how much.
    fn diagnose(&self, limits: Limits) -> String {
        if self.bytes() > limits.bytes {
            format!(
                "the model request is {} bytes even with its earlier turns left out, over the {} bytes the bus carries \
                 (its transport frame limit)",
                self.bytes(),
                limits.bytes
            )
        } else {
            format!(
                "the conversation needs about {} input tokens even with its earlier turns left out, over the {} the \
                 model's context window leaves after the reply (set MOLT_CONTEXT_TOKENS if the model takes more)",
                self.tokens(),
                limits.tokens
            )
        }
    }

    /// Messages before the turns a fit may leave out: the first, and the
    /// checkpoint after it once there is one.
    fn head(&self) -> usize {
        if self.checkpoint().is_some() {
            3
        } else {
            1
        }
    }

    /// The record the checkpoint holds, when the conversation has one.
    pub fn checkpoint(&self) -> Option<&str> {
        let ack = self.messages.get(1)?;
        let said = ack["content"].as_array().and_then(|b| b.first()).and_then(|b| b["text"].as_str());
        if ack["role"] != "assistant" || said != Some(CHECKPOINT) {
            return None;
        }
        let record = self.messages.get(2)?;
        let text = record["content"].as_array().and_then(|b| b.first()).and_then(|b| b["text"].as_str())?;
        (record["role"] == "user" && text.starts_with(RECORD_TAG)).then_some(text)
    }

    /// The messages a provider should cache the request up to, as indexes:
    /// the first message, which never changes, and the checkpoint's record,
    /// which changes only at a fit.
    pub fn cache_points(&self) -> Vec<usize> {
        let mut points = vec![0];
        if self.checkpoint().is_some() {
            points.push(2);
        }
        points
    }

    /// Take `dropped` into what the conversation keeps of its left-out
    /// turns, and write its record into the checkpoint.
    fn add_record(&mut self, dropped: &[Value]) {
        if self.compacted.is_none() {
            // A record with no items behind it, from a conversation this one goes on from: its lines go on.
            let old = self.checkpoint().map(Compacted::from_record).or_else(|| self.legacy_record());
            self.compacted = Some(old.unwrap_or_default());
        }
        if let Some(compacted) = &mut self.compacted {
            compacted.take(dropped);
        }
        self.write_record();
    }

    /// A record an older planner wrote as the first message's last block,
    /// taken out of it: the one time the first message changes, so that it
    /// stays the same from then on.
    fn legacy_record(&mut self) -> Option<Compacted> {
        let blocks = self.messages[0]["content"].as_array_mut()?;
        let text = blocks.last().and_then(|b| b["text"].as_str()).filter(|t| t.starts_with(RECORD_TAG))?;
        let old = Compacted::from_record(text);
        blocks.pop();
        Some(old)
    }

    /// Write the record of the left-out turns into the checkpoint after the
    /// first message, in place of the one an earlier fit wrote. The first
    /// message is left as it is.
    pub fn write_record(&mut self) {
        let Some(compacted) = &self.compacted else { return };
        let record = compacted.render(self.recall, self.standing.as_deref(), self.budget.as_deref(), MAX_RECORD);
        if self.checkpoint().is_some() {
            self.messages[2] = user_blocks(vec![text_block(record)]);
        } else {
            // A record an older planner wrote in the first message: its lines are already in the items.
            self.legacy_record();
            // The turns after the checkpoint start at an assistant turn, as a fit leaves them.
            if self.messages.get(1).is_some_and(|m| m["role"] != "assistant") {
                return;
            }
            let ack = json!({ "role": "assistant", "content": [text_block(CHECKPOINT)] });
            self.messages.splice(1..1, [ack, user_blocks(vec![text_block(record)])]);
        }
        self.known = None;
    }

    /// Cut the biggest tool result to what the limits leave for it. False
    /// when there is none worth cutting.
    fn cut_biggest_result(&mut self, limits: Limits) -> bool {
        let mut biggest: Option<(usize, usize, usize)> = None;
        for (m, message) in self.messages.iter().enumerate().skip(1) {
            let Some(blocks) = message["content"].as_array() else { continue };
            for (b, block) in blocks.iter().enumerate() {
                if block["type"] == "tool_result" && biggest.is_none_or(|(.., n)| size(block) > n) {
                    biggest = Some((m, b, size(block)));
                }
            }
        }
        let Some((m, b, n)) = biggest else { return false };
        let over_bytes = self.bytes().saturating_sub(limits.bytes);
        let over_tokens = self.tokens().saturating_sub(limits.tokens) * BYTES_PER_TOKEN;
        // Its serialized form may be bigger than its text: escapes count too.
        let keep = n.saturating_sub(over_bytes.max(over_tokens as usize) + 1024) / 2;
        if n < 2048 {
            return false;
        }
        let block = &mut self.messages[m]["content"][b];
        let text = result_text(block);
        let kept = clip(&text, keep);
        let note = format!(
            "{kept}\n[cut to fit the conversation: {} more bytes of this result were left out. Ask again for a \
             smaller part if you need them.]",
            text.len() - kept.len()
        );
        block["content"] = Value::String(note);
        true
    }
}

/// The text of a tool result, whose content is a string or text blocks.
pub(crate) fn result_text(block: &Value) -> String {
    match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The start of `text`, at most `max` bytes, never inside a character.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// One part of a left-out turn, on one line and quoted so it cannot close the record.
pub(crate) fn part(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let kept = clip(&one_line, PART);
    let more = if kept.len() < one_line.len() { " …" } else { "" };
    format!("{}{more}", kept.replace("</earlier_turns", "<\\/earlier_turns"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use molt_api::model::{tool_result, user_blocks, user_text, Role};
    use serde_json::json;

    use super::*;

    fn assistant(id: &str, text: &str) -> Value {
        json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "hm", "signature": "sig" },
            { "type": "text", "text": text },
            { "type": "tool_use", "id": id, "name": "read_file", "input": { "path": "a.rs" } },
        ] })
    }

    fn conversation(turns: usize, result: &str) -> Conversation {
        let mut conv = Conversation::new(
            "system",
            Arc::new(Vec::new()),
            user_text("<task>\nDo it.\n</task>"),
            Role::Attempt,
            None,
        );
        for n in 0..turns {
            conv.push(assistant(&format!("t{n}"), &format!("turn {n}")));
            conv.push(user_blocks(vec![tool_result(&format!("t{n}"), result, false)]));
        }
        conv
    }

    /// Every tool_use has its result in the next message, and turns alternate.
    fn valid(conv: &Conversation) -> bool {
        let m = &conv.messages;
        let alternate =
            m.iter().enumerate().all(|(i, msg)| msg["role"] == if i % 2 == 0 { "user" } else { "assistant" });
        let paired = m.windows(2).all(|w| {
            let uses: Vec<&Value> = w[0]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| &b["id"])
                .collect();
            uses.iter().all(|id| w[1]["content"].as_array().into_iter().flatten().any(|b| b["tool_use_id"] == **id))
        });
        alternate && paired
    }

    /// How many messages hold a record of earlier turns.
    fn records(conv: &Conversation) -> usize {
        conv.messages.iter().filter(|m| m.to_string().contains(RECORD_TAG)).count()
    }

    fn digest(v: &Value) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(serde_json::to_vec(v).unwrap()))
    }

    /// A provider's prompt cache, as the Messages API keeps it: an entry for
    /// the request's prefix up to each cache point, read back when a later
    /// request starts with the same bytes.
    #[derive(Default)]
    struct PrefixCache(std::collections::HashSet<String>);

    impl PrefixCache {
        /// Send `conv`'s next request: the longest prefix at one of its
        /// cache points that an earlier request cached, in messages; then
        /// cache its own prefixes.
        fn send(&mut self, conv: &Conversation) -> usize {
            let prefix = |n: usize| {
                let head = json!([conv.system.as_ref(), conv.tools.as_ref(), &conv.messages[..n]]);
                digest(&head)
            };
            let points: Vec<usize> = conv.cache_points().into_iter().map(|p| p + 1).collect();
            let hit = points.iter().copied().filter(|&n| self.0.contains(&prefix(n))).max().unwrap_or(0);
            self.0.extend(points.into_iter().map(prefix));
            hit
        }
    }

    #[test]
    fn the_first_message_never_changes_however_often_turns_are_left_out() {
        let mut conv = conversation(0, "");
        let first = digest(&conv.messages[0]);
        let limits = Limits { tokens: 30_000, bytes: 1 << 20 };
        let mut cache = PrefixCache::default();
        let (mut fits, mut since) = (0, 0);
        for n in 0..400 {
            let fitted = conv.fit(limits).unwrap();
            let hit = cache.send(&conv);
            if fitted.is_some_and(|f| f.left_out > 0) {
                fits += 1;
                since = 0;
                // Right after a fit: the system, the tools and the first message are still cached.
                assert!(hit >= 1, "turn {n}: {hit}");
            } else if fits > 0 && since > 0 {
                // Between fits: the checkpoint too.
                assert_eq!(hit, 3, "turn {n}");
            }
            since += 1;
            assert_eq!(digest(&conv.messages[0]), first, "turn {n}");
            assert!(valid(&conv), "turn {n}");
            assert!(records(&conv) <= 1);
            conv.push(assistant(&format!("t{n}"), &format!("turn {n}")));
            conv.push(user_blocks(vec![tool_result(&format!("t{n}"), "r".repeat(3000), false)]));
        }
        assert!(fits >= 3, "{fits}");
        assert_eq!(conv.cache_points(), [0, 2]);
        // Each fit leaves out down to the low-water mark, so many calls go by between two.
        assert!(400 / fits >= 10, "{fits} fits");
        // Every left-out turn keeps its id and line, and the newest their whole text, for earlier_result.
        let compacted = conv.compacted.as_ref().unwrap();
        assert_eq!(compacted.items.len() as u32, compacted.next);
        assert_eq!(compacted.items[0].line, "You wrote: turn 0");
        assert!(compacted.items.last().unwrap().body.is_some());
    }

    #[test]
    fn a_record_an_older_planner_wrote_in_the_first_message_moves_to_the_checkpoint_once() {
        let mut conv = conversation(40, &"r".repeat(3000));
        let old = format!("{RECORD_TAG}\nEarlier turns were left out.\n\n- You wrote: turn -1\n</earlier_turns>");
        conv.messages[0] = user_blocks(vec![text_block("<task>\nDo it.\n</task>"), text_block(old)]);
        conv.fit(Limits { tokens: 30_000, bytes: 1 << 20 }).unwrap().unwrap();
        assert_eq!(conv.messages[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(records(&conv), 1);
        assert!(conv.checkpoint().unwrap().contains("- You wrote: turn -1"));
        assert!(valid(&conv));

        // A conversation that goes on from one with a checkpoint keeps its lines, and has one checkpoint.
        let mut next = conversation(0, "");
        next.messages = conv.messages.clone();
        next.compacted = None;
        for n in 100..140 {
            next.push(assistant(&format!("t{n}"), &format!("turn {n}")));
            next.push(user_blocks(vec![tool_result(&format!("t{n}"), "r".repeat(3000), false)]));
        }
        next.fit(Limits { tokens: 30_000, bytes: 1 << 20 }).unwrap().unwrap();
        assert_eq!(records(&next), 1);
        let legacy = &next.compacted.as_ref().unwrap().legacy;
        assert!(legacy.iter().any(|l| l == "- You wrote: turn -1"), "{legacy:?}");
        assert!(legacy.iter().any(|l| l == "- [E1] You wrote: turn 0"), "{legacy:?}");
        assert!(valid(&next));
    }

    #[test]
    fn a_conversation_that_fits_is_left_alone() {
        let mut conv = conversation(5, "ok");
        let before = conv.messages.clone();
        assert_eq!(conv.fit(Limits { tokens: 100_000, bytes: 1 << 20 }), Ok(None));
        assert_eq!(conv.messages, before);
    }

    #[test]
    fn the_oldest_turns_are_left_out_at_a_round_trip_with_a_record() {
        let mut conv = conversation(40, &"r".repeat(3000));
        let first = conv.messages[0].clone();
        let last = conv.messages[conv.messages.len() - 2..].to_vec();
        let limits = Limits { tokens: 30_000, bytes: 1 << 20 };
        let fitted = conv.fit(limits).unwrap().unwrap();
        assert!(fitted.left_out > 0 && fitted.left_out.is_multiple_of(2) && fitted.cut == 0, "{fitted:?}");
        assert!(conv.tokens() <= limits.tokens / 2 + 1000, "{}", conv.tokens());
        assert!(valid(&conv));
        // The first message is kept as it was, with the record in a checkpoint after it.
        assert_eq!(conv.messages[0], first);
        assert_eq!(conv.messages[1]["content"][0]["text"], CHECKPOINT);
        let record = conv.checkpoint().unwrap();
        assert!(record.starts_with(RECORD_TAG) && record.ends_with("</earlier_turns>"), "{record}");
        assert!(
            record.contains("- [E1] You wrote: turn 0")
                && record.contains("- [E2] You called read_file: {\"path\":\"a.rs\"}\n  Result: rrr"),
            "{record}"
        );
        assert!(!record.contains("hm"), "thinking stays out");
        // The turns kept are unchanged, signed thinking included.
        assert_eq!(conv.messages[conv.messages.len() - 2..], last[..]);

        // A second fit keeps the first record's lines before its own.
        for n in 40..80 {
            conv.push(assistant(&format!("t{n}"), &format!("turn {n}")));
            conv.push(user_blocks(vec![tool_result(&format!("t{n}"), "r".repeat(3000), false)]));
        }
        conv.fit(limits).unwrap().unwrap();
        assert_eq!(conv.messages[0], first);
        assert_eq!(records(&conv), 1, "one record");
        let record = conv.checkpoint().unwrap();
        // Kept to its size by leaving out its oldest lines, saying so.
        assert!(record.len() <= MAX_RECORD + 100 && record.contains("You wrote: turn 41"), "{record}");
        assert!(
            record.contains("older lines of this record were left out")
                && !record.lines().any(|l| l == "- [E1] You wrote: turn 0"),
            "{record}"
        );
        assert!(valid(&conv));
        // Every left-out turn is still kept whole, by its id.
        let compacted = conv.compacted.as_ref().unwrap();
        assert_eq!(compacted.items[0].body.as_deref(), Some("turn 0"));
        assert_eq!(compacted.items[1].body.as_deref(), Some("r".repeat(3000).as_str()));
    }

    #[test]
    fn what_holds_now_is_the_owners_and_is_rewritten_at_every_fit() {
        let mut conv = conversation(40, &"r".repeat(3000));
        conv.standing = Some("- The done-check is `make`.".into());
        let limits = Limits { tokens: 30_000, bytes: 1 << 20 };
        conv.fit(limits).unwrap().unwrap();
        let record = |conv: &Conversation| conv.checkpoint().unwrap().to_owned();
        assert!(record(&conv).contains("- The done-check is `make`."));
        conv.standing = Some("- The done-check is `make test`.".into());
        for n in 40..80 {
            conv.push(assistant(&format!("t{n}"), &format!("turn {n}")));
            conv.push(user_blocks(vec![tool_result(&format!("t{n}"), "r".repeat(3000), false)]));
        }
        conv.fit(limits).unwrap().unwrap();
        let text = record(&conv);
        assert!(text.contains("- The done-check is `make test`.") && !text.contains("`make`."), "{text}");
        assert_eq!(records(&conv), 1, "one record");
    }

    #[test]
    fn a_result_too_big_alone_is_cut_and_its_text_stays_whole_characters() {
        // Escapes and multi-byte text make the request bigger than the text.
        let result = "é\"\n".repeat(100_000);
        let mut conv = conversation(1, &result);
        let limits = Limits { tokens: 1_000_000, bytes: 64 * 1024 };
        let fitted = conv.fit(limits).unwrap().unwrap();
        assert!(fitted.cut >= 1 && conv.bytes() <= limits.bytes, "{fitted:?} {}", conv.bytes());
        let text = conv.messages[2]["content"][0]["content"].as_str().unwrap();
        assert!(text.starts_with("é\"\n") && text.contains("[cut to fit the conversation: "), "{}", &text[..100]);
        assert!(valid(&conv));

        // By tokens as well as by bytes.
        let mut conv = conversation(1, &"x".repeat(300_000));
        let limits = Limits { tokens: 20_000, bytes: 1 << 30 };
        conv.fit(limits).unwrap().unwrap();
        assert!(conv.tokens() <= 20_000);
    }

    #[test]
    fn what_cannot_fit_says_which_limit() {
        let big = user_text("x".repeat(200_000));
        let mut conv = Conversation::new("system", Arc::new(Vec::new()), big.clone(), Role::Attempt, None);
        let err = conv.fit(Limits { tokens: 10_000, bytes: 1 << 30 }).unwrap_err();
        assert!(err.contains("input tokens") && err.contains("MOLT_CONTEXT_TOKENS"), "{err}");
        let mut conv = Conversation::new("system", Arc::new(Vec::new()), big, Role::Attempt, None);
        let err = conv.fit(Limits { tokens: 1 << 30, bytes: 100_000 }).unwrap_err();
        assert!(err.contains("transport frame limit"), "{err}");
    }

    #[test]
    fn the_estimate_starts_from_what_the_model_measured() {
        let mut conv = conversation(1, "ok");
        conv.measured(&Usage { input_tokens: 10, cache_read_input_tokens: 990, output_tokens: 50, ..Usage::default() });
        assert_eq!(conv.tokens(), 1050);
        let added = user_text("y".repeat(300));
        let more = tokens_of(size(&added));
        conv.push(added);
        assert_eq!(conv.tokens(), 1050 + more);
        assert!(Limits::context_of(Some("gpt-5")) < Limits::context_of(Some("claude-opus-5-5")));
        assert!(Limits::guessed(Some("glm/glm-4.6")) && !Limits::guessed(Some("anthropic/claude-opus-5-5")));
        assert!(!Limits::guessed(None) && !Limits::guessed(Some("haiku")));
        assert_eq!(Limits::new(200_000, 32_000, 1 << 20).tokens, 168_000);
    }
}

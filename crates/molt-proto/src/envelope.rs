use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CapId, MsgId, ServiceId, Target, TraceId};

/// Resources one message may consume. The kernel charges `tokens` and one
/// call against the capability, and uses `ms` as the reply deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub ms: u64,
    #[serde(default)]
    pub calls: u64,
}

impl Budget {
    pub const fn new(tokens: u64, ms: u64, calls: u64) -> Self {
        Self { tokens, ms, calls }
    }

    /// True when every field of `self` is at most the matching field of `other`.
    pub fn fits_within(&self, other: &Budget) -> bool {
        self.tokens <= other.tokens && self.ms <= other.ms && self.calls <= other.calls
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Expects exactly one reply, routed back by the kernel.
    Request,
    /// Answers the request named in `reply_to`.
    Reply,
    /// Published to a topic; no reply.
    Event,
    /// Kernel-only control message. `reply_to` names the request to stop.
    Cancel,
}

/// Deepest nesting of arrays and objects a message may arrive at the kernel
/// with, its own levels included. The kernel logs a message a few levels
/// deeper than it arrived, and `kernel.audit.read` returns it a few more,
/// which must all stay inside what a JSON parser reads (128).
pub const MAX_DEPTH: usize = 100;
/// Longest id (message, trace or capability) or target a message may carry.
pub const MAX_ID: usize = 256;

/// How deeply arrays and objects nest in the JSON text `bytes`.
pub fn nesting(bytes: &[u8]) -> usize {
    let (mut depth, mut deepest, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for &b in bytes {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// How deeply arrays and objects nest in `v`, `v` itself included.
pub fn depth(v: &Value) -> usize {
    match v {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(fields) => 1 + fields.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// One message on the bus.
///
/// `from` is never trusted from the wire: the kernel overwrites it with the
/// identity of the endpoint the message arrived on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: MsgId,
    pub trace_id: TraceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ServiceId>,
    pub to: Target,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<CapId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MsgId>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub payload: Value,
}

impl Envelope {
    /// Decode a message as it arrives at the kernel. Refused: one nested
    /// deeper than [`MAX_DEPTH`], or with an id or target longer than [`MAX_ID`].
    pub fn from_wire(bytes: &[u8]) -> Result<Self, String> {
        let deep = nesting(bytes);
        if deep > MAX_DEPTH {
            return Err(format!("the message is nested {deep} deep; at most {MAX_DEPTH} is allowed"));
        }
        let msg: Envelope = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let ids = [Some(msg.id.as_str()), Some(msg.trace_id.as_str()), msg.reply_to.as_ref().map(MsgId::as_str)];
        let long = ids.into_iter().flatten().chain(msg.cap.as_ref().map(CapId::as_str)).any(|s| s.len() > MAX_ID)
            || msg.to.to_string().len() > MAX_ID;
        if long {
            return Err(format!("an id or the target is longer than {MAX_ID} bytes"));
        }
        Ok(msg)
    }

    pub fn request(trace_id: TraceId, to: Target, cap: CapId, payload: Value) -> Self {
        Self {
            id: MsgId::random(),
            trace_id,
            from: None,
            to,
            kind: Kind::Request,
            cap: Some(cap),
            reply_to: None,
            budget: Budget::default(),
            payload,
        }
    }

    pub fn event(trace_id: TraceId, topic: &str, cap: CapId, payload: Value) -> Result<Self, crate::IdError> {
        Ok(Self {
            id: MsgId::random(),
            trace_id,
            from: None,
            to: format!("topic:{topic}").parse()?,
            kind: Kind::Event,
            cap: Some(cap),
            reply_to: None,
            budget: Budget::default(),
            payload,
        })
    }

    /// A reply to `self`. The target is informational; the kernel routes
    /// replies by `reply_to`, not by `to`.
    pub fn reply(&self, payload: Value) -> Self {
        Self {
            id: MsgId::random(),
            trace_id: self.trace_id.clone(),
            from: None,
            to: self.to.clone(),
            kind: Kind::Reply,
            cap: None,
            reply_to: Some(self.id.clone()),
            budget: Budget::default(),
            payload,
        }
    }

    pub fn error_reply(&self, code: ErrorCode, message: impl Into<String>) -> Self {
        let err = RemoteError { code, message: message.into() };
        self.reply(serde_json::json!({ "error": err }))
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// The error carried by a reply, if it is an error reply.
    pub fn error(&self) -> Option<RemoteError> {
        self.payload.get("error").and_then(|e| serde_json::from_value(e.clone()).ok())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Missing, forged, expired or mismatched capability.
    Denied,
    /// The capability's budget cannot cover this message.
    OverBudget,
    /// The receiver's mailbox is full; retry later.
    Busy,
    /// No such service, or it is not connected.
    Unavailable,
    /// No reply arrived before the deadline.
    Timeout,
    /// Local work stopped; remote side effects and billing may remain unknown.
    Cancelled,
    /// The message itself is malformed.
    Invalid,
    /// The receiving service failed while handling the request.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct RemoteError {
    pub code: ErrorCode,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_refuses_messages_too_deep_or_with_huge_ids() {
        let nested = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
        let msg = |payload: &str| {
            format!(r#"{{"id":"msg_1","trace_id":"trace_1","to":"fs.read","kind":"request","payload":{payload}}}"#)
        };
        assert_eq!(nesting(br#"{"a":"[[[{{","b":[1,{"c":"\"]"}]}"#), 3, "brackets in strings do not count");
        assert!(Envelope::from_wire(msg(&nested(MAX_DEPTH - 1)).as_bytes()).is_ok());
        let err = Envelope::from_wire(msg(&nested(MAX_DEPTH)).as_bytes()).unwrap_err();
        assert!(err.contains("nested"), "{err}");
        let long = msg("1").replace("msg_1", &"m".repeat(MAX_ID + 1));
        assert!(Envelope::from_wire(long.as_bytes()).unwrap_err().contains("longer than"));
        assert!(Envelope::from_wire(b"{").is_err());
        assert_eq!(depth(&serde_json::from_str(&nested(5)).unwrap()), 5);
    }

    #[test]
    fn envelope_json_shape() {
        let msg = Envelope::request(
            TraceId::from_raw("trace_1"),
            "memory.recall".parse().unwrap(),
            CapId::from_raw("cap_1"),
            serde_json::json!({ "k": 5 }),
        )
        .with_budget(Budget::new(2000, 3000, 0));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["to"], "memory.recall");
        assert_eq!(json["kind"], "request");
        assert!(json.get("from").is_none());
        let back: Envelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn error_reply_round_trips() {
        let req = Envelope::request(TraceId::random(), "a.b".parse().unwrap(), CapId::random(), Value::Null);
        let rep = req.error_reply(ErrorCode::Denied, "nope");
        assert_eq!(rep.reply_to.as_ref(), Some(&req.id));
        assert_eq!(rep.error().unwrap().code, ErrorCode::Denied);
    }
}

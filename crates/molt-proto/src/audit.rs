//! `kernel.audit.read`: the messages of one trace, as the audit log has them.
//!
//! The audit log is the ground truth that memory, the evaluator and crash
//! recovery read from. A service holding a capability for
//! `kernel.audit.read` reads it one trace at a time, a page per call. The
//! kernel records each read as the range of entries it returned and a hash
//! of the reply, rather than copying the reply into the log again.

use serde::{Deserialize, Serialize};

use crate::{Envelope, ServiceId, TraceId};

/// The kernel method, as a target.
pub const READ: &str = "kernel.audit.read";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub trace: TraceId,
    /// Where to read on: the `next` of the previous page. `None` starts at
    /// the beginning of the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// Leave out requests to these services; their replies are kept. The
    /// requests to the model gateway, for one, repeat the whole conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_requests_to: Vec<ServiceId>,
    /// Size limit of the page in bytes of log. Default and maximum are set by
    /// the kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl ReadRequest {
    pub fn new(trace: TraceId) -> Self {
        Self { trace, cursor: None, skip_requests_to: Vec::new(), max_bytes: None }
    }
}

/// One message of the trace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Logged {
    pub seq: u64,
    pub ts_ms: u64,
    pub envelope: Envelope,
    /// Set when the message alone is bigger than a page: its payload is
    /// left out (null here) and this is the size of its log entry in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadResponse {
    /// In log order.
    pub entries: Vec<Logged>,
    /// The cursor for the next page; `None` once the end of the log is
    /// reached. A page may hold no entries and still name a next one: the
    /// kernel looks through a limited stretch of log per read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

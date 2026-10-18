//! The `web` service: read a page from a site the user allowed.
//!
//! Commands never have the network (the shell sandbox has none); this is
//! the only way an attempt reads the web. The service fetches over HTTPS
//! only, from the hosts on the allowlist it was started with, checks every
//! redirect against that list, and never connects to a loopback, private,
//! link-local or other non-public address. What it returns goes through the
//! kernel like every reply, so the audit log records each URL and the text
//! read from it.

use serde::{Deserialize, Serialize};

pub const FETCH: &str = "web.fetch";
/// An authenticated web service reports a host-level DNS configuration problem.
/// Paired with ErrorCode::Unavailable; page text cannot trigger this signal.
pub const ENVIRONMENT_UNAVAILABLE: &str = "web network configuration unavailable: ";

/// Fetch one page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    /// An `https://` URL on an allowed host.
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    /// The URL the page came from, after redirects.
    pub url: String,
    /// The HTTP status of the last response.
    pub status: u16,
    /// Its `Content-Type`, or empty.
    pub content_type: String,
    /// The page as text: HTML is turned into plain text, with links kept
    /// as `[text](url)`; other text types are as served.
    pub text: String,
    /// The page was longer than the service reads, or than the text it
    /// returns, and was cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

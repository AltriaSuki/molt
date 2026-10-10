//! The `memory` service: what Molt has learned, and a living model of each
//! project it works on.
//!
//! **Notes** are the semantic memory: facts, conventions, decisions,
//! preferences and lessons, each one sentence long. Every note carries its
//! provenance: the episode (trace) it was learned from, the audit log
//! messages that support it, and the service and version that wrote it, so
//! rolling back a bad version can retract what it learned ([`RETRACT`]).
//! A note without provenance is refused. [`CONSOLIDATE`] reads a finished
//! episode from the audit log and turns what it shows into notes; a note
//! that disagrees with an existing one is kept next to it, both at lower
//! confidence, until more evidence settles it.
//!
//! **The project model** is the symbol and dependency graph of a workspace's
//! source files: where each function, type and method is defined and where it
//! is used. [`INDEX`] brings it up to date incrementally (only files whose
//! contents changed are parsed again), and the service also re-indexes the
//! files the `fs` service reports on [`crate::fs::CHANGED`]. [`MAP`] renders
//! the parts most relevant to a task within a token budget; [`SYMBOLS`] finds
//! a name's definitions and references. The project's decisions and
//! conventions are notes about that workspace.

use serde::{Deserialize, Serialize};

use crate::model::Usage;

pub const REMEMBER: &str = "memory.remember";
pub const RECALL: &str = "memory.recall";
pub const FORGET: &str = "memory.forget";
pub const RETRACT: &str = "memory.retract";
pub const CONSOLIDATE: &str = "memory.consolidate";
pub const INDEX: &str = "memory.index";
pub const MAP: &str = "memory.map";
pub const SYMBOLS: &str = "memory.symbols";
pub const REVIEW: &str = "memory.review";
pub const CORRECT: &str = "memory.correct";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// How the project works: build and test commands, layout, tools.
    Fact,
    /// A rule the project follows: style, patterns, things it avoids.
    Convention,
    /// A choice made in a task, and why.
    Decision,
    /// What the user wants in general.
    Preference,
    /// A pitfall a task ran into, and what got past it.
    Lesson,
}

impl NoteKind {
    pub const ALL: [NoteKind; 5] = [Self::Fact, Self::Convention, Self::Decision, Self::Preference, Self::Lesson];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Convention => "convention",
            Self::Decision => "decision",
            Self::Preference => "preference",
            Self::Lesson => "lesson",
        }
    }
}

impl std::str::FromStr for NoteKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown note kind {s:?}: use fact, convention, decision, preference or lesson"))
    }
}

/// Where a note came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Trace id of the episode the note was learned from.
    pub trace: String,
    /// Ids of messages in that episode, as the audit log has them, that
    /// support the note.
    #[serde(default)]
    pub events: Vec<String>,
    /// The service that wrote the note, as the kernel identified it.
    pub service: String,
    /// The version of that service.
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// `note_` and 32 hex digits.
    pub id: String,
    pub kind: NoteKind,
    pub text: String,
    /// The canonical workspace the note is about; `None` for one that holds
    /// everywhere, such as a preference of the user's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// How sure memory is that the note holds, from 0 to 1.
    pub confidence: f64,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Episodes since its creation that bore it out.
    #[serde(default)]
    pub reinforced: u32,
    /// Notes this one disagrees with. Both stay, at lower confidence, until
    /// more evidence settles it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    pub provenance: Provenance,
    /// Bumped on every change, for optimistic updates.
    pub rev: u64,
}

/// Store a note. The kernel-stamped sender of the request becomes
/// `provenance.service`; `trace` and `version` are required.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RememberRequest {
    pub kind: NoteKind,
    /// One self-contained sentence.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Default 0.6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// The episode the note was learned from.
    pub trace: String,
    #[serde(default)]
    pub events: Vec<String>,
    /// The version of the service sending the request.
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RememberResponse {
    pub note: Note,
    /// An existing note with the same text was found instead of a new one
    /// created. It is reinforced once per episode (`trace`).
    pub reinforced: bool,
}

/// Find notes. A query is matched by keywords (word stems), and results are
/// ranked by relevance, confidence and recency; an empty query ranks every
/// note by confidence and recency alone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecallRequest {
    #[serde(default)]
    pub query: String,
    /// Notes about this workspace, and notes that hold everywhere. `None`:
    /// only notes that hold everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Only these kinds; empty for all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<NoteKind>,
    /// Most notes to return. Default 8, at most 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// Include notes awaiting review, for inspection only. Default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_review: bool,
    /// Record the exact returned notes for this request's authenticated run.
    /// The planner uses `context` or `tool` when it injects the returned text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Explicit file or configuration dependencies, relative to the project.
    #[serde(default)]
    pub dependencies: Vec<FileDependency>,
    /// Changed or unreadable dependencies; requires explicit user review.
    #[serde(default)]
    pub needs_review: Vec<String>,
    #[serde(default)]
    pub temporary: bool,
    /// The user's explanation for the latest review or correction.
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDependency {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecallReason {
    /// FTS keyword expression; the index applies Porter stemming.
    pub keywords: Option<String>,
    /// Keyword relevance normalized within this response.
    pub relevance: Option<f64>,
    pub confidence: f64,
    pub recency: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recalled {
    pub note: Note,
    /// Higher is better; only comparable within one response.
    pub score: f64,
    #[serde(default)]
    pub reason: RecallReason,
    #[serde(default)]
    pub details: NoteDetails,
}

/// Immutable notes returned for one planner context or tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallSnapshot {
    pub run: String,
    pub call: String,
    pub workspace: String,
    pub query: String,
    pub purpose: String,
    pub created_ms: u64,
    pub notes: Vec<Recalled>,
}

/// Attach dependency files, mark temporary experience, or confirm a stale
/// note after reviewing it. A concurrent edit rejects `expected_rev`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub workspace: String,
    pub id: String,
    pub expected_rev: u64,
    pub reason: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub temporary: bool,
}

/// Replace a note while preserving its original evidence and tombstone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectRequest {
    #[serde(flatten)]
    pub review: ReviewRequest,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewResponse {
    pub note: Note,
    pub details: NoteDetails,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResponse {
    pub notes: Vec<Recalled>,
}

/// Tombstone a note. It is no longer recalled; the tombstone and its reason
/// are kept, and the request itself is in the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetResponse {
    /// False when there is no such note or it was already forgotten.
    pub forgotten: bool,
}

/// Undo what a service version did to the notes, as rolling that version
/// back requires: the notes it created are forgotten unless another writer
/// bore them out, and the notes it reinforced or contradicted are worked out
/// again without it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractRequest {
    pub service: String,
    pub version: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractResponse {
    /// Notes forgotten.
    pub retracted: u64,
    /// Notes kept, with the version's part in them undone.
    #[serde(default)]
    pub adjusted: u64,
}

/// Learn from a finished episode: read it from the audit log, have the model
/// extract what will help later tasks in `workspace`, and merge that into
/// the notes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateRequest {
    /// Trace id of the episode, e.g. the trace of a `planner.run` request.
    pub episode: String,
    pub workspace: String,
    /// Model for the extraction. Default set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsolidateResponse {
    /// Notes created.
    pub added: Vec<Note>,
    /// Ids of existing notes the episode bore out.
    pub reinforced: Vec<String>,
    /// Ids of existing notes a new note disagrees with.
    pub contradicted: Vec<String>,
    /// Why nothing was learned, when nothing was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub usage: Usage,
    pub cost_usd: f64,
}

/// Bring the project model of `workspace` up to date.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRequest {
    pub workspace: String,
    /// Only these workspace-relative paths (changed or deleted files).
    /// `None`: the whole workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexResponse {
    /// Files in the model after the update.
    pub files: u64,
    /// Files parsed again because they are new or their contents changed.
    pub parsed: u64,
    /// Files dropped from the model because they are gone or now ignored.
    pub removed: u64,
    /// Definitions in the model after the update.
    pub symbols: u64,
    /// Files left out: too large, binary, or past the file limit.
    pub skipped: u64,
    /// The workspace has more files than the model holds.
    pub truncated: bool,
    pub ms: u64,
}

/// A map of the parts of `workspace` most relevant to `query`: files and
/// the signatures of their most important definitions, ranked by how the
/// code references itself and by what the query mentions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapRequest {
    pub workspace: String,
    /// The task, or any text whose identifiers and paths should rank first.
    #[serde(default)]
    pub query: String,
    /// Size limit of the map, in estimated tokens. Default 4000, at most 32000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapResponse {
    /// One block per file: its path, then `line: signature` lines.
    pub map: String,
    pub files: u32,
    pub symbols: u32,
    /// Estimated tokens in `map`.
    pub tokens: u32,
}

/// Where a name is defined and used, as of the last index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolsRequest {
    pub workspace: String,
    /// An identifier, matched exactly, or ignoring case when nothing matches exactly.
    pub name: String,
    /// Also list references. Default true.
    #[serde(default = "yes")]
    pub references: bool,
    /// Most definitions and most references to return. Default 50, at most 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub path: String,
    /// 1-based.
    pub line: u64,
    /// What the parser calls it: `function`, `method`, `class`, `interface`, ...
    pub kind: String,
    pub name: String,
    /// The definition's first line, trimmed.
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub path: String,
    /// 1-based.
    pub line: u64,
    /// The line, trimmed, as the file has it now.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolsResponse {
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    /// More definitions or references exist than were returned.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kinds_round_trip_as_words() {
        for kind in NoteKind::ALL {
            assert_eq!(kind.as_str().parse::<NoteKind>().unwrap(), kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
        assert!("hunch".parse::<NoteKind>().is_err());
    }

    #[test]
    fn minimal_requests_get_defaults() {
        let req: SymbolsRequest = serde_json::from_value(json!({ "workspace": "/w", "name": "parse" })).unwrap();
        assert!(req.references);
        assert_eq!(req.limit, None);
        let req: RecallRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(req, RecallRequest::default());
        let req: MapRequest = serde_json::from_value(json!({ "workspace": "/w" })).unwrap();
        assert_eq!(req.query, "");
    }
}

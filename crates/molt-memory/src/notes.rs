//! Notes: the semantic memory.
//!
//! A note is one sentence with a kind, a confidence and its provenance. Two
//! notes with the same text (ignoring case, spacing and a final period) about
//! the same workspace are one note: storing it again reinforces it. Recall
//! matches word stems with SQLite's full-text index and ranks the matches by
//! relevance, confidence and recency. Forgetting leaves a tombstone.
//!
//! Every change to a note is a row in the `evidence` table, naming the
//! episode and the writer (service and version) behind it, and a note's
//! confidence, reinforcement count, conflicts and provenance are worked out
//! from its rows. Retracting a version voids its rows and works every note
//! it touched out again, so the notes end up as if that version had never
//! run: what it created is gone unless another writer bore it out, and what
//! it reinforced or contradicted is back where the others left it.

use std::collections::HashSet;

use molt_api::memory::{Note, NoteKind, Provenance, RecallReason, RecallRequest, Recalled};
use molt_proto::RemoteError;
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

use crate::db::Db;
use crate::error::{self, invalid};

/// Notes, their full-text index, the evidence behind each, and the
/// episodes already learned from.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notes (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    norm TEXT NOT NULL,
    workspace TEXT,
    confidence REAL NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    reinforced INTEGER NOT NULL DEFAULT 0,
    conflicts TEXT NOT NULL DEFAULT '[]',
    trace TEXT NOT NULL,
    events TEXT NOT NULL DEFAULT '[]',
    service TEXT NOT NULL,
    version TEXT NOT NULL,
    rev INTEGER NOT NULL DEFAULT 1,
    forgotten_ms INTEGER,
    forget_reason TEXT
);
CREATE INDEX IF NOT EXISTS notes_workspace ON notes(workspace);
CREATE INDEX IF NOT EXISTS notes_writer ON notes(service, version);
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
    USING fts5(text, content='notes', content_rowid='rowid', tokenize='porter unicode61');
CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE OF text ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO notes_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TABLE IF NOT EXISTS evidence (
    note TEXT NOT NULL REFERENCES notes(id),
    trace TEXT NOT NULL,
    what TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    ms INTEGER NOT NULL,
    service TEXT NOT NULL DEFAULT '',
    version TEXT NOT NULL DEFAULT '',
    value REAL,
    void INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS evidence_note ON evidence(note);
CREATE INDEX IF NOT EXISTS evidence_writer ON evidence(service, version);
CREATE TABLE IF NOT EXISTS consolidated (
    episode TEXT PRIMARY KEY,
    ms INTEGER NOT NULL,
    service TEXT NOT NULL,
    version TEXT NOT NULL
);
";

/// Longest note text, in bytes.
pub(crate) const MAX_TEXT: usize = 500;
pub(crate) const DEFAULT_CONFIDENCE: f64 = 0.6;
/// Confidence stays inside these bounds: memory is never certain, and a note
/// worth keeping is never worthless.
const MIN_CONFIDENCE: f64 = 0.05;
const MAX_CONFIDENCE: f64 = 0.95;
/// Each episode that bears a note out closes this share of the gap to certainty.
const REINFORCE: f64 = 0.3;
/// A contradicted note keeps this share of its confidence, and the note
/// contradicting it starts at no more than [`CONTESTED`]. A note whose only
/// remaining evidence is an episode that bore it out starts there too.
const CONTRADICTED: f64 = 0.6;
const CONTESTED: f64 = 0.5;
const DEFAULT_K: u32 = 8;
const MAX_K: u32 = 50;
/// Keyword matches considered before ranking, best matches first.
const CANDIDATES: usize = 500;
/// Recency halves a note's weight for ranking about every this many days.
const HALF_LIFE_DAYS: f64 = 30.0;
const DAY_MS: f64 = 86_400_000.0;

/// What an evidence row records. A note's state is worked out from its rows.
mod what {
    /// The note was created; `value` is its starting confidence and `detail`
    /// the ids of the messages behind it.
    pub const LEARNED: &str = "learned";
    /// A writer stored the same note again: like `learned`, for an existing note.
    pub const RESTATED: &str = "restated";
    /// An episode bore the note out.
    pub const REINFORCED: &str = "reinforced";
    /// The note `detail` says this one is wrong.
    pub const CONTRADICTED: &str = "contradicted";
    /// This note says the note `detail` is wrong.
    pub const CONTESTS: &str = "contests";
    pub const FORGOTTEN: &str = "forgotten";
    pub const RETRACTED: &str = "retracted";
}

/// A note to store.
#[derive(Clone, Debug)]
pub(crate) struct NewNote {
    pub kind: NoteKind,
    pub text: String,
    pub workspace: Option<String>,
    pub confidence: f64,
    pub provenance: Provenance,
}

impl NewNote {
    /// Check and tidy a note before it is stored. A note without its
    /// provenance is refused.
    pub fn checked(mut self) -> Result<Self, RemoteError> {
        self.text = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if self.text.is_empty() {
            return Err(invalid("the note is empty"));
        }
        if self.text.len() > MAX_TEXT {
            return Err(invalid(format!(
                "the note is {} bytes; keep it to one sentence of at most {MAX_TEXT}",
                self.text.len()
            )));
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(invalid("confidence must be between 0 and 1"));
        }
        self.confidence = self.confidence.clamp(MIN_CONFIDENCE, MAX_CONFIDENCE);
        let p = &self.provenance;
        if p.trace.trim().is_empty() || p.service.trim().is_empty() || p.version.trim().is_empty() {
            return Err(invalid(
                "a note needs its provenance: the episode's trace and the writer's service and version",
            ));
        }
        Ok(self)
    }
}

/// Who changed a note: the episode, and the writer that took it from there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct By<'a> {
    pub trace: &'a str,
    pub service: &'a str,
    pub version: &'a str,
}

impl<'a> By<'a> {
    pub fn of(p: &'a Provenance) -> Self {
        Self { trace: &p.trace, service: &p.service, version: &p.version }
    }
}

/// What storing a note did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    /// A new note was created.
    New,
    /// The same note existed, and this episode reinforced it.
    Reinforced,
    /// The same note existed, and this episode had borne it out already.
    Unchanged,
}

/// What storing a correction did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Correction {
    /// The correction is a new note, contesting the old one.
    Added(String),
    /// The correction says what the existing note `0` says: that note was
    /// borne out (`1`: for the first time in this episode) and contests the old one.
    Matched(String, bool),
    /// The correction says what the old note says, so it bore the old note
    /// out instead (`true`: for the first time in this episode).
    Restated(bool),
    /// The old note is gone; nothing was stored.
    Missing,
}

/// What makes two notes the same note: case, spacing and a final period aside.
pub(crate) fn norm(text: &str) -> String {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    words.join(" ").trim_end_matches('.').to_owned()
}

fn new_id() -> String {
    format!("note_{:032x}", rand::random::<u128>())
}

fn events_json(events: &[String]) -> String {
    serde_json::to_string(events).expect("strings serialize")
}

const COLUMNS: &str = "id, kind, text, workspace, confidence, created_ms, updated_ms, reinforced, conflicts, \
                       trace, events, service, version, rev";

fn note_from(row: &Row) -> rusqlite::Result<Note> {
    let kind: String = row.get(1)?;
    let conflicts: String = row.get(8)?;
    let events: String = row.get(10)?;
    let bad = |i, e: String| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e.into());
    Ok(Note {
        id: row.get(0)?,
        kind: kind.parse().map_err(|e| bad(1, e))?,
        text: row.get(2)?,
        workspace: row.get(3)?,
        confidence: row.get(4)?,
        created_ms: row.get::<_, i64>(5)? as u64,
        updated_ms: row.get::<_, i64>(6)? as u64,
        reinforced: row.get::<_, i64>(7)? as u32,
        conflicts: serde_json::from_str(&conflicts).map_err(|e| bad(8, e.to_string()))?,
        provenance: Provenance {
            trace: row.get(9)?,
            events: serde_json::from_str(&events).map_err(|e| bad(10, e.to_string()))?,
            service: row.get(11)?,
            version: row.get(12)?,
        },
        rev: row.get::<_, i64>(13)? as u64,
    })
}

/// The note `id`, unless it is unknown or forgotten.
pub(crate) fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<Note>> {
    conn.query_row(&format!("SELECT {COLUMNS} FROM notes WHERE id = ?1 AND forgotten_ms IS NULL"), [id], note_from)
        .optional()
}

fn live(tx: &Transaction, id: &str) -> rusqlite::Result<bool> {
    tx.query_row("SELECT 1 FROM notes WHERE id = ?1 AND forgotten_ms IS NULL", [id], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
}

fn record(
    tx: &Transaction,
    note: &str,
    by: By,
    what: &str,
    detail: &str,
    value: Option<f64>,
    now: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO evidence (note, trace, what, detail, ms, service, version, value) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![note, by.trace, what, detail, now as i64, by.service, by.version, value],
    )?;
    Ok(())
}

/// Whether the episode `trace` has borne the note out already.
fn borne_out(tx: &Transaction, note: &str, trace: &str) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT 1 FROM evidence WHERE note = ?1 AND trace = ?2 AND void = 0 AND what IN (?3, ?4, ?5) LIMIT 1",
        params![note, trace, what::LEARNED, what::RESTATED, what::REINFORCED],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// A note's state as its evidence gives it.
struct State {
    confidence: f64,
    reinforced: u32,
    updated_ms: i64,
    conflicts: Vec<String>,
    trace: String,
    events: String,
    service: String,
    version: String,
}

/// Work out the note `id` from its evidence still in force: `None` when
/// none of it shows the note at all (its creator was retracted and no other
/// writer bore it out).
///
/// The first row that learned or bore out the note is where it starts, and
/// the writer of that row is its provenance. After that, each episode that
/// bears it out reinforces it once and each episode that contradicts it
/// lowers it once, in the order they happened.
fn replay(tx: &Transaction, id: &str) -> rusqlite::Result<Option<State>> {
    let mut stmt = tx.prepare_cached(
        "SELECT what, value, detail, trace, service, version, ms FROM evidence \
         WHERE note = ?1 AND void = 0 ORDER BY rowid",
    )?;
    let mut rows = stmt.query([id])?;
    let mut state: Option<State> = None;
    let mut bore = HashSet::new();
    let mut lowered = HashSet::new();
    let mut conflicts: Vec<String> = Vec::new();
    while let Some(r) = rows.next()? {
        let (what, value, detail, trace): (String, Option<f64>, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        let ms: i64 = r.get(6)?;
        match (what.as_str(), &mut state) {
            (what::LEARNED | what::RESTATED | what::REINFORCED, None) => {
                let start = match what.as_str() {
                    what::REINFORCED => CONTESTED,
                    _ => value.unwrap_or(DEFAULT_CONFIDENCE),
                };
                let events = if what == what::REINFORCED { "[]".to_owned() } else { detail };
                bore.insert(trace.clone());
                state = Some(State {
                    confidence: start.clamp(MIN_CONFIDENCE, MAX_CONFIDENCE),
                    reinforced: 0,
                    updated_ms: ms,
                    conflicts: Vec::new(),
                    trace,
                    events,
                    service: r.get(4)?,
                    version: r.get(5)?,
                });
            }
            (what::LEARNED | what::RESTATED | what::REINFORCED, Some(s)) => {
                if bore.insert(trace) {
                    s.confidence = (s.confidence + (1.0 - s.confidence) * REINFORCE).min(MAX_CONFIDENCE);
                    s.reinforced += 1;
                    s.updated_ms = ms;
                }
            }
            (what::CONTRADICTED, s) => {
                if !conflicts.contains(&detail) {
                    conflicts.push(detail);
                }
                if let Some(s) = s {
                    if lowered.insert(trace) {
                        s.confidence = (s.confidence * CONTRADICTED).max(MIN_CONFIDENCE);
                        s.updated_ms = ms;
                    }
                }
            }
            (what::CONTESTS, _) if !conflicts.contains(&detail) => conflicts.push(detail),
            _ => {}
        }
    }
    drop(rows);
    let Some(mut state) = state else { return Ok(None) };
    // A conflict with a note that is gone is settled.
    for other in conflicts {
        if other != id && live(tx, &other)? {
            state.conflicts.push(other);
        }
    }
    Ok(Some(state))
}

/// Store the note's state as its evidence gives it. False when none of its
/// evidence is in force any more (the note is left as it was).
fn settle(tx: &Transaction, id: &str) -> rusqlite::Result<bool> {
    let Some(s) = replay(tx, id)? else { return Ok(false) };
    tx.execute(
        "UPDATE notes SET confidence = ?2, reinforced = ?3, updated_ms = ?4, conflicts = ?5, trace = ?6, \
         events = ?7, service = ?8, version = ?9, rev = rev + 1 WHERE id = ?1",
        params![
            id,
            s.confidence,
            s.reinforced,
            s.updated_ms,
            serde_json::to_string(&s.conflicts).expect("strings serialize"),
            s.trace,
            s.events,
            s.service,
            s.version,
        ],
    )?;
    Ok(true)
}

/// Store `new`, or reinforce the live note about the same workspace with the
/// same text. Returns the note as stored and whether an existing one was found.
pub(crate) fn remember(db: &Db, new: NewNote, now: u64) -> Result<(Note, bool), RemoteError> {
    let new = new.checked()?;
    db.with(|conn| -> Result<_, RemoteError> {
        let tx = conn.transaction().map_err(error::db)?;
        if withdrawn(&tx, &new).map_err(error::db)? {
            return Err(invalid("this note was withdrawn; its text cannot be automatically restored"));
        }
        let (id, stored) = store(&tx, &new, now).map_err(error::db)?;
        let note = get(&tx, &id).map_err(error::db)?.expect("the note was just stored");
        tx.commit().map_err(error::db)?;
        Ok((note, stored != Stored::New))
    })
}

pub(crate) fn withdrawn(conn: &Connection, new: &NewNote) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM notes WHERE norm = ?1 AND workspace IS ?2 AND forgotten_ms IS NOT NULL LIMIT 1",
        params![norm(&new.text), new.workspace],
        |_| Ok(()),
    )
    .optional()
    .map(|n| n.is_some())
}

/// The live note about the same workspace with the same text as `new`.
fn same(tx: &Transaction, new: &NewNote) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT id FROM notes WHERE norm = ?1 AND workspace IS ?2 AND forgotten_ms IS NULL",
        params![norm(&new.text), new.workspace],
        |r| r.get(0),
    )
    .optional()
}

/// [`remember`] inside a transaction: the id of the note, and what storing it did.
pub(crate) fn store(tx: &Transaction, new: &NewNote, now: u64) -> rusqlite::Result<(String, Stored)> {
    let p = &new.provenance;
    let by = By::of(p);
    if let Some(id) = same(tx, new)? {
        if borne_out(tx, &id, by.trace)? {
            return Ok((id, Stored::Unchanged));
        }
        record(tx, &id, by, what::RESTATED, &events_json(&p.events), Some(new.confidence), now)?;
        settle(tx, &id)?;
        return Ok((id, Stored::Reinforced));
    }
    let id = new_id();
    tx.execute(
        "INSERT INTO notes (id, kind, text, norm, workspace, confidence, created_ms, updated_ms, trace, events, \
         service, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11)",
        params![
            id,
            new.kind.as_str(),
            new.text,
            norm(&new.text),
            new.workspace,
            new.confidence,
            now as i64,
            p.trace,
            events_json(&p.events),
            p.service,
            p.version,
        ],
    )?;
    record(tx, &id, by, what::LEARNED, &events_json(&p.events), Some(new.confidence), now)?;
    Ok((id, Stored::New))
}

/// Raise the confidence of the live note `id`: the episode `by.trace` bore
/// it out. False when there is no such note, or the episode bore it out already.
pub(crate) fn reinforce(tx: &Transaction, id: &str, by: By, now: u64) -> rusqlite::Result<bool> {
    if !live(tx, id)? || borne_out(tx, id, by.trace)? {
        return Ok(false);
    }
    record(tx, id, by, what::REINFORCED, "", None, now)?;
    settle(tx, id)
}

/// Store `new` as disagreeing with the live note `old`: both stay, the old
/// one at lower confidence and the new one at no more than an even chance,
/// each naming the other, until more evidence settles it. One episode
/// lowers a note once, however many corrections of it it holds.
pub(crate) fn contradict(tx: &Transaction, old: &str, new: &NewNote, now: u64) -> rusqlite::Result<Correction> {
    let old_norm: Option<String> = tx
        .query_row("SELECT norm FROM notes WHERE id = ?1 AND forgotten_ms IS NULL", [old], |r| r.get(0))
        .optional()?;
    let Some(old_norm) = old_norm else { return Ok(Correction::Missing) };
    let by = By::of(&new.provenance);
    if norm(&new.text) == old_norm {
        // The "correction" says what the note already says.
        return Ok(Correction::Restated(reinforce(tx, old, by, now)?));
    }
    let correction = match same(tx, new)? {
        Some(id) => {
            let first = reinforce(tx, &id, by, now)?;
            Correction::Matched(id, first)
        }
        None => {
            let contested = NewNote { confidence: new.confidence.min(CONTESTED), ..new.clone() };
            Correction::Added(store(tx, &contested, now)?.0)
        }
    };
    let (Correction::Added(id) | Correction::Matched(id, _)) = &correction else { unreachable!() };
    record(tx, old, by, what::CONTRADICTED, id, None, now)?;
    record(tx, id, by, what::CONTESTS, old, None, now)?;
    settle(tx, old)?;
    settle(tx, id)?;
    Ok(correction)
}

/// The live notes that list one of `ids` among their conflicts.
fn partners(tx: &Transaction, ids: &[String]) -> rusqlite::Result<Vec<String>> {
    let mut stmt = tx.prepare_cached(
        "SELECT DISTINCT n.id FROM notes n, json_each(n.conflicts) c WHERE c.value = ?1 AND n.forgotten_ms IS NULL",
    )?;
    let mut out = Vec::new();
    for id in ids {
        for found in stmt.query_map([id], |r| r.get::<_, String>(0))? {
            let found = found?;
            if !out.contains(&found) {
                out.push(found);
            }
        }
    }
    Ok(out)
}

pub(crate) fn tombstone(
    tx: &Transaction,
    id: &str,
    what: &str,
    reason: &str,
    trace: &str,
    now: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE notes SET forgotten_ms = ?2, forget_reason = ?3, rev = rev + 1 WHERE id = ?1",
        params![id, now as i64, reason],
    )?;
    let by = By { trace, service: "", version: "" };
    record(tx, id, by, what, reason, None, now)
}

pub(crate) fn settle_partners(tx: &Transaction, id: &str) -> rusqlite::Result<()> {
    for other in partners(tx, &[id.to_owned()])? {
        settle(tx, &other)?;
    }
    Ok(())
}

/// Tombstone the live note `id`. False when there is none.
pub(crate) fn forget(db: &Db, id: &str, reason: &str, trace: &str, now: u64) -> Result<bool, RemoteError> {
    if reason.trim().is_empty() {
        return Err(invalid("say why the note should be forgotten"));
    }
    db.with(|conn| {
        let tx = conn.transaction()?;
        if !live(&tx, id)? {
            return Ok(false);
        }
        tombstone(&tx, id, what::FORGOTTEN, reason, trace, now)?;
        // The notes it disputed are no longer disputed by it.
        for other in partners(&tx, &[id.to_owned()])? {
            settle(&tx, &other)?;
        }
        tx.commit()?;
        Ok(true)
    })
    .map_err(error::db)
}

/// What a retraction changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Retracted {
    /// Notes tombstoned: the version created them and no other writer bore them out.
    pub retracted: u64,
    /// Notes worked out again without the version's evidence.
    pub adjusted: u64,
}

/// Undo what `service` at `version` did to the notes: void its evidence,
/// tombstone the notes nothing else shows, work every other note it touched
/// out again, and let the episodes it learned from be learned from again.
pub(crate) fn retract(
    db: &Db,
    service: &str,
    version: &str,
    reason: &str,
    trace: &str,
    now: u64,
) -> Result<Retracted, RemoteError> {
    if service.trim().is_empty() || version.trim().is_empty() {
        return Err(invalid("name the service and the version whose notes to retract"));
    }
    if reason.trim().is_empty() {
        return Err(invalid("say why the notes should be retracted"));
    }
    db.with(|conn| {
        let tx = conn.transaction()?;
        let touched: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT e.note FROM evidence e JOIN notes n ON n.id = e.note \
                 WHERE e.service = ?1 AND e.version = ?2 AND e.void = 0 AND n.forgotten_ms IS NULL ORDER BY e.note",
            )?;
            let ids = stmt.query_map(params![service, version], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        tx.execute(
            "UPDATE evidence SET void = 1 WHERE service = ?1 AND version = ?2 AND void = 0",
            params![service, version],
        )?;
        let mut gone = Vec::new();
        for id in &touched {
            if replay(&tx, id)?.is_none() {
                tombstone(&tx, id, what::RETRACTED, reason, trace, now)?;
                gone.push(id.clone());
            }
        }
        let mut adjusted = 0;
        let mut others: Vec<String> = touched.iter().filter(|id| !gone.contains(id)).cloned().collect();
        for id in partners(&tx, &gone)? {
            if !others.contains(&id) {
                others.push(id);
            }
        }
        for id in &others {
            adjusted += u64::from(settle(&tx, id)?);
        }
        tx.execute("DELETE FROM consolidated WHERE service = ?1 AND version = ?2", params![service, version])?;
        tx.commit()?;
        Ok(Retracted { retracted: gone.len() as u64, adjusted })
    })
    .map_err(error::db)
}

/// Words worth matching on: none of the most common English words.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "for", "from", "has", "have", "in", "into", "is", "it",
    "its", "make", "of", "on", "or", "so", "that", "the", "this", "to", "was", "we", "with", "you",
];

/// `query` as an FTS5 query that matches any of its words, or `None` when it
/// has no word worth matching. Each word is quoted, so nothing in the query
/// is read as FTS5 syntax.
fn fts_query(query: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let word = word.to_lowercase();
        if word.chars().count() < 2 || STOPWORDS.contains(&word.as_str()) || words.contains(&word) {
            continue;
        }
        words.push(word);
        if words.len() == 32 {
            break;
        }
    }
    (!words.is_empty()).then(|| words.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" OR "))
}

/// Weight for ranking of a note last changed at `updated_ms`, at `now`: 1
/// for now, halving every [`HALF_LIFE_DAYS`].
fn recency(updated_ms: i64, now: u64) -> f64 {
    let age_days = (now as i64).saturating_sub(updated_ms).max(0) as f64 / DAY_MS;
    0.5f64.powf(age_days / HALF_LIFE_DAYS)
}

/// Make `recency(updated_ms, now)` callable from SQL on `conn`.
pub(crate) fn register(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function("recency", 2, FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC, |c| {
        Ok(recency(c.get::<i64>(0)?, c.get::<i64>(1)?.max(0) as u64))
    })
}

/// Which notes a recall looks at besides those about its workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Also the notes that hold everywhere.
    WithGlobal,
    /// Only the workspace's own.
    Own,
}

/// The notes that best answer `req`, best first. `req.workspace` must be
/// the canonical workspace already.
#[cfg(test)]
pub(crate) fn recall(db: &Db, req: &RecallRequest, now: u64) -> Result<Vec<Recalled>, RemoteError> {
    recall_in(db, req, Scope::WithGlobal, now)
}

/// [`recall`], over the notes `scope` names.
pub(crate) fn recall_in(db: &Db, req: &RecallRequest, scope: Scope, now: u64) -> Result<Vec<Recalled>, RemoteError> {
    validate_recall(req)?;
    db.with(|conn| select(conn, req, scope, now)).map_err(error::db)
}

pub(crate) fn validate_recall(req: &RecallRequest) -> Result<(), RemoteError> {
    if req.capture.as_deref().is_some_and(|p| !matches!(p, "context" | "tool")) {
        return Err(invalid("capture must be context or tool"));
    }
    if req.capture.is_some() && (req.include_review || req.workspace.is_none()) {
        return Err(invalid("captured recall requires a workspace and excludes notes awaiting review"));
    }
    if let Some(min) = req.min_confidence {
        if !min.is_finite() || !(0.0..=1.0).contains(&min) {
            return Err(invalid("min_confidence must be between 0 and 1"));
        }
    }
    Ok(())
}

pub(crate) fn select(
    conn: &Connection,
    req: &RecallRequest,
    scope: Scope,
    now: u64,
) -> rusqlite::Result<Vec<Recalled>> {
    let k = req.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K) as usize;
    let fts = fts_query(&req.query);
    if fts.is_none() && !req.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    // Every filter is applied before the candidates are cut, so none of them
    // can empty a recall that has matches.
    let mut filter = match scope {
        Scope::WithGlobal => "n.forgotten_ms IS NULL AND (n.workspace IS NULL OR n.workspace IS ?1)".to_owned(),
        Scope::Own => "n.forgotten_ms IS NULL AND n.workspace IS ?1".to_owned(),
    };
    filter.push_str(" AND n.confidence >= ?3");
    if !req.include_review {
        filter.push_str(" AND NOT EXISTS (SELECT 1 FROM note_details d WHERE d.note = n.id AND json_array_length(json_extract(d.details, '$.needs_review')) > 0)");
    }
    if !req.kinds.is_empty() {
        // The kinds' names are fixed words, safe to write into the query.
        let kinds: Vec<String> = req.kinds.iter().map(|k| format!("'{}'", k.as_str())).collect();
        filter.push_str(&format!(" AND n.kind IN ({})", kinds.join(", ")));
    }
    let columns = COLUMNS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ");
    let min = req.min_confidence.unwrap_or(0.0);
    let candidates: Vec<(Note, f64, molt_api::memory::NoteDetails)> = {
        let details = "COALESCE((SELECT details FROM note_details WHERE note = n.id), '{}')";
        let sql = match &fts {
                Some(_) => format!(
                    "SELECT {columns}, -bm25(notes_fts), {details} FROM notes_fts JOIN notes n ON n.rowid = notes_fts.rowid \
                     WHERE notes_fts MATCH ?2 AND {filter} ORDER BY bm25(notes_fts) LIMIT {CANDIDATES}"
                ),
                // Without words to match, the ranking is known in full: let SQL do it.
                None => format!(
                    "SELECT {columns}, 0.0, {details} FROM notes n WHERE {filter} AND ?2 IS NULL \
                     ORDER BY 0.7 * n.confidence + 0.3 * recency(n.updated_ms, ?4) DESC, n.id LIMIT {k}"
                ),
            };
        let mut stmt = conn.prepare_cached(&sql)?;
        let row = |r: &Row| {
            let details: String = r.get(15)?;
            Ok((note_from(r)?, r.get::<_, f64>(14)?, crate::review::decode(&details)?))
        };
        let rows = match &fts {
            Some(fts) => stmt.query_map(params![req.workspace, fts, min], row)?,
            None => stmt.query_map(params![req.workspace, None::<String>, min, now as i64], row)?,
        };
        rows.collect::<rusqlite::Result<_>>()?
    };

    let best = candidates.iter().map(|(_, rel, _)| *rel).fold(0.0, f64::max);
    let matched = fts.is_some();
    let mut ranked: Vec<Recalled> = candidates
        .into_iter()
        .map(|(note, rel, details)| {
            let recency = recency(note.updated_ms as i64, now);
            let score = if matched {
                let relevance = if best > 0.0 { rel / best } else { 0.0 };
                0.6 * relevance + 0.25 * note.confidence + 0.15 * recency
            } else {
                0.7 * note.confidence + 0.3 * recency
            };
            let reason = RecallReason {
                keywords: fts.clone(),
                relevance: matched.then_some(if best > 0.0 { rel / best } else { 0.0 }),
                confidence: note.confidence,
                recency,
            };
            Recalled { note, score, reason, details }
        })
        .collect();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.note.id.cmp(&b.note.id)));
    ranked.truncate(k);
    Ok(ranked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn provenance(trace: &str) -> Provenance {
        Provenance { trace: trace.into(), events: vec!["msg_1".into()], service: "memory".into(), version: "v1".into() }
    }

    fn note(kind: NoteKind, text: &str, workspace: Option<&str>) -> NewNote {
        NewNote {
            kind,
            text: text.into(),
            workspace: workspace.map(str::to_owned),
            confidence: 0.6,
            provenance: provenance("trace_1"),
        }
    }

    fn ask(query: &str, workspace: Option<&str>) -> RecallRequest {
        RecallRequest { query: query.into(), workspace: workspace.map(str::to_owned), ..Default::default() }
    }

    fn texts(found: &[Recalled]) -> Vec<&str> {
        found.iter().map(|r| r.note.text.as_str()).collect()
    }

    #[test]
    fn a_note_needs_text_bounds_and_provenance() {
        let db = Db::in_memory().unwrap();
        let ok = note(NoteKind::Fact, "  Tests run with\n `cargo test -p molt`. ", Some("/w"));
        let (stored, reinforced) = remember(&db, ok.clone(), 1).unwrap();
        assert!(!reinforced);
        assert_eq!(stored.text, "Tests run with `cargo test -p molt`.");
        assert!(stored.id.starts_with("note_") && stored.id.len() == 37);
        assert_eq!(stored.provenance, provenance("trace_1"));
        assert_eq!((stored.rev, stored.reinforced, stored.created_ms), (1, 0, 1));

        for bad in [
            NewNote { text: " \n".into(), ..ok.clone() },
            NewNote { text: "x".repeat(MAX_TEXT + 1), ..ok.clone() },
            NewNote { confidence: 1.5, ..ok.clone() },
            NewNote { confidence: f64::NAN, ..ok.clone() },
            NewNote { provenance: Provenance { trace: String::new(), ..provenance("t") }, ..ok.clone() },
            NewNote { provenance: Provenance { version: " ".into(), ..provenance("t") }, ..ok.clone() },
        ] {
            assert!(remember(&db, bad, 2).is_err());
        }
        // Certainty is out of reach either way.
        let (sure, _) = remember(&db, NewNote { text: "Sure thing.".into(), confidence: 1.0, ..ok }, 3).unwrap();
        assert_eq!(sure.confidence, MAX_CONFIDENCE);
    }

    #[test]
    fn the_same_note_again_is_reinforced_not_duplicated() {
        let db = Db::in_memory().unwrap();
        let (first, _) = remember(&db, note(NoteKind::Fact, "Use cargo nextest.", Some("/w")), 1).unwrap();
        let again =
            NewNote { provenance: provenance("trace_2"), ..note(NoteKind::Fact, "use  CARGO nextest", Some("/w")) };
        let (second, reinforced) = remember(&db, again, 2).unwrap();
        assert!(reinforced);
        assert_eq!(second.id, first.id);
        assert!(second.confidence > first.confidence);
        assert_eq!((second.reinforced, second.rev, second.updated_ms), (1, 2, 2));
        // Another workspace has its own note.
        let (other, reinforced) = remember(&db, note(NoteKind::Fact, "Use cargo nextest.", Some("/v")), 3).unwrap();
        assert!(!reinforced);
        assert_ne!(other.id, first.id);
        let trail: Vec<(String, String)> = db.with(|c| {
            let mut s = c.prepare("SELECT what, trace FROM evidence WHERE note = ?1 ORDER BY ms").unwrap();
            s.query_map([&first.id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap()
        });
        assert_eq!(trail, [("learned".into(), "trace_1".into()), ("restated".into(), "trace_2".into())]);
    }

    #[test]
    fn recall_matches_stems_within_the_workspace_and_ranks_relevance_first() {
        let db = Db::in_memory().unwrap();
        let now = 100 * DAY;
        for (text, ws) in [
            ("Integration tests live in crates/*/tests and run with cargo test.", Some("/w")),
            ("The CLI prints progress on stderr.", Some("/w")),
            ("Testing the gateway needs a fake Messages API (wiremock).", Some("/w")),
            ("The user prefers short commit messages.", None),
            ("Tests in the other project use pytest.", Some("/other")),
        ] {
            remember(&db, note(NoteKind::Fact, text, ws), now).unwrap();
        }
        let found = recall(&db, &ask("How do I run the tests?", Some("/w")), now).unwrap();
        assert_eq!(texts(&found).len(), 2, "{:?}", texts(&found));
        assert!(found.iter().all(|r| r.note.workspace.as_deref() == Some("/w")));
        assert!(found[0].score >= found[1].score);
        // FTS5 syntax in a query is only words.
        assert!(recall(&db, &ask("tests\" OR NEAR(", Some("/w")), now).unwrap().len() >= 2);
        assert!(recall(&db, &ask("the of and", Some("/w")), now).unwrap().is_empty(), "stopwords match nothing");
        // Without a workspace, only notes that hold everywhere.
        assert_eq!(texts(&recall(&db, &ask("", None), now).unwrap()), ["The user prefers short commit messages."]);
        // An empty query lists everything in scope, k at most.
        let all = recall(&db, &RecallRequest { k: Some(2), ..ask("", Some("/w")) }, now).unwrap();
        assert_eq!(all.len(), 2);
        let all = recall(&db, &ask("", Some("/w")), now).unwrap();
        assert_eq!(all.len(), 4, "three about /w, one everywhere");
    }

    #[test]
    fn recall_filters_by_kind_and_confidence_and_prefers_recent_notes() {
        let db = Db::in_memory().unwrap();
        let now = 400 * DAY;
        let old = remember(&db, note(NoteKind::Fact, "Build with make all.", Some("/w")), 0).unwrap().0;
        let new = remember(&db, note(NoteKind::Fact, "Build with make release.", Some("/w")), now).unwrap().0;
        let rule =
            remember(&db, note(NoteKind::Convention, "Never build in the source tree.", Some("/w")), now).unwrap().0;
        let found = recall(&db, &ask("build", Some("/w")), now).unwrap();
        let fresh = found.iter().position(|r| r.note.id == new.id).unwrap();
        let stale = found.iter().position(|r| r.note.id == old.id).unwrap();
        assert!(fresh < stale, "equal matches: the recent one ranks first");
        let rules = recall(&db, &RecallRequest { kinds: vec![NoteKind::Convention], ..ask("build", Some("/w")) }, now);
        assert_eq!(rules.unwrap().iter().map(|r| r.note.id.clone()).collect::<Vec<_>>(), [rule.id]);
        let sure = RecallRequest { min_confidence: Some(0.9), ..ask("build", Some("/w")) };
        assert!(recall(&db, &sure, now).unwrap().is_empty());
        assert!(recall(&db, &RecallRequest { min_confidence: Some(2.0), ..ask("", None) }, now).is_err());
    }

    #[test]
    fn forgotten_and_retracted_notes_are_not_recalled_but_kept() {
        let db = Db::in_memory().unwrap();
        let a = remember(&db, note(NoteKind::Fact, "Deploy with fly deploy.", Some("/w")), 1).unwrap().0;
        let mut by_v2 = note(NoteKind::Lesson, "Deploys fail without FLY_TOKEN.", Some("/w"));
        by_v2.provenance.version = "v2".into();
        let b = remember(&db, by_v2.clone(), 1).unwrap().0;
        let c = remember(&db, NewNote { text: "Deploy on Fridays.".into(), ..by_v2 }, 1).unwrap().0;

        assert!(forget(&db, &a.id, "", "trace_9", 2).is_err(), "a reason is required");
        assert!(forget(&db, &a.id, "it moved to render", "trace_9", 2).unwrap());
        assert!(!forget(&db, &a.id, "again", "trace_9", 3).unwrap(), "already forgotten");
        assert!(!forget(&db, "note_missing", "no such note", "trace_9", 3).unwrap());
        let done = retract(&db, "memory", "v2", "v2 was rolled back", "trace_9", 4).unwrap();
        assert_eq!(done, Retracted { retracted: 2, adjusted: 0 });
        assert_eq!(retract(&db, "memory", "v2", "again", "trace_9", 5).unwrap(), Retracted::default());
        assert!(recall(&db, &ask("deploy", Some("/w")), 5).unwrap().is_empty());
        db.with(|c2| {
            for id in [&a.id, &b.id, &c.id] {
                assert!(get(c2, id).unwrap().is_none());
                let reason: Option<String> =
                    c2.query_row("SELECT forget_reason FROM notes WHERE id = ?1", [id], |r| r.get(0)).unwrap();
                assert!(reason.is_some(), "the tombstone keeps its reason");
            }
        });
        // Automatic learning cannot undo the user's withdrawal.
        let err = remember(&db, note(NoteKind::Fact, "Deploy with fly deploy.", Some("/w")), 6).unwrap_err();
        assert!(err.message.contains("withdrawn"));
    }

    fn with_tx<R>(db: &Db, f: impl FnOnce(&Transaction) -> rusqlite::Result<R>) -> R {
        db.with(|c| {
            let tx = c.transaction().unwrap();
            let r = f(&tx).unwrap();
            tx.commit().unwrap();
            r
        })
    }

    fn now_of(db: &Db, id: &str) -> Note {
        db.with(|c| get(c, id).unwrap().unwrap())
    }

    fn by<'a>(trace: &'a str, version: &'a str) -> By<'a> {
        By { trace, service: "memory", version }
    }

    #[test]
    fn a_contradiction_keeps_both_notes_at_lower_confidence() {
        let db = Db::in_memory().unwrap();
        let mut sure = note(NoteKind::Fact, "Tests run with npm test.", Some("/w"));
        sure.confidence = 0.9;
        let old = remember(&db, sure, 1).unwrap().0;
        let mut correction = note(NoteKind::Fact, "Tests run with pnpm test; npm fails on the lockfile.", Some("/w"));
        correction.confidence = 0.9;
        correction.provenance.trace = "trace_2".into();
        let Correction::Added(id) = with_tx(&db, |tx| contradict(tx, &old.id, &correction, 2)) else { panic!() };
        let old_now = now_of(&db, &old.id);
        let new = now_of(&db, &id);
        assert!((old_now.confidence - 0.54).abs() < 1e-9, "{}", old_now.confidence);
        assert_eq!(new.confidence, CONTESTED);
        assert_eq!(old_now.conflicts, std::slice::from_ref(&id));
        assert_eq!(new.conflicts, std::slice::from_ref(&old.id));
        // The same episode correcting it again lowers it no further.
        let again = NewNote { text: "Tests run with yarn test.".into(), ..correction.clone() };
        let Correction::Added(other) = with_tx(&db, |tx| contradict(tx, &old.id, &again, 3)) else { panic!() };
        let old_now = now_of(&db, &old.id);
        assert!((old_now.confidence - 0.54).abs() < 1e-9, "{}", old_now.confidence);
        assert_eq!(old_now.conflicts, [id.clone(), other]);
        // Contradicting a note that does not exist stores nothing.
        assert_eq!(with_tx(&db, |tx| contradict(tx, "note_missing", &correction, 3)), Correction::Missing);
        let found = recall(&db, &ask("tests", Some("/w")), 3).unwrap();
        assert_eq!(found.len(), 3, "all stay until evidence settles it");

        // Forgetting a correction settles the dispute it started.
        forget(&db, &id, "wrong", "trace_9", 4).unwrap();
        assert_eq!(now_of(&db, &old.id).conflicts.len(), 1);
    }

    #[test]
    fn a_correction_that_says_what_a_note_says_bears_that_note_out() {
        let db = Db::in_memory().unwrap();
        let old = remember(&db, note(NoteKind::Fact, "Deploys go through make ship.", Some("/w")), 1).unwrap().0;
        let right = remember(&db, note(NoteKind::Fact, "Deploys go through fly deploy.", Some("/w")), 1).unwrap().0;
        let mut same_as_old = note(NoteKind::Fact, "deploys go through make ship", Some("/w"));
        same_as_old.provenance.trace = "trace_2".into();
        assert_eq!(with_tx(&db, |tx| contradict(tx, &old.id, &same_as_old, 2)), Correction::Restated(true));
        let o = now_of(&db, &old.id);
        assert!((o.confidence - 0.72).abs() < 1e-9 && o.conflicts.is_empty(), "{o:?}");

        let mut same_as_right = note(NoteKind::Fact, "Deploys go through fly deploy", Some("/w"));
        same_as_right.provenance.trace = "trace_3".into();
        let done = with_tx(&db, |tx| contradict(tx, &old.id, &same_as_right, 3));
        assert_eq!(done, Correction::Matched(right.id.clone(), true));
        let (o, r) = (now_of(&db, &old.id), now_of(&db, &right.id));
        assert!((o.confidence - 0.432).abs() < 1e-9, "{}", o.confidence);
        assert!((r.confidence - 0.72).abs() < 1e-9, "borne out, not capped: {}", r.confidence);
        assert_eq!((o.conflicts, r.conflicts), (vec![right.id.clone()], vec![old.id.clone()]));
    }

    #[test]
    fn an_episode_bears_a_note_out_once() {
        let db = Db::in_memory().unwrap();
        let first = remember(&db, note(NoteKind::Fact, "Lint with ruff.", Some("/w")), 1).unwrap().0;
        for _ in 0..10 {
            let (again, found) = remember(&db, note(NoteKind::Fact, "Lint with ruff", Some("/w")), 2).unwrap();
            assert!(found);
            assert_eq!((again.confidence, again.reinforced), (first.confidence, 0));
        }
        let later = NewNote { provenance: provenance("trace_2"), ..note(NoteKind::Fact, "Lint with ruff", Some("/w")) };
        assert_eq!(remember(&db, later, 3).unwrap().0.reinforced, 1);
        assert!(!with_tx(&db, |tx| reinforce(tx, &first.id, by("trace_2", "v1"), 4)), "trace_2 bore it out already");
        assert!(with_tx(&db, |tx| reinforce(tx, &first.id, by("trace_3", "v1"), 4)));
        assert_eq!(now_of(&db, &first.id).reinforced, 2);
    }

    #[test]
    fn retracting_a_version_undoes_all_it_did_and_nothing_else() {
        let db = Db::in_memory().unwrap();
        let mut good = note(NoteKind::Fact, "The API lives in src/api.", Some("/w"));
        good.provenance.version = "v0".into();
        let good = remember(&db, good.clone(), 1).unwrap().0;
        let mut odd = note(NoteKind::Fact, "Migrations run with sqlx migrate run.", Some("/w"));
        odd.provenance.version = "v0".into();
        let odd = remember(&db, odd, 1).unwrap().0;
        let before = (now_of(&db, &good.id), now_of(&db, &odd.id));

        // v1, in episode trace_e: bears one note out, disputes another, and
        // creates two notes; another version later bears one of those out.
        let mut wrong = note(NoteKind::Fact, "Migrations run with diesel.", Some("/w"));
        wrong.provenance = Provenance { trace: "trace_e".into(), ..provenance("trace_e") };
        let lone = NewNote { text: "The CLI is in src/cli.".into(), ..wrong.clone() };
        with_tx(&db, |tx| {
            reinforce(tx, &good.id, by("trace_e", "v1"), 2)?;
            contradict(tx, &odd.id, &wrong, 2)?;
            store(tx, &lone, 2)
        });
        let shared = NewNote { text: "Docs build with mdbook.".into(), ..wrong.clone() };
        let (shared_id, _) = with_tx(&db, |tx| store(tx, &shared, 2));
        let mut by_v2 = NewNote { text: "Docs build with mdbook".into(), ..shared.clone() };
        by_v2.provenance = Provenance { trace: "trace_f".into(), version: "v2".into(), ..provenance("trace_f") };
        by_v2.confidence = 0.7;
        remember(&db, by_v2, 3).unwrap();
        db.with(|c| {
            c.execute("INSERT INTO consolidated VALUES ('trace_e', 2, 'memory', 'v1')", []).unwrap();
        });

        let done = retract(&db, "memory", "v1", "v1 was rolled back", "trace_r", 9).unwrap();
        assert_eq!(done, Retracted { retracted: 2, adjusted: 3 });
        // What v1 bore out or disputed is back where it was, conflicts and all.
        let after = (now_of(&db, &good.id), now_of(&db, &odd.id));
        assert_eq!((after.0.confidence, after.0.reinforced), (before.0.confidence, before.0.reinforced));
        assert_eq!((after.1.confidence, after.1.conflicts.len()), (before.1.confidence, 0));
        // The note v2 bore out stays, as v2 learned it.
        let kept = now_of(&db, &shared_id);
        assert_eq!((kept.confidence, kept.reinforced), (0.7, 0));
        assert_eq!((kept.provenance.version.as_str(), kept.provenance.trace.as_str()), ("v2", "trace_f"));
        // The rest of v1's notes are gone, and the episode can be learned from again
        // without counting twice.
        assert!(recall(&db, &ask("diesel cli", Some("/w")), 9).unwrap().is_empty());
        db.with(|c| {
            assert_eq!(c.query_row("SELECT COUNT(*) FROM consolidated", [], |r| r.get::<_, i64>(0)).unwrap(), 0)
        });
        with_tx(&db, |tx| reinforce(tx, &good.id, by("trace_e", "v3"), 10));
        assert_eq!(now_of(&db, &good.id).reinforced, 1);
    }

    #[test]
    fn filters_apply_before_the_candidates_are_cut() {
        let db = Db::in_memory().unwrap();
        for n in 0..(CANDIDATES + 100) {
            remember(&db, note(NoteKind::Fact, &format!("Build target {n} builds with make."), Some("/w")), 1).unwrap();
        }
        let rule = remember(&db, note(NoteKind::Convention, "Never build as root.", Some("/w")), 1).unwrap().0;
        for query in ["build", ""] {
            let req = RecallRequest { kinds: vec![NoteKind::Convention], ..ask(query, Some("/w")) };
            let found = recall(&db, &req, 1).unwrap();
            assert_eq!(found.iter().map(|r| r.note.id.as_str()).collect::<Vec<_>>(), [rule.id.as_str()], "{query:?}");
        }
        // An empty query ranks by confidence and recency over every note.
        let fresh = remember(&db, note(NoteKind::Fact, "Freshly learned.", Some("/w")), 200 * DAY).unwrap().0;
        let top = recall(&db, &RecallRequest { k: Some(1), ..ask("", Some("/w")) }, 200 * DAY).unwrap();
        assert_eq!(top[0].note.id, fresh.id);
        // Only the workspace's own, when asked.
        remember(&db, note(NoteKind::Preference, "Never build on Fridays.", None), 1).unwrap();
        let req = RecallRequest { kinds: vec![NoteKind::Preference], ..ask("build", Some("/w")) };
        assert_eq!(recall(&db, &req, 1).unwrap().len(), 1);
        assert!(recall_in(&db, &req, Scope::Own, 1).unwrap().is_empty());
    }
}

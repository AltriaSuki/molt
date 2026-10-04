//! Notes: the semantic memory.
//!
//! A note is one sentence with a kind, a confidence and its provenance. Two
//! notes with the same text (ignoring case, spacing and a final period) about
//! the same workspace are one note: storing it again reinforces it. Recall
//! matches word stems with SQLite's full-text index and ranks the matches by
//! relevance, confidence and recency. Forgetting leaves a tombstone. Every
//! change also lands in the `evidence` table, so each note's history can be
//! traced back to the episodes behind it.

use molt_api::memory::{Note, NoteKind, Provenance, RecallRequest, Recalled};
use molt_proto::RemoteError;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

use crate::db::Db;
use crate::error::{self, invalid};

/// Notes, their full-text index, the evidence trail of each, and the
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
    ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_note ON evidence(note);
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
/// contradicting it starts at no more than [`CONTESTED`].
const CONTRADICTED: f64 = 0.6;
const CONTESTED: f64 = 0.5;
const DEFAULT_K: u32 = 8;
const MAX_K: u32 = 50;
/// Matches considered before ranking.
const CANDIDATES: usize = 500;
/// Recency halves a note's weight for ranking about every this many days.
const HALF_LIFE_DAYS: f64 = 30.0;
const DAY_MS: f64 = 86_400_000.0;

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

/// What makes two notes the same note: case, spacing and a final period aside.
fn norm(text: &str) -> String {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    words.join(" ").trim_end_matches('.').to_owned()
}

fn new_id() -> String {
    format!("note_{:032x}", rand::random::<u128>())
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

fn record(tx: &Transaction, note: &str, trace: &str, what: &str, detail: &str, now: u64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO evidence (note, trace, what, detail, ms) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![note, trace, what, detail, now as i64],
    )?;
    Ok(())
}

/// Store `new`, or reinforce the live note about the same workspace with the
/// same text. Returns the note as stored and whether it was reinforced.
pub(crate) fn remember(db: &Db, new: NewNote, now: u64) -> Result<(Note, bool), RemoteError> {
    let new = new.checked()?;
    db.with(|conn| {
        let tx = conn.transaction()?;
        let (id, reinforced) = store(&tx, &new, now)?;
        tx.commit()?;
        Ok((get(conn, &id)?.expect("the note was just stored"), reinforced))
    })
    .map_err(error::db)
}

/// [`remember`] inside a transaction: the id of the note, and whether an
/// existing one was reinforced.
pub(crate) fn store(tx: &Transaction, new: &NewNote, now: u64) -> rusqlite::Result<(String, bool)> {
    let norm = norm(&new.text);
    let same: Option<String> = tx
        .query_row(
            "SELECT id FROM notes WHERE norm = ?1 AND workspace IS ?2 AND forgotten_ms IS NULL",
            params![norm, new.workspace],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = same {
        reinforce(tx, &id, &new.provenance.trace, now)?;
        return Ok((id, true));
    }
    let id = new_id();
    let p = &new.provenance;
    tx.execute(
        "INSERT INTO notes (id, kind, text, norm, workspace, confidence, created_ms, updated_ms, trace, events, \
         service, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11)",
        params![
            id,
            new.kind.as_str(),
            new.text,
            norm,
            new.workspace,
            new.confidence,
            now as i64,
            p.trace,
            serde_json::to_string(&p.events).expect("strings serialize"),
            p.service,
            p.version,
        ],
    )?;
    record(tx, &id, &p.trace, "learned", "", now)?;
    Ok((id, false))
}

/// Raise the confidence of the live note `id`: an episode bore it out.
/// False when there is no such note.
pub(crate) fn reinforce(tx: &Transaction, id: &str, trace: &str, now: u64) -> rusqlite::Result<bool> {
    let changed = tx.execute(
        "UPDATE notes SET confidence = MIN(?2, confidence + (1.0 - confidence) * ?3), reinforced = reinforced + 1, \
         updated_ms = ?4, rev = rev + 1 WHERE id = ?1 AND forgotten_ms IS NULL",
        params![id, MAX_CONFIDENCE, REINFORCE, now as i64],
    )?;
    if changed > 0 {
        record(tx, id, trace, "reinforced", "", now)?;
    }
    Ok(changed > 0)
}

/// Store `new` as disagreeing with the live note `old`: both stay, the old
/// one at lower confidence and the new one at no more than an even chance,
/// each naming the other, until more evidence settles it. Returns the new
/// note's id, or `None` when `old` is not a live note (nothing is stored).
pub(crate) fn contradict(tx: &Transaction, old: &str, new: &NewNote, now: u64) -> rusqlite::Result<Option<String>> {
    let trace = &new.provenance.trace;
    let changed = tx.execute(
        "UPDATE notes SET confidence = MAX(?2, confidence * ?3), updated_ms = ?4, rev = rev + 1 \
         WHERE id = ?1 AND forgotten_ms IS NULL",
        params![old, MIN_CONFIDENCE, CONTRADICTED, now as i64],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let contested = NewNote { confidence: new.confidence.min(CONTESTED), ..new.clone() };
    let (id, _) = store(tx, &contested, now)?;
    if id == old {
        // The "correction" says what the note already says.
        return Ok(Some(id));
    }
    for (a, b) in [(old, id.as_str()), (id.as_str(), old)] {
        tx.execute(
            "UPDATE notes SET conflicts = (SELECT json_group_array(value) FROM \
             (SELECT value FROM json_each(conflicts) UNION SELECT ?2)) WHERE id = ?1",
            params![a, b],
        )?;
    }
    record(tx, old, trace, "contradicted", &id, now)?;
    Ok(Some(id))
}

/// Tombstone the live note `id`. False when there is none.
pub(crate) fn forget(db: &Db, id: &str, reason: &str, trace: &str, now: u64) -> Result<bool, RemoteError> {
    if reason.trim().is_empty() {
        return Err(invalid("say why the note should be forgotten"));
    }
    db.with(|conn| {
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE notes SET forgotten_ms = ?2, forget_reason = ?3, rev = rev + 1 \
             WHERE id = ?1 AND forgotten_ms IS NULL",
            params![id, now as i64, reason],
        )?;
        if changed > 0 {
            record(&tx, id, trace, "forgotten", reason, now)?;
        }
        tx.commit()?;
        Ok(changed > 0)
    })
    .map_err(error::db)
}

/// Tombstone every live note `service` at `version` wrote, and let the
/// episodes it learned from be learned from again.
pub(crate) fn retract(
    db: &Db,
    service: &str,
    version: &str,
    reason: &str,
    trace: &str,
    now: u64,
) -> Result<u64, RemoteError> {
    if service.trim().is_empty() || version.trim().is_empty() {
        return Err(invalid("name the service and the version whose notes to retract"));
    }
    if reason.trim().is_empty() {
        return Err(invalid("say why the notes should be retracted"));
    }
    db.with(|conn| {
        let tx = conn.transaction()?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM notes WHERE service = ?1 AND version = ?2 AND forgotten_ms IS NULL ORDER BY id",
            )?;
            let ids = stmt.query_map(params![service, version], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        for id in &ids {
            tx.execute(
                "UPDATE notes SET forgotten_ms = ?2, forget_reason = ?3, rev = rev + 1 WHERE id = ?1",
                params![id, now as i64, reason],
            )?;
            record(&tx, id, trace, "retracted", reason, now)?;
        }
        tx.execute("DELETE FROM consolidated WHERE service = ?1 AND version = ?2", params![service, version])?;
        tx.commit()?;
        Ok(ids.len() as u64)
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

/// The notes that best answer `req`, best first. `req.workspace` must be
/// the canonical workspace already.
pub(crate) fn recall(db: &Db, req: &RecallRequest, now: u64) -> Result<Vec<Recalled>, RemoteError> {
    let k = req.k.unwrap_or(DEFAULT_K).clamp(1, MAX_K) as usize;
    if let Some(min) = req.min_confidence {
        if !min.is_finite() || !(0.0..=1.0).contains(&min) {
            return Err(invalid("min_confidence must be between 0 and 1"));
        }
    }
    let scope = "n.forgotten_ms IS NULL AND (n.workspace IS NULL OR n.workspace IS ?1)";
    let columns = COLUMNS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ");
    let candidates: Vec<(Note, f64)> = db
        .with(|conn| -> rusqlite::Result<_> {
            match fts_query(&req.query) {
                Some(fts) => {
                    let mut stmt = conn.prepare(&format!(
                        "SELECT {columns}, bm25(notes_fts) FROM notes_fts JOIN notes n ON n.rowid = notes_fts.rowid \
                         WHERE notes_fts MATCH ?2 AND {scope} ORDER BY bm25(notes_fts) LIMIT {CANDIDATES}"
                    ))?;
                    let rows =
                        stmt.query_map(params![req.workspace, fts], |r| Ok((note_from(r)?, -r.get::<_, f64>(14)?)))?;
                    rows.collect()
                }
                None if !req.query.trim().is_empty() => Ok(Vec::new()),
                None => {
                    let mut stmt = conn.prepare(&format!(
                        "SELECT {columns} FROM notes n WHERE {scope} \
                         ORDER BY n.confidence DESC, n.updated_ms DESC LIMIT {CANDIDATES}"
                    ))?;
                    let rows = stmt.query_map(params![req.workspace], |r| Ok((note_from(r)?, 0.0)))?;
                    rows.collect()
                }
            }
        })
        .map_err(error::db)?;

    let matched = fts_query(&req.query).is_some();
    let best = candidates.iter().map(|(_, rel)| *rel).fold(0.0, f64::max);
    let mut ranked: Vec<Recalled> = candidates
        .into_iter()
        .filter(|(note, _)| req.kinds.is_empty() || req.kinds.contains(&note.kind))
        .filter(|(note, _)| req.min_confidence.is_none_or(|min| note.confidence >= min))
        .map(|(note, rel)| {
            let age_days = now.saturating_sub(note.updated_ms) as f64 / DAY_MS;
            let recency = 0.5f64.powf(age_days / HALF_LIFE_DAYS);
            let score = if matched {
                let relevance = if best > 0.0 { rel / best } else { 0.0 };
                0.6 * relevance + 0.25 * note.confidence + 0.15 * recency
            } else {
                0.7 * note.confidence + 0.3 * recency
            };
            Recalled { note, score }
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
        assert_eq!(trail, [("learned".into(), "trace_1".into()), ("reinforced".into(), "trace_2".into())]);
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
        assert_eq!(retract(&db, "memory", "v2", "v2 was rolled back", "trace_9", 4).unwrap(), 2);
        assert_eq!(retract(&db, "memory", "v2", "again", "trace_9", 5).unwrap(), 0);
        assert!(recall(&db, &ask("deploy", Some("/w")), 5).unwrap().is_empty());
        db.with(|c2| {
            for id in [&a.id, &b.id, &c.id] {
                assert!(get(c2, id).unwrap().is_none());
                let reason: Option<String> =
                    c2.query_row("SELECT forget_reason FROM notes WHERE id = ?1", [id], |r| r.get(0)).unwrap();
                assert!(reason.is_some(), "the tombstone keeps its reason");
            }
        });
        // A forgotten note's text can be learned afresh.
        let (again, reinforced) =
            remember(&db, note(NoteKind::Fact, "Deploy with fly deploy.", Some("/w")), 6).unwrap();
        assert!(!reinforced);
        assert_ne!(again.id, a.id);
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
        let id = db
            .with(|c| -> rusqlite::Result<_> {
                let tx = c.transaction()?;
                let id = contradict(&tx, &old.id, &correction, 2)?;
                tx.commit()?;
                Ok(id)
            })
            .unwrap()
            .unwrap();
        db.with(|c| {
            let old_now = get(c, &old.id).unwrap().unwrap();
            let new = get(c, &id).unwrap().unwrap();
            assert!(old_now.confidence < old.confidence);
            assert_eq!(new.confidence, CONTESTED);
            assert_eq!(old_now.conflicts, std::slice::from_ref(&id));
            assert_eq!(new.conflicts, std::slice::from_ref(&old.id));
            // Contradicting a note that does not exist stores nothing.
            let tx = c.transaction().unwrap();
            assert_eq!(contradict(&tx, "note_missing", &correction, 3).unwrap(), None);
        });
        let found = recall(&db, &ask("tests", Some("/w")), 2).unwrap();
        assert_eq!(found.len(), 2, "both stay until evidence settles it");
    }
}

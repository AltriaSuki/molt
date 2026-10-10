//! Dependency review and immutable recall records. User edits and a recall's
//! selection/record are committed atomically through the database writer.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

use molt_api::memory::{
    FileDependency, NoteDetails, Provenance, RecallRequest, RecallSnapshot, Recalled, ReviewRequest, ReviewResponse,
};
use molt_proto::RemoteError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::error::{self, invalid};
use crate::notes::{self, NewNote, Scope};
use crate::Db;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS note_details (
    note TEXT PRIMARY KEY REFERENCES notes(id),
    details TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS note_reviews (
    note TEXT NOT NULL REFERENCES notes(id),
    rev INTEGER NOT NULL,
    trace TEXT NOT NULL,
    service TEXT NOT NULL,
    reason TEXT NOT NULL,
    details TEXT NOT NULL,
    ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS recall_snapshots (
    run TEXT NOT NULL,
    call TEXT NOT NULL,
    workspace TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    PRIMARY KEY (run, call)
);
CREATE INDEX IF NOT EXISTS recall_workspace ON recall_snapshots(workspace, run);
";

pub(crate) fn decode<T: DeserializeOwned>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

/// Open each component relative to the last directory descriptor. No
/// symlink (including one swapped in during traversal) can escape the root.
fn fingerprint(workspace: &Path, path: &str) -> Result<String, RemoteError> {
    let components: Vec<_> = Path::new(path).components().collect();
    if components.is_empty()
        || components.len() > 64
        || path.len() > 4096
        || components.iter().any(|c| !matches!(c, Component::Normal(n) if *n != ".molt" && *n != ".git"))
    {
        return Err(invalid("dependencies must be relative files inside the project, outside .molt and .git"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(workspace)
        .map_err(|e| invalid(format!("dependency root: {e}")))?;
    for (i, part) in components.iter().enumerate() {
        let Component::Normal(name) = part else { unreachable!() };
        let name = CString::new(name.as_encoded_bytes()).map_err(|_| invalid("dependency contains a NUL"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if i + 1 < components.len() { libc::O_DIRECTORY } else { 0 };
        // SAFETY: the directory descriptor and NUL-terminated name live
        // across openat; a successful descriptor is owned exactly once.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(invalid(format!("dependency {path}: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: openat returned a fresh owned file descriptor.
        file = unsafe { File::from_raw_fd(fd) };
    }
    let meta = file.metadata().map_err(|e| invalid(format!("dependency {path}: {e}")))?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return Err(invalid(format!("dependency {path} must be a regular file of at most 1 MiB")));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| invalid(format!("dependency {path}: {e}")))?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid(format!("dependency {path} exceeds 1 MiB")));
    }
    Ok(hex::encode(Sha256::digest(&bytes)))
}

struct Changed {
    id: String,
    rev: i64,
    details: NoteDetails,
}

/// Hash off the database lock. The revision lease below prevents an older
/// check from overwriting a user review that happened during the file read.
fn changed(db: &Db, workspace: &str) -> Result<Vec<Changed>, RemoteError> {
    let candidates: Vec<(String, i64, String)> = db.with(|conn| {
        let mut stmt = conn.prepare("SELECT n.id, n.rev, d.details FROM notes n JOIN note_details d ON d.note = n.id WHERE n.workspace = ?1 AND n.forgotten_ms IS NULL")?;
        let rows = stmt.query_map([workspace], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>();
        rows
    }).map_err(error::db)?;
    let mut out = Vec::new();
    for (id, rev, text) in candidates {
        let mut details: NoteDetails = decode(&text).map_err(error::db)?;
        let before = details.needs_review.len();
        for dep in &details.dependencies {
            if !details.needs_review.contains(&dep.path)
                && fingerprint(Path::new(workspace), &dep.path).as_deref() != Ok(dep.sha256.as_str())
            {
                details.needs_review.push(dep.path.clone());
            }
        }
        if details.needs_review.len() > before {
            out.push(Changed { id, rev, details });
        }
    }
    Ok(out)
}

fn save(
    conn: &Connection,
    id: &str,
    details: &NoteDetails,
    trace: &str,
    service: &str,
    reason: &str,
    now: u64,
) -> rusqlite::Result<()> {
    let text = serde_json::to_string(details).expect("note details serialize");
    conn.execute("INSERT INTO note_details (note, details) VALUES (?1, ?2) ON CONFLICT(note) DO UPDATE SET details = excluded.details", params![id, text])?;
    conn.execute("INSERT INTO note_reviews (note, rev, trace, service, reason, details, ms) SELECT id, rev, ?2, ?3, ?4, ?5, ?6 FROM notes WHERE id = ?1", params![id, trace, service, reason, text, now as i64])?;
    Ok(())
}

fn apply(conn: &Connection, changes: Vec<Changed>, trace: &str, now: u64) -> rusqlite::Result<()> {
    for change in changes {
        let n = conn.execute(
            "UPDATE notes SET rev = rev + 1 WHERE id = ?1 AND rev = ?2 AND forgotten_ms IS NULL",
            params![change.id, change.rev],
        )?;
        if n == 1 {
            save(conn, &change.id, &change.details, trace, "memory", "dependency changed or could not be read", now)?;
        }
    }
    Ok(())
}

pub(crate) fn refresh(db: &Db, workspace: &str, trace: &str, now: u64) -> Result<(), RemoteError> {
    let changes = changed(db, workspace)?;
    db.with(|conn| {
        let tx = conn.transaction()?;
        apply(&tx, changes, trace, now)?;
        tx.commit()
    })
    .map_err(error::db)
}

pub(crate) fn recall(
    db: &Db,
    req: &RecallRequest,
    identity: Option<(&str, &str)>,
    now: u64,
) -> Result<Vec<Recalled>, RemoteError> {
    notes::validate_recall(req)?;
    if req.capture.is_some() && identity.is_none() {
        return Err(invalid("captured recall requires an authenticated request"));
    }
    let changes = req.workspace.as_deref().map(|ws| changed(db, ws)).transpose()?.unwrap_or_default();
    db.with(|conn| -> rusqlite::Result<_> {
        let tx = conn.transaction()?;
        apply(&tx, changes, identity.map_or("inspection", |i| i.0), now)?;
        let found = notes::select(&tx, req, Scope::WithGlobal, now)?;
        if let (Some(purpose), Some((run, call))) = (&req.capture, identity) {
            let snapshot = RecallSnapshot {
                run: run.into(),
                call: call.into(),
                workspace: req.workspace.clone().expect("validated workspace"),
                query: req.query.clone(),
                purpose: purpose.clone(),
                created_ms: now,
                notes: found.clone(),
            };
            // A replay of the same envelope must not rewrite historical data.
            let previous: Option<String> = tx
                .query_row(
                    "SELECT snapshot FROM recall_snapshots WHERE run = ?1 AND call = ?2",
                    params![run, call],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                let previous: RecallSnapshot = decode(&previous)?;
                if previous.workspace != snapshot.workspace
                    || previous.query != snapshot.query
                    || previous.purpose != snapshot.purpose
                {
                    return Err(rusqlite::Error::InvalidParameterName(
                        "recorded recall identity belongs to a different request".into(),
                    ));
                }
                tx.commit()?;
                return Ok(previous.notes);
            }
            tx.execute(
                "INSERT INTO recall_snapshots VALUES (?1, ?2, ?3, ?4)",
                params![run, call, snapshot.workspace, serde_json::to_string(&snapshot).expect("snapshot serializes")],
            )?;
        }
        tx.commit()?;
        Ok(found)
    })
    .map_err(error::db)
}

/// Historical views stay project-scoped, including any global notes that
/// were selected for that project at the time.
pub(crate) fn snapshots(db: &Db, workspace: &str, run: &str) -> Result<Vec<RecallSnapshot>, RemoteError> {
    db.read(|conn| {
        let mut stmt =
            conn.prepare("SELECT snapshot FROM recall_snapshots WHERE workspace = ?1 AND run = ?2 ORDER BY rowid")?;
        let rows = stmt.query_map(params![workspace, run], |r| decode(&r.get::<_, String>(0)?))?.collect();
        rows
    })
    .map_err(error::db)
}

/// Review or correct a live project note, atomically with its revision and
/// immutable review record. Original learning evidence is never rewritten.
pub(crate) fn review(
    db: &Db,
    req: &ReviewRequest,
    correction: Option<&str>,
    provenance: Provenance,
    now: u64,
) -> Result<ReviewResponse, RemoteError> {
    if req.reason.trim().is_empty() || req.reason.len() > 2000 || req.depends_on.len() > 16 {
        return Err(invalid("give a reason of at most 2000 bytes and at most 16 dependency files"));
    }
    let mut dependencies = Vec::new();
    for path in &req.depends_on {
        if !dependencies.iter().any(|d: &FileDependency| &d.path == path) {
            dependencies
                .push(FileDependency { path: path.clone(), sha256: fingerprint(Path::new(&req.workspace), path)? });
        }
    }
    let new_text = correction
        .map(|text| {
            NewNote {
                kind: molt_api::memory::NoteKind::Fact,
                text: text.into(),
                workspace: Some(req.workspace.clone()),
                confidence: 0.95,
                provenance: provenance.clone(),
            }
            .checked()
        })
        .transpose()?;
    db.with(|conn| -> Result<_, RemoteError> {
        let tx = conn.transaction().map_err(error::db)?;
        let old =
            notes::get(&tx, &req.id).map_err(error::db)?.ok_or_else(|| invalid("the note is missing or withdrawn"))?;
        if old.workspace.as_deref() != Some(&req.workspace) {
            return Err(invalid("the note belongs to another project (global notes cannot be edited here)"));
        }
        if old.rev != req.expected_rev {
            return Err(invalid(format!(
                "note changed: expected revision {}, current {}; inspect it again",
                req.expected_rev, old.rev
            )));
        }
        let mut details = NoteDetails {
            dependencies,
            needs_review: Vec::new(),
            temporary: req.temporary,
            reason: req.reason.clone(),
            supersedes: None,
        };
        let id = if let Some(mut new) = new_text {
            if notes::norm(&new.text) == notes::norm(&old.text) {
                return Err(invalid("the correction must change the note; use review to confirm it"));
            }
            new.kind = old.kind;
            details.supersedes = Some(old.id.clone());
            if notes::withdrawn(&tx, &new).map_err(error::db)? {
                return Err(invalid("this text was withdrawn; it cannot be automatically restored"));
            }
            notes::tombstone(&tx, &old.id, "forgotten", &req.reason, &provenance.trace, now).map_err(error::db)?;
            notes::settle_partners(&tx, &old.id).map_err(error::db)?;
            notes::store(&tx, &new, now).map_err(error::db)?.0
        } else {
            let previous: Option<String> = tx
                .query_row("SELECT details FROM note_details WHERE note = ?1", [&old.id], |r| r.get(0))
                .optional()
                .map_err(error::db)?;
            if let Some(previous) = previous {
                details.supersedes = decode::<NoteDetails>(&previous).map_err(error::db)?.supersedes;
            }
            tx.execute("UPDATE notes SET rev = rev + 1 WHERE id = ?1", [&old.id]).map_err(error::db)?;
            old.id
        };
        save(&tx, &id, &details, &provenance.trace, &provenance.service, &req.reason, now).map_err(error::db)?;
        let note = notes::get(&tx, &id).map_err(error::db)?.expect("reviewed live note");
        tx.commit().map_err(error::db)?;
        Ok(ReviewResponse { note, details })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_api::memory::NoteKind;
    use std::sync::Arc;

    fn author() -> Provenance {
        Provenance {
            trace: "run_source".into(),
            events: vec!["message_source".into()],
            service: "cli".into(),
            version: "user".into(),
        }
    }

    fn note(db: &Db, workspace: &str, text: &str) -> molt_api::memory::Note {
        notes::remember(
            db,
            NewNote {
                kind: NoteKind::Fact,
                text: text.into(),
                workspace: Some(workspace.into()),
                confidence: 0.8,
                provenance: author(),
            },
            1,
        )
        .unwrap()
        .0
    }

    fn request(workspace: &str, note: &molt_api::memory::Note) -> ReviewRequest {
        ReviewRequest {
            workspace: workspace.into(),
            id: note.id.clone(),
            expected_rev: note.rev,
            reason: "verified against configuration".into(),
            depends_on: vec![],
            temporary: false,
        }
    }

    fn query(workspace: &str) -> RecallRequest {
        RecallRequest { workspace: Some(workspace.into()), ..Default::default() }
    }

    #[test]
    fn dependencies_are_selective_sticky_and_history_is_immutable() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().to_str().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "old config").unwrap();
        let db = Db::in_memory().unwrap();
        let old = note(&db, ws, "Tests run with cargo test.");
        let unrelated = note(&db, ws, "Use snake case for functions.");
        let req = ReviewRequest { depends_on: vec!["Cargo.toml".into()], temporary: true, ..request(ws, &old) };
        let reviewed = review(&db, &req, None, author(), 2).unwrap();
        let q = RecallRequest { query: "tests".into(), capture: Some("context".into()), ..query(ws) };
        let found = recall(&db, &q, Some(("run_a", "call_a")), 3).unwrap();
        assert_eq!(found[0].note, reviewed.note);
        assert!(found[0].details.temporary && found[0].reason.keywords.is_some());
        assert_eq!(found[0].reason.relevance, Some(1.0));
        let snapshot = snapshots(&db, ws, "run_a").unwrap();
        std::fs::write(root.path().join("unrelated.rs"), "fn other() {}").unwrap();
        assert_eq!(recall(&db, &query(ws), None, 4).unwrap().len(), 2);
        std::fs::write(root.path().join("Cargo.toml"), "new config").unwrap();
        assert_eq!(recall(&db, &query(ws), None, 5).unwrap()[0].note.id, unrelated.id);
        let inspect = RecallRequest { include_review: true, query: "tests".into(), ..query(ws) };
        let stale = recall(&db, &inspect, None, 6).unwrap().remove(0);
        assert_eq!(stale.details.needs_review, ["Cargo.toml"]);
        std::fs::write(root.path().join("Cargo.toml"), "old config").unwrap();
        assert_eq!(
            recall(&db, &query(ws), None, 7).unwrap().len(),
            1,
            "restoring a file cannot automatically reactivate a note"
        );
        assert!(review(&db, &req, None, author(), 8).unwrap_err().message.contains("revision"));
        let req = ReviewRequest { expected_rev: stale.note.rev, ..req };
        let rechecked = review(&db, &req, None, author(), 9).unwrap();
        assert_eq!(recall(&db, &query(ws), None, 10).unwrap().len(), 2);
        notes::forget(&db, &rechecked.note.id, "removed by user", "forget_run", 11).unwrap();
        assert_eq!(snapshots(&db, ws, "run_a").unwrap(), snapshot);
        assert_eq!(
            recall(&db, &q, Some(("run_a", "call_a")), 12).unwrap(),
            snapshot[0].notes,
            "same envelope cannot rewrite a snapshot"
        );
        assert!(snapshots(&db, "/another-project", "run_a").unwrap().is_empty());
    }

    #[test]
    fn correction_keeps_evidence_and_withdrawn_text_cannot_return() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().to_str().unwrap();
        let db = Db::in_memory().unwrap();
        let old = note(&db, ws, "Build with npm.");
        let corrected = review(&db, &request(ws, &old), Some("Build with pnpm."), author(), 2).unwrap();
        assert_eq!(corrected.details.supersedes.as_deref(), Some(old.id.as_str()));
        assert_eq!(recall(&db, &query(ws), None, 3).unwrap()[0].note.id, corrected.note.id);
        let new = NewNote {
            kind: old.kind,
            text: "build with NPM".into(),
            workspace: old.workspace.clone(),
            confidence: 0.9,
            provenance: author(),
        };
        assert!(notes::remember(&db, new.clone(), 4).unwrap_err().message.contains("withdrawn"));
        assert!(db.with(|c| notes::withdrawn(c, &new)).unwrap());
        let original: String =
            db.with(|c| c.query_row("SELECT events FROM notes WHERE id = ?1", [&old.id], |r| r.get(0))).unwrap();
        assert_eq!(original, "[\"message_source\"]");
        let reason: String =
            db.with(|c| c.query_row("SELECT forget_reason FROM notes WHERE id = ?1", [&old.id], |r| r.get(0))).unwrap();
        assert_eq!(reason, "verified against configuration");
        notes::retract(&db, "cli", "user", "rollback", "rollback_run", 5).unwrap();
        assert!(recall(&db, &query(ws), None, 6).unwrap().is_empty());
        assert!(review(&db, &request(ws, &old), None, author(), 7).is_err());
    }

    #[test]
    fn dependencies_refuse_symlinks_traversal_devices_and_large_files() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", root.path().join("link")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.path().join("file")).unwrap();
        std::fs::write(root.path().join("large"), vec![0; 1024 * 1024 + 1]).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        for path in [
            "link/passwd",
            "file",
            "../etc/passwd",
            "/etc/passwd",
            "large",
            "dir",
            ".git/config",
            ".molt/state",
            "absent",
        ] {
            assert!(fingerprint(root.path(), path).is_err(), "{path}");
        }
    }

    #[test]
    fn a_stale_dependency_check_does_not_overwrite_a_new_review() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().to_str().unwrap();
        std::fs::write(root.path().join("config"), "one").unwrap();
        let db = Db::in_memory().unwrap();
        let old = note(&db, ws, "Use the configured command.");
        let req = ReviewRequest { depends_on: vec!["config".into()], ..request(ws, &old) };
        let first = review(&db, &req, None, author(), 2).unwrap();
        std::fs::write(root.path().join("config"), "two").unwrap();
        let pending = changed(&db, ws).unwrap();
        let req = ReviewRequest { expected_rev: first.note.rev, ..req };
        let newest = review(&db, &req, None, author(), 3).unwrap();
        db.with(|c| apply(c, pending, "older_check", 4)).unwrap();
        let found = recall(&db, &query(ws), None, 5).unwrap();
        assert_eq!(found[0].note.rev, newest.note.rev);
        assert!(found[0].details.needs_review.is_empty());
    }

    #[test]
    fn concurrent_review_and_capture_keep_note_revisions_consistent() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().to_str().unwrap().to_owned();
        let db = Arc::new(Db::in_memory().unwrap());
        let old = note(&db, &ws, "Build the application.");
        let mut req = request(&ws, &old);
        req.reason = "2".into();
        let first = review(&db, &req, None, author(), 2).unwrap();
        let writer_db = db.clone();
        let writer = std::thread::spawn(move || {
            req.expected_rev = first.note.rev;
            for _ in 0..30 {
                req.reason = (req.expected_rev + 1).to_string();
                req.expected_rev = review(&writer_db, &req, None, author(), 3).unwrap().note.rev;
            }
        });
        for i in 0..30 {
            let q = RecallRequest { capture: Some("tool".into()), ..query(&ws) };
            let found = recall(&db, &q, Some(("parallel_run", &format!("call_{i}"))), 3).unwrap();
            assert_eq!(found[0].details.reason, found[0].note.rev.to_string());
        }
        writer.join().unwrap();
        for snapshot in snapshots(&db, &ws, "parallel_run").unwrap() {
            assert_eq!(snapshot.notes[0].details.reason, snapshot.notes[0].note.rev.to_string());
        }
    }
}

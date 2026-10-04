//! Bringing a model up to date with the files on disk.
//!
//! An update runs in three steps, and only the first and last hold the
//! database: load what the model knows about the files concerned; walk,
//! read, hash and parse outside the lock (in parallel); then write every
//! change in one transaction. Writes are upserts and deletes keyed by path,
//! so two updates of one workspace running at once cannot corrupt the
//! model: the later write wins, and any staleness it leaves is seen (and
//! fixed) by the next update, since what is stored is the size and mtime
//! observed *before* the contents were read.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use molt_api::memory::IndexResponse;
use molt_proto::RemoteError;
use rayon::prelude::*;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use super::lang::{self, Lang, Symbols};
use super::{project_id, project_key};
use crate::db::Db;
use crate::error::{self, invalid};

/// Most files a model holds. Past it, files are left out in walk order.
pub(crate) const MAX_FILES: usize = 20_000;
/// Larger files are left out: generated code, vendored bundles, data.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// A NUL byte in this many leading bytes marks a file as binary.
const SNIFF_BYTES: usize = 8 * 1024;
/// A file rewritten within one mtime tick of being read keeps its mtime, and
/// its size may not change either, so a row whose mtime is this close to (or
/// after) the moment the file was read proves nothing: the file is hashed
/// again until it has been read well after its last change. Two seconds
/// covers the coarsest common timestamps (FAT) and clock jitter.
const RACY_NS: i64 = 2_000_000_000;
/// Directories never part of a workspace, wherever they appear.
const SKIPPED_DIRS: [&str; 2] = [".git", ".molt"];

/// What the model holds about a file, as far as deciding whether to read it
/// again goes.
struct Stored {
    size: i64,
    mtime_ns: i64,
    /// When the contents were last read.
    verified_ns: i64,
    hash: String,
}

/// The part of a model an update starts from.
#[derive(Default)]
struct Model {
    /// Files in the model, including those not loaded into `stored`.
    files: usize,
    truncated: bool,
    /// By path: every file (whole workspace) or the named ones (`paths`).
    stored: HashMap<String, Stored>,
}

/// A source file on disk that may go into the model.
struct Candidate {
    rel: String,
    abs: PathBuf,
    lang: Lang,
    size: i64,
    mtime_ns: i64,
    dev: u64,
    ino: u64,
}

enum Outcome {
    /// Same size and mtime as a row that can be trusted: not read.
    Unchanged,
    /// Read again, but the contents are the same: only the row changes.
    Touched,
    /// New or changed contents.
    Parsed { hash: String, symbols: Symbols },
    /// Grew past the size limit, binary, or unreadable.
    Skipped,
    /// Removed or replaced between the walk and the read.
    Gone,
}

impl Outcome {
    fn in_model(&self) -> bool {
        matches!(self, Outcome::Unchanged | Outcome::Touched | Outcome::Parsed { .. })
    }
}

/// The changes an update writes.
#[derive(Default)]
struct Changes<'a> {
    touched: Vec<&'a Candidate>,
    parsed: Vec<(&'a Candidate, String, Symbols)>,
    removed: Vec<String>,
    truncated: bool,
}

/// See [`super::index`]. At most `max_files` files are kept in the model.
pub(crate) fn index(
    db: &Db,
    root: &Path,
    paths: Option<&[String]>,
    max_files: usize,
) -> Result<IndexResponse, RemoteError> {
    let clock = Instant::now();
    let started_ns = now_ns();
    let key = project_key(root)?;
    let named = paths.map(normalize).transpose()?;
    let model = db.with(|c| load(c, key, named.as_deref())).map_err(error::db)?;

    let mut skipped = 0u64;
    let mut candidates = match &named {
        None => walk(root, &mut skipped),
        Some(named) => look_at(root, named, &mut skipped),
    };
    // Room for the files examined now. A partial update keeps the files the
    // model already has before adding new ones; a full one takes them in
    // walk order, so the model of a tree does not depend on its history.
    let room = match &named {
        None => max_files,
        Some(_) => {
            candidates.sort_by_key(|c| !model.stored.contains_key(&c.rel));
            max_files.saturating_sub(model.files.saturating_sub(model.stored.len()))
        }
    };

    let mut outcomes: Vec<Outcome> = Vec::with_capacity(candidates.len());
    let mut kept = 0;
    // Files are taken a batch at a time so a binary file does not cost a
    // slot, without reading files far past the limit.
    while outcomes.len() < candidates.len() && kept < room {
        let batch = &candidates[outcomes.len()..candidates.len().min(outcomes.len() + room - kept)];
        let results: Vec<Outcome> = batch.par_iter().map(|c| examine(c, model.stored.get(&c.rel))).collect();
        for outcome in results {
            kept += usize::from(outcome.in_model());
            skipped += u64::from(matches!(outcome, Outcome::Skipped));
            outcomes.push(outcome);
        }
    }
    let past_limit = candidates.len() - outcomes.len();
    skipped += past_limit as u64;

    let mut changes =
        Changes { truncated: past_limit > 0 || (named.is_some() && model.truncated), ..Changes::default() };
    let mut in_model: HashSet<&str> = HashSet::with_capacity(kept);
    for (candidate, outcome) in candidates.iter().zip(outcomes) {
        if outcome.in_model() {
            in_model.insert(&candidate.rel);
        }
        match outcome {
            Outcome::Touched => changes.touched.push(candidate),
            Outcome::Parsed { hash, symbols } => changes.parsed.push((candidate, hash, symbols)),
            Outcome::Unchanged | Outcome::Skipped | Outcome::Gone => {}
        }
    }
    changes.removed = model.stored.into_keys().filter(|path| !in_model.contains(path.as_str())).collect();
    changes.removed.sort();

    let parsed = changes.parsed.len() as u64;
    let (files, symbols, removed) = db.with(|c| write(c, key, started_ns, &changes)).map_err(error::db)?;
    Ok(IndexResponse {
        files,
        parsed,
        removed,
        symbols,
        skipped,
        truncated: changes.truncated,
        ms: clock.elapsed().as_millis() as u64,
    })
}

/// `paths` as stored: relative, `/`-separated, without `.` components or
/// duplicates. `""` and `.` name the workspace itself, which is no file.
fn normalize(paths: &[String]) -> Result<Vec<String>, RemoteError> {
    let mut out = Vec::with_capacity(paths.len());
    let mut seen = HashSet::new();
    for path in paths {
        if path.contains('\0') {
            return Err(invalid("path contains a NUL byte"));
        }
        let mut parts = Vec::new();
        for part in Path::new(path).components() {
            match part {
                Component::Normal(name) => parts.push(name.to_str().unwrap_or_default()),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(invalid(format!("{path}: `..` is not allowed; name a path inside the workspace")))
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid(format!(
                        "{path}: absolute paths are not allowed; use a path relative to the workspace"
                    )))
                }
            }
        }
        let rel = parts.join("/");
        if !rel.is_empty() && seen.insert(rel.clone()) {
            out.push(rel);
        }
    }
    Ok(out)
}

/// What the model of `key` holds about every file, or about `named` ones.
fn load(conn: &mut Connection, key: &str, named: Option<&[String]>) -> rusqlite::Result<Model> {
    let Some(project) = project_id(conn, key)? else { return Ok(Model::default()) };
    let (files, truncated): (i64, bool) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM files WHERE project = ?1), truncated FROM projects WHERE id = ?1",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let row = |r: &rusqlite::Row| -> rusqlite::Result<(String, Stored)> {
        Ok((r.get(0)?, Stored { size: r.get(1)?, mtime_ns: r.get(2)?, verified_ns: r.get(3)?, hash: r.get(4)? }))
    };
    let mut stored = HashMap::new();
    match named {
        None => {
            let mut stmt =
                conn.prepare("SELECT path, size, mtime_ns, verified_ns, hash FROM files WHERE project = ?1")?;
            for item in stmt.query_map([project], row)? {
                let (path, file) = item?;
                stored.insert(path, file);
            }
        }
        Some(named) => {
            let mut stmt = conn.prepare(
                "SELECT path, size, mtime_ns, verified_ns, hash FROM files WHERE project = ?1 AND path = ?2",
            )?;
            for path in named {
                if let Some(item) = stmt.query_map(params![project, path], row)?.next() {
                    let (path, file) = item?;
                    stored.insert(path, file);
                }
            }
        }
    }
    Ok(Model { files: files as usize, truncated, stored })
}

/// Every source file of the workspace that may go into the model, in walk
/// order. Files over the size limit, and source files whose path is not
/// UTF-8 (it could not be stored), are counted in `skipped`.
fn walk(root: &Path, skipped: &mut u64) -> Vec<Candidate> {
    let mut out = Vec::new();
    for abs in molt_tools::workspace_files(root) {
        let Ok(rel) = abs.strip_prefix(root) else { continue };
        let Some(rel) = rel.to_str() else {
            *skipped += u64::from(Lang::of(&rel.to_string_lossy()).is_some());
            continue;
        };
        let Some(lang) = Lang::of(rel) else { continue };
        // Gone since the walk saw it, or replaced by something else.
        let Ok(meta) = fs::symlink_metadata(&abs) else { continue };
        if !meta.is_file() {
            continue;
        }
        let rel = rel.to_owned();
        match candidate(rel, abs, lang, &meta) {
            Some(c) => out.push(c),
            None => *skipped += 1,
        }
    }
    out
}

/// The `named` files that may go into the model. A path that is not a
/// source file in the workspace yields nothing, which drops it from the model.
fn look_at(root: &Path, named: &[String], skipped: &mut u64) -> Vec<Candidate> {
    let mut out = Vec::new();
    for rel in named {
        let Some(lang) = Lang::of(rel) else { continue };
        if rel.split('/').any(|part| SKIPPED_DIRS.contains(&part)) {
            continue;
        }
        let abs = root.join(rel);
        // The walk does not follow symlinks, so a file reached through a
        // symlinked directory is not part of the workspace (and may be
        // outside it). The root is canonical, so any symlink on the way
        // shows as a difference.
        let parent = abs.parent().unwrap_or(root);
        if parent.canonicalize().ok().as_deref() != Some(parent) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&abs) else { continue };
        if !meta.is_file() {
            continue;
        }
        match candidate(rel.clone(), abs, lang, &meta) {
            Some(c) => out.push(c),
            None => *skipped += 1,
        }
    }
    out
}

/// A candidate for the regular file `meta` describes, or `None` if it is too large.
fn candidate(rel: String, abs: PathBuf, lang: Lang, meta: &Metadata) -> Option<Candidate> {
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let mtime_ns = meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec());
    Some(Candidate { rel, abs, lang, size: meta.len() as i64, mtime_ns, dev: meta.dev(), ino: meta.ino() })
}

/// Decide what to do with one file, reading and parsing it if need be.
fn examine(file: &Candidate, stored: Option<&Stored>) -> Outcome {
    if let Some(s) = stored {
        let trusted = s.mtime_ns.saturating_add(RACY_NS) <= s.verified_ns;
        if trusted && s.size == file.size && s.mtime_ns == file.mtime_ns {
            return Outcome::Unchanged;
        }
    }
    let bytes = match read(file) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Outcome::Gone,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Outcome::Gone,
        Err(e) => {
            tracing::debug!("cannot read {}: {e}", file.rel);
            return Outcome::Skipped;
        }
    };
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return Outcome::Skipped;
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    if stored.is_some_and(|s| s.hash == hash) {
        return Outcome::Touched;
    }
    let symbols = lang::extract(file.lang, &bytes);
    Outcome::Parsed { hash, symbols }
}

/// The contents of `file`, up to one byte past the size limit, or `None` if
/// what is there now is not the file the walk saw (a symlink, say).
fn read(file: &Candidate) -> io::Result<Option<Vec<u8>>> {
    let handle = File::open(&file.abs)?;
    let meta = handle.metadata()?;
    if !meta.is_file() || meta.dev() != file.dev || meta.ino() != file.ino {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(file.size as usize + 1);
    handle.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Write `changes` to the model of `key` in one transaction. Returns the
/// files and definitions in the model afterwards, and the files removed.
fn write(conn: &mut Connection, key: &str, started_ns: i64, changes: &Changes) -> rusqlite::Result<(u64, u64, u64)> {
    let tx = conn.transaction()?;
    let project: i64 = tx.query_row(
        "INSERT INTO projects (root, indexed_ms, truncated) VALUES (?1, ?2, ?3)
         ON CONFLICT (root) DO UPDATE SET indexed_ms = excluded.indexed_ms, truncated = excluded.truncated
         RETURNING id",
        params![key, now_ns() / 1_000_000, changes.truncated],
        |r| r.get(0),
    )?;
    let mut removed = 0;
    {
        let mut remove = tx.prepare_cached("DELETE FROM files WHERE project = ?1 AND path = ?2")?;
        for path in &changes.removed {
            removed += remove.execute(params![project, path])? as u64;
        }
        let mut touch = tx.prepare_cached(
            "UPDATE files SET size = ?3, mtime_ns = ?4, verified_ns = ?5 WHERE project = ?1 AND path = ?2",
        )?;
        for file in &changes.touched {
            touch.execute(params![project, file.rel, file.size, file.mtime_ns, started_ns])?;
        }
        let mut upsert = tx.prepare_cached(
            "INSERT INTO files (project, path, size, mtime_ns, verified_ns, hash, lang)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (project, path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns,
                 verified_ns = excluded.verified_ns, hash = excluded.hash, lang = excluded.lang
             RETURNING id",
        )?;
        let mut clear_defs = tx.prepare_cached("DELETE FROM defs WHERE file = ?1")?;
        let mut clear_refs = tx.prepare_cached("DELETE FROM refs WHERE file = ?1")?;
        let mut add_def = tx.prepare_cached(
            "INSERT INTO defs (project, file, name, kind, line, signature) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut add_ref = tx.prepare_cached("INSERT INTO refs (project, file, name, line) VALUES (?1, ?2, ?3, ?4)")?;
        for (file, hash, symbols) in &changes.parsed {
            let id: i64 = upsert.query_row(
                params![project, file.rel, file.size, file.mtime_ns, started_ns, hash, file.lang.name()],
                |r| r.get(0),
            )?;
            clear_defs.execute([id])?;
            clear_refs.execute([id])?;
            for def in &symbols.defs {
                add_def.execute(params![project, id, def.name, def.kind, def.line, def.signature])?;
            }
            for r in &symbols.refs {
                add_ref.execute(params![project, id, r.name, r.line])?;
            }
        }
    }
    let (files, symbols): (i64, i64) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM files WHERE project = ?1), (SELECT COUNT(*) FROM defs WHERE project = ?1)",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    tx.commit()?;
    Ok((files as u64, symbols as u64, removed))
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_files(dir: &Path, names: &[&str]) {
        for name in names {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("fn f_{}() {{}}\n", name.replace(['/', '.'], "_"))).unwrap();
        }
    }

    #[test]
    fn past_the_file_limit_files_are_left_out_in_walk_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(&root, &["a.rs", "b.rs", "c.rs", "d/e.rs", "README.md"]);
        let db = Db::in_memory().unwrap();

        let r = index(&db, &root, None, 3).unwrap();
        assert_eq!((r.files, r.parsed, r.skipped, r.truncated), (3, 3, 1, true));
        let paths: Vec<String> = db.with(|c| {
            let mut stmt = c.prepare("SELECT path FROM files ORDER BY path").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        });
        assert_eq!(paths, ["a.rs", "b.rs", "c.rs"]);

        // A new file named explicitly finds the model full; files already
        // in it keep their place.
        write_files(&root, &["0.rs"]);
        let named = ["0.rs".to_owned(), "a.rs".to_owned()];
        let r = index(&db, &root, Some(&named), 3).unwrap();
        assert_eq!((r.files, r.skipped, r.truncated), (3, 1, true));

        // Once there is room, the model fills up and is no longer truncated.
        let r = index(&db, &root, None, 10).unwrap();
        assert_eq!((r.files, r.parsed, r.skipped, r.truncated), (5, 2, 0, false));
        let r = index(&db, &root, Some(&named), 10).unwrap();
        assert!(!r.truncated);
    }

    #[test]
    fn a_binary_file_does_not_take_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.rs"), b"fn a() {}\0").unwrap();
        write_files(&root, &["b.rs", "c.rs"]);
        let db = Db::in_memory().unwrap();
        let r = index(&db, &root, None, 2).unwrap();
        assert_eq!((r.files, r.skipped, r.truncated), (2, 1, false));
    }

    #[test]
    fn paths_are_normalized_and_checked() {
        let named = |p: &[&str]| normalize(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(named(&["./a//b.rs", "a/b.rs", ".", ""]).unwrap(), ["a/b.rs"]);
        assert!(named(&["a/../b.rs"]).unwrap_err().message.contains("`..`"));
        assert!(named(&["/etc/passwd"]).unwrap_err().message.contains("absolute"));
        assert!(named(&["a\0b"]).is_err());
    }
}

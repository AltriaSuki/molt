//! Bringing a model up to date with the files on disk.
//!
//! An update loads what the model knows about the files concerned, then
//! takes them a batch at a time: it reads, hashes and parses a batch outside
//! the lock (in parallel) and writes the batch's changes in transactions of
//! bounded size. The database is held only briefly, at most one batch of
//! symbols is in memory, and an update that is stopped keeps what it did. Files that are
//! gone are dropped at the end. Writes are upserts and deletes keyed by
//! path, so two updates of one workspace running at once cannot corrupt the
//! model: the later write wins, and any staleness it leaves is seen (and
//! fixed) by the next update, since what is stored is the size and mtime
//! observed *before* the contents were read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use molt_api::memory::IndexResponse;
use molt_proto::RemoteError;
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
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
pub(super) const SKIPPED_DIRS: [&str; 2] = [".git", ".molt"];
/// Files examined at a time.
const BATCH: usize = 256;
/// Most rows one transaction writes (about), so that a write holds the
/// database briefly however many symbols a batch of files has.
const WRITE_ROWS: usize = 20_000;
/// A walk keeps this many times the room in the model, in case some files
/// turn out binary; past that, source files are only counted.
const WALK_SLACK: usize = 2;

/// What the model holds about a file, as far as deciding whether to read it
/// again goes.
struct Stored {
    size: i64,
    mtime_ns: i64,
    /// When the contents were last read.
    verified_ns: i64,
    hash: String,
    /// What parsed it (see [`Lang::fingerprint`]).
    extractor: String,
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

/// A file's size and mtime, as it was when read.
#[derive(Clone, Copy)]
struct Seen {
    size: i64,
    mtime_ns: i64,
}

impl Seen {
    fn of(meta: &Metadata) -> Self {
        let mtime_ns = meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec());
        Self { size: i64::try_from(meta.len()).unwrap_or(i64::MAX), mtime_ns }
    }
}

enum Outcome {
    /// Same size and mtime as a row that can be trusted: not read.
    Unchanged,
    /// Read again, but the contents are the same: only the row changes.
    Touched(Seen),
    /// New or changed contents, or parsed by an older extractor.
    Parsed { seen: Seen, hash: String, symbols: Symbols },
    /// Grew past the size limit, binary, or unreadable.
    Skipped,
    /// Removed, or no longer a regular file reached without a symlink.
    Gone,
}

impl Outcome {
    fn in_model(&self) -> bool {
        matches!(self, Outcome::Unchanged | Outcome::Touched(_) | Outcome::Parsed { .. })
    }
}

/// Changes written in one transaction.
#[derive(Default)]
struct Changes<'a> {
    touched: Vec<(&'a Candidate, Seen)>,
    parsed: Vec<(&'a Candidate, Seen, String, Symbols)>,
}

/// See [`super::index_skipping`]. At most `max_files` files are kept in the model.
pub(crate) fn index(
    db: &Db,
    root: &Path,
    paths: Option<&[String]>,
    skip: &[PathBuf],
    max_files: usize,
) -> Result<IndexResponse, RemoteError> {
    let clock = Instant::now();
    let started_ns = now_ns();
    let key = project_key(root)?;
    let named = paths.map(normalize).transpose()?;
    let model = db.with(|c| load(c, key, named.as_deref())).map_err(error::db)?;

    let left_out = left_out(skip);
    let mut skipped = 0u64;
    let (mut candidates, overflow) = match &named {
        None => walk(root, left_out, max_files.saturating_mul(WALK_SLACK), &mut skipped),
        Some(named) => (look_at(root, named, left_out, &mut skipped), 0),
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

    let (mut next, mut kept, mut parsed) = (0, 0, 0u64);
    let mut in_model: HashSet<&str> = HashSet::new();
    // A batch never holds more files than there is room for, so a binary
    // file does not cost a slot, and no file far past the limit is read.
    while next < candidates.len() && kept < room {
        let batch = &candidates[next..candidates.len().min(next + (room - kept).min(BATCH))];
        next += batch.len();
        let outcomes: Vec<Outcome> = batch.par_iter().map(|c| examine(c, model.stored.get(&c.rel))).collect();
        let mut changes = Changes::default();
        for (candidate, outcome) in batch.iter().zip(outcomes) {
            if outcome.in_model() {
                kept += 1;
                in_model.insert(&candidate.rel);
            }
            match outcome {
                Outcome::Touched(seen) => changes.touched.push((candidate, seen)),
                Outcome::Parsed { seen, hash, symbols } => changes.parsed.push((candidate, seen, hash, symbols)),
                Outcome::Skipped => skipped += 1,
                Outcome::Unchanged | Outcome::Gone => {}
            }
        }
        parsed += changes.parsed.len() as u64;
        let mut chunk = Changes { touched: changes.touched, parsed: Vec::new() };
        let mut rows = chunk.touched.len();
        for file in changes.parsed {
            rows += 1 + file.3.defs.len() + file.3.refs.len();
            chunk.parsed.push(file);
            if rows >= WRITE_ROWS {
                db.with(|c| write(c, key, started_ns, &chunk)).map_err(error::db)?;
                (chunk, rows) = (Changes::default(), 0);
            }
        }
        if !chunk.touched.is_empty() || !chunk.parsed.is_empty() {
            db.with(|c| write(c, key, started_ns, &chunk)).map_err(error::db)?;
        }
    }
    let past_limit = (candidates.len() - next) as u64 + overflow;
    skipped += past_limit;
    let truncated = past_limit > 0 || (named.is_some() && model.truncated);

    let mut gone: Vec<&str> = model.stored.keys().map(String::as_str).filter(|path| !in_model.contains(path)).collect();
    gone.sort_unstable();
    let mut removed = 0;
    for chunk in gone.chunks(BATCH) {
        removed += db.with(|c| remove(c, key, chunk)).map_err(error::db)?;
    }
    let (files, symbols) = db.with(|c| finish(c, key, truncated)).map_err(error::db)?;
    Ok(IndexResponse { files, parsed, removed, symbols, skipped, truncated, ms: clock.elapsed().as_millis() as u64 })
}

/// Directories a model leaves out besides those the walk rules do: code
/// that belongs to other projects or to a build (`node_modules`, a Python
/// virtualenv, a cache directory such as Cargo's `target`, which carries a
/// `CACHEDIR.TAG`), and `skip`. Such directories are often not in a
/// project's `.gitignore`, and would fill the model before its own code.
fn left_out(skip: &[PathBuf]) -> impl Fn(&Path) -> bool + Clone + Send + Sync + 'static {
    let skip: Arc<[PathBuf]> = skip.into();
    move |dir: &Path| {
        let has = |name: &str| fs::symlink_metadata(dir.join(name)).is_ok();
        dir.file_name().is_some_and(|n| n == "node_modules")
            || has("pyvenv.cfg")
            || has("CACHEDIR.TAG")
            || skip.iter().any(|s| s == dir)
    }
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
        let stored = Stored {
            size: r.get(1)?,
            mtime_ns: r.get(2)?,
            verified_ns: r.get(3)?,
            hash: r.get(4)?,
            extractor: r.get(5)?,
        };
        Ok((r.get(0)?, stored))
    };
    const COLUMNS: &str = "path, size, mtime_ns, verified_ns, hash, extractor";
    let mut stored = HashMap::new();
    match named {
        None => {
            let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM files WHERE project = ?1"))?;
            for item in stmt.query_map([project], row)? {
                let (path, file) = item?;
                stored.insert(path, file);
            }
        }
        Some(named) => {
            let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM files WHERE project = ?1 AND path = ?2"))?;
            for path in named {
                if let Some((path, file)) = stmt.query_row(params![project, path], row).optional()? {
                    stored.insert(path, file);
                }
            }
        }
    }
    Ok(Model { files: files as usize, truncated, stored })
}

/// The first `limit` source files of the workspace that may go into the
/// model, in walk order, and how many more there are. Files over the size
/// limit, and source files whose path is not UTF-8 (it could not be
/// stored), are counted in `skipped`.
fn walk(
    root: &Path,
    left_out: impl Fn(&Path) -> bool + Clone + Send + Sync + 'static,
    limit: usize,
    skipped: &mut u64,
) -> (Vec<Candidate>, u64) {
    let (mut out, mut more) = (Vec::new(), 0);
    for abs in molt_tools::workspace_files(root, left_out) {
        let Ok(rel) = abs.strip_prefix(root) else { continue };
        let Some(rel) = rel.to_str() else {
            *skipped += u64::from(Lang::of(&rel.to_string_lossy()).is_some());
            continue;
        };
        let Some(lang) = Lang::of(rel) else { continue };
        if out.len() >= limit {
            more += 1;
            continue;
        }
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
    (out, more)
}

/// The `named` files that may go into the model. A path that is not a
/// source file in the workspace yields nothing, which drops it from the model.
fn look_at(
    root: &Path,
    named: &[String],
    left_out: impl Fn(&Path) -> bool + Clone + Send + Sync + 'static,
    skipped: &mut u64,
) -> Vec<Candidate> {
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
        if !no_symlink_to(&abs) {
            continue;
        }
        // An ignored or left-out file is not in the model, whichever way it is reached.
        if !molt_tools::is_workspace_file(root, &abs, left_out.clone()) {
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

/// True when the directory holding `abs` (under a canonical root) is
/// reached without a symlink.
fn no_symlink_to(abs: &Path) -> bool {
    abs.parent().is_some_and(|parent| parent.canonicalize().ok().as_deref() == Some(parent))
}

/// A candidate for the regular file `meta` describes, or `None` if it is too large.
fn candidate(rel: String, abs: PathBuf, lang: Lang, meta: &Metadata) -> Option<Candidate> {
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let Seen { size, mtime_ns } = Seen::of(meta);
    Some(Candidate { rel, abs, lang, size, mtime_ns, dev: meta.dev(), ino: meta.ino() })
}

/// Decide what to do with one file, reading and parsing it if need be. A
/// file parsed by another extractor (an older grammar or query) is parsed
/// again whatever its contents.
fn examine(file: &Candidate, stored: Option<&Stored>) -> Outcome {
    let extractor = file.lang.fingerprint();
    let stored = stored.filter(|s| extractor == Some(s.extractor.as_str()));
    if let Some(s) = stored {
        let trusted = s.mtime_ns.saturating_add(RACY_NS) <= s.verified_ns;
        if trusted && s.size == file.size && s.mtime_ns == file.mtime_ns {
            return Outcome::Unchanged;
        }
    }
    let (bytes, seen) = match read(file) {
        Ok(Some(read)) => read,
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
        return Outcome::Touched(seen);
    }
    let symbols = lang::extract(file.lang, &bytes);
    Outcome::Parsed { seen, hash, symbols }
}

/// The contents of `file`, up to one byte past the size limit, with its
/// size and mtime as read; `None` if no regular file is there now. A
/// symlink put in its place is not followed and a FIFO is not waited on. A
/// file replaced since the walk (an editor saving by renaming over it) is
/// read as it is now, as long as no symlink leads to it.
fn read(file: &Candidate) -> io::Result<Option<(Vec<u8>, Seen)>> {
    let handle = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&file.abs) {
        Ok(handle) => handle,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = handle.metadata()?;
    if !meta.is_file() {
        return Ok(None);
    }
    if (meta.dev(), meta.ino()) != (file.dev, file.ino) && !no_symlink_to(&file.abs) {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(meta.len().min(MAX_FILE_BYTES) as usize + 1);
    handle.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(Some((bytes, Seen::of(&meta))))
}

/// The id of the model of `key`, made if it has none.
fn project(tx: &Transaction, key: &str) -> rusqlite::Result<i64> {
    tx.query_row(
        "INSERT INTO projects (root) VALUES (?1) ON CONFLICT (root) DO UPDATE SET root = excluded.root RETURNING id",
        [key],
        |r| r.get(0),
    )
}

/// Write one batch of changes to the model of `key`, in one transaction.
fn write(conn: &mut Connection, key: &str, started_ns: i64, changes: &Changes) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let project = project(&tx, key)?;
    {
        let mut touch = tx.prepare_cached(
            "UPDATE files SET size = ?3, mtime_ns = ?4, verified_ns = ?5 WHERE project = ?1 AND path = ?2",
        )?;
        for (file, seen) in &changes.touched {
            touch.execute(params![project, file.rel, seen.size, seen.mtime_ns, started_ns])?;
        }
        let mut upsert = tx.prepare_cached(
            "INSERT INTO files (project, path, size, mtime_ns, verified_ns, hash, lang, extractor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (project, path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns,
                 verified_ns = excluded.verified_ns, hash = excluded.hash, lang = excluded.lang,
                 extractor = excluded.extractor
             RETURNING id",
        )?;
        let mut clear_defs = tx.prepare_cached("DELETE FROM defs WHERE file = ?1")?;
        let mut clear_refs = tx.prepare_cached("DELETE FROM refs WHERE file = ?1")?;
        let mut add_def = tx.prepare_cached(
            "INSERT INTO defs (project, file, name, kind, line, signature) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut add_ref =
            tx.prepare_cached("INSERT INTO refs (project, file, name, uses, lines) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for (file, seen, hash, symbols) in &changes.parsed {
            let extractor = file.lang.fingerprint().unwrap_or_default();
            let id: i64 = upsert.query_row(
                params![project, file.rel, seen.size, seen.mtime_ns, started_ns, hash, file.lang.name(), extractor],
                |r| r.get(0),
            )?;
            clear_defs.execute([id])?;
            clear_refs.execute([id])?;
            for def in &symbols.defs {
                add_def.execute(params![project, id, def.name, def.kind, def.line, def.signature])?;
            }
            let mut uses: BTreeMap<&str, (u32, Vec<u32>)> = BTreeMap::new();
            for r in &symbols.refs {
                let (count, lines) = uses.entry(&r.name).or_default();
                *count += 1;
                lines.push(r.line);
            }
            for (name, (count, mut lines)) in uses {
                lines.sort_unstable();
                lines.dedup();
                add_ref.execute(params![project, id, name, count, super::encode_lines(&lines)])?;
            }
        }
    }
    tx.commit()
}

/// Drop `paths` from the model of `key`, with their symbols. Returns how many were there.
fn remove(conn: &mut Connection, key: &str, paths: &[&str]) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    let project = project(&tx, key)?;
    let mut removed = 0;
    {
        let mut remove = tx.prepare_cached("DELETE FROM files WHERE project = ?1 AND path = ?2")?;
        for path in paths {
            removed += remove.execute(params![project, path])? as u64;
        }
    }
    tx.commit()?;
    Ok(removed)
}

/// Record that the model of `key` was brought up to date. Returns the files
/// and definitions in it.
fn finish(conn: &mut Connection, key: &str, truncated: bool) -> rusqlite::Result<(u64, u64)> {
    let tx = conn.transaction()?;
    let project = project(&tx, key)?;
    tx.execute(
        "UPDATE projects SET indexed_ms = ?2, truncated = ?3 WHERE id = ?1",
        params![project, now_ns() / 1_000_000, truncated],
    )?;
    let (files, symbols): (i64, i64) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM files WHERE project = ?1), (SELECT COUNT(*) FROM defs WHERE project = ?1)",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    tx.commit()?;
    Ok((files as u64, symbols as u64))
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

        let r = index(&db, &root, None, &[], 3).unwrap();
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
        let r = index(&db, &root, Some(&named), &[], 3).unwrap();
        assert_eq!((r.files, r.skipped, r.truncated), (3, 1, true));

        // Once there is room, the model fills up and is no longer truncated.
        let r = index(&db, &root, None, &[], 10).unwrap();
        assert_eq!((r.files, r.parsed, r.skipped, r.truncated), (5, 2, 0, false));
        let r = index(&db, &root, Some(&named), &[], 10).unwrap();
        assert!(!r.truncated);
    }

    #[test]
    fn a_binary_file_does_not_take_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.rs"), b"fn a() {}\0").unwrap();
        write_files(&root, &["b.rs", "c.rs"]);
        let db = Db::in_memory().unwrap();
        let r = index(&db, &root, None, &[], 2).unwrap();
        assert_eq!((r.files, r.skipped, r.truncated), (2, 1, false));
    }

    #[test]
    fn a_file_replaced_since_the_walk_is_read_as_it_is_now_unless_it_is_no_longer_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_files(&root, &["a.rs", "b.rs"]);
        let abs = root.join("a.rs");
        let seen =
            |abs: &Path| candidate("a.rs".into(), abs.to_owned(), Lang::Rust, &fs::metadata(abs).unwrap()).unwrap();
        let file = seen(&abs);
        // Saved by an editor that writes a new file and renames it over the old.
        fs::write(root.join("a.rs.tmp"), "fn saved() {}\n").unwrap();
        fs::rename(root.join("a.rs.tmp"), &abs).unwrap();
        match examine(&file, None) {
            Outcome::Parsed { symbols, seen, .. } => {
                assert_eq!(symbols.defs[0].name, "saved");
                assert_eq!(seen.size, 14);
            }
            _ => panic!("the file was not read"),
        }
        // A symlink in its place is not followed, even to a file in the workspace.
        let file = seen(&abs);
        fs::remove_file(&abs).unwrap();
        std::os::unix::fs::symlink(root.join("b.rs"), &abs).unwrap();
        assert!(matches!(examine(&file, None), Outcome::Gone));
        // Nor is a FIFO waited on.
        fs::remove_file(&abs).unwrap();
        let fifo = std::ffi::CString::new(abs.clone().into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        assert!(matches!(examine(&file, None), Outcome::Gone));
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

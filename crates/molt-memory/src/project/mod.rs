//! The project model: the symbol and dependency graph of a workspace.
//!
//! Each workspace (keyed by its canonical path) has a row per source file in
//! a language Molt can parse, with that file's definitions and references.
//! [`index`] keeps it in step with the disk, reading only files whose size or
//! mtime changed and parsing only those whose contents did; [`map`] ranks the
//! files by how the code references itself (PageRank over the reference
//! graph, personalized by what a query mentions) and renders the best
//! definitions within a token budget; [`symbols`] answers where a name is
//! defined and used.
//!
//! The model is a cache of what the files say, never a source of truth: any
//! row can be rebuilt by indexing again, so it is written for speed (one
//! transaction per update) rather than kept forever.

use std::path::{Path, PathBuf};

use molt_api::memory::{IndexResponse, MapRequest, MapResponse, SymbolsRequest, SymbolsResponse};
use molt_proto::RemoteError;

use crate::db::Db;
use crate::error::invalid;

mod lang;
mod lookup;
mod repo_map;
mod update;

/// The project tables. A file's definitions and references go with it
/// (`ON DELETE CASCADE`, which `Db` turns on); `project` is repeated in them
/// so a name is looked up within one project through one index. A file's
/// references to one name are one row: how often it is used, and the lines
/// it is used on (ascending, as little-endian `u32`s), which keeps the table
/// a fraction of a row per use. The name indexes cover what the map reads,
/// so it reads no table rows. Definitions can also be found ignoring case
/// through an index; references are searched that way only after an exact
/// miss, and an index for it would cost more to keep up than it saves.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    indexed_ms INTEGER NOT NULL DEFAULT 0,
    truncated INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    project INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    verified_ns INTEGER NOT NULL,
    hash TEXT NOT NULL,
    lang TEXT NOT NULL,
    extractor TEXT NOT NULL,
    UNIQUE (project, path)
);
CREATE TABLE IF NOT EXISTS defs (
    project INTEGER NOT NULL,
    file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    signature TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS defs_name ON defs(project, name, file, line);
CREATE INDEX IF NOT EXISTS defs_name_nocase ON defs(project, name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS defs_file ON defs(file);
CREATE TABLE IF NOT EXISTS refs (
    project INTEGER NOT NULL,
    file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    uses INTEGER NOT NULL,
    lines BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS refs_name ON refs(project, name, file, uses);
CREATE INDEX IF NOT EXISTS refs_file ON refs(file);
";

/// Bring the model of the canonical workspace directory `root` up to date:
/// all of it, or only `paths` (workspace-relative, `/`-separated).
///
/// The whole workspace is walked by the same rules as the `fs` service
/// (`.gitignore` applies; `.git` and `.molt` are skipped; symlinks are not
/// followed), leaving out other projects' code and build output as well:
/// `node_modules`, Python virtualenvs and cache directories (those with a
/// `CACHEDIR.TAG`, such as Cargo's `target`). Files that are no longer
/// found are dropped. With `paths`, only those are looked at: each is
/// indexed again if the walk would reach it, and dropped otherwise.
pub fn index(db: &Db, root: &Path, paths: Option<&[String]>) -> Result<IndexResponse, RemoteError> {
    index_skipping(db, root, paths, &[])
}

/// [`index`], leaving out the directories `skip` too (absolute, under the
/// canonical `root`): Molt's data directory, with the forks in it, when it
/// is inside the workspace.
pub fn index_skipping(
    db: &Db,
    root: &Path,
    paths: Option<&[String]>,
    skip: &[PathBuf],
) -> Result<IndexResponse, RemoteError> {
    update::index(db, root, paths, skip, update::MAX_FILES)
}

/// The map of `root` for `req.query` (`req.workspace` is ignored: `root` is it, resolved).
///
/// `max_tokens` is clamped to 1..=32000 (default 4000); a budget too small
/// for one definition yields an empty map. A workspace never indexed has an
/// empty map too.
pub fn map(db: &Db, root: &Path, req: &MapRequest) -> Result<MapResponse, RemoteError> {
    repo_map::map(db, root, req)
}

/// Definitions and references of `req.name` in `root`.
pub fn symbols(db: &Db, root: &Path, req: &SymbolsRequest) -> Result<SymbolsResponse, RemoteError> {
    lookup::symbols(db, root, req)
}

/// The key a workspace's model is stored under.
fn project_key(root: &Path) -> Result<&str, RemoteError> {
    root.to_str().ok_or_else(|| invalid(format!("workspace {} is not valid UTF-8", root.display())))
}

/// The id of the model of the workspace `key`, if it was ever indexed.
fn project_id(conn: &rusqlite::Connection, key: &str) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT id FROM projects WHERE root = ?1", [key], |r| r.get(0)).optional()
}

/// The lines of a file's references to one name, as stored.
fn encode_lines(lines: &[u32]) -> Vec<u8> {
    lines.iter().flat_map(|l| l.to_le_bytes()).collect()
}

/// The lines [`encode_lines`] stored.
fn decode_lines(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b))
}

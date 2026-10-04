//! Where a name is defined and used.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use molt_api::memory::{Definition, Reference, SymbolsRequest, SymbolsResponse};
use molt_proto::RemoteError;
use rusqlite::{params, Connection};

use super::{project_id, project_key};
use crate::db::Db;
use crate::error::{self, invalid};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
/// Longest reference text returned, in characters.
const MAX_TEXT_CHARS: usize = 200;
/// Most of a file read to show reference lines. Indexed files are at most
/// 1 MiB, but one may have grown since.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// See [`super::symbols`].
pub(crate) fn symbols(db: &Db, root: &Path, req: &SymbolsRequest) -> Result<SymbolsResponse, RemoteError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(invalid("name is empty; give the identifier to look up"));
    }
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let key = project_key(root)?;
    let mut found = db
        .read(|c| {
            let Some(project) = project_id(c, key)? else { return Ok(Found::default()) };
            let exact = find(c, project, name, false, req.references, limit + 1)?;
            if exact.definitions.is_empty() && exact.lines.is_empty() {
                find(c, project, name, true, req.references, limit + 1)
            } else {
                Ok(exact)
            }
        })
        .map_err(error::db)?;
    let truncated = found.definitions.len() > limit || found.lines.len() > limit;
    found.definitions.truncate(limit);
    found.lines.truncate(limit);
    let references = texts(root, found.lines);
    Ok(SymbolsResponse { definitions: found.definitions, references, truncated })
}

/// What the model has on a name.
#[derive(Default)]
struct Found {
    definitions: Vec<Definition>,
    /// Lines referencing it: path and line, ordered.
    lines: Vec<(String, u64)>,
}

/// Up to `limit` definitions of `name`, and as many lines that reference it
/// if `references`.
fn find(
    conn: &Connection,
    project: i64,
    name: &str,
    nocase: bool,
    references: bool,
    limit: usize,
) -> rusqlite::Result<Found> {
    let collate = if nocase { "COLLATE NOCASE" } else { "" };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT f.path, d.line, d.kind, d.name, d.signature FROM defs d JOIN files f ON f.id = d.file
         WHERE d.project = ?1 AND d.name = ?2 {collate} ORDER BY f.path, d.line, d.name LIMIT ?3"
    ))?;
    let definitions = stmt
        .query_map(params![project, name, limit as i64], |r| {
            let signature: String = r.get(4)?;
            Ok(Definition {
                path: r.get(0)?,
                line: r.get::<_, i64>(1)? as u64,
                kind: r.get(2)?,
                name: r.get(3)?,
                signature: signature.trim().to_owned(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !references {
        return Ok(Found { definitions, lines: Vec::new() });
    }
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT f.path, r.lines FROM refs r JOIN files f ON f.id = r.file
         WHERE r.project = ?1 AND r.name = ?2 {collate} ORDER BY f.path, r.name"
    ))?;
    let mut rows = stmt.query(params![project, name])?;
    let mut lines = Vec::new();
    // Ignoring case, a file may use several spellings of the name: its
    // lines are merged before they are counted.
    let mut file: Option<(String, Vec<u32>)> = None;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        if file.as_ref().is_some_and(|(p, _)| *p != path) {
            add_lines(&mut lines, file.take());
            if lines.len() >= limit {
                break;
            }
        }
        let (_, at) = file.get_or_insert_with(|| (path, Vec::new()));
        at.extend(super::decode_lines(row.get_ref(1)?.as_blob()?));
    }
    add_lines(&mut lines, file);
    lines.truncate(limit);
    Ok(Found { definitions, lines })
}

/// Add one file's reference lines to `lines`, in order, each once.
fn add_lines(lines: &mut Vec<(String, u64)>, file: Option<(String, Vec<u32>)>) {
    let Some((path, mut at)) = file else { return };
    at.sort_unstable();
    at.dedup();
    lines.extend(at.into_iter().map(|line| (path.clone(), u64::from(line))));
}

/// The references at `lines` (ordered by path), each with its line as the
/// file has it now. Each file is read once; a line that no longer exists,
/// or a file that cannot be read, gives an empty text.
fn texts(root: &Path, lines: Vec<(String, u64)>) -> Vec<Reference> {
    let mut out: Vec<Reference> = Vec::with_capacity(lines.len());
    let mut start = 0;
    while start < lines.len() {
        let path = &lines[start].0;
        let end = start + lines[start..].iter().take_while(|(p, _)| p == path).count();
        let contents = read(root, path).unwrap_or_default();
        let mut file_lines = contents.split(|&b| b == b'\n');
        let mut at = 0;
        for (path, line) in &lines[start..end] {
            // `lines` are ascending within a file, so one pass suffices.
            let text = file_lines.nth((*line).saturating_sub(at + 1) as usize).map(text).unwrap_or_default();
            at = *line;
            out.push(Reference { path: path.clone(), line: *line, text });
        }
        start = end;
    }
    out
}

/// The contents of the workspace file `rel`, if it is a regular file
/// reached from the root without a symlink, and not inside `.git` or
/// `.molt` (a row can outlive its file, and something else can take its
/// place). A FIFO is not waited on.
fn read(root: &Path, rel: &str) -> Option<Vec<u8>> {
    if rel.split('/').any(|part| super::update::SKIPPED_DIRS.contains(&part)) {
        return None;
    }
    let path = root.join(rel);
    if path.parent()?.canonicalize().ok()? != path.parent()? {
        return None;
    }
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// A line as a reference's text: trimmed and cut to [`MAX_TEXT_CHARS`].
fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line).trim().chars().take(MAX_TEXT_CHARS).collect()
}

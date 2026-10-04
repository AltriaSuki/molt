//! `fs.read`, `fs.write`, `fs.edit`, `fs.list` and `fs.search`.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use molt_api::fs::{
    EditRequest, EditResponse, Entry, EntryKind, FilesChanged, ListRequest, ListResponse, Match, ReadRequest,
    ReadResponse, SearchRequest, SearchResponse, WriteRequest, WriteResponse,
};
use molt_proto::RemoteError;
use regex::RegexBuilder;

use crate::error::invalid;
use crate::paths::{self, shown};
use crate::{walk, Roots};

const DEFAULT_READ_LINES: u64 = 2000;
const MAX_READ_LINES: u64 = 10_000;
const MAX_READ_BYTES: usize = 256 * 1024;
pub(crate) const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_LIST_DEPTH: u32 = 2;
const MAX_LIST_DEPTH: u32 = 10;
const MAX_LIST_ENTRIES: usize = 2000;
const DEFAULT_SEARCH_RESULTS: u32 = 200;
const MAX_SEARCH_RESULTS: u32 = 1000;
const MAX_SEARCH_FILE: u64 = 4 * 1024 * 1024;
const MAX_MATCH_CHARS: usize = 500;
const REGEX_SIZE_LIMIT: usize = 4 * 1024 * 1024;
/// A NUL byte in this many leading bytes marks a file as binary.
pub(crate) const SNIFF_BYTES: usize = 8 * 1024;

pub(crate) fn read(roots: &Roots, req: ReadRequest) -> Result<ReadResponse, RemoteError> {
    let ws = roots.workspace(&req.workspace)?;
    let name = shown(&req.path);
    let path = paths::existing(&ws, &paths::relative(&req.path)?, name)?;
    let io = |e| crate::error::io(name, e);
    if path.is_dir() {
        return Err(invalid(format!("{name} is a directory; use fs.list")));
    }
    if !path.is_file() {
        return Err(invalid(format!("{name} is not a regular file")));
    }
    let offset = req.offset.unwrap_or(1).max(1);
    let limit = req.limit.unwrap_or(DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES);
    let binary = || invalid(format!("{name} is a binary file"));

    let mut reader = BufReader::with_capacity(64 * 1024, File::open(&path).map_err(io)?);
    let head = reader.fill_buf().map_err(io)?;
    if head[..head.len().min(SNIFF_BYTES)].contains(&0) {
        return Err(binary());
    }
    let (mut content, mut lines, mut total) = (String::new(), 0u64, 0u64);
    let (mut full, mut cut) = (false, false);
    let mut buf = Vec::new();
    // Lines outside the window are skipped without being kept, so a huge
    // file costs time but not memory.
    loop {
        let n = if total + 1 >= offset && !full {
            let room = MAX_READ_BYTES - content.len();
            buf.clear();
            let n = (&mut reader).take(room as u64 + 1).read_until(b'\n', &mut buf).map_err(io)?;
            if n == 0 {
                break;
            }
            if buf.len() <= room {
                content.push_str(text(&buf, false).ok_or_else(binary)?);
                lines += 1;
                full = lines == limit;
            } else {
                // A line longer than what is left. Only a first line is cut,
                // so a window always makes progress.
                if lines == 0 {
                    content.push_str(text(&buf[..room], true).ok_or_else(binary)?);
                    lines = 1;
                    cut = true;
                }
                full = true;
                if buf.last() != Some(&b'\n') {
                    reader.skip_until(b'\n').map_err(io)?;
                }
            }
            n
        } else {
            reader.skip_until(b'\n').map_err(io)?
        };
        if n == 0 {
            break;
        }
        total += 1;
    }
    let truncated = cut || offset - 1 + lines < total;
    Ok(ReadResponse { content, first_line: offset, lines, total_lines: total, truncated })
}

/// `bytes` as text, or `None` for binary data. A `prefix` may end in the
/// middle of a character, which is dropped.
fn text(bytes: &[u8], prefix: bool) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(e) if prefix && e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

pub(crate) fn write(roots: &Roots, req: WriteRequest) -> Result<WriteResponse, RemoteError> {
    if req.content.len() > MAX_WRITE_BYTES {
        return Err(invalid(format!("content is {} bytes; the limit is {MAX_WRITE_BYTES}", req.content.len())));
    }
    let ws = roots.workspace(&req.workspace)?;
    let name = shown(&req.path);
    let dest = paths::writable(&ws, &paths::relative(&req.path)?, name)?;
    let io = |e| crate::error::io(name, e);
    let mode = match fs::metadata(&dest) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io)?;
        // The missing directories were just created; make sure nothing
        // swapped one for a link out in the meantime.
        paths::confine(&ws, &parent.canonicalize().map_err(io)?, name)?;
    }
    let created = mode.is_none();
    atomic_write(&dest, req.content.as_bytes(), mode).map_err(io)?;
    Ok(WriteResponse { bytes: req.content.len() as u64, created })
}

/// The file a successful write or edit of `path` in `workspace` changed,
/// unless the workspace is a fork (or anything else in scratch).
pub(crate) fn changed(roots: &Roots, workspace: &str, path: &str) -> Option<FilesChanged> {
    let ws = roots.workspace(workspace).ok()?;
    if ws.starts_with(&roots.scratch) {
        return None;
    }
    let file = paths::existing(&ws, &paths::relative(path).ok()?, path).ok()?;
    Some(FilesChanged { workspace: ws.to_string_lossy().into_owned(), paths: vec![paths::slash(&ws, &file)] })
}

pub(crate) fn edit(roots: &Roots, req: EditRequest) -> Result<EditResponse, RemoteError> {
    if req.old.is_empty() {
        return Err(invalid("old is empty; give the exact text to replace"));
    }
    if req.old == req.new {
        return Err(invalid("old and new are the same; nothing would change"));
    }
    let ws = roots.workspace(&req.workspace)?;
    let name = shown(&req.path);
    let path = paths::writable(&ws, &paths::relative(&req.path)?, name)?;
    let io = |e| crate::error::io(name, e);
    let meta = fs::metadata(&path).map_err(io)?;
    if !meta.is_file() {
        return Err(invalid(format!("{name} is not a regular file")));
    }
    if meta.len() > MAX_WRITE_BYTES as u64 {
        return Err(invalid(format!("{name} is {} bytes; files over {MAX_WRITE_BYTES} cannot be edited", meta.len())));
    }
    let old =
        String::from_utf8(fs::read(&path).map_err(io)?).map_err(|_| invalid(format!("{name} is a binary file")))?;
    let count = old.matches(&req.old).count();
    match count {
        0 => return Err(invalid(format!("old text not found in {name}"))),
        1 => {}
        n if !req.replace_all => {
            return Err(invalid(format!(
                "old text occurs {n} times in {name}; add surrounding context to make it unique, or set replace_all"
            )))
        }
        _ => {}
    }
    // Sized before it is built, so an edit far over the limit costs no memory.
    // Matches do not overlap, so they cover at most the whole file.
    let edits = if req.replace_all { count } else { 1 };
    let size = (old.len() - edits * req.old.len()).saturating_add(edits.saturating_mul(req.new.len()));
    if size > MAX_WRITE_BYTES {
        return Err(invalid(format!("the edited {name} would be {size} bytes; the limit is {MAX_WRITE_BYTES}")));
    }
    let new = if req.replace_all { old.replace(&req.old, &req.new) } else { old.replacen(&req.old, &req.new, 1) };
    atomic_write(&path, new.as_bytes(), Some(meta.permissions())).map_err(io)?;
    Ok(EditResponse { replacements: count as u64 })
}

/// Replace `dest` with `data`: write a temporary file next to it, then
/// rename it over `dest`, so readers see the old file or the new one, never
/// a mix. `mode` is applied before the rename.
pub(crate) fn atomic_write(dest: &Path, data: &[u8], mode: Option<Permissions>) -> io::Result<()> {
    let tmp = temp_path(dest)?;
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data)?;
        if let Some(mode) = mode {
            file.set_permissions(mode)?;
        }
        drop(file);
        fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A fresh name in `dest`'s directory for a file that will be renamed onto `dest`.
pub(crate) fn temp_path(dest: &Path) -> io::Result<PathBuf> {
    let dir = dest.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    Ok(dir.join(format!(".molt-{:016x}.tmp", rand::random::<u64>())))
}

pub(crate) fn list(roots: &Roots, req: ListRequest) -> Result<ListResponse, RemoteError> {
    let ws = roots.workspace(&req.workspace)?;
    let raw = req.path.as_deref().unwrap_or("");
    let name = shown(raw);
    let target = paths::existing(&ws, &paths::relative(raw)?, name)?;
    let depth = req.depth.unwrap_or(DEFAULT_LIST_DEPTH).clamp(1, MAX_LIST_DEPTH) as usize;
    if !target.is_dir() {
        let meta = fs::symlink_metadata(&target).map_err(|e| crate::error::io(name, e))?;
        let entry = Entry { path: paths::slash(&ws, &target), kind: EntryKind::File, size: meta.len() };
        return Ok(ListResponse { entries: vec![entry], truncated: false });
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in walk::walk(&ws, &target, Some(depth), &roots.scratch) {
        let entry = match item {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "fs.list skipped an entry");
                continue;
            }
        };
        if entries.len() == MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let (kind, size) = match entry.file_type() {
            Some(t) if t.is_dir() => (EntryKind::Dir, 0),
            Some(t) if t.is_symlink() => (EntryKind::Symlink, 0),
            _ => (EntryKind::File, entry.metadata().map(|m| m.len()).unwrap_or(0)),
        };
        entries.push(Entry { path: paths::slash(&ws, entry.path()), kind, size });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ListResponse { entries, truncated })
}

pub(crate) fn search(roots: &Roots, req: SearchRequest) -> Result<SearchResponse, RemoteError> {
    let ws = roots.workspace(&req.workspace)?;
    let raw = req.path.as_deref().unwrap_or("");
    let name = shown(raw);
    let target = paths::existing(&ws, &paths::relative(raw)?, name)?;
    let regex = RegexBuilder::new(&req.pattern)
        .case_insensitive(req.case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| invalid(format!("bad pattern: {e}")))?;
    // Matched against the workspace-relative path, so `*.rs` matches
    // `src/a.rs`; a glob without a `/` may also match just the file name.
    let glob = match &req.glob {
        Some(g) => Some((
            GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .map_err(|e| invalid(format!("bad glob: {e}")))?
                .compile_matcher(),
            !g.contains('/'),
        )),
        None => None,
    };
    let max = req.max_results.unwrap_or(DEFAULT_SEARCH_RESULTS).clamp(1, MAX_SEARCH_RESULTS) as usize;

    let candidates: Box<dyn Iterator<Item = PathBuf>> = if target.is_dir() {
        Box::new(
            walk::walk(&ws, &target, None, &roots.scratch)
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .map(|e| e.into_path()),
        )
    } else {
        Box::new(std::iter::once(target))
    };
    let mut matches = Vec::new();
    for file in candidates {
        let rel = paths::slash(&ws, &file);
        if let Some((glob, by_name)) = &glob {
            let name_matches = || *by_name && file.file_name().is_some_and(|n| glob.is_match(n));
            if !glob.is_match(&rel) && !name_matches() {
                continue;
            }
        }
        let Some(content) = searchable(&file) else { continue };
        for (i, line) in content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if matches.len() == max {
                return Ok(SearchResponse { matches, truncated: true });
            }
            matches.push(Match {
                path: rel.clone(),
                line: i as u64 + 1,
                text: line.chars().take(MAX_MATCH_CHARS).collect(),
            });
        }
    }
    Ok(SearchResponse { matches, truncated: false })
}

/// The text of a file worth searching: a regular file, not too large, not
/// binary. Opening a FIFO would block until a writer came, so the file is
/// opened without blocking and checked through the handle, which a swap
/// after the first check cannot fool.
fn searchable(path: &Path) -> Option<String> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_SEARCH_FILE {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_SEARCH_FILE).read_to_end(&mut bytes).ok()?;
    if is_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_drops_a_cut_character_only_from_a_prefix() {
        let s = "aé".as_bytes();
        assert_eq!(text(&s[..2], true), Some("a"));
        assert_eq!(text(&s[..2], false), None);
        assert_eq!(text(b"a\0b", false), None);
        assert_eq!(text(&[0xff, b'a'], true), None);
    }
}

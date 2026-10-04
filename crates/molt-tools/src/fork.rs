//! `fs.fork`, `fs.diff`, `fs.merge` and `fs.drop`.
//!
//! A fork is `scratch/fork-<id>`, with its metadata in
//! `scratch/fork-<id>.json`: the workspace it was copied from and a hash of
//! every copied file. Diff and merge compare three states of each path: the
//! hash at fork time, the fork now and the original now. Forks and their
//! metadata are private to the service's user: they copy private source, and
//! merge trusts them.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use molt_api::fs::{
    Change, ChangeKind, DiffRequest, DiffResponse, DropRequest, DropResponse, ForkRequest, ForkResponse, MergeRequest,
    MergeResponse,
};
use molt_proto::RemoteError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::error::{failed, invalid};
use crate::files::{atomic_write, is_binary, temp_path};
use crate::{paths, walk, Roots};

const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MAX_DIFF_FILE: u64 = 4 * 1024 * 1024;
const DIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// The state of a path that is a directory, or sits below something that is
/// not a directory. Never equal to a hash.
const NOT_A_FILE: &str = "not-a-file";

#[derive(Serialize, Deserialize)]
struct ForkMeta {
    /// The canonical workspace the fork was copied from.
    base: PathBuf,
    /// The hash of every copied file and symlink, by workspace-relative path.
    files: BTreeMap<String, String>,
    /// Entries the ignore rules excluded, linked to the original instead of copied.
    #[serde(default)]
    links: BTreeSet<String>,
}

struct Fork {
    dir: PathBuf,
    meta: ForkMeta,
}

pub(crate) fn fork(roots: &Roots, req: ForkRequest) -> Result<ForkResponse, RemoteError> {
    let base = roots.workspace(&req.workspace)?;
    let name = format!("fork-{:012x}", rand::random::<u64>() >> 16);
    let dir = roots.scratch.join(&name);
    DirBuilder::new().mode(0o700).create(&dir).map_err(|e| failed(format!("creating {}: {e}", dir.display())))?;
    let made = copy_tree(&base, &dir, &roots.scratch).and_then(|meta| {
        let json = serde_json::to_vec_pretty(&meta).map_err(|e| failed(e.to_string()))?;
        atomic_write(&meta_path(&dir), &json, Some(Permissions::from_mode(0o600)))
            .map_err(|e| failed(format!("writing fork metadata: {e}")))?;
        Ok(meta.files.len() as u64)
    });
    match made {
        Ok(files) => Ok(ForkResponse { fork: dir.to_string_lossy().into_owned(), files }),
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// Copy what the walk rules include from `base` into the empty `dest`, and
/// link every other entry to the original.
fn copy_tree(base: &Path, dest: &Path, scratch: &Path) -> Result<ForkMeta, RemoteError> {
    let err = |path: &Path, e: io::Error| failed(format!("fork: {}: {e}", path.display()));
    let mut files = BTreeMap::new();
    let mut included = HashSet::new();
    // The fork itself keeps only the owner's bits of the workspace's mode.
    let top = fs::metadata(base).map_err(|e| err(base, e))?.permissions().mode() & 0o700;
    let mut dirs = vec![(PathBuf::new(), Permissions::from_mode(top))];
    for item in walk::walk(base, base, None, scratch) {
        let entry = item.map_err(|e| failed(format!("fork: {e}")))?;
        let from = entry.path();
        let rel = from.strip_prefix(base).unwrap_or(from).to_path_buf();
        let to = dest.join(&rel);
        let Some(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            fs::create_dir(&to).map_err(|e| err(&to, e))?;
            dirs.push((rel.clone(), entry.metadata().map_err(|e| failed(format!("fork: {e}")))?.permissions()));
        } else if kind.is_file() {
            files.insert(rel_string(&rel), copy_file(from, &to).map_err(|e| err(from, e))?);
        } else if kind.is_symlink() {
            let target = fs::read_link(from).map_err(|e| err(from, e))?;
            symlink(&target, &to).map_err(|e| err(&to, e))?;
            files.insert(rel_string(&rel), link_hash(&target));
        } else {
            // Sockets, fifos and devices are linked below, like ignored entries.
            continue;
        }
        included.insert(rel);
    }

    // Ignored entries (dependencies, build outputs, .env files) stay usable
    // in the fork without being copied.
    let mut links = BTreeSet::new();
    for (dir, _) in &dirs {
        let from = base.join(dir);
        for child in fs::read_dir(&from).map_err(|e| err(&from, e))? {
            let child = child.map_err(|e| err(&from, e))?;
            let name = child.file_name();
            let rel = dir.join(&name);
            let original = base.join(&rel);
            if walk::SKIPPED.iter().any(|s| name == *s) || included.contains(&rel) || original == scratch {
                continue;
            }
            symlink(&original, dest.join(&rel)).map_err(|e| err(&original, e))?;
            links.insert(rel_string(&rel));
        }
    }

    // Last, and deepest first, so a read-only directory is filled before it is locked.
    for (dir, mode) in dirs.iter().rev() {
        let to = dest.join(dir);
        fs::set_permissions(&to, mode.clone()).map_err(|e| err(&to, e))?;
    }
    Ok(ForkMeta { base: base.to_path_buf(), files, links })
}

/// Copy a regular file with its permission bits; returns its hash.
fn copy_file(from: &Path, to: &Path) -> io::Result<String> {
    let mut src = File::open(from)?;
    let mut dst = fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
    }
    dst.set_permissions(src.metadata()?.permissions())?;
    Ok(hex::encode(hasher.finalize()))
}

fn rel_string(rel: &Path) -> String {
    paths::slash(Path::new(""), rel)
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn link_hash(target: &Path) -> String {
    format!("link:{}", hex::encode(Sha256::digest(target.as_os_str().as_encoded_bytes())))
}

/// The state of `rel` under `root`: `None` when missing, else its hash (or
/// [`NOT_A_FILE`]). Symlinks are never followed, including in parent
/// directories, so a link swapped in cannot lead the caller outside `root`.
fn state(root: &Path, rel: &str) -> io::Result<Option<String>> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut path = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let last = i + 1 == parts.len();
        if !last && !meta.is_dir() {
            return Ok(Some(NOT_A_FILE.to_owned()));
        }
        if last {
            let kind = meta.file_type();
            return Ok(Some(if kind.is_file() {
                hash_file(&path)?
            } else if kind.is_symlink() {
                link_hash(&fs::read_link(&path)?)
            } else {
                NOT_A_FILE.to_owned()
            }));
        }
    }
    Ok(None)
}

fn meta_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_owned();
    name.push(".json");
    dir.with_file_name(name)
}

fn is_fork_name(name: &str) -> bool {
    name.strip_prefix("fork-")
        .is_some_and(|id| id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Where the fork a request names lives: a direct child of scratch with a
/// fork's name. The path need not exist.
fn fork_path(roots: &Roots, fork: &str) -> Result<PathBuf, RemoteError> {
    let not_a_fork = || invalid(format!("{fork} is not a fork"));
    if fork.contains('\0') {
        return Err(not_a_fork());
    }
    let path = roots.scratch.join(fork);
    let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else { return Err(not_a_fork()) };
    let parent = parent.canonicalize().map_err(|_| not_a_fork())?;
    if parent != roots.scratch || !name.to_str().is_some_and(is_fork_name) {
        return Err(not_a_fork());
    }
    Ok(parent.join(name))
}

/// The fork a request names, with its metadata.
fn open(roots: &Roots, fork: &str) -> Result<Fork, RemoteError> {
    let not_a_fork = || invalid(format!("{fork} is not a fork"));
    let dir = fork_path(roots, fork)?;
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return Err(not_a_fork());
    }
    let json = match fs::read(meta_path(&dir)) {
        Ok(json) => json,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_a_fork()),
        Err(e) => return Err(failed(format!("reading the metadata of {fork}: {e}"))),
    };
    let meta: ForkMeta =
        serde_json::from_slice(&json).map_err(|e| failed(format!("the metadata of {fork} is corrupt: {e}")))?;
    // The metadata sits in scratch, where commands run; check it before trusting it.
    let base_ok = meta.base.canonicalize().is_ok_and(|b| b == meta.base && b.is_dir() && roots.contains(&b));
    if !base_ok {
        return Err(failed(format!("the workspace {fork} was forked from, {}, is gone", meta.base.display())));
    }
    if let Some(bad) = meta.files.keys().chain(&meta.links).find(|p| p.is_empty() || paths::relative(p).is_err()) {
        return Err(failed(format!("the metadata of {fork} is corrupt: bad path {bad}")));
    }
    Ok(Fork { dir, meta })
}

/// The hash of every file and symlink in the fork now, by path.
fn current(fork: &Fork, scratch: &Path) -> Result<BTreeMap<String, String>, RemoteError> {
    let err = |e: io::Error| failed(format!("reading the fork: {e}"));
    let mut now = BTreeMap::new();
    for item in walk::walk(&fork.dir, &fork.dir, None, scratch) {
        let entry = item.map_err(|e| failed(format!("reading the fork: {e}")))?;
        let Some(kind) = entry.file_type() else { continue };
        let rel = paths::slash(&fork.dir, entry.path());
        if kind.is_symlink() && fork.meta.links.contains(&rel) {
            // The link fork made to an ignored entry is not a change.
            if fs::read_link(entry.path()).is_ok_and(|t| t == fork.meta.base.join(&rel)) {
                continue;
            }
        }
        if kind.is_file() || kind.is_symlink() {
            if let Some(hash) = state(&fork.dir, &rel).map_err(err)?.filter(|h| h != NOT_A_FILE) {
                now.insert(rel, hash);
            }
        }
    }
    // A copied file the rules now exclude (say, a pattern added to
    // .gitignore) still exists; it is not deleted.
    for rel in fork.meta.files.keys() {
        if !now.contains_key(rel) {
            if let Some(hash) = state(&fork.dir, rel).map_err(err)?.filter(|h| h != NOT_A_FILE) {
                now.insert(rel.clone(), hash);
            }
        }
    }
    Ok(now)
}

fn changes(before: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<Change> {
    let paths: BTreeSet<&String> = before.keys().chain(now.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let kind = match (before.get(path), now.get(path)) {
                (None, Some(_)) => ChangeKind::Added,
                (Some(a), Some(b)) if a != b => ChangeKind::Modified,
                (Some(_), None) => ChangeKind::Deleted,
                _ => return None,
            };
            Some(Change { path: path.clone(), kind })
        })
        .collect()
}

pub(crate) fn diff(roots: &Roots, req: DiffRequest) -> Result<DiffResponse, RemoteError> {
    let fork = open(roots, &req.fork)?;
    let now = current(&fork, &roots.scratch)?;
    let changes = changes(&fork.meta.files, &now);
    let mut patch = String::new();
    let mut truncated = false;
    for change in &changes {
        let section = file_patch(&fork, change, now.get(&change.path));
        if patch.len() + section.len() > MAX_PATCH_BYTES {
            let room = MAX_PATCH_BYTES - patch.len();
            // Cut after a newline, which is always a character boundary.
            let cut = section.as_bytes()[..room].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            patch.push_str(&section[..cut]);
            truncated = true;
            break;
        }
        patch.push_str(&section);
    }
    Ok(DiffResponse { changes, patch, truncated })
}

/// The patch for one changed path. The old side is the original as it is
/// now, which is only the fork-time content while its hash still matches.
fn file_patch(fork: &Fork, change: &Change, in_fork: Option<&String>) -> String {
    let path = &change.path;
    let base = &fork.meta.base;
    let in_base = match state(base, path) {
        Ok(s) => s,
        Err(e) => return format!("# {path}: cannot read the original: {e}\n"),
    };
    if in_base.as_ref() == in_fork {
        return format!("# {path}: the original already has this change\n");
    }
    if in_base.as_ref() != fork.meta.files.get(path) {
        return format!("# {path}: the original changed since the fork; merging would conflict\n");
    }
    let old = match change.kind {
        ChangeKind::Added => Side::Missing,
        _ => Side::read(&base.join(path)),
    };
    let new = match change.kind {
        ChangeKind::Deleted => Side::Missing,
        _ => Side::read(&fork.dir.join(path)),
    };
    let (a, b) = (format!("a/{path}"), format!("b/{path}"));
    match (old, new) {
        (Side::Unreadable(e), _) | (_, Side::Unreadable(e)) => format!("# {path}: cannot read: {e}\n"),
        (_, Side::Link(t)) => format!("# {path}: {} symlink to {}\n", kind_word(change.kind), t.display()),
        (Side::Link(_), Side::Missing) => format!("# {path}: deleted symlink\n"),
        (Side::Link(_), _) => format!("# {path}: symlink replaced by a file\n"),
        (Side::Large, _) | (_, Side::Large) => format!("# {path}: {}, too large to show\n", kind_word(change.kind)),
        (Side::Binary, Side::Missing) => format!("Binary file {a} deleted\n"),
        (Side::Missing, Side::Binary) => format!("Binary file {b} added\n"),
        (Side::Binary, _) | (_, Side::Binary) => format!("Binary files {a} and {b} differ\n"),
        (old, new) => {
            let (old_name, old) = match old {
                Side::Text(t) => (a.as_str(), t),
                _ => ("/dev/null", String::new()),
            };
            let (new_name, new) = match new {
                Side::Text(t) => (b.as_str(), t),
                _ => ("/dev/null", String::new()),
            };
            let diff = TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(&old, &new);
            format!("--- {old_name}\n+++ {new_name}\n{}", diff.unified_diff())
        }
    }
}

fn kind_word(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

/// One side of a file's diff.
enum Side {
    Missing,
    Text(String),
    Binary,
    Large,
    Link(PathBuf),
    Unreadable(io::Error),
}

impl Side {
    /// Read `path` without following a final symlink. Callers have already
    /// checked that no parent is a symlink.
    fn read(path: &Path) -> Side {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => return Side::Unreadable(e),
        };
        if meta.file_type().is_symlink() {
            return fs::read_link(path).map_or_else(Side::Unreadable, Side::Link);
        }
        if !meta.is_file() {
            return Side::Unreadable(io::Error::other("not a regular file"));
        }
        if meta.len() > MAX_DIFF_FILE {
            return Side::Large;
        }
        match fs::read(path) {
            Ok(bytes) if is_binary(&bytes) => Side::Binary,
            Ok(bytes) => String::from_utf8(bytes).map_or(Side::Binary, Side::Text),
            Err(e) => Side::Unreadable(e),
        }
    }
}

/// The canonical workspace `fork` was copied from, if it is a fork.
pub(crate) fn base(roots: &Roots, fork: &str) -> Option<PathBuf> {
    open(roots, fork).ok().map(|f| f.meta.base)
}

pub(crate) fn merge(roots: &Roots, lock: &Mutex<()>, req: MergeRequest) -> Result<MergeResponse, RemoteError> {
    let _merging = lock.lock().unwrap_or_else(|e| e.into_inner());
    let fork = open(roots, &req.fork)?;
    let now = current(&fork, &roots.scratch)?;
    let changes = changes(&fork.meta.files, &now);
    let base = &fork.meta.base;

    // Every check comes before the first write, so a conflict leaves the original untouched.
    let mut conflicts = Vec::new();
    let mut work = Vec::new();
    for change in &changes {
        let in_base = state(base, &change.path).map_err(|e| failed(format!("{}: {e}", change.path)))?;
        let wanted = now.get(&change.path);
        if in_base.as_ref() == wanted {
            // The original already has what the fork has.
            continue;
        }
        if in_base.as_ref() != fork.meta.files.get(&change.path) {
            conflicts.push(change.path.as_str());
            continue;
        }
        work.push(change);
    }
    if !conflicts.is_empty() {
        return Err(failed(format!(
            "conflict: the original workspace changed these files since the fork: {}",
            conflicts.join(", ")
        )));
    }
    apply_all(&work, |change| apply(&fork, change))?;
    if req.drop {
        remove(&fork.dir).map_err(|e| failed(format!("the merge succeeded, but dropping the fork failed: {e}")))?;
    }
    Ok(MergeResponse { changes })
}

/// Run `apply` on each change in turn. A failure after the first write is
/// reported as `partial:` with the paths already written, since the
/// original then holds some of the fork's changes.
fn apply_all(work: &[&Change], mut apply: impl FnMut(&Change) -> io::Result<()>) -> Result<(), RemoteError> {
    let mut written: Vec<&str> = Vec::new();
    for change in work {
        if let Err(e) = apply(change) {
            let path = &change.path;
            return Err(failed(if written.is_empty() {
                format!("merging {path}: {e}")
            } else {
                format!("partial: merging {path} failed ({e}) after these files were written: {}", written.join(", "))
            }));
        }
        written.push(&change.path);
    }
    Ok(())
}

/// Make the original's `change.path` what it is in the fork.
fn apply(fork: &Fork, change: &Change) -> io::Result<()> {
    let base = &fork.meta.base;
    let dest = base.join(&change.path);
    let parent = dest.parent().ok_or_else(|| io::Error::other("no parent directory"))?;
    if change.kind == ChangeKind::Deleted {
        match fs::remove_file(&dest) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        // Remove directories the deletion emptied, as git would.
        let mut dir = parent;
        while dir != base && fs::remove_dir(dir).is_ok() {
            dir = match dir.parent() {
                Some(d) => d,
                None => break,
            };
        }
        return Ok(());
    }
    fs::create_dir_all(parent)?;
    if !parent.canonicalize()?.starts_with(base) {
        return Err(io::Error::other("the path leads outside the workspace"));
    }
    let src = fork.dir.join(&change.path);
    let meta = fs::symlink_metadata(&src)?;
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Err(io::Error::other("not a regular file or symlink in the fork"));
    }
    if meta.file_type().is_symlink() {
        let tmp = temp_path(&dest)?;
        symlink(fs::read_link(&src)?, &tmp)?;
        return fs::rename(&tmp, &dest).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        });
    }
    // A replaced file keeps the original's mode, as fs.write does.
    let mode = match fs::symlink_metadata(&dest) {
        Ok(m) if m.is_file() => m.permissions(),
        _ => meta.permissions(),
    };
    atomic_write(&dest, &fs::read(&src)?, Some(mode))
}

pub(crate) fn drop_fork(roots: &Roots, lock: &Mutex<()>, req: DropRequest) -> Result<DropResponse, RemoteError> {
    let _merging = lock.lock().unwrap_or_else(|e| e.into_inner());
    let dir = fork_path(roots, &req.fork)?;
    let has_meta = fs::symlink_metadata(meta_path(&dir)).is_ok();
    let dropped = match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Ok(m) if m.is_dir() && has_meta => true,
        _ => return Err(invalid(format!("{} is not a fork", req.fork))),
    };
    remove(&dir).map_err(|e| failed(format!("dropping {}: {e}", req.fork)))?;
    Ok(DropResponse { dropped })
}

/// Delete a fork and its metadata. Links inside it are removed, never followed.
fn remove(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    match fs::remove_file(meta_path(dir)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_names_are_strict() {
        assert!(is_fork_name("fork-0123456789ab"));
        assert!(!is_fork_name("fork-0123456789AB"));
        assert!(!is_fork_name("fork-0123456789a"));
        assert!(!is_fork_name("fork-0123456789abc"));
        assert!(!is_fork_name("spoon-0123456789ab"));
    }

    #[test]
    fn a_merge_that_stops_after_a_write_names_what_was_written() {
        let change = |path: &str| Change { path: path.into(), kind: ChangeKind::Modified };
        let work = [change("a.txt"), change("b.txt"), change("c.txt")];
        let work: Vec<&Change> = work.iter().collect();
        let failing =
            |at: &'static str| move |c: &Change| if c.path == at { Err(io::Error::other("disk full")) } else { Ok(()) };

        let e = apply_all(&work, failing("c.txt")).unwrap_err();
        assert_eq!(e.code, molt_proto::ErrorCode::Failed);
        assert_eq!(e.message, "partial: merging c.txt failed (disk full) after these files were written: a.txt, b.txt");
        let e = apply_all(&work, failing("a.txt")).unwrap_err();
        assert_eq!(e.message, "merging a.txt: disk full", "nothing was written yet");
        assert!(apply_all(&work, failing("z.txt")).is_ok());
    }

    #[test]
    fn changes_are_sorted_and_classified() {
        let before: BTreeMap<_, _> = [("b", "1"), ("c", "2"), ("d", "3")].map(|(k, v)| (k.into(), v.into())).into();
        let now: BTreeMap<_, _> = [("a", "9"), ("b", "1"), ("c", "5")].map(|(k, v)| (k.into(), v.into())).into();
        let got: Vec<_> = changes(&before, &now).into_iter().map(|c| (c.path, c.kind)).collect();
        assert_eq!(
            got,
            [("a".into(), ChangeKind::Added), ("c".into(), ChangeKind::Modified), ("d".into(), ChangeKind::Deleted)]
        );
    }
}

//! `fs.fork`, `fs.diff`, `fs.merge`, `fs.narrow` and `fs.drop`.
//!
//! A fork is `scratch/fork-<id>`, with its metadata in
//! `scratch/fork-<id>.json`: the workspace it was copied from and the state
//! of every copied file, its content hash and whether it is executable. Diff
//! and merge compare three states of each path: the state at fork time, the
//! fork now and the original now. Forks and their metadata are private to
//! the service's user: they copy private source, and merge trusts them.
//!
//! `scratch/fork-<id>.written` lists the paths writes and edits made in the
//! fork. A file a command created that looks like build output, and that no
//! write or edit made, is left out of the result (see [`artifacts`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use molt_api::fs::{
    Change, ChangeKind, DiffRequest, DiffResponse, DropRequest, DropResponse, ForkRequest, ForkResponse, LeftOut,
    MergeRequest, MergeResponse, NarrowRequest, ViewRequest, ViewResponse,
};
use molt_proto::RemoteError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::error::{failed, invalid};
use crate::files::{atomic_write, is_binary};
use crate::{apply, git, paths, walk, Roots};

const MAX_PATCH_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DIFF_FILE: u64 = 4 * 1024 * 1024;
pub(crate) const DIFF_TIMEOUT: Duration = Duration::from_secs(2);
/// The state of a path that is a directory, or sits below something that is
/// not a directory. Never equal to a hash.
pub(crate) const NOT_A_FILE: &str = "not-a-file";
/// Appended to the hash of a regular file its owner can execute.
pub(crate) const EXECUTABLE: &str = "+x";
/// The metadata format. Version 0 recorded content hashes only.
const META_VERSION: u32 = 1;
/// How many left-out paths a reply names.
const LEFT_OUT_SHOWN: usize = 5;
/// Directories that only tools write: caches, bytecode and installed packages.
const BUILD_DIRS: [&str; 6] = ["__pycache__", ".pytest_cache", "node_modules", ".mypy_cache", ".ruff_cache", ".tox"];
/// Compiled files: bytecode, objects, Rust libraries, shared libraries and Java classes.
const BUILD_EXTENSIONS: [&str; 9] = ["pyc", "pyo", "o", "rlib", "rmeta", "so", "dylib", "dll", "class"];
/// What a `CACHEDIR.TAG` starts with (https://bford.info/cachedir/); Cargo writes one in every target directory.
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

#[derive(Serialize, Deserialize)]
struct ForkMeta {
    /// Absent, and so 0, in metadata older than [`META_VERSION`] 1.
    #[serde(default)]
    version: u32,
    /// The canonical workspace the fork was copied from.
    base: PathBuf,
    /// The state of every copied file and symlink, by workspace-relative path.
    files: BTreeMap<String, String>,
    /// Entries the ignore rules excluded, linked to the original instead of copied.
    #[serde(default)]
    links: BTreeSet<String>,
    /// The commit the original's git `HEAD` named when the fork was made,
    /// which its history queries take as their baseline (see [`crate::git`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git: Option<git::Baseline>,
}

struct Fork {
    dir: PathBuf,
    meta: ForkMeta,
}

pub(crate) fn fork(roots: &Roots, req: ForkRequest) -> Result<ForkResponse, RemoteError> {
    let base = roots.workspace(&req.workspace)?;
    // Before the copy, so the copy holds that commit's files or ones changed after it.
    let baseline = git::baseline(&base);
    let name = format!("fork-{:012x}", rand::random::<u64>() >> 16);
    let dir = roots.scratch.join(&name);
    DirBuilder::new().mode(0o700).create(&dir).map_err(|e| failed(format!("creating {}: {e}", dir.display())))?;
    let made = copy_tree(&base, &dir, &roots.scratch).and_then(|mut meta| {
        meta.git = baseline;
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
    Ok(ForkMeta { version: META_VERSION, base: base.to_path_buf(), files, links, git: None })
}

/// Copy a regular file with its permission bits; returns its state.
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
    let mode = src.metadata()?.permissions().mode();
    dst.set_permissions(Permissions::from_mode(mode))?;
    Ok(file_state(hex::encode(hasher.finalize()), mode))
}

/// A regular file's state: its content hash, marked when its owner can
/// execute it. That is the one mode bit a merge carries, as in git.
fn file_state(hash: String, mode: u32) -> String {
    if mode & 0o100 != 0 {
        hash + EXECUTABLE
    } else {
        hash
    }
}

fn is_executable(state: &str) -> bool {
    state.ends_with(EXECUTABLE)
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

/// The state of `rel` under `root`: `None` when missing, else its
/// [`file_state`], its symlink hash or [`NOT_A_FILE`]. Symlinks are never
/// followed, including in parent directories, so a link swapped in cannot
/// lead the caller outside `root`.
pub(crate) fn state(root: &Path, rel: &str) -> io::Result<Option<String>> {
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
                file_state(hash_file(&path)?, meta.permissions().mode())
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

pub(crate) fn is_fork_name(name: &str) -> bool {
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
    if meta.version > META_VERSION {
        return Err(failed(format!("{fork} was made by a newer Molt; merge or drop it with that version")));
    }
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

impl Fork {
    /// The state of `rel` under `root`, comparable with the fork-time state.
    /// A version 0 fork recorded no modes, so its states leave them out and
    /// its merges keep the original's modes, as they did when it was made.
    fn state(&self, root: &Path, rel: &str) -> io::Result<Option<String>> {
        let state = state(root, rel)?;
        if self.meta.version == 0 {
            return Ok(state.map(|s| s.strip_suffix(EXECUTABLE).map(str::to_owned).unwrap_or(s)));
        }
        Ok(state)
    }
}

/// The state of every file and symlink in the fork now, by path.
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
            if let Some(hash) = fork.state(&fork.dir, &rel).map_err(err)?.filter(|h| h != NOT_A_FILE) {
                now.insert(rel, hash);
            }
        }
    }
    // A copied file the rules now exclude (say, a pattern added to
    // .gitignore) still exists; it is not deleted.
    for rel in fork.meta.files.keys() {
        if !now.contains_key(rel) {
            if let Some(hash) = fork.state(&fork.dir, rel).map_err(err)?.filter(|h| h != NOT_A_FILE) {
                now.insert(rel.clone(), hash);
            }
        }
    }
    Ok(now)
}

pub(crate) fn fingerprint(roots: &Roots, name: &str) -> Result<String, RemoteError> {
    let fork = open(roots, name)?;
    let files = current(&fork, &roots.scratch)?;
    fingerprint_of(&fork.dir, &files)
}

fn fingerprint_of(dir: &Path, files: &BTreeMap<String, String>) -> Result<String, RemoteError> {
    // Recovery freezes all file permission bits, including a new file's
    // read/write permissions. Diff's execute-only identity is insufficient.
    let mut inventory = BTreeMap::new();
    for (path, state) in files {
        let meta = fs::symlink_metadata(dir.join(path)).map_err(|e| failed(format!("candidate {path}: {e}")))?;
        let mode = meta.is_file().then(|| meta.permissions().mode() & 0o7777);
        inventory.insert(path, (state, mode));
    }
    let bytes = serde_json::to_vec(&inventory).map_err(|e| failed(e.to_string()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(crate) fn changes(before: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<Change> {
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
    let (changes, left_out) = artifacts(&fork, changes(&fork.meta.files, &now), &now);
    let changes = only(changes, &req.paths)?;
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
    Ok(DiffResponse { changes, patch, truncated, left_out })
}

pub(crate) fn view(roots: &Roots, lock: &Mutex<()>, req: ViewRequest) -> Result<ViewResponse, RemoteError> {
    // A drop or merge must not replace the fork while its identity is checked.
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    let fork = open(roots, &req.fork)?;
    let now = current(&fork, &roots.scratch)?;
    let fingerprint = |files: &BTreeMap<String, String>| -> Result<String, RemoteError> {
        let bytes = serde_json::to_vec(files).map_err(|e| failed(e.to_string()))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    };
    Ok(ViewResponse {
        workspace: fork.meta.base.to_string_lossy().into_owned(),
        fork: fork.dir.to_string_lossy().into_owned(),
        baseline: fingerprint(&fork.meta.files)?,
        version: fingerprint(&now)?,
    })
}

/// The patch for one changed path. The old side is the original as it is
/// now, which is only the fork-time content while its hash still matches.
fn file_patch(fork: &Fork, change: &Change, in_fork: Option<&String>) -> String {
    let path = &change.path;
    let base = &fork.meta.base;
    let in_base = match fork.state(base, path) {
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
    // Only a regular file's state is marked; a symlink's never ends in the mark.
    let mode = match (fork.meta.files.get(path), in_fork) {
        (Some(was), Some(now)) if change.kind == ChangeKind::Modified && is_executable(was) != is_executable(now) => {
            if is_executable(now) {
                format!("# {path}: made executable\n")
            } else {
                format!("# {path}: no longer executable\n")
            }
        }
        _ => String::new(),
    };
    let same_content = match (fork.meta.files.get(path), in_fork) {
        (Some(was), Some(now)) => {
            was.strip_suffix(EXECUTABLE).unwrap_or(was) == now.strip_suffix(EXECUTABLE).unwrap_or(now)
        }
        _ => false,
    };
    if same_content {
        return mode;
    }
    let (a, b) = (format!("a/{path}"), format!("b/{path}"));
    let patch = match (old, new) {
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
    };
    mode + &patch
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

/// A fork as its history queries see it (see [`crate::git`]).
pub(crate) struct Snapshot {
    pub dir: PathBuf,
    /// The canonical workspace it was copied from.
    pub base: PathBuf,
    /// The state of each file when it was made, by path.
    pub at_fork: BTreeMap<String, String>,
    /// The state of each file now, without the build output diff leaves out.
    pub now: BTreeMap<String, String>,
    /// The entries linked to the original rather than copied.
    pub links: BTreeSet<String>,
    /// What it changed, as diff lists it.
    pub changes: Vec<Change>,
    pub baseline: Option<git::Baseline>,
}

/// The fork `name` as it was made and as it is now.
pub(crate) fn snapshot(roots: &Roots, name: &str) -> Result<Snapshot, RemoteError> {
    let fork = open(roots, name)?;
    let now = current(&fork, &roots.scratch)?;
    let (changes, _) = artifacts(&fork, changes(&fork.meta.files, &now), &now);
    let changed: HashSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    let now =
        now.into_iter().filter(|(p, _)| fork.meta.files.contains_key(p) || changed.contains(p.as_str())).collect();
    let Fork { dir, meta } = fork;
    Ok(Snapshot { dir, base: meta.base, at_fork: meta.files, now, links: meta.links, changes, baseline: meta.git })
}

/// True when `path` names a fork: a child of scratch with a fork's name.
pub(crate) fn names_fork(roots: &Roots, path: &str) -> bool {
    fork_path(roots, path).is_ok()
}

/// The canonical workspace `fork` was copied from, if it is a fork.
pub(crate) fn base(roots: &Roots, fork: &str) -> Option<PathBuf> {
    open(roots, fork).ok().map(|f| f.meta.base)
}

/// Carry a fork's changes back into its original. `written` receives the
/// paths written in the original, also when the merge fails partway.
pub(crate) fn merge(
    roots: &Roots,
    lock: &Mutex<()>,
    req: MergeRequest,
    written: &mut Vec<String>,
) -> Result<MergeResponse, RemoteError> {
    let _merging = lock.lock().unwrap_or_else(|e| e.into_inner());
    let _process_lock = apply::lock(roots)?;
    let fork = open(roots, &req.fork)?;
    let now = current(&fork, &roots.scratch)?;
    let (changes, left_out) = artifacts(&fork, changes(&fork.meta.files, &now), &now);
    let changes = only(changes, &req.paths)?;
    let base = &fork.meta.base;

    // Every check comes before the first write, so a conflict leaves the original untouched.
    let mut conflicts = Vec::new();
    let mut work = Vec::new();
    for change in &changes {
        let in_base = fork.state(base, &change.path).map_err(|e| failed(format!("{}: {e}", change.path)))?;
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
    let operation = if work.is_empty() {
        None
    } else {
        Some(apply::begin(
            roots,
            apply::Candidate {
                workspace: base,
                fork: &fork.dir,
                fingerprint: fingerprint_of(&fork.dir, &now)?,
                changes: &work,
                version: fork.meta.version,
                before: &fork.meta.files,
                after: &now,
            },
            written,
        )?)
    };
    if req.drop {
        remove(&fork.dir).map_err(|e| match &operation {
            Some(id) => {
                failed(format!("partial: operation {id}: all files applied, but dropping the fork failed: {e}"))
            }
            None => failed(format!("the merge succeeded, but dropping the fork failed: {e}")),
        })?;
    }
    Ok(MergeResponse { changes, operation, left_out })
}

/// `changes` cut down to `paths`, or all of them when `paths` is empty.
fn only(changes: Vec<Change>, paths: &[String]) -> Result<Vec<Change>, RemoteError> {
    if paths.is_empty() {
        return Ok(changes);
    }
    let unknown: Vec<&str> =
        paths.iter().filter(|p| !changes.iter().any(|c| &c.path == *p)).map(String::as_str).collect();
    if !unknown.is_empty() {
        return Err(invalid(format!("not among the fork's changes: {}", unknown.join(", "))));
    }
    Ok(changes.into_iter().filter(|c| paths.contains(&c.path)).collect())
}

/// A new fork of `req.fork`'s original as it is now, with `req.paths` of
/// its changes carried over.
pub(crate) fn narrow(roots: &Roots, lock: &Mutex<()>, req: NarrowRequest) -> Result<ForkResponse, RemoteError> {
    if req.paths.is_empty() {
        return Err(invalid("narrow needs the paths to carry over"));
    }
    // A merge or drop must not change the fork while it is copied from.
    let _merging = lock.lock().unwrap_or_else(|e| e.into_inner());
    let fork = open(roots, &req.fork)?;
    let now = current(&fork, &roots.scratch)?;
    let (changes, _) = artifacts(&fork, changes(&fork.meta.files, &now), &now);
    let changes = only(changes, &req.paths)?;
    let made = self::fork(roots, ForkRequest { workspace: fork.meta.base.to_string_lossy().into_owned() })?;
    let dir = PathBuf::from(&made.fork);
    match carry(&fork.dir, &dir, &changes) {
        Ok(()) => Ok(made),
        Err(e) => {
            let _ = remove(&dir);
            Err(e)
        }
    }
}

/// Put `changes` as `from` has them into the fresh fork `to`.
fn carry(from: &Path, to: &Path, changes: &[Change]) -> Result<(), RemoteError> {
    let mut written = String::new();
    for change in changes {
        let err = |e: io::Error| failed(format!("narrow: {}: {e}", change.path));
        // A link the new fork made to an ignored entry leads to the original: never write through one.
        let rel = Path::new(&change.path);
        for parent in rel.ancestors().skip(1).filter(|p| !p.as_os_str().is_empty()) {
            if fs::symlink_metadata(to.join(parent)).is_ok_and(|m| !m.is_dir()) {
                return Err(failed(format!(
                    "narrow: {}: {} is not a directory in the workspace now",
                    change.path,
                    parent.display()
                )));
            }
        }
        let dest = to.join(rel);
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&dest).map_err(err)?,
            Ok(_) => fs::remove_file(&dest).map_err(err)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
        if change.kind == ChangeKind::Deleted {
            continue;
        }
        if let Some(parent) = dest.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(err)?;
        }
        let source = from.join(rel);
        if fs::symlink_metadata(&source).map_err(err)?.file_type().is_symlink() {
            symlink(fs::read_link(&source).map_err(err)?, &dest).map_err(err)?;
        } else {
            copy_file(&source, &dest).map_err(err)?;
        }
        // As the new fork's own writes, so that its diff leaves out the same build artifacts.
        written.push_str(&serde_json::to_string(&change.path).map_err(|e| failed(e.to_string()))?);
        written.push('\n');
    }
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(written_path(to))
        .and_then(|mut f| f.write_all(written.as_bytes()))
        .map_err(|e| failed(format!("narrow: recording the carried files: {e}")))
}

/// `mode` made executable or not by its owner. Making it executable also
/// lets whoever can read it execute it, as `chmod +x` does under the usual
/// umask; making it not executable clears every execute bit.
pub(crate) fn with_executable(mode: u32, executable: bool) -> u32 {
    if (mode & 0o100 != 0) == executable {
        mode
    } else if executable {
        mode | 0o100 | ((mode & 0o044) >> 2)
    } else {
        mode & !0o111
    }
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
    for path in [written_path(dir), meta_path(dir)] {
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

fn written_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_owned();
    name.push(".written");
    dir.with_file_name(name)
}

/// Note that a write or edit made `file` (absolute, canonical) in the
/// canonical workspace `ws`, when `ws` is a fork or inside one.
pub(crate) fn record_written(roots: &Roots, ws: &Path, file: &Path) -> io::Result<()> {
    let Ok(rel) = ws.strip_prefix(&roots.scratch) else { return Ok(()) };
    let Some(name) = rel.components().next().and_then(|c| c.as_os_str().to_str()).filter(|n| is_fork_name(n)) else {
        return Ok(());
    };
    let dir = roots.scratch.join(name);
    if !file.starts_with(&dir) || !fs::symlink_metadata(meta_path(&dir)).is_ok_and(|m| m.is_file()) {
        return Ok(());
    }
    let mut line = serde_json::to_string(&paths::slash(&dir, file)).map_err(io::Error::other)?;
    line.push('\n');
    use std::os::unix::fs::OpenOptionsExt;
    // One short append per write: concurrent writes cannot interleave it.
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(written_path(&dir))?
        .write_all(line.as_bytes())
}

/// The paths writes and edits made in `fork`.
fn written(fork: &Fork) -> HashSet<String> {
    let Ok(text) = fs::read_to_string(written_path(&fork.dir)) else { return HashSet::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// `changes` without the build artifacts, and what was left out. An
/// artifact is a regular file that a command added: the original does not
/// have it and no write or edit made it. It sits in a build or cache
/// directory, or is compiled code. Files the original has, ignored files and
/// every modification or deletion stay in the result.
fn artifacts(fork: &Fork, changes: Vec<Change>, now: &BTreeMap<String, String>) -> (Vec<Change>, Option<LeftOut>) {
    if !changes.iter().any(|c| c.kind == ChangeKind::Added) {
        return (changes, None);
    }
    let written = written(fork);
    let mut tagged = HashMap::new();
    let (mut kept, mut left) = (Vec::new(), Vec::new());
    for change in changes {
        let path = &change.path;
        let artifact = change.kind == ChangeKind::Added
            && !written.contains(path)
            && now.get(path).is_some_and(|s| !s.starts_with("link:"))
            && matches!(fork.state(&fork.meta.base, path), Ok(None))
            && is_artifact(&fork.dir, path, now.get(path).is_some_and(|s| is_executable(s)), &mut tagged);
        if artifact {
            left.push(change.path);
        } else {
            kept.push(change);
        }
    }
    let left_out = (!left.is_empty())
        .then(|| LeftOut { count: left.len() as u64, paths: left.into_iter().take(LEFT_OUT_SHOWN).collect() });
    (kept, left_out)
}

/// Whether the file `rel` in `dir` looks like build output. `tagged`
/// remembers which directories hold a `CACHEDIR.TAG`.
fn is_artifact(dir: &Path, rel: &str, executable: bool, tagged: &mut HashMap<String, bool>) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let Some((file, dirs)) = parts.split_last() else { return false };
    if dirs.iter().any(|d| BUILD_DIRS.contains(d)) {
        return true;
    }
    // Cargo's output when CARGO_TARGET_DIR points into the workspace. Only at
    // the top, where a source directory is rarely named so.
    if dirs.first().is_some_and(|d| *d == "target" || d.starts_with("target-") || d.starts_with("target_")) {
        return true;
    }
    for i in 1..=dirs.len() {
        let sub = dirs[..i].join("/");
        let tag = *tagged.entry(sub).or_insert_with_key(|sub| cache_dir(&dir.join(sub)));
        if tag {
            return true;
        }
    }
    if Path::new(file).extension().and_then(|e| e.to_str()).is_some_and(|e| BUILD_EXTENSIONS.contains(&e)) {
        return true;
    }
    // A compiled program; a script is text.
    executable && sniff(&dir.join(rel)).is_some_and(|bytes| is_binary(&bytes))
}

/// Whether `dir` is marked as a cache directory.
fn cache_dir(dir: &Path) -> bool {
    let tag = dir.join("CACHEDIR.TAG");
    fs::symlink_metadata(&tag).is_ok_and(|m| m.is_file())
        && sniff(&tag).is_some_and(|bytes| bytes.starts_with(CACHEDIR_SIGNATURE))
}

/// The start of a regular file, without following a symlink.
fn sniff(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(8 * 1024).read_to_end(&mut bytes).ok()?;
    Some(bytes)
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

//! Durable per-file apply/undo. A directory tree is never switched atomically.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use molt_api::fs::{
    AppliesRequest, AppliesResponse, ApplyOperation, ApplyPath, ApplyPathStatus, ApplyStatus, Change, RecoverRequest,
    RecoverResponse, RecoveryAction,
};
use molt_proto::RemoteError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{failed, invalid};
use crate::{files::temp_path, fork, paths, Roots};

const JOURNAL: &str = "journal.json";

fn storage(roots: &Roots) -> Result<PathBuf, RemoteError> {
    let data = roots.scratch.join(".molt");
    let dir = data.join("applies");
    for path in [&data, &dir] {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => sync_dir(path.parent().unwrap()).map_err(io_error)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(e)),
        }
        let meta = fs::symlink_metadata(path).map_err(io_error)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if !meta.is_dir()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.permissions().mode() & 0o077 != 0
            || path.canonicalize().ok().as_ref() != Some(path)
        {
            return Err(invalid("the apply data directory must be private and reached without symlinks"));
        }
    }
    Ok(dir)
}

/// Before/after identity includes all permission bits, not just executable state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum State {
    Missing,
    File { hash: String, bytes: u64, mode: u32 },
    Link { target: PathBuf },
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    before: State,
    after: State,
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    id: String,
    workspace: PathBuf,
    fork: PathBuf,
    candidate: String,
    status: ApplyStatus,
    entries: Vec<Entry>,
    error: Option<String>,
}

pub(crate) struct Candidate<'a> {
    pub workspace: &'a Path,
    pub fork: &'a Path,
    pub fingerprint: String,
    pub changes: &'a [&'a Change],
    pub version: u32,
    pub before: &'a std::collections::BTreeMap<String, String>,
    pub after: &'a std::collections::BTreeMap<String, String>,
}

fn fork_state(state: &State, version: u32) -> Option<String> {
    match state {
        State::Missing => None,
        State::File { hash, mode, .. } => {
            Some(if version > 0 && mode & 0o100 != 0 { format!("{hash}+x") } else { hash.clone() })
        }
        State::Link { target } => {
            Some(format!("link:{}", hex::encode(Sha256::digest(target.as_os_str().as_encoded_bytes()))))
        }
    }
}

pub(crate) struct ApplyLock(File);

impl Drop for ApplyLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork can inherit the descriptor briefly before exec,
        // even with CLOEXEC. Close alone would leave its shared flock held.
        // SAFETY: the owned file descriptor remains live until this Drop ends.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Serializes merge/recovery across fs service instances and offline commands.
pub(crate) fn lock(roots: &Roots) -> Result<ApplyLock, RemoteError> {
    let data = storage(roots)?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(data.parent().unwrap().join("apply.lock"))
        .map_err(io_error)?;
    use std::os::fd::AsRawFd;
    // SAFETY: f owns a live file descriptor.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            failed("another workspace apply/recovery is running; retry after it finishes")
        } else {
            io_error(error)
        });
    }
    Ok(ApplyLock(f))
}

fn io_error(e: io::Error) -> RemoteError {
    failed(format!("apply recovery: {e}"))
}

fn operation_dir(roots: &Roots, id: &str) -> Result<PathBuf, RemoteError> {
    if !id
        .strip_prefix("apply-")
        .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(invalid("invalid apply operation id"));
    }
    let dir = storage(roots)?.join(id);
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o077 == 0) {
        return Err(invalid(format!("{id} is not an apply operation")));
    }
    Ok(dir)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn durable_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(dest)?;
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dest)?;
        sync_dir(dest.parent().ok_or_else(|| io::Error::other("no parent"))?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn save(dir: &Path, record: &Record) -> io::Result<()> {
    durable_write(&dir.join(JOURNAL), &serde_json::to_vec_pretty(record).map_err(io::Error::other)?)
}

/// No parent symlink is followed, even when creating a missing directory.
fn destination(root: &Path, rel: &str, create: bool) -> io::Result<PathBuf> {
    let relative = paths::relative(rel).map_err(|e| io::Error::other(e.message))?;
    let mut dir = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            dir.push(part);
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(root.join(relative)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&dir)?;
                    sync_dir(dir.parent().ok_or_else(|| io::Error::other("no parent"))?)?;
                }
                Err(e) => return Err(e),
                _ => return Err(io::Error::other(format!("{rel}: parent is not a directory without symlinks"))),
            }
        }
    }
    Ok(root.join(relative))
}

/// Optionally copy regular-file contents into a private, fsynced staging blob.
fn capture(root: &Path, rel: &str, blob: Option<&Path>) -> io::Result<State> {
    let path = destination(root, rel, false)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State::Missing),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Ok(State::Link { target: fs::read_link(path)? });
    }
    if !meta.is_file() {
        return Err(io::Error::other(format!("{rel}: not a file or symlink")));
    }
    let mut f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("the file changed while staging"));
    }
    let mut out = blob.map(|p| OpenOptions::new().write(true).create_new(true).mode(0o600).open(p)).transpose()?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        bytes += n as u64;
        if let Some(o) = &mut out {
            o.write_all(&buf[..n])?;
        }
    }
    if let Some(o) = out {
        o.sync_all()?;
    }
    Ok(State::File { hash: hex::encode(hash.finalize()), bytes, mode: meta.permissions().mode() & 0o7777 })
}

fn mode_after(before: &State, after: &mut State, version: u32) {
    if let State::File { mode: wanted, .. } = after {
        *wanted = match before {
            State::File { mode, .. } if version == 0 => *mode,
            State::File { mode, .. } => fork::with_executable(*mode, *wanted & 0o100 != 0),
            _ => *wanted & 0o777,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    Prepared(usize),
    Staged,
    BeforeWrite(usize),
    AfterWrite(usize),
    Recorded(usize),
    Finished,
}

pub(crate) fn begin(roots: &Roots, candidate: Candidate<'_>, written: &mut Vec<String>) -> Result<String, RemoteError> {
    begin_with(roots, candidate, written, &mut |_| Ok(()))
}

fn begin_with(
    roots: &Roots,
    c: Candidate<'_>,
    written: &mut Vec<String>,
    hook: &mut impl FnMut(Point) -> io::Result<()>,
) -> Result<String, RemoteError> {
    let id = format!("apply-{:016x}", rand::random::<u64>());
    let store = storage(roots)?;
    let dir = store.join(&id);
    DirBuilder::new().mode(0o700).create(&dir).map_err(io_error)?;
    sync_dir(&store).map_err(io_error)?;
    let mut r = Record {
        version: 1,
        id: id.clone(),
        workspace: c.workspace.into(),
        fork: c.fork.into(),
        candidate: c.fingerprint,
        status: ApplyStatus::Preparing,
        entries: Vec::new(),
        error: None,
    };
    save(&dir, &r).map_err(io_error)?;
    let stage = (|| {
        for (i, change) in c.changes.iter().enumerate() {
            let before = capture(c.workspace, &change.path, Some(&dir.join(format!("{i}.before"))))?;
            let mut after = capture(c.fork, &change.path, Some(&dir.join(format!("{i}.after"))))?;
            if fork_state(&before, c.version).as_ref() != c.before.get(&change.path)
                || fork_state(&after, c.version).as_ref() != c.after.get(&change.path)
            {
                return Err(io::Error::other(format!("conflict: {} changed while staging", change.path)));
            }
            mode_after(&before, &mut after, c.version);
            r.entries.push(Entry { path: change.path.clone(), before, after });
            sync_dir(&dir)?;
            save(&dir, &r)?;
            hook(Point::Prepared(i))?;
        }
        if fork::fingerprint(roots, &r.fork.to_string_lossy()).map_err(|e| io::Error::other(e.message))? != r.candidate
        {
            return Err(io::Error::other("candidate fork changed while staging; no workspace writes were made"));
        }
        r.status = ApplyStatus::Staged;
        save(&dir, &r)?;
        hook(Point::Staged)
    })();
    if let Err(e) = stage {
        return Err(failed(format!("operation {id}: staging failed ({e}); no workspace writes were made")));
    }
    execute(&dir, &mut r, RecoveryAction::Resume, written, hook).map_err(|e| {
        let prefix = if written.is_empty() { "" } else { "partial: " };
        failed(format!("{prefix}operation {id}: {e}; inspect it and choose resume or undo"))
    })?;
    Ok(id)
}

fn load(roots: &Roots, id: &str, workspace: &Path) -> Result<(PathBuf, Record), RemoteError> {
    let dir = operation_dir(roots, id)?;
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(dir.join(JOURNAL)).map_err(io_error)?;
    let r: Record = serde_json::from_reader(f).map_err(|e| failed(format!("invalid apply journal: {e}")))?;
    if r.version != 1
        || r.id != id
        || r.workspace != workspace
        || r.workspace.canonicalize().ok().as_deref() != Some(workspace)
        || r.fork.parent() != Some(roots.scratch.as_path())
        || !r.fork.file_name().and_then(|n| n.to_str()).is_some_and(fork::is_fork_name)
    {
        return Err(invalid("the operation does not belong to this workspace or journal version"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for e in &r.entries {
        if e.path.is_empty() || paths::relative(&e.path).is_err() || !seen.insert(&e.path) {
            return Err(invalid("invalid path in apply journal"));
        }
    }
    Ok((dir, r))
}

fn summary(r: &Record) -> ApplyOperation {
    ApplyOperation {
        id: r.id.clone(),
        workspace: r.workspace.to_string_lossy().into(),
        fork: r.fork.to_string_lossy().into(),
        candidate: r.candidate.clone(),
        status: r.status,
        error: r.error.clone(),
        paths: r
            .entries
            .iter()
            .map(|e| {
                let status = match capture(&r.workspace, &e.path, None) {
                    Ok(s) if s == e.after => ApplyPathStatus::Applied,
                    Ok(s) if s == e.before => ApplyPathStatus::Pending,
                    _ => ApplyPathStatus::Conflict,
                };
                ApplyPath { path: e.path.clone(), status }
            })
            .collect(),
    }
}

pub(crate) fn list(roots: &Roots, req: AppliesRequest) -> Result<AppliesResponse, RemoteError> {
    let workspace = roots.workspace(&req.workspace)?;
    let mut ids = match req.operation {
        Some(id) => vec![id],
        None => fs::read_dir(storage(roots)?)
            .map_err(io_error)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|s| s.starts_with("apply-"))
            .collect(),
    };
    ids.sort();
    let mut operations = Vec::new();
    for id in ids {
        // Other workspaces' operations are private to their workspace authority.
        let dir = operation_dir(roots, &id)?;
        let f =
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(dir.join(JOURNAL)).map_err(io_error)?;
        let r: Record = serde_json::from_reader(f).map_err(|e| failed(format!("invalid apply journal {id}: {e}")))?;
        if r.workspace != workspace {
            continue;
        }
        let (_, r) = load(roots, &id, &workspace)?;
        operations.push(summary(&r));
    }
    Ok(AppliesResponse { operations })
}

fn restore(root: &Path, rel: &str, state: &State, blob: &Path) -> io::Result<()> {
    let path = destination(root, rel, !matches!(state, State::Missing))?;
    if matches!(state, State::Missing) {
        match fs::remove_file(&path) {
            Ok(()) => sync_dir(path.parent().ok_or_else(|| io::Error::other("no parent"))?)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        let mut parent = path.parent().ok_or_else(|| io::Error::other("no parent"))?;
        while parent != root && fs::remove_dir(parent).is_ok() {
            parent = parent.parent().ok_or_else(|| io::Error::other("no parent"))?;
            sync_dir(parent)?;
        }
        return Ok(());
    }
    let tmp = temp_path(&path)?;
    let result = (|| {
        match state {
            State::Link { target } => symlink(target, &tmp)?,
            State::File { mode, .. } => {
                let mut src = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(blob)?;
                let mut dest = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
                io::copy(&mut src, &mut dest)?;
                dest.set_permissions(Permissions::from_mode(*mode))?;
                dest.sync_all()?;
                // Validate the blob before it can replace any user file.
                if capture(
                    tmp.parent().unwrap(),
                    tmp.file_name().unwrap().to_str().ok_or_else(|| io::Error::other("invalid staging name"))?,
                    None,
                )? != *state
                {
                    return Err(io::Error::other("staged contents are corrupt"));
                }
            }
            State::Missing => unreachable!(),
        }
        fs::rename(&tmp, &path)?;
        sync_dir(path.parent().ok_or_else(|| io::Error::other("no parent"))?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn execute(
    dir: &Path,
    r: &mut Record,
    action: RecoveryAction,
    written: &mut Vec<String>,
    hook: &mut impl FnMut(Point) -> io::Result<()>,
) -> io::Result<()> {
    // Inspect every path before any write, then inspect it again at its write
    // boundary. This refuses edits observed between attempts; callers must
    // exclude simultaneous external writers during the actual apply.
    let conflicts: Vec<_> = r
        .entries
        .iter()
        .filter_map(|e| match capture(&r.workspace, &e.path, None) {
            Ok(s) if s == e.before || s == e.after => None,
            _ => Some(e.path.clone()),
        })
        .collect();
    if !conflicts.is_empty() {
        r.status = ApplyStatus::NeedsRecovery;
        r.error = Some(format!("conflict: {}", conflicts.join(", ")));
        save(dir, r)?;
        return Err(io::Error::other(r.error.clone().unwrap()));
    }
    r.status = match action {
        RecoveryAction::Resume => ApplyStatus::Applying,
        RecoveryAction::Undo => ApplyStatus::Undoing,
    };
    r.error = None;
    save(dir, r)?;
    let indices: Vec<_> = match action {
        RecoveryAction::Resume => (0..r.entries.len()).collect(),
        RecoveryAction::Undo => (0..r.entries.len()).rev().collect(),
    };
    let result = (|| {
        for i in indices {
            let e = &r.entries[i];
            let (wanted, other, suffix) = match action {
                RecoveryAction::Resume => (&e.after, &e.before, "after"),
                RecoveryAction::Undo => (&e.before, &e.after, "before"),
            };
            let current = capture(&r.workspace, &e.path, None)?;
            if current == *wanted {
                continue;
            }
            if current != *other {
                return Err(io::Error::other(format!("conflict: {}", e.path)));
            }
            hook(Point::BeforeWrite(i))?;
            written.push(e.path.clone());
            restore(&r.workspace, &e.path, wanted, &dir.join(format!("{i}.{suffix}")))?;
            hook(Point::AfterWrite(i))?;
            save(dir, r)?;
            hook(Point::Recorded(i))?;
        }
        r.status = match action {
            RecoveryAction::Resume => ApplyStatus::Applied,
            RecoveryAction::Undo => ApplyStatus::Undone,
        };
        save(dir, r)?;
        hook(Point::Finished)
    })();
    if let Err(e) = &result {
        r.status = ApplyStatus::NeedsRecovery;
        r.error = Some(e.to_string());
        let _ = save(dir, r);
    }
    result
}

pub(crate) fn recover(
    roots: &Roots,
    req: RecoverRequest,
    written: &mut Vec<String>,
) -> Result<RecoverResponse, RemoteError> {
    let _lock = lock(roots)?;
    let workspace = roots.workspace(&req.workspace)?;
    let (dir, mut r) = load(roots, &req.operation, &workspace)?;
    if r.status == ApplyStatus::Preparing {
        if req.action == RecoveryAction::Resume {
            return Err(failed("staging did not finish; no workspace writes were made, start a new merge"));
        }
        r.status = ApplyStatus::Undone;
        save(&dir, &r).map_err(io_error)?;
        return Ok(RecoverResponse { operation: summary(&r) });
    }
    let terminal = match req.action {
        RecoveryAction::Resume => ApplyStatus::Applied,
        RecoveryAction::Undo => ApplyStatus::Undone,
    };
    if r.status == terminal {
        let s = summary(&r);
        let expected = match req.action {
            RecoveryAction::Resume => ApplyPathStatus::Applied,
            RecoveryAction::Undo => ApplyPathStatus::Pending,
        };
        if s.paths.iter().any(|p| p.status != expected) {
            let conflicts: Vec<_> = s.paths.iter().filter(|p| p.status != expected).map(|p| p.path.as_str()).collect();
            return Err(failed(format!(
                "conflict: files changed after this operation finished: {}",
                conflicts.join(", ")
            )));
        }
        return Ok(RecoverResponse { operation: s });
    }
    if req.action == RecoveryAction::Resume {
        match fs::symlink_metadata(&r.fork) {
            Ok(_) if fork::fingerprint(roots, &r.fork.to_string_lossy())? != r.candidate => {
                return Err(failed(
                    "candidate fork changed; refusing to resume a different candidate, choose undo instead",
                ));
            }
            // Run cleanup may have removed the source after a crash. The
            // immutable, checksum-verified postimages still identify exactly
            // what this operation originally planned to write.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
            _ => {}
        }
    }
    execute(&dir, &mut r, req.action, written, &mut |_| Ok(()))
        .map_err(|e| failed(format!("partial: operation {}: {e}", r.id)))?;
    Ok(RecoverResponse { operation: summary(&r) })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;

    use super::*;
    use molt_api::fs::{ChangeKind, ForkRequest};

    struct Setup {
        _tmp: tempfile::TempDir,
        roots: Roots,
        source: PathBuf,
    }

    impl Setup {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().join("project");
            fs::create_dir(&workspace).unwrap();
            for n in 0..3 {
                fs::write(workspace.join(format!("{n}.txt")), format!("before {n}\n")).unwrap();
            }
            let roots = Roots { root: workspace, scratch: tmp.path().join("work") }.canonical().unwrap();
            let source = PathBuf::from(
                fork::fork(&roots, ForkRequest { workspace: roots.root.to_string_lossy().into() }).unwrap().fork,
            );
            for n in 0..3 {
                fs::write(source.join(format!("{n}.txt")), format!("after {n}\n")).unwrap();
            }
            Self { _tmp: tmp, roots, source }
        }

        fn inject(&self, point: Point) -> String {
            let error = start(&self.roots, &self.source, &mut |at| {
                if at == point {
                    Err(io::Error::other("injected disk failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(error.message.contains("operation apply-"), "{error:?}");
            self.operation().id
        }

        fn operation(&self) -> ApplyOperation {
            list(&self.roots, AppliesRequest { workspace: self.roots.root.to_string_lossy().into(), operation: None })
                .unwrap()
                .operations
                .into_iter()
                .next()
                .unwrap()
        }

        fn recover(&self, id: &str, action: RecoveryAction) -> Result<RecoverResponse, RemoteError> {
            recover(
                &self.roots,
                RecoverRequest { workspace: self.roots.root.to_string_lossy().into(), operation: id.into(), action },
                &mut Vec::new(),
            )
        }

        fn assert_files(&self, after: bool) {
            for n in 0..3 {
                assert_eq!(
                    fs::read_to_string(self.roots.root.join(format!("{n}.txt"))).unwrap(),
                    format!("{} {n}\n", if after { "after" } else { "before" })
                );
            }
        }
    }

    fn start(
        roots: &Roots,
        source: &Path,
        hook: &mut impl FnMut(Point) -> io::Result<()>,
    ) -> Result<String, RemoteError> {
        let changes: Vec<_> = (0..3).map(|n| Change { path: format!("{n}.txt"), kind: ChangeKind::Modified }).collect();
        let refs: Vec<_> = changes.iter().collect();
        let inventory = |root: &Path| -> BTreeMap<String, String> {
            changes
                .iter()
                .map(|c| {
                    let state = capture(root, &c.path, None).unwrap();
                    (c.path.clone(), fork_state(&state, 1).unwrap())
                })
                .collect()
        };
        let before = inventory(&roots.root);
        let after = inventory(source);
        begin_with(
            roots,
            Candidate {
                workspace: &roots.root,
                fork: source,
                fingerprint: fork::fingerprint(roots, &source.to_string_lossy()).unwrap(),
                changes: &refs,
                version: 1,
                before: &before,
                after: &after,
            },
            &mut Vec::new(),
            hook,
        )
    }

    #[test]
    fn first_middle_and_last_write_failures_can_resume_or_undo_idempotently() {
        for point in [
            Point::BeforeWrite(0),
            Point::BeforeWrite(1),
            Point::BeforeWrite(2),
            Point::AfterWrite(0),
            Point::AfterWrite(1),
            Point::AfterWrite(2),
            Point::Recorded(0),
            Point::Recorded(1),
            Point::Recorded(2),
        ] {
            for action in [RecoveryAction::Resume, RecoveryAction::Undo] {
                let s = Setup::new();
                let id = s.inject(point);
                assert_eq!(s.operation().status, ApplyStatus::NeedsRecovery);
                for _ in 0..2 {
                    s.recover(&id, action).unwrap();
                }
                s.assert_files(action == RecoveryAction::Resume);
            }
        }
    }

    #[test]
    fn later_edits_and_candidate_changes_are_preserved_and_reported_by_path() {
        let s = Setup::new();
        let id = s.inject(Point::AfterWrite(0));
        fs::write(s.roots.root.join("0.txt"), "user's later edit\n").unwrap();
        for action in [RecoveryAction::Resume, RecoveryAction::Undo] {
            let e = s.recover(&id, action).unwrap_err();
            assert!(e.message.contains("conflict: 0.txt"), "{e:?}");
        }
        assert_eq!(fs::read_to_string(s.roots.root.join("0.txt")).unwrap(), "user's later edit\n");
        assert_eq!(fs::read_to_string(s.roots.root.join("1.txt")).unwrap(), "before 1\n");
        assert_eq!(s.operation().paths[0].status, ApplyPathStatus::Conflict);

        let s = Setup::new();
        let id = s.inject(Point::AfterWrite(0));
        fs::write(s.source.join("1.txt"), "a different candidate\n").unwrap();
        assert!(s.recover(&id, RecoveryAction::Resume).unwrap_err().message.contains("candidate fork changed"));
        s.recover(&id, RecoveryAction::Undo).unwrap();
        s.assert_files(false);
    }

    #[test]
    fn corrupt_staged_contents_never_replace_a_workspace_file() {
        let s = Setup::new();
        let id = s.inject(Point::BeforeWrite(0));
        fs::write(operation_dir(&s.roots, &id).unwrap().join("0.after"), "corrupt\n").unwrap();
        assert!(s.recover(&id, RecoveryAction::Resume).unwrap_err().message.contains("staged contents are corrupt"));
        s.assert_files(false);
        s.recover(&id, RecoveryAction::Undo).unwrap();
    }

    #[test]
    fn a_candidates_non_executable_permission_change_also_refuses_resume() {
        let s = Setup::new();
        let id = s.inject(Point::AfterWrite(0));
        fs::set_permissions(s.source.join("1.txt"), Permissions::from_mode(0o600)).unwrap();
        assert!(s.recover(&id, RecoveryAction::Resume).unwrap_err().message.contains("candidate fork changed"));
        s.recover(&id, RecoveryAction::Undo).unwrap();
        s.assert_files(false);
    }

    #[test]
    fn staging_rechecks_the_baseline_before_any_workspace_write() {
        let s = Setup::new();
        let e = start(&s.roots, &s.source, &mut |point| {
            if point == Point::Prepared(0) {
                fs::write(s.roots.root.join("1.txt"), "user's concurrent edit\n")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(e.message.contains("staging failed") && e.message.contains("conflict: 1.txt"), "{e:?}");
        assert_eq!(fs::read_to_string(s.roots.root.join("0.txt")).unwrap(), "before 0\n");
        assert_eq!(fs::read_to_string(s.roots.root.join("1.txt")).unwrap(), "user's concurrent edit\n");
        assert_eq!(fs::read_to_string(s.roots.root.join("2.txt")).unwrap(), "before 2\n");
    }

    #[test]
    fn adding_another_candidate_file_during_staging_cannot_change_the_approved_plan() {
        let s = Setup::new();
        let e = start(&s.roots, &s.source, &mut |point| {
            if point == Point::Prepared(0) {
                fs::write(s.source.join("new.txt"), "different candidate\n")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(e.message.contains("candidate fork changed while staging"), "{e:?}");
        s.assert_files(false);
        assert!(!s.roots.root.join("new.txt").exists());
    }

    #[test]
    fn permission_edits_conflict_and_operation_ids_cannot_escape_scratch() {
        let s = Setup::new();
        let id = s.inject(Point::AfterWrite(0));
        fs::set_permissions(s.roots.root.join("0.txt"), Permissions::from_mode(0o600)).unwrap();
        assert!(s.recover(&id, RecoveryAction::Undo).unwrap_err().message.contains("conflict: 0.txt"));
        assert_eq!(fs::metadata(s.roots.root.join("0.txt")).unwrap().permissions().mode() & 0o777, 0o600);
        for id in ["../", "/tmp", "apply-../../", "apply-000000000000000G"] {
            assert!(s.recover(id, RecoveryAction::Undo).is_err());
        }
        let guard = lock(&s.roots).unwrap();
        assert!(s.recover(&id, RecoveryAction::Undo).unwrap_err().message.contains("another workspace apply"));
        drop(guard);
    }

    #[test]
    fn an_inherited_descriptor_cannot_hold_the_lock_after_the_operation_finishes() {
        let s = Setup::new();
        let guard = lock(&s.roots).unwrap();
        let inherited = guard.0.try_clone().unwrap();
        drop(guard);
        let next = lock(&s.roots).unwrap();
        drop(next);
        drop(inherited);
    }

    // The parent launches this exact test to stop without running destructors.
    #[test]
    fn crash_child() {
        let Ok(root) = std::env::var("MOLT_APPLY_CRASH_ROOT") else { return };
        let source = PathBuf::from(std::env::var("MOLT_APPLY_CRASH_SOURCE").unwrap());
        let target = std::env::var("MOLT_APPLY_CRASH_POINT").unwrap();
        let root = PathBuf::from(root);
        let roots = Roots { root: root.join("project"), scratch: root.join("work") }.canonical().unwrap();
        start(&roots, &source, &mut |point| {
            if format!("{point:?}") == target {
                std::process::exit(86);
            }
            Ok(())
        })
        .unwrap();
        panic!("the requested crash boundary did not run");
    }

    #[test]
    fn restarting_after_each_durable_stage_observes_disk_truth_and_recovers() {
        let points = [
            Point::Prepared(0),
            Point::Prepared(2),
            Point::Staged,
            Point::BeforeWrite(0),
            Point::BeforeWrite(1),
            Point::BeforeWrite(2),
            Point::AfterWrite(0),
            Point::AfterWrite(1),
            Point::AfterWrite(2),
            Point::Recorded(0),
            Point::Recorded(1),
            Point::Recorded(2),
            Point::Finished,
        ];
        for point in points {
            for action in [RecoveryAction::Resume, RecoveryAction::Undo] {
                let s = Setup::new();
                let out = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "apply::tests::crash_child", "--nocapture"])
                    .env("MOLT_APPLY_CRASH_ROOT", s._tmp.path())
                    .env("MOLT_APPLY_CRASH_SOURCE", &s.source)
                    .env("MOLT_APPLY_CRASH_POINT", format!("{point:?}"))
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(86), "{point:?}: {}", String::from_utf8_lossy(&out.stderr));
                let record = s.operation();
                if matches!(point, Point::Prepared(_)) && action == RecoveryAction::Resume {
                    assert!(s.recover(&record.id, action).unwrap_err().message.contains("staging did not finish"));
                    s.assert_files(false);
                } else {
                    for _ in 0..2 {
                        s.recover(&record.id, action).unwrap();
                    }
                    s.assert_files(action == RecoveryAction::Resume);
                }
            }
        }
    }
}

//! Saved outputs (#238): the whole output of a command whose reply had to
//! cut it, and of a process that printed more than its ring keeps, on disk
//! under an opaque id, for `shell.output` to read a page at a time, search
//! or follow.
//!
//! The service saves an output only once it outgrows what the reply or the
//! ring carries, so a short command writes nothing. Every output it saved
//! counts against a quota: one output may hold [`OutputLimits::per_output`]
//! bytes, all of them together [`OutputLimits::quota`], and the disk must
//! keep [`OutputLimits::min_free`]. Room is reserved a block at a time;
//! when the quota is reached the oldest outputs whose commands ended are
//! deleted first, and when nothing can be, the output stops growing and
//! says why ([`OutputCut`]). An output is kept [`OutputLimits::kept`] after
//! its command ended. One deleted, for either reason, is remembered as gone,
//! so a read says it expired or was evicted instead of that it never was.
//!
//! An output belongs to the caller and the workspace whose command printed
//! it, as a process does (see `process`): anyone else is refused. The files
//! live in `<scratch>/.molt/output/<pid>-<random>`, which no workspace can
//! name and the sandbox does not mount; a service removes its directory
//! when it stops, and the directories of services no longer running when
//! it starts.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use molt_api::shell::{
    OutputCut, OutputEnd, OutputMatch, OutputPage, OutputRequest, OutputStream, SavedOutput, MAX_OUTPUT_MATCHES,
    MAX_OUTPUT_PAGE, MAX_OUTPUT_SEARCH,
};
use molt_proto::{ErrorCode, RemoteError};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::build_dir::private_dir;
use crate::error::{failed, invalid};
use crate::shell::lock;
use crate::Roots;

/// Room is reserved this much at a time, and the disk's free space looked at as often.
const BLOCK: u64 = 1 << 20;
/// Text a read returns when it does not say.
const DEFAULT_PAGE: u32 = 16 << 10;
/// Matches a search returns when it does not say.
const DEFAULT_MATCHES: u32 = 20;
/// The longest a read waits for an open output to grow.
const MAX_WAIT_MS: u64 = 30_000;
/// The most of a matching line a search returns.
const MATCH_LINE: usize = 400;
/// Outputs remembered as gone.
const GONE_KEPT: usize = 4096;

/// How much the shell service may keep of the outputs it saves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLimits {
    /// The most bytes one output keeps.
    pub per_output: u64,
    /// The most bytes all outputs together keep.
    pub quota: u64,
    /// The most outputs kept at once.
    pub max_outputs: usize,
    /// How long an output is kept after its command ended.
    pub kept: Duration,
    /// Free space the disk keeps: an output stops growing below it.
    pub min_free: u64,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            per_output: 64 << 20,
            quota: 512 << 20,
            max_outputs: 256,
            kept: Duration::from_secs(2 * 60 * 60),
            min_free: 256 << 20,
        }
    }
}

/// Who ran the command whose output it is, and where.
#[derive(Clone, Debug)]
pub(crate) struct Origin {
    /// The caller the kernel named.
    pub caller: Option<String>,
    /// The canonical workspace.
    pub ws: PathBuf,
    /// The workspace as the request named it, which still matches once the directory is gone.
    pub given: String,
    pub command: String,
    pub trace: Option<String>,
    pub request: Option<String>,
}

/// Who asks: the caller the kernel named, and the request, for the audit log.
#[derive(Clone, Debug, Default)]
pub(crate) struct Asker {
    pub caller: Option<String>,
    pub trace: Option<String>,
    pub request: Option<String>,
}

impl Asker {
    /// Who ran `command` in the canonical workspace `ws`, which the request named `given`.
    pub(crate) fn origin(&self, ws: &Path, given: &str, command: &str) -> Origin {
        Origin {
            caller: self.caller.clone(),
            ws: ws.to_path_buf(),
            given: given.to_owned(),
            command: command.to_owned(),
            trace: self.trace.clone(),
            request: self.request.clone(),
        }
    }
}

impl Origin {
    fn owned_by(&self, caller: Option<&str>, workspace: &str, ws: Option<&Path>) -> bool {
        self.caller.as_deref() == caller && (self.given == workspace || ws == Some(self.ws.as_path()))
    }
}

/// Where the outputs go.
pub(crate) struct Store {
    dir: PathBuf,
    limits: OutputLimits,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    slots: HashMap<String, Slot>,
    /// Bytes reserved by all of them.
    used: u64,
    gone: VecDeque<(String, Arc<Origin>, Gone)>,
}

struct Slot {
    entry: Arc<Entry>,
    reserved: u64,
    /// When its command ended.
    closed_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
enum Gone {
    Expired,
    Evicted,
}

struct Entry {
    id: String,
    stream: OutputStream,
    origin: Arc<Origin>,
    path: PathBuf,
    meta: Mutex<Meta>,
    /// Woken when it grows and when it closes.
    changed: Notify,
}

#[derive(Default)]
struct Meta {
    total: u64,
    stored: u64,
    cut: Option<OutputCut>,
    open: bool,
    end: Option<OutputEnd>,
    sha256: Option<String>,
}

impl Entry {
    fn saved(&self, kept: Duration) -> SavedOutput {
        let meta = lock(&self.meta);
        SavedOutput {
            id: self.id.clone(),
            stream: self.stream,
            command: self.origin.command.clone(),
            total: meta.total,
            stored: meta.stored,
            cut: meta.cut,
            open: meta.open,
            end: meta.end,
            sha256: meta.sha256.clone(),
            trace: self.origin.trace.clone(),
            request: self.origin.request.clone(),
            kept_s: kept.as_secs(),
        }
    }
}

/// An output as it stands, to put in a reply.
#[derive(Clone)]
pub(crate) struct Handle {
    entry: Arc<Entry>,
    kept: Duration,
}

impl Handle {
    pub(crate) fn saved(&self) -> SavedOutput {
        self.entry.saved(self.kept)
    }
}

impl Store {
    /// A store of its own in `scratch`. The directories of services that
    /// no longer run are removed: nobody can name their outputs any more.
    pub(crate) fn open(scratch: &Path, limits: OutputLimits) -> io::Result<Self> {
        let data = scratch.join(".molt");
        let base = data.join("output");
        private_dir(&data)?;
        private_dir(&base)?;
        let own = std::process::id();
        for dir in fs::read_dir(&base)?.flatten() {
            let name = dir.file_name();
            let pid = name.to_str().and_then(|n| n.split('-').next()).and_then(|p| p.parse::<i32>().ok());
            if pid.is_some_and(|pid| pid != own as i32 && !alive(pid)) {
                let _ = fs::remove_dir_all(dir.path());
            }
        }
        let dir = base.join(format!("{own}-{:08x}", rand::random::<u32>()));
        private_dir(&dir)?;
        Ok(Self { dir, limits, state: Mutex::default() })
    }

    /// Start saving an output of `origin`'s command with what it printed so
    /// far. None when no more outputs may be kept, or the file cannot be made.
    pub(crate) fn create(self: &Arc<Self>, origin: Arc<Origin>, stream: OutputStream, first: &[u8]) -> Option<Writer> {
        let id = format!("out_{:032x}", rand::random::<u128>());
        let path = self.dir.join(&id);
        let entry = {
            let mut state = lock(&self.state);
            self.prune(&mut state);
            while state.slots.len() >= self.limits.max_outputs {
                if !self.evict_one(&mut state, &id) {
                    tracing::debug!("not saving an output: every output kept is still being written");
                    return None;
                }
            }
            let file =
                OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path);
            let file = match file {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not save a command's output");
                    return None;
                }
            };
            let meta = Meta { open: true, ..Meta::default() };
            let entry = Arc::new(Entry {
                id: id.clone(),
                stream,
                origin,
                path,
                meta: Mutex::new(meta),
                changed: Notify::new(),
            });
            state.slots.insert(id, Slot { entry: entry.clone(), reserved: 0, closed_at: None });
            (entry, file)
        };
        let (entry, file) = entry;
        let mut writer = Writer { entry, store: self.clone(), file, hasher: Sha256::new(), reserved: 0, closed: false };
        writer.write(first);
        Some(writer)
    }

    /// Reserve `wanted` more bytes for output `id`, or as much as there is
    /// room for: what it got, and why it got less than `needed` when it did.
    fn reserve(&self, id: &str, needed: u64, wanted: u64) -> (u64, Option<OutputCut>) {
        let mut state = lock(&self.state);
        self.prune(&mut state);
        if free_space(&self.dir).is_some_and(|free| free < self.limits.min_free.saturating_add(needed)) {
            return (0, Some(OutputCut::DiskLow));
        }
        while state.used.saturating_add(needed) > self.limits.quota {
            if !self.evict_one(&mut state, id) {
                break;
            }
        }
        let room = self.limits.quota.saturating_sub(state.used);
        let granted = wanted.min(room);
        let Some(slot) = state.slots.get_mut(id) else { return (0, Some(OutputCut::QuotaFull)) };
        slot.reserved += granted;
        state.used += granted;
        (granted, (granted < needed).then_some(OutputCut::QuotaFull))
    }

    /// Output `id`'s command ended with `stored` bytes saved: give back the rest of its room.
    fn settle(&self, id: &str, stored: u64) {
        let mut state = lock(&self.state);
        let Some(slot) = state.slots.get_mut(id) else { return };
        let spare = slot.reserved.saturating_sub(stored);
        slot.reserved -= spare;
        slot.closed_at = Some(Instant::now());
        state.used -= spare;
    }

    /// Forget the outputs kept long enough.
    fn prune(&self, state: &mut State) {
        let old: Vec<String> = state
            .slots
            .iter()
            .filter(|(_, slot)| slot.closed_at.is_some_and(|at| at.elapsed() >= self.limits.kept))
            .map(|(id, _)| id.clone())
            .collect();
        for id in old {
            Self::remove(state, &id, Gone::Expired);
        }
    }

    /// Delete the output that ended longest ago, but not `except`; false when there is none.
    fn evict_one(&self, state: &mut State, except: &str) -> bool {
        let oldest = state
            .slots
            .iter()
            .filter(|(id, _)| id.as_str() != except)
            .filter_map(|(id, slot)| slot.closed_at.map(|at| (at, id.clone())))
            .min()
            .map(|(_, id)| id);
        match oldest {
            Some(id) => {
                Self::remove(state, &id, Gone::Evicted);
                true
            }
            None => false,
        }
    }

    fn remove(state: &mut State, id: &str, why: Gone) {
        let Some(slot) = state.slots.remove(id) else { return };
        state.used -= slot.reserved;
        if let Err(e) = fs::remove_file(&slot.entry.path) {
            tracing::debug!(output = id, error = %e, "could not delete a saved output");
        }
        state.gone.push_back((id.to_owned(), slot.entry.origin.clone(), why));
        while state.gone.len() > GONE_KEPT {
            state.gone.pop_front();
        }
    }

    /// The output `id` that `caller`'s command in `workspace` printed.
    fn find(&self, roots: &Roots, caller: Option<&str>, workspace: &str, id: &str) -> Result<Arc<Entry>, RemoteError> {
        let ws = roots.workspace(workspace).ok();
        let denied = || RemoteError {
            code: ErrorCode::Denied,
            message: format!(
                "output {id} was printed by a command of another workspace or caller: only the work that ran the \
                 command may read it"
            ),
        };
        let mut state = lock(&self.state);
        self.prune(&mut state);
        if let Some(slot) = state.slots.get(id) {
            return match slot.entry.origin.owned_by(caller, workspace, ws.as_deref()) {
                true => Ok(slot.entry.clone()),
                false => Err(denied()),
            };
        }
        if let Some((_, origin, why)) = state.gone.iter().rev().find(|(gone, ..)| gone == id) {
            if !origin.owned_by(caller, workspace, ws.as_deref()) {
                return Err(denied());
            }
            let how = match why {
                Gone::Expired => format!(
                    "it was kept {} minutes after its command ended and then deleted",
                    self.limits.kept.as_secs() / 60
                ),
                Gone::Evicted => {
                    "it was deleted to keep the shell service's saved outputs under their disk quota".to_owned()
                }
            };
            return Err(invalid(format!(
                "output {id} is gone: {how}, so what it held cannot be read. Run the command again if you need it"
            )));
        }
        Err(invalid(format!(
            "there is no saved output {id}: it was never saved, or the shell service restarted since and its \
             outputs went with it"
        )))
    }

    /// Answer `req` for `caller`.
    pub(crate) async fn page(
        &self,
        roots: &Roots,
        caller: Option<&str>,
        req: OutputRequest,
        cancel: CancellationToken,
    ) -> Result<OutputPage, RemoteError> {
        let search = req.search.filter(|s| !s.is_empty());
        if search.as_ref().is_some_and(|s| s.len() > MAX_OUTPUT_SEARCH) {
            return Err(invalid(format!(
                "search is over {MAX_OUTPUT_SEARCH} bytes: look for a shorter piece of the line"
            )));
        }
        let entry = self.find(roots, caller, &req.workspace, &req.id)?;
        let offset = req.offset.unwrap_or(0);
        let wait = Duration::from_millis(req.wait_ms.unwrap_or(0).min(MAX_WAIT_MS));
        if search.is_none() && !wait.is_zero() {
            wait_past(&entry, offset, wait, &cancel).await;
        }
        let saved = entry.saved(self.limits.kept);
        if offset > saved.stored {
            return Err(invalid(format!(
                "offset {offset} is past the end of output {}: {} bytes of it are saved",
                saved.id, saved.stored
            )));
        }
        let path = entry.path.clone();
        let read = match search {
            Some(text) => {
                let max = req.max_matches.unwrap_or(DEFAULT_MATCHES).clamp(1, MAX_OUTPUT_MATCHES) as usize;
                let (stored, ci) = (saved.stored, req.case_insensitive);
                tokio::task::spawn_blocking(move || search_file(&path, stored, offset, &text, ci, max)).await
            }
            None => {
                let limit = u64::from(req.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_OUTPUT_PAGE));
                let len = limit.min(saved.stored - offset);
                tokio::task::spawn_blocking(move || read_file(&path, offset, len)).await
            }
        };
        let found = read.map_err(|e| failed(format!("shell.output failed: {e}")))?;
        let (text, matches, next, more) = found.map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                invalid(format!("output {} is gone: it was deleted while it was read; run the command again", saved.id))
            } else {
                failed(format!("reading output {}: {e}", saved.id))
            }
        })?;
        Ok(OutputPage { output: saved, offset, next, text, matches, more })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Wait until `entry` holds more than `offset` bytes, closes, `wait` passes or `cancel` fires.
async fn wait_past(entry: &Entry, offset: u64, wait: Duration, cancel: &CancellationToken) {
    let until = tokio::time::Instant::now() + wait;
    loop {
        let changed = entry.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let meta = lock(&entry.meta);
            if !meta.open || meta.stored > offset {
                return;
            }
        }
        tokio::select! {
            _ = &mut changed => {}
            _ = tokio::time::sleep_until(until) => return,
            _ = cancel.cancelled() => return,
        }
    }
}

type Found = (String, Vec<OutputMatch>, u64, bool);

fn read_file(path: &Path, offset: u64, len: u64) -> io::Result<Found> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut bytes)?;
    let next = offset + bytes.len() as u64;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), Vec::new(), next, false))
}

/// The lines of the first `stored` bytes of `path` that start at `offset`
/// or later and hold `text`, at most `max`, numbered from the first line.
fn search_file(path: &Path, stored: u64, offset: u64, text: &str, ci: bool, max: usize) -> io::Result<Found> {
    let pattern = regex::bytes::RegexBuilder::new(&regex::escape(text))
        .case_insensitive(ci)
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut reader = BufReader::new(File::open(path)?.take(stored));
    let (mut line, mut at, mut number) = (Vec::new(), 0u64, 0u64);
    let mut matches = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok((String::new(), matches, stored, false));
        }
        number += 1;
        let start = at;
        at += n as u64;
        if start < offset {
            continue;
        }
        if let Some(found) = pattern.find(&line) {
            matches.push(OutputMatch { line: number, offset: start, text: around(&line, found.start()) });
            if matches.len() >= max {
                return Ok((String::new(), matches, at, at < stored));
            }
        }
    }
}

/// `line` without its newline, or a piece of it around byte `at` when it is long.
fn around(line: &[u8], at: usize) -> String {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() <= MATCH_LINE {
        return String::from_utf8_lossy(line).into_owned();
    }
    let start = at.saturating_sub(MATCH_LINE / 4).min(line.len() - MATCH_LINE);
    let end = start + MATCH_LINE;
    let mut out = String::new();
    if start > 0 {
        out.push_str("[...] ");
    }
    out.push_str(&String::from_utf8_lossy(&line[start..end]));
    if end < line.len() {
        out.push_str(" [...]");
    }
    out
}

/// Writes one output, and closes it when dropped.
pub(crate) struct Writer {
    entry: Arc<Entry>,
    store: Arc<Store>,
    file: File,
    hasher: Sha256,
    /// Bytes reserved for it so far.
    reserved: u64,
    closed: bool,
}

impl Writer {
    pub(crate) fn handle(&self) -> Handle {
        Handle { entry: self.entry.clone(), kept: self.store.limits.kept }
    }

    /// Save `bytes`, or as many as its limits allow; the rest is counted, and the output marked cut.
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        let (stored, mut cut) = {
            let mut meta = lock(&self.entry.meta);
            meta.total += bytes.len() as u64;
            if meta.cut.is_some() || self.closed {
                return;
            }
            (meta.stored, None)
        };
        let limit = self.store.limits.per_output;
        let mut take = bytes.len() as u64;
        if stored + take > limit {
            take = limit.saturating_sub(stored);
            cut = Some(OutputCut::OutputLimit);
        }
        if stored + take > self.reserved {
            let needed = stored + take - self.reserved;
            let wanted = needed.max(BLOCK.min(limit.saturating_sub(self.reserved)));
            let (granted, short) = self.store.reserve(&self.entry.id, needed, wanted);
            self.reserved += granted;
            if granted < needed {
                take = self.reserved.saturating_sub(stored);
                cut = short.or(cut);
            }
        }
        let data = &bytes[..take as usize];
        if let Err(e) = self.file.write_all(data) {
            tracing::warn!(output = %self.entry.id, error = %e, "could not save a command's output");
            let mut meta = lock(&self.entry.meta);
            meta.cut = Some(OutputCut::WriteFailed);
            drop(meta);
            self.entry.changed.notify_waiters();
            return;
        }
        self.hasher.update(data);
        {
            let mut meta = lock(&self.entry.meta);
            meta.stored += take;
            if meta.cut.is_none() {
                meta.cut = cut;
            }
        }
        self.entry.changed.notify_waiters();
    }

    /// No more comes: its command ended as `end` says, or in a way not known.
    pub(crate) fn close(&mut self, end: Option<OutputEnd>) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        let _ = self.file.flush();
        let stored = {
            let mut meta = lock(&self.entry.meta);
            meta.open = false;
            meta.end = end;
            meta.sha256 = Some(hex::encode(std::mem::take(&mut self.hasher).finalize()));
            meta.stored
        };
        self.store.settle(&self.entry.id, stored);
        self.entry.changed.notify_waiters();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // A request dropped before its command ended: how it ended is not known.
        self.close(None);
    }
}

/// Whether process `pid` still runs.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Bytes free for this user on the disk that holds `dir`.
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: path is a valid C string and stat a place for the answer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(ws: &Path, caller: &str) -> Arc<Origin> {
        Arc::new(Origin {
            caller: Some(caller.into()),
            ws: ws.to_path_buf(),
            given: ws.to_string_lossy().into_owned(),
            command: "make test".into(),
            trace: Some("trace-1".into()),
            request: Some("msg-1".into()),
        })
    }

    fn store(limits: OutputLimits) -> (tempfile::TempDir, Roots, Arc<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("ws")).unwrap();
        let roots = Roots { root: tmp.path().join("ws"), scratch: tmp.path().join("scratch") }.canonical().unwrap();
        let store = Arc::new(Store::open(&roots.scratch, limits).unwrap());
        (tmp, roots, store)
    }

    fn request(roots: &Roots, id: &str) -> OutputRequest {
        OutputRequest {
            workspace: roots.root.to_string_lossy().into_owned(),
            id: id.into(),
            offset: None,
            limit: None,
            search: None,
            case_insensitive: false,
            max_matches: None,
            wait_ms: None,
        }
    }

    #[tokio::test]
    async fn an_output_past_its_limit_or_the_quota_is_cut_and_says_why() {
        let limits = OutputLimits { per_output: 100, quota: 150, ..OutputLimits::default() };
        let (_tmp, roots, store) = store(limits);
        let mut first = store.create(origin(&roots.root, "a"), OutputStream::Stdout, &[b'x'; 60]).unwrap();
        first.write(&[b'y'; 60]);
        first.close(Some(OutputEnd { exit_code: Some(0), ..OutputEnd::default() }));
        let saved = first.handle().saved();
        assert_eq!((saved.total, saved.stored, saved.cut), (120, 100, Some(OutputCut::OutputLimit)));
        assert!(!saved.whole() && !saved.open);
        assert_eq!(
            saved.sha256.as_deref(),
            Some(hex::encode(Sha256::digest([[b'x'; 60].as_slice(), &[b'y'; 40]].concat())).as_str())
        );

        // The second needs more than is left once the first ended: the first is given up for it.
        let mut second = store.create(origin(&roots.root, "a"), OutputStream::Stderr, &[b'z'; 80]).unwrap();
        let why =
            store.page(&roots, Some("a"), request(&roots, &saved.id), CancellationToken::new()).await.unwrap_err();
        assert!(why.message.contains("is gone") && why.message.contains("disk quota"), "{}", why.message);
        // While it is open, nothing else can be given up, so the third stops at the quota.
        let mut third = store.create(origin(&roots.root, "a"), OutputStream::Stdout, &[b'w'; 90]).unwrap();
        let cut = third.handle().saved();
        assert_eq!((cut.total, cut.stored, cut.cut), (90, 50, Some(OutputCut::QuotaFull)));
        second.close(None);
        third.close(None);
        assert_eq!(second.handle().saved().end, None, "an end it was not told is not made up");
    }

    #[tokio::test]
    async fn an_output_is_read_in_pages_searched_and_kept_from_other_work() {
        let (_tmp, roots, store) = store(OutputLimits::default());
        let mut text = String::new();
        for n in 1..=5000 {
            text.push_str(&format!("ok line {n}\n"));
            if n == 2500 {
                text.push_str("error[E0308]: mismatched types in the middle\n");
            }
        }
        let mut writer = store.create(origin(&roots.root, "a"), OutputStream::Stdout, text.as_bytes()).unwrap();
        writer.close(Some(OutputEnd { exit_code: Some(101), ..OutputEnd::default() }));
        let id = writer.handle().saved().id;

        let mut find = request(&roots, &id);
        find.search = Some("ERROR[e0308]".into());
        find.case_insensitive = true;
        let found = store.page(&roots, Some("a"), find, CancellationToken::new()).await.unwrap();
        assert_eq!(found.matches.len(), 1);
        let hit = &found.matches[0];
        assert_eq!((hit.line, hit.text.as_str()), (2501, "error[E0308]: mismatched types in the middle"));
        assert_eq!(&text[hit.offset as usize..][..5], "error");

        let mut page = request(&roots, &id);
        page.offset = Some(hit.offset);
        page.limit = Some(20);
        let read = store.page(&roots, Some("a"), page, CancellationToken::new()).await.unwrap();
        assert_eq!((read.text.as_str(), read.next), ("error[E0308]: mismat", hit.offset + 20));
        assert_eq!(read.output.end.and_then(|e| e.exit_code), Some(101));

        let other = store.page(&roots, Some("b"), request(&roots, &id), CancellationToken::new()).await.unwrap_err();
        assert_eq!(other.code, ErrorCode::Denied, "{}", other.message);
        let unknown = store.page(&roots, Some("a"), request(&roots, "out_0"), CancellationToken::new()).await;
        assert!(unknown.unwrap_err().message.contains("no saved output"));
    }

    #[tokio::test]
    async fn an_expired_output_says_so_and_only_to_its_owner() {
        let limits = OutputLimits { kept: Duration::ZERO, ..OutputLimits::default() };
        let (_tmp, roots, store) = store(limits);
        let mut writer = store.create(origin(&roots.root, "a"), OutputStream::Stdout, b"done\n").unwrap();
        writer.close(None);
        let id = writer.handle().saved().id;
        let why = store.page(&roots, Some("a"), request(&roots, &id), CancellationToken::new()).await.unwrap_err();
        assert!(why.message.contains("is gone: it was kept 0 minutes"), "{}", why.message);
        let other = store.page(&roots, Some("b"), request(&roots, &id), CancellationToken::new()).await.unwrap_err();
        assert_eq!(other.code, ErrorCode::Denied);
    }

    #[tokio::test]
    async fn a_read_of_an_open_output_waits_for_more_and_returns_when_it_closes() {
        let (_tmp, roots, store) = store(OutputLimits::default());
        let mut writer = store.create(origin(&roots.root, "a"), OutputStream::Combined, b"one\n").unwrap();
        let id = writer.handle().saved().id;
        let mut follow = request(&roots, &id);
        follow.offset = Some(4);
        follow.wait_ms = Some(5_000);
        let reader = {
            let (store, roots) = (store.clone(), roots.clone());
            tokio::spawn(async move { store.page(&roots, Some("a"), follow, CancellationToken::new()).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer.write(b"two\n");
        let page = reader.await.unwrap().unwrap();
        assert_eq!((page.text.as_str(), page.next), ("two\n", 8));
        assert!(page.output.open);
        drop(writer);
        let mut last = request(&roots, &id);
        last.offset = Some(8);
        last.wait_ms = Some(5_000);
        let started = Instant::now();
        let page = store.page(&roots, Some("a"), last, CancellationToken::new()).await.unwrap();
        assert!(page.text.is_empty() && !page.output.open && started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_long_matching_line_is_cut_around_the_match() {
        let mut line = vec![b'a'; 2000];
        line.extend_from_slice(b"panicked");
        line.extend(vec![b'b'; 2000]);
        let shown = around(&line, 2000);
        assert!(shown.starts_with("[...] ") && shown.ends_with(" [...]") && shown.contains("panicked"), "{shown}");
        assert!(shown.len() < MATCH_LINE + 20);
    }
}

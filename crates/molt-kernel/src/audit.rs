//! Append-only, hash-chained audit log.
//!
//! One writer task owns the file. Callers submit events over a channel and
//! get back a receipt that resolves once the entry is durable; the writer
//! batches several entries per fsync. Entries are JSON lines:
//!
//! ```text
//! {"seq":7,"ts_ms":1791000000000,"prev":"<hex>","hash":"<hex>","event":{...}}
//! ```
//!
//! `hash = sha256(prev || seq || ts_ms || event bytes)`, computed over the exact
//! event bytes written, so [`verify`] detects any edit, deletion or reorder.
//! Nothing in the kernel exposes a way to rewrite or delete entries.
//!
//! The log holds every message, file contents and model conversations
//! included, so a new log, and a directory created for it, is readable by
//! its owner only.
//!
//! Services read it back one trace at a time through `kernel.audit.read`
//! ([`read_trace`]).
//!
//! Large payload values, model conversations above all, are written once
//! and referenced after that; the `refs` module says which lines this adds
//! and how the hash chain still covers every byte.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use molt_proto::audit::{Logged, ReadRequest, ReadResponse};
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, MsgId, ServiceId, Target, TraceId, VersionId};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use refs::{Lines, Store};

mod refs;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const MAX_BATCH: usize = 512;
/// Maximum serialized entry size, including its terminating newline. This
/// leaves room for the audit wrapper around an 8 MiB transport frame and caps
/// memory use even when a damaged file contains no line breaks.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A message the kernel accepted and is about to deliver.
    Message {
        envelope: Envelope,
    },
    /// A message the kernel refused.
    Denied {
        from: ServiceId,
        msg: MsgId,
        to: Target,
        code: ErrorCode,
        reason: String,
    },
    CapGranted {
        cap: CapId,
        holder: ServiceId,
        target: Target,
        budget: Budget,
        parent: Option<CapId>,
    },
    CapRevoked {
        cap: CapId,
    },
    ServiceRegistered {
        service: ServiceId,
        version: Option<VersionId>,
    },
    ServiceStarted {
        service: ServiceId,
        pid: Option<u32>,
    },
    ServiceExited {
        service: ServiceId,
        status: String,
    },
    ServiceGaveUp {
        service: ServiceId,
        restarts: u32,
    },
    VersionStored {
        service: ServiceId,
        version: VersionId,
        by: String,
    },
    VersionPromoted {
        service: ServiceId,
        from: Option<VersionId>,
        to: VersionId,
        by: String,
    },
    /// A page of the log handed to a service by `kernel.audit.read`, in
    /// place of a copy of the reply: entries `first_seq..=last_seq` of
    /// `trace`, and the SHA-256 of the reply payload as delivered.
    AuditRead {
        by: ServiceId,
        reply: MsgId,
        reply_to: MsgId,
        trace: TraceId,
        entries: u64,
        first_seq: Option<u64>,
        last_seq: Option<u64>,
        sha256: String,
    },
}

#[derive(Serialize)]
struct EntryOut<'a> {
    seq: u64,
    ts_ms: u64,
    prev: &'a str,
    hash: &'a str,
    event: &'a RawValue,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    refs: &'a [[u64; 2]],
}

#[derive(Deserialize)]
struct EntryIn<'a> {
    seq: u64,
    ts_ms: u64,
    prev: String,
    hash: String,
    #[serde(borrow)]
    event: &'a RawValue,
    #[serde(default)]
    refs: Vec<[u64; 2]>,
}

impl EntryIn<'_> {
    /// The event as written, its taken-out values put back.
    fn restore(&self, lines: &mut Option<Lines>, path: &Path, here: u64) -> Result<std::borrow::Cow<'_, str>, String> {
        if self.refs.is_empty() {
            return Ok(self.event.get().into());
        }
        let lines = match lines {
            Some(lines) => lines,
            None => lines.insert(Lines::open(path).map_err(|e| e.to_string())?),
        };
        lines.restore(self.event.get(), &self.refs, here).map(Into::into)
    }
}

/// One entry as read back from the log.
#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: u64,
    pub ts_ms: u64,
    pub hash: String,
    pub event: AuditEvent,
}

fn digest(prev: &str, seq: u64, ts_ms: u64, event: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(seq.to_le_bytes());
    h.update(ts_ms.to_le_bytes());
    h.update(event.as_bytes());
    hex::encode(h.finalize())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit writer stopped")]
    Stopped,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: {reason}")]
    Corrupt { line: usize, reason: String },
    #[error("entry at byte {offset}: {reason}")]
    CorruptAt { offset: u64, reason: String },
    #[error("cursor {0} is not the start of an entry")]
    Cursor(u64),
}

enum Job {
    Append(Box<AuditEvent>, oneshot::Sender<u64>),
    /// Write everything queued before it, then stop the writer.
    Close(oneshot::Sender<()>),
}

/// Handle to the single audit writer. Cheap to clone.
#[derive(Clone)]
pub struct AuditLog {
    tx: mpsc::Sender<Job>,
    path: PathBuf,
}

/// Resolves to the entry's sequence number once it is durable.
pub struct Receipt(oneshot::Receiver<u64>);

impl Receipt {
    pub async fn durable(self) -> Result<u64, AuditError> {
        self.0.await.map_err(|_| AuditError::Stopped)
    }
}

impl AuditLog {
    /// Open (or continue) the log at `path` and start its writer task.
    /// With `fsync` false, entries are flushed but not synced; for tests only.
    pub async fn open(path: impl Into<PathBuf>, fsync: bool) -> Result<Self, AuditError> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            let mut dirs = tokio::fs::DirBuilder::new();
            dirs.recursive(true);
            #[cfg(unix)]
            dirs.mode(0o700);
            dirs.create(dir).await?;
        }
        let (mut seq, mut prev, mut store) = (0u64, GENESIS.to_owned(), Store::default());
        if tokio::fs::try_exists(&path).await? {
            let (head, _) = scan(path.clone(), 0, true).await?;
            seq = head.seq;
            prev = head.hash;
            store = head.store;
            tracing::info!(entries = seq, "continuing audit log");
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).await?;
        let (tx, rx) = mpsc::channel(4096);
        tokio::spawn(writer(file, rx, seq, prev, store, fsync));
        Ok(Self { tx, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue an event. Entries are written in submission order; await the
    /// receipt to know it is durable. An entry larger than [`MAX_ENTRY_BYTES`]
    /// stops the writer without writing that batch or acknowledging its events.
    pub async fn submit(&self, event: AuditEvent) -> Result<Receipt, AuditError> {
        let (ack, rx) = oneshot::channel();
        self.tx.send(Job::Append(Box::new(event), ack)).await.map_err(|_| AuditError::Stopped)?;
        Ok(Receipt(rx))
    }

    /// Submit and wait until durable.
    pub async fn append(&self, event: AuditEvent) -> Result<u64, AuditError> {
        self.submit(event).await?.durable().await
    }

    /// Write every entry submitted so far, then stop the writer. That closes
    /// every clone of this handle: later submits fail with
    /// [`AuditError::Stopped`]. Await it before the runtime shuts down, which
    /// would otherwise cut off the last batch mid-write.
    pub async fn close(&self) {
        let (done, rx) = oneshot::channel();
        if self.tx.send(Job::Close(done)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

async fn writer(
    mut file: tokio::fs::File,
    mut rx: mpsc::Receiver<Job>,
    mut seq: u64,
    mut prev: String,
    mut store: Store,
    fsync: bool,
) {
    let mut buf = Vec::new();
    let mut acks = Vec::new();
    let mut closed = None;
    while let Some(first) = rx.recv().await {
        let mut job = Some(first);
        while let Some(next) = job.take() {
            let (event, ack) = match next {
                Job::Append(event, ack) => (event, ack),
                Job::Close(done) => {
                    closed = Some(done);
                    break;
                }
            };
            seq += 1;
            let ts_ms = now_ms();
            let raw = serde_json::value::to_raw_value(&event).expect("audit events always serialize");
            let hash = digest(&prev, seq, ts_ms, raw.get());
            let (compacted, refs) = match store.compact(raw.get(), &mut buf) {
                Some((text, refs)) => (Some(RawValue::from_string(text).expect("compacted events stay JSON")), refs),
                None => (None, Vec::new()),
            };
            let event = compacted.as_deref().unwrap_or(&raw);
            let start = buf.len();
            serde_json::to_writer(&mut buf, &EntryOut { seq, ts_ms, prev: &prev, hash: &hash, event, refs: &refs })
                .expect("audit entries always serialize");
            buf.push(b'\n');
            // Readers restore the entry whole, so its whole size counts.
            if buf.len() - start - event.get().len() + raw.get().len() > MAX_ENTRY_BYTES {
                // Never write a record that the bounded readers cannot reopen.
                tracing::error!(limit = MAX_ENTRY_BYTES, "audit entry is too large; stopping the writer");
                return;
            }
            prev = hash;
            acks.push((seq, ack));
            if acks.len() < MAX_BATCH {
                job = rx.try_recv().ok();
            }
        }
        let written = async {
            if buf.is_empty() {
                return Ok(());
            }
            file.write_all(&buf).await?;
            file.flush().await?;
            if fsync {
                file.sync_data().await?;
            }
            std::io::Result::Ok(())
        }
        .await;
        store.len += buf.len() as u64;
        buf.clear();
        if let Err(e) = written {
            // Without a durable log the kernel must not deliver anything:
            // dropping the acks fails every pending receipt.
            tracing::error!(error = %e, "audit log write failed; stopping the writer");
            return;
        }
        for (s, ack) in acks.drain(..) {
            let _ = ack.send(s);
        }
        if let Some(done) = closed.take() {
            let _ = done.send(());
            return;
        }
    }
}

struct ChainHead {
    seq: u64,
    hash: String,
    /// The blobs and runs written, to reference them again.
    store: Store,
}

// File IO, hashing and JSON parsing all run off the async runtime. Only the
// current line and at most `keep` decoded entries survive each iteration.
async fn scan(path: PathBuf, keep: usize, verify_chain: bool) -> Result<(ChainHead, Vec<Entry>), AuditError> {
    tokio::task::spawn_blocking(move || scan_file(&path, keep, verify_chain)).await.map_err(std::io::Error::other)?
}

fn scan_file(path: &Path, keep: usize, verify_chain: bool) -> Result<(ChainHead, Vec<Entry>), AuditError> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut head = ChainHead { seq: 0, hash: GENESIS.to_owned(), store: Store::default() };
    let mut entries = VecDeque::new();
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut lines = None;
    loop {
        let here = head.store.len;
        line.clear();
        // `take` caps the allocation before read_until can encounter an
        // arbitrarily long or unterminated record. The extra byte distinguishes
        // an exactly full, valid record from an oversized one.
        let n = reader.by_ref().take(MAX_ENTRY_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        line_number += 1;
        let corrupt = |reason: String| AuditError::Corrupt { line: line_number, reason };
        if n > MAX_ENTRY_BYTES {
            return Err(corrupt(format!("entry exceeds the {MAX_ENTRY_BYTES}-byte limit")));
        }
        if line.last() != Some(&b'\n') {
            return Err(corrupt("entry is missing its terminating newline".into()));
        }
        head.store.len += n as u64;
        if refs::is_stored(&line) {
            refs::check_stored(&line, here).map_err(corrupt)?;
            head.store.learn(&line, here);
            continue;
        }
        let e: EntryIn = serde_json::from_slice(&line).map_err(|e| corrupt(e.to_string()))?;
        let event = e.restore(&mut lines, path, here).map_err(corrupt)?;
        if verify_chain {
            let expected_seq = head.seq.checked_add(1).ok_or_else(|| corrupt("sequence overflow".into()))?;
            if e.seq != expected_seq {
                return Err(corrupt(format!("sequence {} where {expected_seq} was expected", e.seq)));
            }
            if e.prev != head.hash {
                return Err(corrupt("previous-hash link is broken".into()));
            }
            if digest(&e.prev, e.seq, e.ts_ms, &event) != e.hash {
                return Err(corrupt("hash does not match the entry".into()));
            }
        }
        // Even discarded records must be valid events: corruption before the
        // requested tail must not disappear just because fewer entries are kept.
        let event = serde_json::from_str(&event).map_err(|e| corrupt(e.to_string()))?;
        head.seq = e.seq;
        head.hash = e.hash;
        if keep > 0 {
            if entries.len() == keep {
                entries.pop_front();
            }
            entries.push_back(Entry { seq: e.seq, ts_ms: e.ts_ms, hash: head.hash.clone(), event });
        }
    }
    Ok((head, entries.into()))
}

/// Check the whole chain in one pass, keeping only one bounded record at a
/// time. Returns the number of entries. Oversized or incomplete records fail.
pub async fn verify(path: &Path) -> Result<u64, AuditError> {
    Ok(scan(path.to_owned(), 0, true).await?.0.seq)
}

/// Read every entry, checking record syntax but not the hash chain (call
/// [`verify`] for that). Memory necessarily grows with the returned records.
pub async fn read_all(path: &Path) -> Result<Vec<Entry>, AuditError> {
    tail(path, usize::MAX).await
}

/// The last `n` entries in file order, retaining at most `n` decoded records.
/// Scans and validates record syntax throughout the file, but does not verify
/// the hash chain. For `n == 0`, opens the file without reading any records.
pub async fn tail(path: &Path, n: usize) -> Result<Vec<Entry>, AuditError> {
    if n == 0 {
        tokio::fs::File::open(path).await?;
        return Ok(Vec::new());
    }
    Ok(scan(path.to_owned(), n, false).await?.1)
}

/// Default size of one page of [`read_trace`], in bytes of log.
pub const READ_PAGE: u64 = 2 * 1024 * 1024;
/// Largest page a reader may ask for. Twice this still fits a transport frame.
pub const MAX_READ_PAGE: u64 = 3 * 1024 * 1024;

/// How far one [`read_trace_within`] may look.
#[derive(Clone, Copy, Debug)]
pub struct Scan {
    /// Stop looking at this time.
    pub until: Option<std::time::Instant>,
    /// Stop after this many bytes of log.
    pub max_bytes: u64,
}

/// Bytes of log one `kernel.audit.read` looks through at most; a reader
/// whose trace is further on reads on from the page's `next`.
pub const MAX_SCAN: u64 = 256 * 1024 * 1024;

/// [`read_trace_within`] with no limit on how far it looks.
pub fn read_trace(path: &Path, req: &ReadRequest) -> Result<ReadResponse, AuditError> {
    read_trace_within(path, req, Scan { until: None, max_bytes: u64::MAX })
}

/// One page of the messages of `req.trace` in the log at `path`, from
/// `req.cursor` on. Reads the file directly and blocks: run it off the
/// async runtime. A page holds at least one message unless the log has none
/// left or `scan` ran out first (then `next` says where to read on), so a
/// reader always makes progress; a message bigger than the page comes with
/// its payload left out. A last line still being written counts as not
/// there yet.
pub fn read_trace_within(path: &Path, req: &ReadRequest, scan: Scan) -> Result<ReadResponse, AuditError> {
    let max = req.max_bytes.unwrap_or(READ_PAGE).clamp(1, MAX_READ_PAGE);
    let mut file = std::fs::File::open(path)?;
    let start = req.cursor.unwrap_or(0);
    if start > file.metadata()?.len() {
        return Err(AuditError::Cursor(start));
    }
    if start > 0 {
        let mut before = [0u8];
        file.seek(SeekFrom::Start(start - 1))?;
        file.read_exact(&mut before)?;
        if before[0] != b'\n' {
            return Err(AuditError::Cursor(start));
        }
    }
    file.seek(SeekFrom::Start(start))?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    // A cheap test before parsing a line. Strings in a payload are escaped, so
    // only an envelope's own field matches; the parsed trace id is checked too.
    let needle = format!("\"trace_id\":{}", serde_json::to_string(&req.trace).map_err(std::io::Error::other)?);
    let (mut offset, mut used) = (start, 0u64);
    let mut entries = Vec::new();
    let mut line = Vec::new();
    let mut lines = None;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)? as u64;
        if n == 0 || line.last() != Some(&b'\n') {
            return Ok(ReadResponse { entries, next: None });
        }
        let here = offset;
        offset += n;
        let corrupt = |reason: String| AuditError::CorruptAt { offset: here, reason };
        let text = std::str::from_utf8(&line).map_err(|e| corrupt(e.to_string()))?;
        if refs::is_stored(&line) || !text.contains(&needle) {
            let out_of_time = scan.until.is_some_and(|until| std::time::Instant::now() >= until);
            if offset - start >= scan.max_bytes || out_of_time {
                return Ok(ReadResponse { entries, next: Some(offset) });
            }
            continue;
        }
        let entry: EntryIn = serde_json::from_str(text).map_err(|e| corrupt(e.to_string()))?;
        // Taken-out values leave a `0` in their place, so the envelope reads
        // without them; they are put back only for a message of the page.
        let event: AuditEvent = serde_json::from_str(entry.event.get()).map_err(|e| corrupt(e.to_string()))?;
        let AuditEvent::Message { mut envelope } = event else { continue };
        let skipped =
            envelope.kind == Kind::Request && envelope.to.service().is_some_and(|s| req.skip_requests_to.contains(s));
        if envelope.trace_id != req.trace || skipped {
            continue;
        }
        let mut n = n;
        if !entry.refs.is_empty() {
            let restored = entry.restore(&mut lines, path, here).map_err(corrupt)?;
            // The page holds the restored message, so that is what it costs.
            n = n - entry.event.get().len() as u64 + restored.len() as u64;
            let event = serde_json::from_str(&restored).map_err(|e| corrupt(e.to_string()))?;
            let AuditEvent::Message { envelope: whole } = event else {
                return Err(corrupt("restored entry is not a message".into()));
            };
            envelope = whole;
        }
        if used + n > max && !entries.is_empty() {
            return Ok(ReadResponse { entries, next: Some(here) });
        }
        let omitted = (n > max).then(|| {
            envelope.payload = serde_json::Value::Null;
            n
        });
        used += n;
        entries.push(Logged { seq: entry.seq, ts_ms: entry.ts_ms, envelope, omitted });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(n: u32) -> AuditEvent {
        AuditEvent::ServiceStarted { service: ServiceId::new("s").unwrap(), pid: Some(n) }
    }

    fn record(seq: u64, prev: &str, event: &str) -> String {
        let event = RawValue::from_string(event.to_owned()).unwrap();
        let hash = digest(prev, seq, 123, event.get());
        let mut line =
            serde_json::to_string(&EntryOut { seq, ts_ms: 123, prev, hash: &hash, event: &event, refs: &[] }).unwrap();
        line.push('\n');
        line
    }

    #[tokio::test]
    async fn empty_logs_and_zero_length_tails_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(verify(&path).await.unwrap(), 0);
        assert!(tail(&path, 3).await.unwrap().is_empty());
        assert!(read_all(&path).await.unwrap().is_empty());
        let log = AuditLog::open(&path, false).await.unwrap();
        assert_eq!(log.append(ev(1)).await.unwrap(), 1);
        log.close().await;
        assert!(tail(&path, 0).await.unwrap().is_empty());
        std::fs::write(&path, b"not JSON\xff").unwrap();
        assert!(tail(&path, 0).await.unwrap().is_empty(), "zero entries does not scan");
        assert!(matches!(tail(&dir.path().join("missing"), 0).await, Err(AuditError::Io(_))));
    }

    #[tokio::test]
    async fn a_tail_keeps_only_the_requested_entries_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        for n in 0..20 {
            log.append(ev(n)).await.unwrap();
        }
        log.close().await;
        for n in [1, 3, 20, 30, usize::MAX] {
            let entries = tail(&path, n).await.unwrap();
            let first = 20 - n.min(20);
            assert_eq!(entries.len(), n.min(20));
            for (i, entry) in entries.iter().enumerate() {
                assert_eq!(entry.seq, (first + i + 1) as u64);
                assert_eq!(entry.event, ev((first + i) as u32));
            }
        }
        assert_eq!(read_all(&path).await.unwrap().len(), 20);
    }

    #[tokio::test]
    async fn verification_hashes_the_original_event_bytes_and_resumes_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let event = r#"{ "status" : "退出 café 🚀 \u0061", "service":"s", "type" : "service_exited" }"#;
        std::fs::write(&path, record(1, GENESIS, event)).unwrap();
        assert_eq!(verify(&path).await.unwrap(), 1);
        let entries = tail(&path, 1).await.unwrap();
        assert_eq!(entries[0].event, serde_json::from_str::<AuditEvent>(event).unwrap());
        let log = AuditLog::open(&path, false).await.unwrap();
        assert_eq!(log.append(ev(2)).await.unwrap(), 2);
        log.close().await;
        assert_eq!(verify(&path).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn verification_and_open_refuse_sequence_link_and_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let first = record(1, GENESIS, &serde_json::to_string(&ev(1)).unwrap());
        let head: EntryIn = serde_json::from_str(&first).unwrap();
        let second = record(2, &head.hash, &serde_json::to_string(&ev(2)).unwrap());
        let mut cases = Vec::new();
        for (key, value, reason) in [
            ("seq", serde_json::json!(9), "sequence"),
            ("prev", serde_json::json!(GENESIS), "previous-hash"),
            ("hash", serde_json::json!(GENESIS), "hash does not match"),
            ("event", serde_json::to_value(ev(99)).unwrap(), "hash does not match"),
        ] {
            let mut changed: serde_json::Value = serde_json::from_str(&second).unwrap();
            changed[key] = value;
            cases.push((format!("{first}{changed}\n"), reason));
        }
        cases.push((format!("{first}{first}"), "sequence"));
        for (text, expected) in cases {
            std::fs::write(&path, &text).unwrap();
            let err = verify(&path).await.unwrap_err();
            assert!(matches!(&err, AuditError::Corrupt { line: 2, reason } if reason.contains(expected)), "{err}");
            assert!(AuditLog::open(&path, false).await.is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "rejected logs are never changed");
        }
    }

    #[tokio::test]
    async fn malformed_oversized_and_unterminated_records_are_never_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let valid = record(1, GENESIS, &serde_json::to_string(&ev(1)).unwrap());
        let cases = [
            b"{broken JSON}\n".to_vec(),
            b"\xff\n".to_vec(),
            b"\n".to_vec(),
            valid.as_bytes()[..valid.len() / 2].to_vec(),
            valid.trim_end_matches('\n').as_bytes().to_vec(),
            record(2, GENESIS, r#"{"type":"not_an_event"}"#).into_bytes(),
            vec![b'x'; MAX_ENTRY_BYTES + 1],
        ];
        for bad in cases {
            let mut bytes = valid.as_bytes().to_vec();
            bytes.extend_from_slice(&bad);
            std::fs::write(&path, &bytes).unwrap();
            assert!(matches!(verify(&path).await, Err(AuditError::Corrupt { line: 2, .. })));
            assert!(matches!(tail(&path, 1).await, Err(AuditError::Corrupt { line: 2, .. })));
            assert!(AuditLog::open(&path, false).await.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        // The corrupt record would be outside the returned tail.
        std::fs::write(&path, format!("not JSON\n{valid}")).unwrap();
        assert!(matches!(tail(&path, 1).await, Err(AuditError::Corrupt { line: 1, .. })));
    }

    #[tokio::test]
    async fn an_entry_at_the_size_limit_can_be_verified_and_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut event = AuditEvent::ServiceExited { service: ServiceId::new("s").unwrap(), status: String::new() };
        let overhead = record(1, GENESIS, &serde_json::to_string(&event).unwrap()).len();
        if let AuditEvent::ServiceExited { status, .. } = &mut event {
            *status = "x".repeat(MAX_ENTRY_BYTES - overhead);
        }
        let line = record(1, GENESIS, &serde_json::to_string(&event).unwrap());
        assert_eq!(line.len(), MAX_ENTRY_BYTES);
        std::fs::write(&path, &line).unwrap();
        assert_eq!(verify(&path).await.unwrap(), 1);
        assert_eq!(tail(&path, 1).await.unwrap()[0].event, event);
        let log = AuditLog::open(&path, false).await.unwrap();
        assert_eq!(log.append(ev(2)).await.unwrap(), 2);
        log.close().await;
        assert_eq!(verify(&path).await.unwrap(), 2);
        std::fs::write(&path, format!(" {}", line)).unwrap();
        assert!(matches!(verify(&path).await, Err(AuditError::Corrupt { line: 1, .. })));
    }

    #[tokio::test]
    async fn the_writer_never_persists_an_entry_the_readers_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        log.append(ev(1)).await.unwrap();
        let before = std::fs::read(&path).unwrap();
        let event =
            AuditEvent::ServiceExited { service: ServiceId::new("s").unwrap(), status: "x".repeat(MAX_ENTRY_BYTES) };
        assert!(matches!(log.append(event).await, Err(AuditError::Stopped)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let log = AuditLog::open(&path, false).await.unwrap();
        assert_eq!(log.append(ev(2)).await.unwrap(), 2);
        log.close().await;
        assert_eq!(verify(&path).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn chain_verifies_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        for n in 0..10 {
            assert_eq!(log.append(ev(n)).await.unwrap(), n as u64 + 1);
        }
        drop(log);
        let log = AuditLog::open(&path, false).await.unwrap();
        assert_eq!(log.append(ev(99)).await.unwrap(), 11);
        assert_eq!(verify(&path).await.unwrap(), 11);
    }

    #[tokio::test]
    async fn close_writes_everything_submitted_then_stops_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        let mut receipts = Vec::new();
        for n in 0..100 {
            receipts.push(log.submit(ev(n)).await.unwrap());
        }
        log.clone().close().await;
        assert_eq!(verify(&path).await.unwrap(), 100, "every entry is on disk once close returns");
        for (i, r) in receipts.into_iter().enumerate() {
            assert_eq!(r.durable().await.unwrap(), i as u64 + 1);
        }
        assert!(matches!(log.append(ev(100)).await, Err(AuditError::Stopped)));
        log.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_log_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("audit.jsonl");
        AuditLog::open(&path, false).await.unwrap().append(ev(0)).await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.path().join("data")), 0o700);
    }

    fn message(trace: &TraceId, to: &str, kind: Kind, payload: serde_json::Value) -> AuditEvent {
        let mut envelope = Envelope::request(trace.clone(), to.parse().unwrap(), CapId::random(), payload);
        envelope.kind = kind;
        AuditEvent::Message { envelope }
    }

    #[tokio::test]
    async fn a_trace_is_read_back_in_pages_without_the_skipped_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        let (mine, other) = (TraceId::random(), TraceId::random());
        // A payload that quotes another trace's id as text must not match it.
        let quoted = serde_json::json!({ "note": format!("\"trace_id\":\"{other}\"") });
        log.append(message(&other, "fs.read", Kind::Request, serde_json::json!(0))).await.unwrap();
        log.append(message(&mine, "model.complete", Kind::Request, serde_json::json!("big"))).await.unwrap();
        log.append(message(&mine, "model.complete", Kind::Reply, serde_json::json!("answer"))).await.unwrap();
        log.append(ev(1)).await.unwrap();
        for n in 0..10 {
            log.append(message(&mine, "fs.read", Kind::Request, serde_json::json!(n))).await.unwrap();
        }
        log.append(message(&mine, "fs.read", Kind::Request, quoted.clone())).await.unwrap();
        log.close().await;

        let all = read_trace(&path, &ReadRequest::new(other.clone())).unwrap();
        assert_eq!(all.entries.len(), 1, "the quoted id is not a message of that trace");
        assert_eq!(all.next, None);

        let mut req = ReadRequest::new(mine.clone());
        req.skip_requests_to = vec![ServiceId::new("model").unwrap()];
        let whole = read_trace(&path, &req).unwrap();
        let payloads: Vec<_> = whole.entries.iter().map(|e| e.envelope.payload.clone()).collect();
        assert_eq!(payloads[0], serde_json::json!("answer"), "the model's reply is kept, its request left out");
        assert_eq!(payloads.len(), 12);
        assert_eq!(payloads[11], quoted);

        // Small pages still make progress and add up to the whole.
        req.max_bytes = Some(1);
        let mut seen = Vec::new();
        loop {
            let page = read_trace(&path, &req).unwrap();
            assert_eq!(page.entries.len(), 1);
            assert!(page.entries[0].omitted.is_some(), "a message bigger than the page has its payload left out");
            seen.push(page.entries[0].seq);
            match page.next {
                Some(next) => req.cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, whole.entries.iter().map(|e| e.seq).collect::<Vec<_>>());

        // A cursor that is not the start of an entry is refused.
        req.cursor = Some(3);
        assert!(matches!(read_trace(&path, &req), Err(AuditError::Cursor(3))));
        req.cursor = Some(u64::MAX);
        assert!(matches!(read_trace(&path, &req), Err(AuditError::Cursor(_))));
    }

    #[tokio::test]
    async fn a_scan_that_runs_out_says_where_to_read_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        let (mine, other) = (TraceId::random(), TraceId::random());
        for n in 0..20 {
            log.append(message(&other, "fs.read", Kind::Request, serde_json::json!(n))).await.unwrap();
        }
        log.append(message(&mine, "fs.read", Kind::Request, serde_json::json!("found"))).await.unwrap();
        log.close().await;

        let req = ReadRequest::new(mine.clone());
        let short = Scan { until: None, max_bytes: 1 };
        let mut cursor = None;
        let mut calls = 0;
        let found = loop {
            calls += 1;
            let page = read_trace_within(&path, &ReadRequest { cursor, ..req.clone() }, short).unwrap();
            if !page.entries.is_empty() {
                break page.entries;
            }
            cursor = Some(page.next.expect("a scan cut short says where to read on"));
        };
        assert_eq!(calls, 21, "one line per call");
        assert_eq!(found[0].envelope.payload, serde_json::json!("found"));

        let past = Scan { until: Some(std::time::Instant::now()), max_bytes: u64::MAX };
        let page = read_trace_within(&path, &req, past).unwrap();
        assert!(page.entries.is_empty() && page.next.is_some(), "out of time: a page with nothing, and where to go on");
    }

    #[tokio::test]
    async fn a_line_still_being_written_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        let trace = TraceId::random();
        log.append(message(&trace, "fs.read", Kind::Request, serde_json::json!(1))).await.unwrap();
        log.close().await;
        let mut text = std::fs::read_to_string(&path).unwrap();
        let half = text.clone();
        text.push_str(&half[..half.len() / 2]);
        std::fs::write(&path, text).unwrap();
        let page = read_trace(&path, &ReadRequest::new(trace)).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next, None);
    }

    #[tokio::test]
    async fn receipts_resolve_in_submission_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path().join("a.jsonl"), false).await.unwrap();
        let mut receipts = Vec::new();
        for n in 0..1000 {
            receipts.push(log.submit(ev(n)).await.unwrap());
        }
        for (i, r) in receipts.into_iter().enumerate() {
            assert_eq!(r.durable().await.unwrap(), i as u64 + 1);
        }
    }

    /// A model request of `turns` turns: the same system prompt and tools
    /// every turn, and the conversation so far.
    fn conversation(trace: &TraceId, turns: usize) -> AuditEvent {
        let mut messages = Vec::new();
        for n in 0..turns {
            messages
                .push(serde_json::json!({ "role": "user", "content": format!("result {n}: {}", "r".repeat(2000)) }));
            messages.push(serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": format!("turn {n}") }] }));
        }
        let tools: Vec<_> =
            (0..5).map(|n| serde_json::json!({ "name": format!("tool{n}"), "description": "d".repeat(300) })).collect();
        let payload =
            serde_json::json!({ "system": "s".repeat(5000), "messages": messages, "tools": tools, "max_tokens": 100 });
        message(trace, "model.complete", Kind::Request, payload)
    }

    #[tokio::test]
    async fn a_conversation_sent_whole_every_turn_is_logged_once_and_read_back_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trace = TraceId::random();
        let len = || std::fs::metadata(&path).unwrap().len();
        let mut sizes = vec![0];
        let events: Vec<AuditEvent> = (1..=50).map(|turn| conversation(&trace, turn)).collect();
        let mut log = AuditLog::open(&path, false).await.unwrap();
        for turn in 1..=50 {
            if turn == 25 {
                // Reopened, the log still references what it wrote before.
                log.close().await;
                log = AuditLog::open(&path, false).await.unwrap();
            }
            log.append(events[turn - 1].clone()).await.unwrap();
            log.append(ev(turn as u32)).await.unwrap();
            sizes.push(len());
        }
        log.close().await;
        let added: Vec<u64> = sizes.windows(2).map(|w| w[1] - w[0]).collect();
        let whole = serde_json::to_string(&events[49]).unwrap().len() as u64;
        // Each turn adds about its two new messages, not the conversation again.
        assert!(added[49] < added[1] + 300, "{added:?}");
        assert!(added[24] < added[1] + 300, "{added:?}");
        assert!(len() < 2 * whole, "{} bytes for a {whole}-byte conversation", len());

        assert_eq!(verify(&path).await.unwrap(), 100);
        let entries = read_all(&path).await.unwrap();
        let read: Vec<&AuditEvent> = entries.iter().step_by(2).map(|e| &e.event).collect();
        assert!(read.iter().zip(&events).all(|(r, e)| *r == e));
        // Pages are as big as the messages they restore.
        let mut req = ReadRequest::new(trace.clone());
        let mut logged = Vec::new();
        loop {
            let page = read_trace(&path, &req).unwrap();
            let size: usize = page.entries.iter().map(|e| serde_json::to_string(&e.envelope).unwrap().len()).sum();
            assert!(size as u64 <= READ_PAGE, "{size}");
            logged.extend(page.entries);
            match page.next {
                Some(next) => req.cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(logged.len(), 50);
        for (logged, event) in logged.iter().zip(&events) {
            let AuditEvent::Message { envelope } = event else { unreachable!() };
            assert_eq!((&logged.envelope, logged.omitted), (envelope, None));
        }
    }

    #[tokio::test]
    async fn a_changed_or_missing_blob_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trace = TraceId::random();
        let log = AuditLog::open(&path, false).await.unwrap();
        log.append(conversation(&trace, 1)).await.unwrap();
        log.append(conversation(&trace, 2)).await.unwrap();
        log.close().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().next().unwrap().starts_with("{\"blob\":"));

        // The data no longer matches the blob's hash.
        std::fs::write(&path, text.replacen("result 0", "result 9", 1)).unwrap();
        assert!(matches!(verify(&path).await, Err(AuditError::Corrupt { .. })));
        assert!(read_trace(&path, &ReadRequest::new(trace.clone())).is_err());

        // A consistent blob with other content changes what the entry restores to.
        let blob = text.lines().find(|l| l.contains("result 0")).unwrap();
        let data = &blob[blob.find("\"data\":").unwrap() + 7..blob.len() - 1];
        let forged = data.replacen("result 0", "result 9", 1);
        let key = hex::encode(Sha256::digest(forged.as_bytes()));
        let line = format!("{{\"blob\":\"{key}\",\"data\":{forged}}}");
        assert_eq!(line.len(), blob.len());
        std::fs::write(&path, text.replacen(blob, &line, 1)).unwrap();
        assert!(matches!(verify(&path).await, Err(AuditError::Corrupt { .. })));

        // A run whose blob is gone cannot be restored.
        let without: String = text.lines().filter(|l| *l != blob).map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, without).unwrap();
        assert!(matches!(verify(&path).await, Err(AuditError::Corrupt { .. })));
        assert!(read_all(&path).await.is_err());
    }

    #[tokio::test]
    async fn small_payloads_are_written_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path, false).await.unwrap();
        let trace = TraceId::random();
        let event =
            message(&trace, "fs.read", Kind::Request, serde_json::json!({ "path": "a.rs", "text": "x".repeat(200) }));
        log.append(event.clone()).await.unwrap();
        log.close().await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains("\"refs\""));
        assert_eq!(read_all(&path).await.unwrap()[0].event, event);
    }
}

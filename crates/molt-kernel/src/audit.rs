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

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use molt_proto::audit::{Logged, ReadRequest, ReadResponse};
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, MsgId, ServiceId, Target, TraceId, VersionId};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const MAX_BATCH: usize = 512;

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
}

#[derive(Deserialize)]
struct EntryIn<'a> {
    seq: u64,
    ts_ms: u64,
    prev: String,
    hash: String,
    #[serde(borrow)]
    event: &'a RawValue,
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
        let (mut seq, mut prev) = (0u64, GENESIS.to_owned());
        if tokio::fs::try_exists(&path).await? {
            let n = verify(&path).await?;
            if let Some(last) = tail(&path, 1).await?.pop() {
                seq = last.seq;
                prev = last.hash;
            }
            tracing::info!(entries = n, "continuing audit log");
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).await?;
        let (tx, rx) = mpsc::channel(4096);
        tokio::spawn(writer(file, rx, seq, prev, fsync));
        Ok(Self { tx, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue an event. Entries are written in submission order; await the
    /// receipt to know it is durable.
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

async fn writer(mut file: tokio::fs::File, mut rx: mpsc::Receiver<Job>, mut seq: u64, mut prev: String, fsync: bool) {
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
            serde_json::to_writer(&mut buf, &EntryOut { seq, ts_ms, prev: &prev, hash: &hash, event: &raw })
                .expect("audit entries always serialize");
            buf.push(b'\n');
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

/// Check the whole chain. Returns the number of entries.
pub async fn verify(path: &Path) -> Result<u64, AuditError> {
    let text = tokio::fs::read_to_string(path).await?;
    let mut prev = GENESIS.to_owned();
    let mut expected_seq = 1u64;
    for (i, line) in text.lines().enumerate() {
        let corrupt = |reason: String| AuditError::Corrupt { line: i + 1, reason };
        let e: EntryIn = serde_json::from_str(line).map_err(|e| corrupt(e.to_string()))?;
        if e.seq != expected_seq {
            return Err(corrupt(format!("sequence {} where {expected_seq} was expected", e.seq)));
        }
        if e.prev != prev {
            return Err(corrupt("previous-hash link is broken".into()));
        }
        if digest(&e.prev, e.seq, e.ts_ms, e.event.get()) != e.hash {
            return Err(corrupt("hash does not match the entry".into()));
        }
        serde_json::from_str::<AuditEvent>(e.event.get()).map_err(|e| corrupt(e.to_string()))?;
        prev = e.hash;
        expected_seq += 1;
    }
    Ok(expected_seq - 1)
}

/// Read every entry (verifying nothing; call [`verify`] for that).
pub async fn read_all(path: &Path) -> Result<Vec<Entry>, AuditError> {
    tail(path, usize::MAX).await
}

/// The last `n` entries.
pub async fn tail(path: &Path, n: usize) -> Result<Vec<Entry>, AuditError> {
    let text = tokio::fs::read_to_string(path).await?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let corrupt = |reason: String| AuditError::Corrupt { line: start + i + 1, reason };
            let e: EntryIn = serde_json::from_str(line).map_err(|e| corrupt(e.to_string()))?;
            let event = serde_json::from_str(e.event.get()).map_err(|e| corrupt(e.to_string()))?;
            Ok(Entry { seq: e.seq, ts_ms: e.ts_ms, hash: e.hash, event })
        })
        .collect()
}

/// Default size of one page of [`read_trace`], in bytes of log.
pub const READ_PAGE: u64 = 2 * 1024 * 1024;
/// Largest page a reader may ask for. Twice this still fits a transport frame.
pub const MAX_READ_PAGE: u64 = 3 * 1024 * 1024;

/// One page of the messages of `req.trace` in the log at `path`, from
/// `req.cursor` on. Reads the file directly and blocks: run it off the
/// async runtime. A page holds at least one message unless the log has none
/// left, so a reader always makes progress; a message bigger than the page
/// comes with its payload left out. A last line still being written counts
/// as not there yet.
pub fn read_trace(path: &Path, req: &ReadRequest) -> Result<ReadResponse, AuditError> {
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
        if !text.contains(&needle) {
            continue;
        }
        let entry: EntryIn = serde_json::from_str(text).map_err(|e| corrupt(e.to_string()))?;
        let event: AuditEvent = serde_json::from_str(entry.event.get()).map_err(|e| corrupt(e.to_string()))?;
        let AuditEvent::Message { mut envelope } = event else { continue };
        let skipped =
            envelope.kind == Kind::Request && envelope.to.service().is_some_and(|s| req.skip_requests_to.contains(s));
        if envelope.trace_id != req.trace || skipped {
            continue;
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
}

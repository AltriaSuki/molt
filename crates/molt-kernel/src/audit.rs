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

use std::path::{Path, PathBuf};

use molt_proto::{Budget, CapId, Envelope, ErrorCode, MsgId, ServiceId, Target, VersionId};
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
}

type Job = (AuditEvent, oneshot::Sender<u64>);

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
            tokio::fs::create_dir_all(dir).await?;
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
        let file = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
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
        self.tx.send((event, ack)).await.map_err(|_| AuditError::Stopped)?;
        Ok(Receipt(rx))
    }

    /// Submit and wait until durable.
    pub async fn append(&self, event: AuditEvent) -> Result<u64, AuditError> {
        self.submit(event).await?.durable().await
    }
}

async fn writer(mut file: tokio::fs::File, mut rx: mpsc::Receiver<Job>, mut seq: u64, mut prev: String, fsync: bool) {
    let mut buf = Vec::new();
    let mut acks = Vec::new();
    while let Some(first) = rx.recv().await {
        let mut job = Some(first);
        while let Some((event, ack)) = job.take() {
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

//! One Unix socket per service, for a single host.
//!
//! The kernel binds `<dir>/<service>.sock` when it opens an endpoint. A
//! service connects and must send its secret as the first frame; the kernel
//! then stamps everything read from that connection with the service's id.
//! A newer authenticated connection replaces an older one, so a restarted
//! service can reconnect. Frames are length-prefixed JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use molt_proto::{Envelope, ServiceId};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::{Inbound, Link, Secret, Transport, TransportError, MAX_FRAME};

/// Frames the kernel may queue for one service before sends fail with `Busy`.
pub const INBOX_CAPACITY: usize = 256;
/// Messages from all services the kernel may have unread before readers wait.
const INBOUND_CAPACITY: usize = 1024;

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder().max_frame_length(MAX_FRAME).new_codec()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Hello {
    secret: String,
}

struct Endpoint {
    accept_task: JoinHandle<()>,
    /// Sender to the writer task of the current connection, if any.
    conn: Arc<Mutex<Option<Conn>>>,
}

struct Conn {
    generation: u64,
    out: mpsc::Sender<Bytes>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

pub struct UnixTransport {
    dir: PathBuf,
    endpoints: Mutex<HashMap<ServiceId, Endpoint>>,
    inbound_tx: mpsc::Sender<Inbound>,
    inbound_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
}

impl UnixTransport {
    /// Use `dir` for the sockets. It is created with mode 0700 if missing.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, TransportError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Ok(Self { dir, endpoints: Mutex::default(), inbound_tx, inbound_rx: Mutex::new(Some(inbound_rx)) })
    }

    pub fn socket_path(dir: &Path, id: &ServiceId) -> PathBuf {
        dir.join(format!("{id}.sock"))
    }
}

#[async_trait]
impl Transport for UnixTransport {
    async fn open(&self, id: &ServiceId, secret: &Secret) -> Result<(), TransportError> {
        self.close(id).await?;
        let path = Self::socket_path(&self.dir, id);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let conn: Arc<Mutex<Option<Conn>>> = Arc::default();
        let accept_task =
            tokio::spawn(accept_loop(listener, id.clone(), secret.clone(), conn.clone(), self.inbound_tx.clone()));
        self.endpoints.lock().unwrap().insert(id.clone(), Endpoint { accept_task, conn });
        Ok(())
    }

    async fn close(&self, id: &ServiceId) -> Result<(), TransportError> {
        if let Some(ep) = self.endpoints.lock().unwrap().remove(id) {
            ep.accept_task.abort();
            ep.conn.lock().unwrap().take();
            let _ = std::fs::remove_file(Self::socket_path(&self.dir, id));
        }
        Ok(())
    }

    async fn send(&self, to: &ServiceId, msg: &Envelope) -> Result<(), TransportError> {
        let frame = Bytes::from(serde_json::to_vec(msg)?);
        let out = {
            let eps = self.endpoints.lock().unwrap();
            let ep = eps.get(to).ok_or_else(|| TransportError::NotConnected(to.clone()))?;
            let conn = ep.conn.lock().unwrap();
            conn.as_ref().map(|c| c.out.clone()).ok_or_else(|| TransportError::NotConnected(to.clone()))?
        };
        out.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => TransportError::Busy(to.clone()),
            mpsc::error::TrySendError::Closed(_) => TransportError::NotConnected(to.clone()),
        })
    }

    fn inbound(&self) -> Option<BoxStream<'static, Inbound>> {
        let rx = self.inbound_rx.lock().unwrap().take()?;
        Some(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) }).boxed())
    }

    fn address(&self) -> String {
        format!("unix:{}", self.dir.display())
    }
}

impl Drop for UnixTransport {
    fn drop(&mut self) {
        for (id, ep) in self.endpoints.get_mut().unwrap().drain() {
            ep.accept_task.abort();
            let _ = std::fs::remove_file(Self::socket_path(&self.dir, &id));
        }
    }
}

async fn accept_loop(
    listener: UnixListener,
    id: ServiceId,
    secret: Secret,
    slot: Arc<Mutex<Option<Conn>>>,
    inbound: mpsc::Sender<Inbound>,
) {
    let mut generation = 0u64;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (rd, wr) = stream.into_split();
        let mut frames = FramedRead::new(rd, codec());
        // The first frame must carry the secret. Anything else drops the connection.
        let hello = match tokio::time::timeout(std::time::Duration::from_secs(5), frames.next()).await {
            Ok(Some(Ok(buf))) => serde_json::from_slice::<Hello>(&buf).ok(),
            _ => None,
        };
        if !hello.is_some_and(|h| secret.matches(&h.secret)) {
            tracing::warn!(service = %id, "rejected connection with a bad secret");
            continue;
        }
        generation += 1;
        let (out, out_rx) = mpsc::channel(INBOX_CAPACITY);
        let writer = tokio::spawn(write_loop(wr, out_rx));
        // Install the connection before its reader runs, so a reply to the
        // first message read can already be sent back. Replacing the slot
        // drops (and aborts) the previous connection.
        let mut guard = slot.lock().unwrap();
        let reader = tokio::spawn(read_loop(frames, id.clone(), inbound.clone(), slot.clone(), generation));
        *guard = Some(Conn { generation, out, reader, writer });
    }
}

async fn write_loop(wr: OwnedWriteHalf, mut rx: mpsc::Receiver<Bytes>) {
    let mut sink = FramedWrite::new(wr, codec());
    while let Some(frame) = rx.recv().await {
        if sink.send(frame).await.is_err() {
            break;
        }
    }
}

async fn read_loop(
    mut frames: FramedRead<OwnedReadHalf, LengthDelimitedCodec>,
    id: ServiceId,
    inbound: mpsc::Sender<Inbound>,
    slot: Arc<Mutex<Option<Conn>>>,
    generation: u64,
) {
    while let Some(Ok(buf)) = frames.next().await {
        match serde_json::from_slice::<Envelope>(&buf) {
            Ok(msg) => {
                if inbound.send(Inbound { from: id.clone(), msg }).await.is_err() {
                    break;
                }
            }
            Err(e) => tracing::warn!(service = %id, error = %e, "dropped a malformed frame"),
        }
    }
    // Clear the slot only if it still holds this connection (a newer one may
    // have replaced it). Dropping the Conn aborts this task, which is ending anyway.
    let mut guard = slot.lock().unwrap();
    if guard.as_ref().is_some_and(|c| c.generation == generation) {
        guard.take();
    }
}

/// A service's connection to its endpoint socket.
pub struct UnixLink {
    tx: tokio::sync::Mutex<FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>>,
    rx: tokio::sync::Mutex<FramedRead<OwnedReadHalf, LengthDelimitedCodec>>,
}

impl UnixLink {
    pub async fn connect(dir: &Path, id: &ServiceId, secret: &Secret) -> Result<Self, TransportError> {
        let stream = UnixStream::connect(UnixTransport::socket_path(dir, id)).await?;
        let (rd, wr) = stream.into_split();
        let mut tx = FramedWrite::new(wr, codec());
        let hello = serde_json::to_vec(&Hello { secret: secret.expose().to_owned() })?;
        tx.send(Bytes::from(hello)).await?;
        Ok(Self { tx: tokio::sync::Mutex::new(tx), rx: tokio::sync::Mutex::new(FramedRead::new(rd, codec())) })
    }
}

#[async_trait]
impl Link for UnixLink {
    async fn send(&self, msg: &Envelope) -> Result<(), TransportError> {
        let frame = Bytes::from(serde_json::to_vec(msg)?);
        self.tx.lock().await.send(frame).await.map_err(TransportError::from)
    }

    async fn recv(&self) -> Option<Envelope> {
        let mut rx = self.rx.lock().await;
        loop {
            let buf = rx.next().await?.ok()?;
            match serde_json::from_slice(&buf) {
                Ok(msg) => return Some(msg),
                Err(e) => tracing::warn!(error = %e, "dropped a malformed frame from the kernel"),
            }
        }
    }
}

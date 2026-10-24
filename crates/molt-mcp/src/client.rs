//! A client of one MCP server over stdio: newline-delimited JSON-RPC 2.0
//! on the server's stdin and stdout.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Notify};
use tokio_util::sync::CancellationToken;

use crate::cache::ListedTool;

/// The protocol version asked for; a server answers with the one it speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// The longest message read from a server.
const MAX_MESSAGE: usize = 16 << 20;
/// How much of the server's stderr is kept, to say why it failed.
const STDERR_TAIL: usize = 2048;
/// Most pages of a tool list, and most tools.
const MAX_PAGES: usize = 20;
const MAX_TOOLS: usize = 256;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running server.
pub(crate) struct Client {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next: AtomicU64,
    /// Cancelled when the server exits or its output ends.
    gone: CancellationToken,
    /// Why it is gone.
    why: Arc<Mutex<Option<String>>>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    /// Told when the server says its tool list changed.
    pub changed: Arc<Notify>,
    child: Mutex<Child>,
    pgid: Option<libc::pid_t>,
    _guard: Option<molt_tools::sandbox::Guard>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        let _ = lock(&self.child).start_kill();
    }
}

impl Client {
    /// Start `command` and open the MCP session with it.
    pub async fn start(
        mut command: Command,
        guard: Option<molt_tools::sandbox::Guard>,
        timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| format!("could not start it: {e}"))?;
        let pgid = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
        let stdin = child.stdin.take().ok_or("it has no stdin")?;
        let stdout = child.stdout.take().ok_or("it has no stdout")?;
        let stderr_pipe = child.stderr.take().ok_or("it has no stderr")?;
        let client = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Arc::default(),
            next: AtomicU64::new(1),
            gone: CancellationToken::new(),
            why: Arc::default(),
            stderr: Arc::default(),
            changed: Arc::default(),
            child: Mutex::new(child),
            pgid,
            _guard: guard,
        });
        tokio::spawn(keep_tail(stderr_pipe, client.stderr.clone()));
        tokio::spawn(read(Arc::downgrade(&client), stdout));
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "molt", "version": env!("CARGO_PKG_VERSION") },
        });
        client.request("initialize", init, timeout, None).await?;
        client.notify("notifications/initialized", json!({})).await?;
        Ok(client)
    }

    pub fn is_gone(&self) -> bool {
        self.gone.is_cancelled()
    }

    /// Cancelled when the server is gone.
    pub fn gone(&self) -> CancellationToken {
        self.gone.clone()
    }

    /// Why the server is gone, with the end of what it wrote to stderr.
    fn gone_reason(&self) -> String {
        let status = lock(&self.child).try_wait().ok().flatten();
        let why = lock(&self.why).clone().unwrap_or_else(|| "its output ended".into());
        let mut said = format!("it exited ({})", status.map(|s| s.to_string()).unwrap_or(why));
        let tail = lock(&self.stderr).iter().copied().collect::<Vec<u8>>();
        let tail = String::from_utf8_lossy(&tail);
        if !tail.trim().is_empty() {
            said.push_str(&format!("; it wrote: {}", tail.trim()));
        }
        said
    }

    async fn send(&self, message: Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let wrote = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        };
        wrote.await.map_err(|_| self.gone_reason())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// One request and its result, or why there is none.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, String> {
        if self.is_gone() {
            return Err(self.gone_reason());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if let Err(e) = self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);
        let outcome = tokio::select! {
            got = rx => got.unwrap_or_else(|_| Err(self.gone_reason())),
            () = self.gone.cancelled() => Err(self.gone_reason()),
            () = tokio::time::sleep(timeout) => {
                Err(format!("it did not answer {method} within {} ms", timeout.as_millis()))
            }
            () = cancel.cancelled() => Err("the call was cancelled".into()),
        };
        if lock(&self.pending).remove(&id).is_some() && !self.is_gone() {
            let _ = self
                .notify("notifications/cancelled", json!({ "requestId": id, "reason": "molt stopped waiting" }))
                .await;
        }
        outcome
    }

    /// Every tool the server lists.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<ListedTool>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.request("tools/list", params, timeout, None).await?;
            for tool in page.get("tools").and_then(Value::as_array).into_iter().flatten() {
                match serde_json::from_value::<ListedTool>(tool.clone()) {
                    Ok(tool) if tools.len() < MAX_TOOLS => tools.push(tool),
                    Ok(tool) => tracing::warn!(tool = %tool.name, "more than {MAX_TOOLS} tools: left out"),
                    Err(e) => tracing::warn!(error = %e, "a listed tool is malformed: left out"),
                }
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }
}

async fn keep_tail(mut pipe: impl AsyncRead + Unpin, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut tail = lock(&tail);
        tail.extend(&buf[..n]);
        while tail.len() > STDERR_TAIL {
            tail.pop_front();
        }
    }
}

/// Read the server's messages until its output ends: answer its requests,
/// pass on results, and note that its tool list changed.
async fn read(client: std::sync::Weak<Client>, stdout: tokio::process::ChildStdout) {
    let mut reader = BufReader::new(stdout);
    let why = loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_MESSAGE as u64 + 1).read_until(b'\n', &mut line).await {
            Ok(0) => break None,
            Ok(_) if line.len() > MAX_MESSAGE => break Some(format!("it sent a message over {MAX_MESSAGE} bytes")),
            Ok(_) => {}
            Err(e) => break Some(format!("reading its output failed: {e}")),
        }
        let Some(client) = client.upgrade() else { return };
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            if !line.iter().all(u8::is_ascii_whitespace) {
                tracing::debug!("an MCP server wrote a line that is not JSON");
            }
            continue;
        };
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                } else {
                    json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": format!("molt does not serve {method}") } })
                };
                let _ = client.send(reply).await;
            }
            (Some("notifications/tools/list_changed"), None) => client.changed.notify_one(),
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let Some(tx) = lock(&client.pending).remove(&id) else { continue };
                let result = match (message.get("result"), message.get("error")) {
                    (Some(result), _) => Ok(result.clone()),
                    (None, Some(error)) => Err(format!(
                        "it answered with an error: {} ({})",
                        error.get("message").and_then(Value::as_str).unwrap_or("no message"),
                        error.get("code").map(Value::to_string).unwrap_or_default()
                    )),
                    (None, None) => Err("it answered with neither a result nor an error".into()),
                };
                let _ = tx.send(result);
            }
            (None, None) => {}
        }
    };
    if let Some(client) = client.upgrade() {
        *lock(&client.why) = why;
        // Give the exit status a moment to be collected for the message.
        let _ = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if lock(&client.child).try_wait().ok().flatten().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        client.gone.cancel();
        lock(&client.pending).clear();
    }
}

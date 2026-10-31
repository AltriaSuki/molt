//! A tiny service for trying the kernel out and for end-to-end tests.
//!
//! Methods: `echo` returns its payload, `sleep` waits `payload` milliseconds
//! first, `crash` exits the process without replying, and `count` adds one
//! to a counter kept in its private state directory (`MOLT_STATE_DIR`, which
//! a sandboxed service gets) and returns it.

use molt_proto::{ErrorCode, RemoteError, Target};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let svc = molt_sdk::Service::connect_from_env().await?;
    svc.serve(|req| async move {
        let method = match &req.to {
            Target::Method { method, .. } => method.as_str(),
            _ => "",
        };
        match method {
            "echo" => Ok(req.payload),
            "sleep" => {
                let ms = req.payload.as_u64().unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(req.payload)
            }
            "crash" => std::process::exit(70),
            "count" => count().map(serde_json::Value::from).map_err(|e| RemoteError {
                code: ErrorCode::Failed,
                message: format!("the counter in MOLT_STATE_DIR: {e}"),
            }),
            other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
        }
    })
    .await;
    Ok(())
}

/// One more than the counter in the state directory, written back.
fn count() -> std::io::Result<u64> {
    let dir = std::env::var_os("MOLT_STATE_DIR").ok_or_else(|| std::io::Error::other("it is not set"))?;
    let path = std::path::Path::new(&dir).join("count");
    let now = match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().parse::<u64>().map_err(std::io::Error::other)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    } + 1;
    std::fs::write(&path, now.to_string())?;
    Ok(now)
}

//! A tiny service for trying the kernel out and for end-to-end tests.
//!
//! Methods: `echo` returns its payload, `sleep` waits `payload` milliseconds
//! first, and `crash` exits the process without replying.

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
            other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
        }
    })
    .await;
    Ok(())
}

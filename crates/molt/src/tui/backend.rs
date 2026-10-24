//! Keep the kernel and sessions alive when the terminal process is suspended.
//! The child owns the same host-side CLI identity as the former in-process UI.
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::app::{Action, Msg};
use super::Runtime;
use crate::Config;

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Init {
    cfg: Config,
    workspace: String,
}

async fn write<W: AsyncWrite + Unpin>(out: &mut W, value: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "interface backend frame exceeds 16 MiB");
    out.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}

async fn read<R: AsyncRead + Unpin>(input: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    let mut size = [0; 4];
    if input.read(&mut size[..1]).await? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut size[1..]).await?;
    let len = u32::from_be_bytes(size) as usize;
    ensure!(len > 0 && len <= MAX_FRAME, "invalid interface backend frame size");
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

pub(super) struct Backend {
    child: Child,
    input: ChildStdin,
}

impl Backend {
    pub async fn start(
        cfg: Config,
        workspace: String,
        tx: mpsc::UnboundedSender<Msg>,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("ui-backend").stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true).process_group(0);
        let mut child = command.spawn().context("starting the interface backend")?;
        let mut input = child.stdin.take().context("interface backend has no input")?;
        let mut output = child.stdout.take().context("interface backend has no output")?;
        write(&mut input, &Init { cfg, workspace }).await?;
        tokio::spawn(async move {
            let result = async {
                while let Some(bytes) = read(&mut output).await? {
                    let msg = serde_json::from_slice(&bytes).context("invalid interface backend message")?;
                    if tx.send(Msg::Backend { generation, message: Box::new(msg) }).is_err() {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                anyhow::bail!("interface backend closed; pending operations have unknown settlement")
            }
            .await;
            if let Err(e) = result {
                let _ = tx.send(Msg::Backend { generation, message: Box::new(Msg::BackendFailed(e.to_string())) });
            }
        });
        Ok(Self { child, input })
    }

    pub async fn send(&mut self, action: &Action) -> anyhow::Result<()> {
        write(&mut self.input, action).await
    }

    pub async fn shutdown(self, keep: &[String]) -> anyhow::Result<()> {
        let Self { mut child, mut input } = self;
        write(&mut input, &Action::Quit { keep: keep.to_vec() }).await?;
        drop(input); // A blocked stdin read in the child must see EOF before its runtime exits.
        let status = child.wait().await?;
        ensure!(status.success(), "interface backend exited {status}");
        Ok(())
    }
}

pub(super) async fn serve() -> anyhow::Result<()> {
    let mut input = tokio::io::stdin();
    let bytes = read(&mut input).await?.context("interface backend needs its private initialization frame")?;
    let Init { cfg, workspace } = serde_json::from_slice(&bytes)?;
    let stop = crate::stop_signal()?;
    tokio::pin!(stop);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let output_file = protocol_output()?;
    let (out, mut frames) = mpsc::channel(64);
    let collector = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match out.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(Msg::Progress(_))) => {
                    // Preview sequences expose gaps and final RPC results retain
                    // authoritative settlement. A stopped UI cannot block the bus.
                }
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    if out.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
    });
    let writer = tokio::spawn(async move {
        let mut output = tokio::fs::File::from_std(output_file);
        while let Some(msg) = frames.recv().await {
            write(&mut output, &msg).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    // A separate reader owns each whole frame: selecting on startup/signals
    // must not cancel a partially consumed header or body.
    let (actions, mut incoming) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        loop {
            let frame = read(&mut input).await;
            let ended = !matches!(frame, Ok(Some(_)));
            if actions.send(frame).is_err() || ended {
                break;
            }
        }
    });
    let (sessions, mut started) = mpsc::unbounded_channel();
    let mut rt = Runtime {
        cfg: Arc::new(cfg),
        workspace,
        tx,
        sessions,
        session: None,
        starting: false,
        work: Vec::new(),
        before: None,
    };
    let mut keep = Vec::new();
    // The interface went without saying what to keep: it crashed or was killed.
    let mut orphaned = false;
    let result = loop {
        tokio::select! {
            frame = incoming.recv() => {
                match frame.unwrap_or(Ok(None)) {
                    Ok(Some(bytes)) => match serde_json::from_slice::<Action>(&bytes) {
                        Ok(Action::Quit { keep: k }) => { keep = k; break Ok(()); },
                        Ok(action) => rt.exec(action),
                        Err(e) => break Err(e.into()),
                    },
                    Ok(None) => { orphaned = true; break Ok(()) },
                    Err(e) => { orphaned = true; break Err(e) },
                }
            },
            Some(session) = started.recv() => {
                let msg = rt.started(session);
                let _ = rt.tx.send(msg);
            },
            _ = &mut stop => break Ok(()),
        }
    };
    for work in rt.work.drain(..) {
        work.abort();
    }
    if rt.starting {
        if let Some(Ok(session)) = started.recv().await {
            rt.session = Some(session);
        }
    }
    if let Some(session) = rt.session.take() {
        if orphaned {
            session.abandon().await;
        } else {
            session.shutdown(&keep).await;
        }
    }
    reader.abort();
    collector.abort();
    writer.abort();
    result
}

/// Services inherit stdout as their log, never the private protocol descriptor.
fn protocol_output() -> anyhow::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: duplicate this process's pipe with close-on-exec; only the
    // returned File owns the new descriptor. dup2 changes our stdout to our log.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(fd >= 0, "could not keep the interface protocol output");
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } >= 0,
        "could not separate service output from the interface protocol"
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pipe_frames_round_trip_and_reject_oversize_and_truncation() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write(&mut a, &Action::Quit { keep: vec!["fork-a".into()] }).await.unwrap();
        let bytes = read(&mut b).await.unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Action>(&bytes).unwrap(), Action::Quit { keep: vec!["fork-a".into()] });
        a.write_all(&((MAX_FRAME + 1) as u32).to_be_bytes()).await.unwrap();
        assert!(read(&mut b).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        assert!(read(&mut b).await.is_err());
    }
}

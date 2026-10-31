//! The interface as a client of the protocol (RFC 0004). Its server is a
//! child of its own (`molt ui-backend`), so the kernel and the sessions live
//! on when the terminal process is suspended; the child keeps the same
//! host-side CLI identity as the former in-process UI. Each [`Action`] that
//! needs the services becomes a request, and each answer and progress event
//! becomes the [`Msg`] the app took before.
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context;
use molt_api::planner::Control;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use super::app::{Action, Msg};
use crate::protocol::{self as p, code, method, Error, Id, ServerMessage};
use crate::Config;

/// What the interface hands its server before the protocol starts: one
/// line, on a pipe only the two of them hold.
#[derive(Serialize, Deserialize)]
struct Init {
    cfg: Config,
    workspace: String,
}

/// Turns an answer into what the app is told, if anything.
type Handler = Box<dyn FnOnce(Result<Value, Error>) -> Option<Msg> + Send>;
type Pending = Arc<Mutex<HashMap<u64, Handler>>>;

pub(super) struct Backend {
    child: Option<Child>,
    input: Box<dyn AsyncWrite + Unpin + Send>,
    pending: Pending,
    next: u64,
}

impl Backend {
    pub async fn start(
        cfg: Config,
        workspace: String,
        tx: mpsc::UnboundedSender<Msg>,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command.arg("ui-backend").stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true).process_group(0);
        let mut child = command.spawn().context("starting the interface backend")?;
        let mut input = child.stdin.take().context("interface backend has no input")?;
        let output = child.stdout.take().context("interface backend has no output")?;
        p::write_line(&mut input, &Init { cfg, workspace }).await?;
        let mut backend = Self::over(output, input, tx, generation).await?;
        backend.child = Some(child);
        Ok(backend)
    }

    /// A client on `output` and `input` of a server, introduced to it.
    async fn over(
        output: impl AsyncRead + Unpin + Send + 'static,
        input: impl AsyncWrite + Unpin + Send + 'static,
        tx: mpsc::UnboundedSender<Msg>,
        generation: u64,
    ) -> anyhow::Result<Self> {
        let pending = Pending::default();
        let answers = pending.clone();
        tokio::spawn(async move {
            let backend = |message: Msg| Msg::Backend { generation, message: Box::new(message) };
            let result = async {
                let mut output = BufReader::new(output);
                let mut line = Vec::new();
                while p::read_line(&mut output, &mut line).await? {
                    let msg = match serde_json::from_slice(&line).context("invalid interface backend message")? {
                        ServerMessage::Response(response) => {
                            let Some(Id::Number(id)) = response.id else { continue };
                            let handler = answers.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                            match handler {
                                Some(handler) => handler(response.outcome()),
                                None => None,
                            }
                        }
                        // An event of a newer server is left out, as the app leaves out what it does not know.
                        ServerMessage::Notification(note) if note.method == method::PROGRESS => {
                            serde_json::from_value(note.params).ok().map(Msg::Progress)
                        }
                        ServerMessage::Notification(_) => None,
                    };
                    if let Some(msg) = msg {
                        if tx.send(backend(msg)).is_err() {
                            return Ok::<_, anyhow::Error>(());
                        }
                    }
                }
                anyhow::bail!("interface backend closed; pending operations have unknown settlement")
            }
            .await;
            if let Err(e) = result {
                let _ = tx.send(backend(Msg::BackendFailed(format!("{e:#}"))));
            }
        });
        let mut backend = Self { child: None, input: Box::new(input), pending, next: 0 };
        let hello = p::Initialize {
            protocol: p::VERSION,
            client: p::Client { name: "molt-tui".into(), version: env!("CARGO_PKG_VERSION").into() },
        };
        backend
            .request(
                method::INITIALIZE,
                hello,
                Box::new(|answer| {
                    answer.err().map(|e| Msg::BackendFailed(format!("the backend refused: {}", e.message)))
                }),
            )
            .await?;
        Ok(backend)
    }

    async fn request(&mut self, method: &str, params: impl Serialize, handler: Handler) -> anyhow::Result<()> {
        self.next += 1;
        let request = p::Request::new(Id::Number(self.next), method, params)?;
        // Before it is written, so the answer finds it however soon it comes.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(self.next, handler);
        p::write_line(&mut self.input, &request).await?;
        Ok(())
    }

    pub async fn send(&mut self, action: Action) -> anyhow::Result<()> {
        let Some((method, params, handler)) = request(action)? else { return Ok(()) };
        self.request(method, params, handler).await
    }

    pub async fn shutdown(mut self, keep: &[String]) -> anyhow::Result<()> {
        self.request(method::SESSION_CLOSE, p::Keep { keep: keep.to_vec() }, Box::new(|_| None)).await?;
        let Self { child, input, .. } = self;
        drop(input); // A blocked read in the child must see the end before its runtime exits.
        if let Some(mut child) = child {
            let status = child.wait().await?;
            anyhow::ensure!(status.success(), "interface backend exited {status}");
        }
        Ok(())
    }
}

/// The request `action` makes, and what its answer tells the app; `None`
/// for an action the loop carries out itself.
fn request(action: Action) -> anyhow::Result<Option<(&'static str, Value, Handler)>> {
    fn to(params: impl Serialize) -> serde_json::Result<Value> {
        serde_json::to_value(params)
    }
    let made: (&'static str, Value, Handler) = match action {
        Action::StartSession => (
            method::SESSION_OPEN,
            to(p::Empty {})?,
            Box::new(|answer| match answer {
                Ok(opened) => Some(match serde_json::from_value::<p::SessionOpened>(opened) {
                    Ok(o) => Msg::SessionUp {
                        versions: o.versions,
                        memory_problem: o.memory_problem,
                        default_model: o.default_model,
                    },
                    Err(e) => Msg::SessionFailed(format!("the answer to session.open is not what it should be: {e}")),
                }),
                // Asked for while one starts: that one's answer comes.
                Err(e) if e.code == code::STARTING => None,
                Err(e) => Some(Msg::SessionFailed(e.message)),
            }),
        ),
        Action::Design { req, trace } => (
            method::TASK_DESIGN,
            to(p::Task { trace: trace.clone(), request: req })?,
            on(move |result: Result<p::Designed, String>| match result {
                Ok(d) => Msg::Designed { trace, result: Ok(d.design), existing: d.existing },
                Err(e) => Msg::Designed { trace, result: Err(e), existing: Vec::new() },
            }),
        ),
        Action::Run { req, trace } => (
            method::TASK_SUBMIT,
            to(p::Task { trace: trace.clone(), request: req })?,
            on(move |result| Msg::Ran { trace, result: Box::new(result) }),
        ),
        Action::Plan { req, trace } => (
            method::TASK_PLAN,
            to(p::Plan { trace: trace.clone(), request: *req })?,
            on(move |result| Msg::Planned { trace, result: Box::new(result) }),
        ),
        Action::Control { trace, action } => {
            let (method, params) = match &action {
                Control::Constrain(constraint) => {
                    (method::TASK_STEER, to(p::Steer { trace: trace.clone(), constraint: constraint.clone() })?)
                }
                Control::Pause => (method::TASK_PAUSE, to(p::Run { trace: trace.clone() })?),
                Control::Resume => (method::TASK_RESUME, to(p::Run { trace: trace.clone() })?),
            };
            (method, params, on(move |result| Msg::Controlled { trace, action, result }))
        }
        Action::Learn { trace, model } => (
            method::TASK_LEARN,
            to(p::Learn { trace: trace.clone(), model })?,
            on(move |result| Msg::Learned { trace, result }),
        ),
        Action::Diff { fork } => {
            (method::RESULT_DIFF, to(p::Fork { fork: fork.clone() })?, on(move |result| Msg::Diff { fork, result }))
        }
        Action::Apply { fork } => (
            method::RESULT_APPLY,
            to(p::Apply { fork, paths: Vec::new() })?,
            on(|result: Result<molt_api::fs::MergeResponse, String>| Msg::Applied(result.map(|_| ()))),
        ),
        Action::ApplyOnly { fork, paths } => (
            method::RESULT_APPLY,
            to(p::Apply { fork, paths })?,
            on(|result: Result<molt_api::fs::MergeResponse, String>| Msg::Applied(result.map(|_| ()))),
        ),
        Action::Recheck { fork, only, check } => (
            method::RESULT_RECHECK,
            to(p::Recheck { fork: fork.clone(), only: only.clone(), check })?,
            on(move |result| Msg::Rechecked { fork, only, result }),
        ),
        Action::PrepareCommit { trace } => (method::COMMIT_PREPARE, to(p::Run { trace })?, Box::new(|_| None)),
        Action::Commit { trace, task, resp } => {
            (method::COMMIT_MAKE, to(p::Commit { trace, task, response: resp })?, answered(Msg::Committed))
        }
        Action::Undo { run } => (method::RUNS_UNDO, to(p::Undo { run })?, answered(Msg::Undone)),
        Action::Discard { fork } => (
            method::RESULT_DISCARD,
            to(p::Fork { fork })?,
            on(|result: Result<p::Empty, String>| Msg::Discarded(result.map(|_| ()))),
        ),
        Action::LoadNotes { query } => (method::NOTES_LIST, to(p::Query { query })?, on(Msg::Notes)),
        Action::Forget { id, reason } => (
            method::NOTES_FORGET,
            to(p::NoteReason { id: id.clone(), reason })?,
            on(move |result: Result<p::Forgot, String>| Msg::Forgot { id, result: result.map(|f| f.forgotten) }),
        ),
        Action::Remember { text } => (
            method::NOTES_REMEMBER,
            to(p::Remember { text })?,
            on(|result: Result<p::NoteId, String>| Msg::Remembered(result.map(|n| n.id))),
        ),
        Action::Promote { id, reason } => (
            method::NOTES_PROMOTE,
            to(p::NoteReason { id: id.clone(), reason })?,
            on(move |result: Result<p::NoteId, String>| Msg::Promoted { id, result: result.map(|n| n.id) }),
        ),
        Action::ChangeNote { review, text } => {
            (method::NOTES_CHANGE, to(p::ChangeNote { review, text })?, on(Msg::ChangedNote))
        }
        Action::UsedNotes { run } => (
            method::NOTES_USED,
            to(p::UsedNotes { run: run.clone() })?,
            on(move |result| Msg::UsedNotes { run, result }),
        ),
        Action::Command { command, trace } => (
            method::COMMAND_RUN,
            to(p::Command { trace: trace.clone(), command })?,
            on(move |result| Msg::CommandRan { trace, result }),
        ),
        Action::StopCommand { trace } => (method::COMMAND_STOP, to(p::Run { trace })?, Box::new(|_| None)),
        Action::ListRuns => (
            method::RUNS_LIST,
            to(p::Empty {})?,
            Box::new(|answer| {
                let runs = answer.ok().and_then(|runs| serde_json::from_value(runs).ok()).unwrap_or_default();
                Some(Msg::Runs(runs))
            }),
        ),
        Action::DropRun { run } => (
            method::RUNS_DROP,
            to(p::DropRun { run: run.clone() })?,
            on(move |result: Result<p::Empty, String>| Msg::RunDropped { run, result: result.map(|_| ()) }),
        ),
        Action::Interrupt { keep, attempts, check_files, drop_run } => (
            method::TASK_STOP,
            to(p::Stop { keep, attempts, check_files, drop_run })?,
            Box::new(|answer| match answer {
                Ok(stopped) => {
                    let p::Stopped { kept, notes } = serde_json::from_value(stopped).unwrap_or_default();
                    Some(Msg::Stopped { kept, notes })
                }
                // The session that is starting comes up, to be stopped then.
                Err(e) if e.code == code::STARTING => None,
                Err(e) => Some(Msg::Stopped { kept: Vec::new(), notes: vec![e.message] }),
            }),
        ),
        // The loop carries these out: they need the terminal, or read the workspace.
        Action::Quit { .. }
        | Action::RestartSession { .. }
        | Action::ShowLog
        | Action::Edit { .. }
        | Action::Redraw
        | Action::Suspend
        | Action::SaveTheme(_)
        | Action::Alert { .. }
        | Action::ListFiles
        | Action::ReadAudit { .. }
        | Action::RunHistory
        | Action::Export { .. }
        | Action::Changes
        | Action::FileDiff { .. }
        | Action::EditFork { .. } => return Ok(None),
    };
    Ok(Some(made))
}

/// The answer to a call on the session, as `msg` takes it. Without a session
/// the app is told the services are down, and a call `task.stop` cut off
/// tells it nothing, as before the protocol.
fn on<T: DeserializeOwned + 'static>(msg: impl FnOnce(Result<T, String>) -> Msg + Send + 'static) -> Handler {
    Box::new(move |answer| match answer {
        Err(e) if e.code == code::NO_SESSION => Some(Msg::SessionFailed(e.message)),
        Err(e) if e.code == code::CANCELLED => None,
        answer => Some(msg(typed(answer))),
    })
}

/// [`on`] for the calls a stop does not end and that say themselves that
/// the services are down.
fn answered<T: DeserializeOwned + 'static>(msg: impl FnOnce(Result<T, String>) -> Msg + Send + 'static) -> Handler {
    Box::new(move |answer| Some(msg(typed(answer))))
}

fn typed<T: DeserializeOwned>(answer: Result<Value, Error>) -> Result<T, String> {
    let value = answer.map_err(|e| e.message)?;
    serde_json::from_value(value).map_err(|e| format!("the answer is not what it should be: {e}"))
}

/// `molt ui-backend`: the interface's own server. The config it resolved
/// comes first, then the client protocol, on the same pipe.
pub(super) async fn serve() -> anyhow::Result<()> {
    let output = tokio::fs::File::from_std(crate::serve::protocol_output()?);
    let mut input = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    anyhow::ensure!(
        p::read_line(&mut input, &mut line).await?,
        "interface backend needs its private initialization line"
    );
    let Init { cfg, workspace } = serde_json::from_slice(&line)?;
    let stop = crate::stop_signal()?;
    let server = crate::serve::Server::new(cfg, workspace);
    crate::serve::stdio_with(server, input, output, stop).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_proto::TraceId;

    /// The interface's client on a server with no services, over an in-memory pipe.
    async fn client(workspace: &str) -> (Backend, mpsc::UnboundedReceiver<Msg>) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (theirs_in, theirs_out) = tokio::io::split(theirs);
        let server = crate::serve::Server::new(Config::default(), workspace.to_owned());
        tokio::spawn(async move {
            server.connection(theirs_in, theirs_out, tokio_util::sync::CancellationToken::new()).await
        });
        let (input, output) = tokio::io::split(ours);
        let (tx, rx) = mpsc::unbounded_channel();
        (Backend::over(input, output, tx, 7).await.unwrap(), rx)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Msg {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        let Msg::Backend { generation: 7, message } = msg else { panic!("{msg:?}") };
        *message
    }

    #[tokio::test]
    async fn the_interfaces_actions_go_out_as_requests_and_come_back_as_the_messages_it_took_before() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let (mut backend, mut rx) = client(ws.to_str().unwrap()).await;
        backend.send(Action::ListRuns).await.unwrap();
        assert!(matches!(next(&mut rx).await, Msg::Runs(runs) if runs.is_empty()));
        // Without a session, a call says the services are down, as the in-process runtime did.
        backend.send(Action::Diff { fork: "/f".into() }).await.unwrap();
        assert!(matches!(next(&mut rx).await, Msg::SessionFailed(e) if e == "the services are not up"));
        backend.send(Action::Undo { run: None }).await.unwrap();
        assert!(matches!(next(&mut rx).await, Msg::Undone(Err(e)) if e == "the services are not up"));
        let trace = TraceId::random();
        let control = Control::Pause;
        backend.send(Action::Control { trace: trace.clone(), action: control }).await.unwrap();
        assert!(matches!(next(&mut rx).await, Msg::SessionFailed(_)));
        backend
            .send(Action::Interrupt { keep: vec![], attempts: vec![], check_files: vec![], drop_run: None })
            .await
            .unwrap();
        assert!(matches!(next(&mut rx).await, Msg::Stopped { kept, notes } if kept.is_empty() && notes.is_empty()));
        // What the loop carries out itself sends nothing; the next message answers the next request.
        backend.send(Action::Redraw).await.unwrap();
        backend.send(Action::StopCommand { trace }).await.unwrap();
        backend
            .send(Action::Commit {
                trace: TraceId::random(),
                task: "t".into(),
                resp: Box::new(crate::serve::tests::run_response("b.txt")),
            })
            .await
            .unwrap();
        assert!(
            matches!(next(&mut rx).await, Msg::Committed(Err(e)) if e.contains("turned on after the task started"))
        );
        backend.shutdown(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn a_server_that_goes_away_leaves_the_interface_saying_so() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (input, output) = tokio::io::split(ours);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = Backend::over(input, output, tx, 7).await.unwrap();
        drop(theirs);
        let msg = next(&mut rx).await;
        assert!(matches!(&msg, Msg::BackendFailed(e) if e.contains("unknown settlement")), "{msg:?}");
        drop(backend);
    }
}

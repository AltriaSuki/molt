//! `molt` with no subcommand: an interface in the terminal that keeps the
//! agent services up and takes one task after another.
//!
//! [`app`] holds the state and the rules, [`view`] draws it, and the loop
//! here carries the [`app::Action`]s out on a [`Session`] and feeds what
//! comes back, with the keys, to the app.

mod app;
mod text;
mod view;

use std::future::Future;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use self::app::{Action, App, Msg};
use crate::session::Session;
use crate::{agent, Config};

/// How often the spinners turn.
const TICK: Duration = Duration::from_millis(150);

pub struct Options {
    pub config: Option<PathBuf>,
    pub workspace: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub pass_env: Vec<String>,
}

/// Run the interface until the user quits. Returns the forks the user kept.
pub async fn run(opts: Options) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "molt needs a terminal for its interface; in scripts, use `molt do TASK`"
    );
    let stop = crate::stop_signal()?;
    let workspace = opts.workspace.canonicalize().with_context(|| format!("workspace {}", opts.workspace.display()))?;
    anyhow::ensure!(workspace.is_dir(), "the workspace {} is not a directory", workspace.display());
    let path = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let mut cfg = agent::resolve_config(opts.config.as_deref(), &workspace, opts.data_dir.as_deref())?;
    if let Some(shell) = cfg.service_mut("shell") {
        shell.pass_env.extend(opts.pass_env);
    }
    if std::env::var_os("RUST_LOG").is_none() {
        for svc in &mut cfg.services {
            svc.env.entry("RUST_LOG".into()).or_insert_with(|| "warn".into());
        }
    }
    std::fs::create_dir_all(&cfg.kernel.data_dir)
        .with_context(|| format!("data dir {}", cfg.kernel.data_dir.display()))?;
    // The services write their logs to stderr, which they share with molt;
    // on the screen they would tear through the interface.
    let log = cfg.kernel.data_dir.join("tui.log");
    let stderr = Redirect::stderr_to(&log)?;
    let mut terminal = ratatui::init();
    let restore = stderr.saved;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // SAFETY: dup2 on descriptors this process owns.
        unsafe { libc::dup2(restore, libc::STDERR_FILENO) };
        hook(info);
    }));
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let home = std::env::var("HOME").ok();
    let result = event_loop(&mut terminal, cfg, path, home.as_deref(), stop).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    drop(stderr);
    let _ = std::panic::take_hook();
    result.with_context(|| format!("the services' log is in {}", log.display()))
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    cfg: Config,
    workspace: String,
    home: Option<&str>,
    stop: impl Future<Output = i32> + Send + 'static,
) -> anyhow::Result<Vec<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sessions, mut started) = mpsc::unbounded_channel();
    read_terminal(tx.clone());
    let ticks = tx.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(TICK);
        while ticks.send(Msg::Tick).is_ok() {
            every.tick().await;
        }
    });
    let signals = tx.clone();
    tokio::spawn(async move {
        stop.await;
        let _ = signals.send(Msg::Signal);
    });

    let mut app = App::new(&workspace, home);
    let mut rt =
        Runtime { cfg: Arc::new(cfg), workspace, tx, sessions, session: None, starting: false, work: Vec::new() };
    rt.exec(Action::StartSession);
    let mut keep = Vec::new();
    loop {
        terminal.draw(|frame| view::draw(frame, &mut app))?;
        let msg = tokio::select! {
            Some(msg) = rx.recv() => msg,
            Some(session) = started.recv() => rt.started(session),
        };
        let mut actions = app.update(msg);
        while let Ok(msg) = rx.try_recv() {
            actions.extend(app.update(msg));
        }
        for action in actions {
            if let Action::Quit { keep: k } = action {
                keep = k;
            } else {
                rt.exec(action);
            }
        }
        if app.quit {
            break;
        }
    }
    for work in rt.work.drain(..) {
        work.abort();
    }
    if rt.starting {
        if let Some(Ok(session)) = started.recv().await {
            rt.session = Some(session);
        }
    }
    if let Some(session) = rt.session.take() {
        session.shutdown(&keep).await;
    }
    Ok(keep)
}

/// Keys, pastes and resizes, read on a thread of their own: crossterm's
/// reads block.
fn read_terminal(tx: UnboundedSender<Msg>) {
    std::thread::spawn(move || loop {
        let msg = match event::read() {
            Ok(Event::Key(key)) => Msg::Key(key),
            Ok(Event::Paste(text)) => Msg::Paste(text),
            Ok(Event::Resize(..)) => Msg::Tick,
            Ok(_) => continue,
            Err(_) => break,
        };
        if tx.send(msg).is_err() {
            break;
        }
    });
}

struct Runtime {
    cfg: Arc<Config>,
    workspace: String,
    tx: UnboundedSender<Msg>,
    sessions: UnboundedSender<Result<Arc<Session>, String>>,
    session: Option<Arc<Session>>,
    /// A session is starting; its result comes on `sessions`.
    starting: bool,
    /// Calls in flight, stopped on an interrupt.
    work: Vec<JoinHandle<()>>,
}

impl Runtime {
    fn started(&mut self, session: Result<Arc<Session>, String>) -> Msg {
        self.starting = false;
        match session {
            Ok(session) => {
                let msg =
                    Msg::SessionUp { versions: session.versions().to_vec(), memory_problem: session.memory_problem() };
                self.session = Some(session);
                msg
            }
            Err(e) => Msg::SessionFailed(e),
        }
    }

    fn exec(&mut self, action: Action) {
        self.work.retain(|w| !w.is_finished());
        match action {
            Action::StartSession => {
                self.starting = true;
                let (cfg, workspace, tx, sessions) =
                    (self.cfg.clone(), self.workspace.clone(), self.tx.clone(), self.sessions.clone());
                tokio::spawn(async move {
                    let (events, mut progress) = mpsc::unbounded_channel();
                    tokio::spawn(async move {
                        while let Some(event) = progress.recv().await {
                            if tx.send(Msg::Progress(event)).is_err() {
                                break;
                            }
                        }
                    });
                    let session = Session::start(&cfg, &workspace, events).await;
                    let _ = sessions.send(session.map(Arc::new).map_err(|e| format!("{e:#}")));
                });
            }
            Action::Design { req, trace } => self.call(move |s| async move {
                let result = s.design(&req, &trace).await.map_err(|e| format!("{e:#}"));
                Msg::Designed { trace, result }
            }),
            Action::Run { req, trace } => self.call(move |s| async move {
                let result = s.run(&req, &trace).await.map_err(|e| format!("{e:#}"));
                Msg::Ran { trace, result }
            }),
            Action::Learn { trace } => self.call(move |s| async move {
                let result = s.learn(&trace).await;
                Msg::Learned { trace, result }
            }),
            Action::Apply { fork } => self.call(move |s| async move {
                Msg::Applied(s.apply(&fork).await.map(|_| ()).map_err(|e| format!("{e:#}")))
            }),
            Action::Discard { fork } => {
                self.call(move |s| async move { Msg::Discarded(s.discard(&fork).await.map_err(|e| format!("{e:#}"))) })
            }
            Action::LoadNotes { query } => {
                self.call(move |s| async move { Msg::Notes(s.notes(&query).await.map_err(|e| format!("{e:#}"))) })
            }
            Action::Forget { id, reason } => self.call(move |s| async move {
                let result = s.forget(&id, &reason).await.map_err(|e| format!("{e:#}"));
                Msg::Forgot { id, result }
            }),
            Action::Interrupt { keep } => {
                for work in self.work.drain(..) {
                    work.abort();
                }
                let tx = self.tx.clone();
                match self.session.take() {
                    Some(session) => {
                        tokio::spawn(async move {
                            session.shutdown(&keep).await;
                            let _ = tx.send(Msg::Stopped);
                        });
                    }
                    None if self.starting => {}
                    None => {
                        let _ = tx.send(Msg::Stopped);
                    }
                }
            }
            // The loop handles quitting.
            Action::Quit { .. } => {}
        }
    }

    /// Make a call on the session in a task of its own; its message goes to the app.
    fn call<F, Fut>(&mut self, work: F)
    where
        F: FnOnce(Arc<Session>) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let Some(session) = self.session.clone() else {
            let _ = tx.send(Msg::SessionFailed("the services are not up".into()));
            return;
        };
        self.work.push(tokio::spawn(async move {
            let _ = tx.send(work(session).await);
        }));
    }
}

/// Standard error sent to a file until dropped.
struct Redirect {
    saved: libc::c_int,
}

impl Redirect {
    fn stderr_to(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: dup and dup2 on descriptors this process owns; `file` stays
        // open until dup2 has made its copy.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        anyhow::ensure!(saved >= 0, "could not keep standard error: {}", std::io::Error::last_os_error());
        if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(saved) };
            anyhow::bail!("could not send standard error to {}: {e}", path.display());
        }
        Ok(Self { saved })
    }
}

impl Drop for Redirect {
    fn drop(&mut self) {
        // SAFETY: as in `stderr_to`.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
    }
}

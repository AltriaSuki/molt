//! `molt` with no subcommand: an interface in the terminal that keeps the
//! agent services up and takes one task after another.
//!
//! [`app`] holds the state and the rules, [`view`] draws it, and the loop
//! here carries the [`app::Action`]s out on a [`Session`] and feeds what
//! comes back, with the keys, to the app.

mod app;
mod backend;
mod commands;
mod kept;
mod log;
mod notice;
mod preview;
mod saved;
mod settings;
mod state;
mod text;
mod transcript;
mod view;

use std::future::Future;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use self::app::{Action, App, Msg};
use self::notice::Notice;
use crate::session::Session;
use crate::{agent, Config};

/// How often the spinners turn.
const TICK: Duration = Duration::from_millis(app::TICK_MS);

pub struct Options {
    pub config: Option<PathBuf>,
    pub workspace: PathBuf,
    /// A project of the registry, by id or name, in place of `workspace`.
    pub project: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub pass_env: Vec<String>,
    /// Dependency caches commands build from offline (`--deps-cache`).
    pub deps_cache: Vec<String>,
    /// Hosts the agent may read web pages from.
    pub allow_web: Vec<String>,
    pub web_resolver: Option<String>,
}

/// Run the interface until the user quits. Returns the forks the user kept.
pub async fn run(opts: Options) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "molt needs a terminal for its interface; in scripts, use `molt do TASK`"
    );
    let stop = crate::stop_signal()?;
    let (workspace, project) = agent::locate(opts.project.as_deref(), &opts.workspace, opts.config.is_some())?;
    let path = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let mut cfg =
        agent::resolve_config(opts.config.as_deref(), &workspace, opts.data_dir.as_deref(), project.as_ref())?;
    if let Some(shell) = cfg.service_mut("shell") {
        shell.pass_env.extend(opts.pass_env);
    }
    agent::deps_cache(&mut cfg, &opts.deps_cache)?;
    agent::allow_web(&mut cfg, &opts.allow_web)?;
    if let Some(resolver) = &opts.web_resolver {
        cfg.web_resolver(resolver)?;
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
        emit(notice::POP_TITLE);
        hook(info);
    }));
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableFocusChange);
    emit(notice::PUSH_TITLE);
    // A virtual project's output directory is otherwise hard to discover.
    let home = std::env::var("HOME").ok().filter(|_| !project.as_ref().is_some_and(|p| p.paths.is_empty()));
    let mut unexecuted = Vec::new();
    let result = event_loop(&mut terminal, cfg, path, home.as_deref(), stop, &mut unexecuted).await;
    emit(notice::POP_TITLE);
    let _ = execute!(std::io::stdout(), DisableFocusChange, DisableBracketedPaste);
    ratatui::restore();
    drop(stderr);
    let _ = std::panic::take_hook();
    for (index, task) in unexecuted.iter().enumerate() {
        println!("not run (queued {}):\n{task}", index + 1);
    }
    result.with_context(|| format!("the services' log is in {}", log.display()))
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    cfg: Config,
    workspace: String,
    home: Option<&str>,
    stop: impl Future<Output = i32> + Send + 'static,
    unexecuted: &mut Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pause = Arc::new(Pause::default());
    read_terminal(tx.clone(), pause.clone());
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
    app.secrets = secrets_in_env();
    app.secrets.extend(
        cfg.services
            .iter()
            .flat_map(|service| &service.env)
            .filter(|(name, value)| value.len() >= 8 && secret_name(name))
            .map(|(_, value)| value.clone()),
    );
    app.rules = cfg.confirm.clone();
    app.load_commands(Path::new(&workspace));
    let store = match state::Store::new(&cfg.kernel.data_dir, Path::new(&workspace)) {
        Ok(store) => {
            match store.history(&app.secrets) {
                Ok((entries, damaged)) => {
                    app.restore_history(entries);
                    if damaged {
                        app.flash = Some("some damaged input history entries were ignored".into());
                    }
                }
                Err(e) => app.flash = Some(format!("saved input history ignored: {e}")),
            }
            Some(store)
        }
        Err(e) => {
            app.flash = Some(format!("workspace interface state could not be opened: {e}"));
            None
        }
    };
    settings::restore(&mut app, &cfg.tui, store.as_ref());
    if let Some(store) = &store {
        match kept::load(store, &app.secrets) {
            Ok(entries) => app.restore_kept(entries),
            Err(e) => app.flash = Some(format!("saved kept results ignored: {e}")),
        }
        match saved::load(store) {
            Ok(Some(saved)) => app.restore_session(saved),
            Ok(None) => {}
            Err(e) => app.flash = Some(format!("the saved session was not picked up: {e:#}")),
        }
    }
    let theme_file = cfg.kernel.data_dir.join("tui-theme.json");
    match std::fs::read(&theme_file) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(theme) => app.theme = theme,
            Err(e) => app.flash = Some(format!("saved theme ignored: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => app.flash = Some(format!("could not load saved theme: {e}")),
    }
    // Show startup state while DNS probes run; no backend or model has started yet.
    terminal.draw(|frame| view::draw(frame, &mut app))?;
    for warning in agent::web_warnings(&cfg).await? {
        app.log.extend(text::block(
            &app::scrub(&format!("web DNS preflight: {warning}"), &app.secrets),
            "",
            text::Tone::Warn,
        ));
    }
    for missing in agent::dependency_warnings(&cfg, Path::new(&workspace)) {
        app.log.extend(text::block(&format!("dependency preflight: {missing}"), "", text::Tone::Warn));
    }
    let data_dir = cfg.kernel.data_dir.clone();
    let (lister, root) = (tx.clone(), PathBuf::from(&workspace));
    let checkpoints = agent::checkpoint_dir(&cfg);
    // What stopped runs there are to go on with: read from their checkpoints, with no model call.
    list_runs_later(&lister, checkpoints.clone(), &workspace);
    let mut saved_session = None;
    let mut backend = backend::Backend::start(cfg.clone(), workspace.clone(), tx, app.backend_generation).await?;
    backend.send(&Action::StartSession).await?;
    let notice = Notice::from_env(|name| std::env::var(name).ok());
    let mut keep = Vec::new();
    let mut title = String::new();
    loop {
        if app.title() != title {
            title = app.title();
            emit(&notice::title(&title));
        }
        terminal.draw(|frame| view::draw(frame, &mut app))?;
        let Some(msg) = rx.recv().await else { break };
        let mut actions = app.update(msg);
        while let Ok(msg) = rx.try_recv() {
            actions.extend(app.update(msg));
        }
        for change in app.take_preference_changes() {
            if let Some(store) = &store {
                if let Err(e) = settings::persist(store, &change, &app.secrets) {
                    app.flash = Some(format!("settings changed but could not be saved: {e}"));
                }
            }
        }
        for change in app.take_history_changes() {
            if let Some(store) = &store {
                if let Err(e) = store.change_history(&change, &app.secrets) {
                    app.flash = Some(format!("input history could not be saved: {e}"));
                }
            }
        }
        for change in app.take_kept_changes() {
            if let Some(store) = &store {
                if let Err(e) = kept::persist(store, &change, &app.secrets) {
                    app.flash = Some(format!("kept-results metadata could not be saved: {e}"));
                }
            }
        }
        if let Some(store) = &store {
            // Before the actions are carried out, so a task is on disk before its run starts.
            let snapshot = app.snapshot();
            if saved_session.as_ref() != Some(&snapshot) {
                if let Err(e) = saved::save(store, &snapshot) {
                    app.flash = Some(format!("the session could not be saved: {e:#}"));
                }
                saved_session = Some(snapshot);
            }
        }
        for action in actions {
            match action {
                Action::Quit { keep: k } => keep = k,
                Action::Alert { text } => emit(&notice.alert(&text)),
                Action::RestartSession { keep } => {
                    app.backend_generation += 1;
                    if let Err(e) = backend.shutdown(&keep).await {
                        app.flash =
                            Some(format!("previous backend shutdown failed: {}", text::clean_line(&e.to_string())));
                    }
                    backend =
                        backend::Backend::start(cfg.clone(), workspace.clone(), lister.clone(), app.backend_generation)
                            .await?;
                    backend.send(&Action::StartSession).await?;
                }
                Action::ShowLog => {
                    let (tx, path) = (lister.clone(), data_dir.join("tui.log"));
                    tokio::task::spawn_blocking(move || {
                        let result = log::tail(&path).map_err(|e| e.to_string());
                        let _ = tx.send(Msg::ServiceLog(result));
                    });
                }
                Action::ListRuns => list_runs_later(&lister, checkpoints.clone(), &workspace),
                Action::ListFiles => {
                    let (tx, root) = (lister.clone(), root.clone());
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(list_files(&root));
                    });
                }
                Action::ReadAudit { trace, cursor } => {
                    let (tx, log) = (lister.clone(), data_dir.join("audit.jsonl"));
                    tokio::task::spawn_blocking(move || {
                        let result = read_audit(&log, &trace, cursor);
                        let _ = tx.send(Msg::Audit { trace, result });
                    });
                }
                Action::Export { text } => {
                    if let Err(e) = edit_outside(terminal, &pause, &data_dir, "transcript", &text) {
                        app.flash = Some(format!("the editor failed: {}", text::clean_line(&e)));
                    }
                }
                Action::Redraw => redraw(terminal)?,
                Action::Suspend => {
                    suspend(terminal, &pause)?;
                    // The shell had the window meanwhile: set the title again.
                    title.clear();
                }
                Action::SaveTheme(theme) => {
                    if let Err(e) = save_theme(&data_dir, theme) {
                        app.flash = Some(format!("theme changed but could not be saved: {e}"));
                    }
                }
                Action::Edit { text } => {
                    let edited = edit_outside(terminal, &pause, &data_dir, "task", &text);
                    app.update(Msg::Edited(edited));
                }
                Action::EditFork { fork } => {
                    // Through the loop, so the check it starts goes to the backend like any other action.
                    let result = step_aside(terminal, &pause, Path::new(&fork));
                    let _ = lister.send(Msg::ForkEdited { fork, result });
                }
                action => backend.send(&action).await?,
            }
        }
        if app.quit {
            break;
        }
    }
    *unexecuted = app.unexecuted();
    if let Some(store) = &store {
        // Nothing in flight: the next interface starts afresh. A task in flight stays, to go on with.
        let snapshot = app.snapshot();
        let result = if snapshot.active.is_none() { saved::remove(store) } else { saved::save(store, &snapshot) };
        if let Err(e) = result {
            tracing::warn!("the session could not be saved: {e:#}");
        }
    }
    backend.shutdown(&keep).await?;
    Ok(keep)
}

/// Internal child entry point. Standard input/output are private framed pipes,
/// not a terminal or the capability bus.
pub async fn serve_backend() -> anyhow::Result<()> {
    backend::serve().await
}

/// Keeps the terminal reader off the terminal while another program has it.
#[derive(Default)]
struct Pause {
    paused: AtomicBool,
    /// Held by the reader while it waits on the terminal.
    reading: Mutex<()>,
}

/// Keys, pastes and resizes, read on a thread of their own: crossterm's
/// reads block.
fn read_terminal(tx: UnboundedSender<Msg>, pause: Arc<Pause>) {
    std::thread::spawn(move || loop {
        if pause.paused.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let msg = {
            let _reading = pause.reading.lock().unwrap_or_else(PoisonError::into_inner);
            if pause.paused.load(Ordering::SeqCst) {
                continue;
            }
            // A bounded wait, so a pause never waits long for the reader to let go.
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            match event::read() {
                Ok(Event::Key(key)) => Msg::Key(key),
                Ok(Event::Paste(text)) => Msg::Paste(text),
                Ok(Event::FocusGained) => Msg::Focus(true),
                Ok(Event::FocusLost) => Msg::Focus(false),
                Ok(Event::Resize(..)) => Msg::Tick,
                Ok(_) => continue,
                Err(_) => break,
            }
        };
        if tx.send(msg).is_err() {
            break;
        }
    });
}

/// Values of environment variables named like credentials. Commands never
/// get them, but a user can pass any variable to the shell, so the interface
/// hides them in what commands print.
fn secrets_in_env() -> Vec<String> {
    std::env::vars().filter(|(name, value)| value.len() >= 8 && secret_name(name)).map(|(_, value)| value).collect()
}

fn secret_name(name: &str) -> bool {
    ["API_KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|marker| name.to_ascii_uppercase().contains(marker))
}

/// How long an interrupt waits before it lists a fork's changes again,
/// after the shell's grace for a stopped command (TERM, then KILL 500 ms on).
const RELIST_AFTER: Duration = Duration::from_secs(1);

/// What an interrupted attempt left in its fork.
#[derive(Debug, PartialEq)]
enum Leftover {
    /// Changes beyond the check's own files: the attempt's work, to keep.
    Work,
    /// Nothing but the check's files.
    Empty,
    /// Its changes could not be listed, and the fork is still there: kept,
    /// since it may hold work.
    Unlisted(String),
    /// Its changes could not be listed because the fork is gone.
    Gone(String),
}

fn leftover(fork: &str, listed: anyhow::Result<Vec<String>>, check_files: &[String]) -> Leftover {
    match listed {
        Ok(paths) if paths.iter().any(|p| !check_files.contains(p)) => Leftover::Work,
        Ok(_) => Leftover::Empty,
        Err(e) if Path::new(fork).exists() => Leftover::Unlisted(format!("{e:#}")),
        Err(e) => Leftover::Gone(format!("{e:#}")),
    }
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
    /// The work tree as it was when the task `.0` started, for `/commit on`.
    before: Option<(String, anyhow::Result<crate::git::Before>)>,
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
                if self.starting || self.session.is_some() {
                    return;
                }
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
                // Which check files would replace the workspace's own, for confirm mode to stop on.
                let existing = match &result {
                    Ok(d) => {
                        let root = Path::new(s.workspace());
                        d.files
                            .iter()
                            .filter(|f| root.join(&f.path).symlink_metadata().is_ok())
                            .map(|f| f.path.clone())
                            .collect()
                    }
                    Err(_) => Vec::new(),
                };
                Msg::Designed { trace, result, existing }
            }),
            Action::Run { req, trace } => self.call(move |s| async move {
                let result = s.run(&req, &trace).await.map_err(|e| format!("{e:#}"));
                Msg::Ran { trace, result }
            }),
            Action::Control { trace, action } => self.call(move |s| async move {
                let result = s.control(&trace, action.clone()).await.map_err(|e| format!("{e:#}"));
                Msg::Controlled { trace, action, result }
            }),
            Action::Learn { trace, model } => self.call(move |s| async move {
                let result = s.learn(&trace, model).await;
                Msg::Learned { trace, result }
            }),
            Action::Diff { fork } => self.call(move |s| async move {
                let result = s.diff(&fork).await.map_err(|e| format!("{e:#}"));
                Msg::Diff { fork, result }
            }),
            Action::Apply { fork } => self.call(move |s| async move {
                Msg::Applied(s.apply(&fork).await.map(|_| ()).map_err(|e| format!("{e:#}")))
            }),
            Action::ApplyOnly { fork, paths } => self.call(move |s| async move {
                Msg::Applied(s.apply_only(&fork, &paths).await.map(|_| ()).map_err(|e| format!("{e:#}")))
            }),
            Action::Recheck { fork, only, check } => self.call(move |s| async move {
                let result = s.recheck(&fork, only.as_deref(), check.as_ref()).await.map_err(|e| format!("{e:#}"));
                Msg::Rechecked { fork, only, result }
            }),
            Action::Undo { run } => {
                let (session, tx) = (self.session.clone(), self.tx.clone());
                // The interface waits for both Git and memory before quitting.
                // Like commits, this is not among the interruptible attempt work.
                tokio::spawn(async move {
                    let result = match session {
                        Some(session) => session.undo(run).await.map_err(|e| format!("{e:#}")),
                        None => Err("the services are not up".into()),
                    };
                    let _ = tx.send(Msg::Undone(result));
                });
            }
            Action::PrepareCommit { trace } => {
                self.before = Some((trace.to_string(), crate::git::Before::look(Path::new(&self.workspace))));
            }
            Action::Commit { trace, task, resp } => {
                let before = match self.before.take() {
                    Some((run, before)) if run == trace.as_str() => before,
                    other => {
                        self.before = other;
                        Err(anyhow::anyhow!("/commit was turned on after the task started"))
                    }
                };
                let tx = self.tx.clone();
                // Not among the interruptible work: a commit half made would leave the index behind.
                tokio::spawn(async move {
                    let made = tokio::task::spawn_blocking(move || before?.commit(&resp, trace.as_str(), &task, None))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|made| made);
                    let _ = tx.send(Msg::Committed(made.map_err(|e| format!("{e:#}"))));
                });
            }
            Action::Discard { fork } => {
                self.call(move |s| async move { Msg::Discarded(s.discard(&fork).await.map_err(|e| format!("{e:#}"))) })
            }
            Action::LoadNotes { query } => self
                .call(move |s| async move { Msg::Notes(s.inspect_notes(&query).await.map_err(|e| format!("{e:#}"))) }),
            Action::Forget { id, reason } => self.call(move |s| async move {
                let result = s.forget(&id, &reason).await.map_err(|e| format!("{e:#}"));
                Msg::Forgot { id, result }
            }),
            Action::Remember { text } => self.call(move |s| async move {
                Msg::Remembered(s.remember(&text).await.map(|r| r.note.id).map_err(|e| format!("{e:#}")))
            }),
            Action::Promote { id, reason } => self.call(move |s| async move {
                let result = s.promote(&id, &reason).await.map(|n| n.id).map_err(|e| format!("{e:#}"));
                Msg::Promoted { id, result }
            }),
            Action::ChangeNote { review, text } => self.call(move |s| async move {
                Msg::ChangedNote(s.change_note(review, text).await.map_err(|e| format!("{e:#}")))
            }),
            Action::UsedNotes { run } => self.call(move |s| async move {
                let result = s.used_notes(&run).await.map_err(|e| format!("{e:#}"));
                Msg::UsedNotes { run, result }
            }),

            Action::DropRun { run } => self.call(move |s| async move {
                let result = s.drop_run(&run).await.map_err(|e| format!("{e:#}"));
                Msg::RunDropped { run, result }
            }),
            Action::Interrupt { keep, attempts, check_files, drop_run } => {
                for work in self.work.drain(..) {
                    work.abort();
                }
                let tx = self.tx.clone();
                match self.session.take() {
                    Some(session) => {
                        tokio::spawn(async move {
                            let mut listed = Vec::new();
                            for fork in &attempts {
                                listed.push(session.changes(fork).await);
                            }
                            if listed.iter().any(Result::is_err) {
                                // A command the attempt started may still be writing in its fork until the
                                // shell has stopped it: look once more after that.
                                tokio::time::sleep(RELIST_AFTER).await;
                                for (fork, result) in attempts.iter().zip(listed.iter_mut()) {
                                    if result.is_err() {
                                        *result = session.changes(fork).await;
                                    }
                                }
                            }
                            let (mut kept, mut notes) = (Vec::new(), Vec::new());
                            for (fork, result) in attempts.into_iter().zip(listed) {
                                match leftover(&fork, result, &check_files) {
                                    Leftover::Work => kept.push(fork),
                                    Leftover::Empty => {
                                        // The planner left it to us; a scratch dir outside the data dir is not
                                        // cleaned up at shutdown.
                                        if let Err(e) = session.discard(&fork).await {
                                            tracing::warn!("could not drop {fork}: {e:#}");
                                        }
                                    }
                                    Leftover::Unlisted(e) => {
                                        tracing::warn!("could not list the changes in {fork}: {e}");
                                        notes.push(format!(
                                            "the changes in {fork} could not be listed, so it is kept in case it holds work: {e}"
                                        ));
                                        kept.push(fork);
                                    }
                                    Leftover::Gone(e) => {
                                        tracing::warn!("could not list the changes in {fork}: {e}");
                                        notes.push(format!("{fork} is gone, with whatever it held: {e}"));
                                    }
                                }
                            }
                            let all: Vec<String> = keep.into_iter().chain(kept.iter().cloned()).collect();
                            session.shutdown_dropping(&all, drop_run.as_deref()).await;
                            let _ = tx.send(Msg::Stopped { kept, notes });
                        });
                    }
                    None if self.starting => {}
                    None => {
                        let _ = tx.send(Msg::Stopped { kept: Vec::new(), notes: Vec::new() });
                    }
                }
            }
            // The loop handles these: they need the terminal.
            Action::Quit { .. }
            | Action::RestartSession { .. }
            | Action::ShowLog
            | Action::Edit { .. }
            | Action::Redraw
            | Action::Suspend
            | Action::SaveTheme(_)
            | Action::Alert { .. }
            | Action::ListFiles
            | Action::ListRuns
            | Action::ReadAudit { .. }
            | Action::Export { .. }
            | Action::EditFork { .. } => {}
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

/// `text` as the user leaves it in `$VISUAL` or `$EDITOR` (`vi` without
/// either), run on the terminal while the interface steps aside.
fn edit_outside(
    terminal: &mut DefaultTerminal,
    pause: &Pause,
    dir: &Path,
    name: &str,
    text: &str,
) -> Result<String, String> {
    use std::io::Write;

    // In the private data dir: the draft may hold anything the user typed.
    let path = dir.join(format!("{name}-{}.md", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let edited = step_aside(terminal, pause, &path)
        .and_then(|()| std::fs::read_to_string(&path).map_err(|e| format!("could not read {}: {e}", path.display())));
    let _ = std::fs::remove_file(&path);
    edited
}

/// Open `path`, a file or a directory, in `$VISUAL` or `$EDITOR` (`vi`
/// without either) on the terminal while the interface steps aside.
fn step_aside(terminal: &mut DefaultTerminal, pause: &Pause, path: &Path) -> Result<(), String> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
        .unwrap_or_else(|| "vi".into());
    pause.paused.store(true, Ordering::SeqCst);
    let reading = pause.reading.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = execute!(std::io::stdout(), DisableFocusChange, DisableBracketedPaste);
    ratatui::restore();
    // `sh` splits the variable as a shell would, so `EDITOR="code --wait"` works.
    let status = tokio::task::block_in_place(|| {
        std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).status()
    });
    // Not `terminal.clear()`: it asks the terminal where the cursor is. An empty frame over the
    // cleared screen makes the next draw a full one.
    let back = terminal::enable_raw_mode()
        .and_then(|()| {
            execute!(
                std::io::stdout(),
                EnterAlternateScreen,
                Clear(ClearType::All),
                EnableBracketedPaste,
                EnableFocusChange
            )
        })
        .and_then(|()| terminal.draw(|_| {}).map(|_| ()));
    drop(reading);
    pause.paused.store(false, Ordering::SeqCst);

    back.map_err(|e| format!("could not take the terminal back: {e}"))?;
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{editor} {s}")),
        Err(e) => Err(format!("could not start {editor}: {e}")),
    }
}

fn redraw(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    execute!(std::io::stdout(), Clear(ClearType::All))?;
    terminal.draw(|_| {}).map(|_| ())
}

fn save_theme(dir: &Path, theme: text::Theme) -> anyhow::Result<()> {
    use std::io::Write;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let temporary = dir.join(format!("tui-theme-{}-{nonce}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
    let result = (|| -> anyhow::Result<()> {
        file.write_all(&serde_json::to_vec(&theme)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join("tui-theme.json"))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

fn suspend(terminal: &mut DefaultTerminal, pause: &Pause) -> anyhow::Result<()> {
    pause.paused.store(true, Ordering::SeqCst);
    let reading = pause.reading.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = execute!(std::io::stdout(), DisableFocusChange, DisableBracketedPaste);
    emit(notice::POP_TITLE);
    ratatui::restore();
    // Stop only the interface process. Its service children continue working.
    // SIGCONT resumes this call, before the terminal reader is unpaused.
    let stopped = tokio::task::block_in_place(|| {
        // SAFETY: raise sends a standard signal to the current process.
        unsafe { libc::raise(libc::SIGTSTP) }
    });
    let restored = terminal::enable_raw_mode()
        .and_then(|()| execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange))
        .and_then(|()| redraw(terminal));
    emit(notice::PUSH_TITLE);
    drop(reading);
    pause.paused.store(false, Ordering::SeqCst);
    restored.context("could not restore the terminal after suspension")?;
    anyhow::ensure!(stopped == 0, "could not suspend the interface");
    Ok(())
}

/// List the stopped runs of `workspace` in `dir` off the loop; they come as [`Msg::Runs`].
fn list_runs_later(tx: &UnboundedSender<Msg>, dir: Option<PathBuf>, workspace: &str) {
    let (tx, workspace) = (tx.clone(), workspace.to_owned());
    tokio::task::spawn_blocking(move || {
        let _ = tx.send(Msg::Runs(dir.map_or_else(Vec::new, |dir| list_runs(&dir, &workspace))));
    });
}

/// The runs of `workspace` that stopped with a checkpoint in `dir`, newest
/// first, without their conversations; one whose checkpoint cannot be read
/// comes with why.
fn list_runs(dir: &Path, workspace: &str) -> Vec<app::Stopped> {
    molt_planner::checkpoint::list(dir)
        .into_iter()
        .filter_map(|(run, saved)| match saved {
            Ok(mut saved) if saved.request.workspace == workspace => {
                let gone = saved
                    .attempts
                    .iter()
                    .filter(|a| a.end.is_none())
                    .filter_map(|a| a.fork.clone())
                    .filter(|fork| !Path::new(fork).is_dir())
                    .collect();
                for attempt in &mut saved.attempts {
                    attempt.messages = Vec::new();
                }
                Some(app::Stopped { run, saved: Ok(saved), gone })
            }
            Ok(_) => None,
            Err(e) => Some(app::Stopped { run, saved: Err(e), gone: Vec::new() }),
        })
        .collect()
}

/// The workspace's files for `@` completion, as the services see them:
/// `.gitignore` applies, and `.git` and `.molt` are left out.
fn list_files(root: &Path) -> Msg {
    let mut files: Vec<String> = molt_tools::workspace_files(root, |_| false)
        .filter_map(|p| p.strip_prefix(root).ok()?.to_str().map(str::to_owned))
        .take(app::LISTED_FILES + 1)
        .collect();
    let complete = files.len() <= app::LISTED_FILES;
    files.truncate(app::LISTED_FILES);
    Msg::Files { files, complete }
}

/// Bytes of log looked through per read of the audit log, so a read hands
/// back a place to read on from even where the run has nothing.
const AUDIT_SCAN: u64 = 1 << 20;
/// Messages handed to the app at once; the app asks for the rest.
const AUDIT_ENTRIES: usize = 2_000;

/// Run `trace`'s messages in the audit log at `log` from `cursor` on, read
/// directly as `molt audit` does, cut down to what the transcript shows.
fn read_audit(log: &Path, trace: &molt_proto::TraceId, cursor: Option<u64>) -> Result<app::AuditPage, String> {
    use molt_kernel::audit::{read_trace_within, Scan, MAX_READ_PAGE};
    use molt_proto::audit::ReadRequest;

    let mut req = ReadRequest::new(trace.clone());
    req.max_bytes = Some(MAX_READ_PAGE);
    req.cursor = cursor;
    let mut entries = Vec::new();
    loop {
        let page = read_trace_within(log, &req, Scan { until: None, max_bytes: AUDIT_SCAN })
            .map_err(|e| format!("{}: {e}", log.display()))?;
        entries.extend(page.entries.into_iter().map(|mut e| {
            transcript::slim(&mut e);
            e
        }));
        // The end of the log: the last line-aligned place is where to read on
        // next time, and what was read again from there is skipped.
        let Some(next) = page.next else { return Ok(app::AuditPage { entries, cursor: req.cursor, more: false }) };
        req.cursor = Some(next);
        if entries.len() >= AUDIT_ENTRIES {
            return Ok(app::AuditPage { entries, cursor: req.cursor, more: true });
        }
    }
}

/// Escape sequences straight to the terminal, between frames; best effort.
fn emit(bytes: &str) {
    use std::io::Write;

    let mut out = std::io::stdout();
    let _ = out.write_all(bytes.as_bytes()).and_then(|()| out.flush());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_fork_whose_changes_cannot_be_listed_is_kept_while_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().to_str().unwrap();
        let check = vec!["tests/check.rs".to_owned()];
        let unreadable = || Err(anyhow::anyhow!("fs.diff failed: reading the fork: No such file or directory"));
        assert_eq!(
            leftover(fork, unreadable(), &check),
            Leftover::Unlisted("fs.diff failed: reading the fork: No such file or directory".into())
        );
        assert!(matches!(leftover("/nowhere/fork-gone", unreadable(), &check), Leftover::Gone(_)));
        assert_eq!(leftover(fork, Ok(vec!["src/lib.rs".into()]), &check), Leftover::Work);
        assert_eq!(leftover(fork, Ok(check.clone()), &check), Leftover::Empty, "the check's own files are no work");
    }

    #[tokio::test]
    async fn the_runtime_commits_what_the_task_merged_against_the_tree_it_started_from() {
        use molt_api::fs::{Change, ChangeKind};
        use molt_api::planner::{Outcome, RunResponse};
        use molt_proto::TraceId;
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
            git(d, &["config", key, value]);
        }
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "start"]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (sessions, _started) = mpsc::unbounded_channel();
        let workspace = d.to_str().unwrap().to_owned();
        let mut rt = Runtime {
            cfg: Arc::new(Config::default()),
            workspace,
            tx,
            sessions,
            session: None,
            starting: false,
            work: Vec::new(),
            before: None,
        };
        let resp = |path: &str| RunResponse {
            outcome: Outcome::Unverified,
            check: None,
            winner: Some(0),
            summary: "Write b".into(),
            changes: vec![Change { path: path.into(), kind: ChangeKind::Added }],
            left_out: None,
            patch: String::new(),
            patch_truncated: false,
            applied: true,
            fork: None,
            review: None,
            attempts: vec![],
            usage: Default::default(),
            cost_usd: 0.0,
            uncounted_calls: 0,
            checkpoint: None,
            requirements: vec![],
            selection: Default::default(),
            rubric: vec![],
            judgement: vec![],
            dispute: None,
            verification: None,
            workflow: None,
            conversation: Vec::new(),
        };
        let trace = TraceId::random();
        rt.exec(Action::PrepareCommit { trace: trace.clone() });
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        rt.exec(Action::Commit { trace: trace.clone(), task: "write b".into(), resp: Box::new(resp("b.txt")) });
        let Some(Msg::Committed(Ok(Some(made)))) = rx.recv().await else { panic!("not committed") };
        assert_eq!((made.subject.as_str(), made.branch.as_deref()), ("Write b", Some("main")));
        assert_eq!(git(d, &["show", "--name-only", "--format=", "HEAD"]), "b.txt\n");
        let trailer = git(d, &["log", "-1", "--format=%(trailers:key=Molt-Run,valueonly)"]);
        assert_eq!(trailer.trim(), trace.as_str());
        // Turned on after the task started: nothing was looked at for it.
        rt.exec(Action::Commit { trace: TraceId::random(), task: "late".into(), resp: Box::new(resp("b.txt")) });
        let Some(Msg::Committed(Err(e))) = rx.recv().await else { panic!("committed") };
        assert!(e.contains("turned on after the task started"), "{e}");
    }
    #[test]
    fn stopped_runs_are_this_workspaces_say_which_forks_are_gone_and_name_damage() {
        use molt_api::planner::RunRequest;
        use molt_planner::checkpoint::Saved;
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().join("fork-kept");
        std::fs::create_dir(&fork).unwrap();
        let mut here = Saved::new("trace_here", RunRequest::new("t", "/w"));
        here.attempt(0).fork = Some(fork.to_str().unwrap().into());
        here.attempt(0).messages = vec![serde_json::json!({"role": "user", "content": "long"})];
        here.attempt(1).fork = Some("/nowhere/fork-gone".into());
        let other = Saved::new("trace_other", RunRequest::new("t", "/elsewhere"));
        for saved in [&here, &other] {
            std::fs::write(dir.path().join(format!("{}.json", saved.run)), serde_json::to_vec(saved).unwrap()).unwrap();
        }
        std::fs::write(dir.path().join("trace_bad.json"), "{\"format\":1,").unwrap();
        let mut runs = list_runs(dir.path(), "/w");
        runs.sort_by(|a, b| a.run.cmp(&b.run));
        assert_eq!(runs.iter().map(|r| r.run.as_str()).collect::<Vec<_>>(), ["trace_bad", "trace_here"]);
        assert!(runs[0].saved.as_ref().unwrap_err().contains("damaged"));
        assert_eq!(runs[1].gone, ["/nowhere/fork-gone"]);
        assert!(runs[1].saved.as_ref().unwrap().attempts[0].messages.is_empty(), "conversations stay on disk");
    }

    #[test]
    fn themes_are_saved_privately_and_replaced_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save_theme(dir.path(), text::Theme::Light).unwrap();
        save_theme(dir.path(), text::Theme::Ansi).unwrap();
        let path = dir.path().join("tui-theme.json");
        assert_eq!(serde_json::from_slice::<text::Theme>(&std::fs::read(&path).unwrap()).unwrap(), text::Theme::Ansi);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn listed_files_are_relative_and_follow_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/lexer.rs"), "").unwrap();
        std::fs::write(root.join("target/out"), "").unwrap();
        let Msg::Files { files, complete } = list_files(&root) else { panic!() };
        assert!(complete);
        assert_eq!(files, [".gitignore", "src/lexer.rs"]);
    }

    #[tokio::test]
    async fn a_runs_transcript_is_read_from_the_audit_log_and_read_on_from_where_it_stopped() {
        use molt_kernel::audit::{AuditEvent, AuditLog};
        use molt_proto::TraceId;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let trace = TraceId::random();
        let mut log = transcript::tests::two_turns(&trace);
        let last = log.pop().unwrap();
        let audit = AuditLog::open(&path, false).await.unwrap();
        let other = transcript::tests::two_turns(&TraceId::random());
        for entry in other.into_iter().take(2).chain(log) {
            audit.append(AuditEvent::Message { envelope: entry.envelope }).await.unwrap();
        }
        let page = tokio::task::spawn_blocking({
            let (path, trace) = (path.clone(), trace.clone());
            move || read_audit(&path, &trace, None)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(!page.more);
        assert_eq!(page.entries.len(), 6, "the run's messages alone");
        assert!(page.entries.iter().all(|e| e.envelope.trace_id == trace));
        let shell = page.entries.iter().find(|e| e.envelope.to.to_string() == "shell.run").unwrap();
        assert!(shell.envelope.payload.is_null(), "requests to tools are not kept");
        let mut reader = transcript::Reader::default();
        reader.add(page.entries);
        assert_eq!(reader.turns.len(), 2);
        assert_eq!(reader.turns[1].steps[0].result, None);

        audit.append(AuditEvent::Message { envelope: last.envelope }).await.unwrap();
        audit.close().await;
        let more = read_audit(&path, &trace, page.cursor).unwrap();
        reader.add(more.entries);
        assert_eq!(reader.turns[1].steps[0].result.as_deref(), Some("edited"));
        let missing = read_audit(&dir.path().join("none.jsonl"), &trace, None).unwrap_err();
        assert!(missing.contains("none.jsonl"), "{missing}");
    }
}

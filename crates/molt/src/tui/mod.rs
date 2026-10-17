//! `molt` with no subcommand: an interface in the terminal that keeps the
//! agent services up and takes one task after another.
//!
//! [`app`] holds the state and the rules, [`view`] draws it, and the loop
//! here carries the [`app::Action`]s out on a [`Session`] and feeds what
//! comes back, with the keys, to the app.

mod app;
mod backend;
mod commands;
mod preview;
mod text;
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
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen};
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
    /// A project of the registry, by id or name, in place of `workspace`.
    pub project: Option<String>,
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
    let (workspace, project) = agent::locate(opts.project.as_deref(), &opts.workspace, opts.config.is_some())?;
    let path = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let mut cfg =
        agent::resolve_config(opts.config.as_deref(), &workspace, opts.data_dir.as_deref(), project.as_ref())?;
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
    app.rules = cfg.confirm.clone();
    app.load_commands(Path::new(&workspace));
    let theme_file = cfg.kernel.data_dir.join("tui-theme.json");
    match std::fs::read(&theme_file) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(theme) => app.theme = theme,
            Err(e) => app.flash = Some(format!("saved theme ignored: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => app.flash = Some(format!("could not load saved theme: {e}")),
    }
    let data_dir = cfg.kernel.data_dir.clone();
    let mut backend = backend::Backend::start(cfg, workspace, tx).await?;
    backend.send(&Action::StartSession).await?;
    let mut keep = Vec::new();
    loop {
        terminal.draw(|frame| view::draw(frame, &mut app))?;
        let Some(msg) = rx.recv().await else { break };
        let mut actions = app.update(msg);
        while let Ok(msg) = rx.try_recv() {
            actions.extend(app.update(msg));
        }
        for action in actions {
            match action {
                Action::Quit { keep: k } => keep = k,
                Action::Redraw => redraw(terminal)?,
                Action::Suspend => suspend(terminal, &pause)?,
                Action::SaveTheme(theme) => {
                    if let Err(e) = save_theme(&data_dir, theme) {
                        app.flash = Some(format!("theme changed but could not be saved: {e}"));
                    }
                }
                Action::Edit { text } => {
                    let edited = edit_outside(terminal, &pause, &data_dir, &text);
                    app.update(Msg::Edited(edited));
                }
                action => backend.send(&action).await?,
            }
        }
        if app.quit {
            break;
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
    const NAMES: &[&str] = &["API_KEY", "TOKEN", "SECRET", "PASSWORD"];
    std::env::vars()
        .filter(|(name, value)| value.len() >= 8 && NAMES.iter().any(|n| name.to_ascii_uppercase().contains(n)))
        .map(|(_, value)| value)
        .collect()
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
            Action::Apply { fork } => self.call(move |s| async move {
                Msg::Applied(s.apply(&fork).await.map(|_| ()).map_err(|e| format!("{e:#}")))
            }),
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

            Action::Interrupt { keep, attempts, check_files } => {
                for work in self.work.drain(..) {
                    work.abort();
                }
                let tx = self.tx.clone();
                match self.session.take() {
                    Some(session) => {
                        tokio::spawn(async move {
                            let mut kept = Vec::new();
                            for fork in attempts {
                                // A fork with nothing but the check's own files holds no work.
                                match session.changes(&fork).await {
                                    Ok(paths) if paths.iter().any(|p| !check_files.contains(p)) => kept.push(fork),
                                    Ok(_) => {}
                                    Err(e) => tracing::warn!("could not list the changes in {fork}: {e:#}"),
                                }
                            }
                            let all: Vec<String> = keep.into_iter().chain(kept.iter().cloned()).collect();
                            session.shutdown(&all).await;
                            let _ = tx.send(Msg::Stopped { kept });
                        });
                    }
                    None if self.starting => {}
                    None => {
                        let _ = tx.send(Msg::Stopped { kept: Vec::new() });
                    }
                }
            }
            // The loop handles these: they need the terminal.
            Action::Quit { .. } | Action::Edit { .. } | Action::Redraw | Action::Suspend | Action::SaveTheme(_) => {}
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
fn edit_outside(terminal: &mut DefaultTerminal, pause: &Pause, dir: &Path, text: &str) -> Result<String, String> {
    use std::io::Write;

    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
        .unwrap_or_else(|| "vi".into());
    // In the private data dir: the draft may hold anything the user typed.
    let path = dir.join(format!("task-{}.md", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;

    pause.paused.store(true, Ordering::SeqCst);
    let reading = pause.reading.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // `sh` splits the variable as a shell would, so `EDITOR="code --wait"` works.
    let status = tokio::task::block_in_place(|| {
        std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status()
    });
    // Not `terminal.clear()`: it asks the terminal where the cursor is. An empty frame over the
    // cleared screen makes the next draw a full one.
    let back = terminal::enable_raw_mode()
        .and_then(|()| execute!(std::io::stdout(), EnterAlternateScreen, Clear(ClearType::All), EnableBracketedPaste))
        .and_then(|()| terminal.draw(|_| {}).map(|_| ()));
    drop(reading);
    pause.paused.store(false, Ordering::SeqCst);

    let edited = std::fs::read_to_string(&path).map_err(|e| format!("could not read {}: {e}", path.display()));
    let _ = std::fs::remove_file(&path);
    back.map_err(|e| format!("could not take the terminal back: {e}"))?;
    match status {
        Ok(s) if s.success() => edited,
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
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // Stop only the interface process. Its service children continue working.
    // SIGCONT resumes this call, before the terminal reader is unpaused.
    let stopped = tokio::task::block_in_place(|| {
        // SAFETY: raise sends a standard signal to the current process.
        unsafe { libc::raise(libc::SIGTSTP) }
    });
    let restored = terminal::enable_raw_mode()
        .and_then(|()| execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste))
        .and_then(|()| redraw(terminal));
    drop(reading);
    pause.paused.store(false, Ordering::SeqCst);
    restored.context("could not restore the terminal after suspension")?;
    anyhow::ensure!(stopped == 0, "could not suspend the interface");
    Ok(())
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
}

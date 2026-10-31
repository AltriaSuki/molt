//! `molt` with no subcommand: an interface in the terminal that keeps the
//! agent services up and takes one task after another.
//!
//! [`app`] holds the state and the rules, [`view`] draws it, and the loop
//! here carries the [`app::Action`]s out and feeds what comes back, with the
//! keys, to the app. What needs the services goes to the interface's own
//! server ([`crate::serve`]) through the client protocol, as any client's
//! would ([`backend`]); the terminal, the editor, and reading the workspace,
//! git and the logs stay here.

mod app;
mod backend;
pub(crate) mod commands;
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

use self::app::{Action, App, Msg};
use self::notice::Notice;
use crate::config::secret_name;
use crate::{agent, Config};

/// How often the spinners turn.
const TICK: Duration = Duration::from_millis(app::TICK_MS);

pub use crate::serve::Options;

/// Run the interface until the user quits. Returns the forks the user kept.
pub async fn run(opts: Options) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "molt needs a terminal for its interface; in scripts, use `molt do TASK`"
    );
    let stop = crate::stop_signal()?;
    // One session each time the interface is opened, unless one is named.
    let session = opts.session.clone().unwrap_or_else(crate::runs::new_session);
    let (cfg, path, project) = crate::serve::configure(opts)?;
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
    let result = event_loop(&mut terminal, cfg, path, home.as_deref(), session, stop, &mut unexecuted).await;
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
    session: String,
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
    app.run_session = session;
    app.secrets = secrets_in_env();
    app.secrets.extend(
        cfg.services
            .iter()
            .flat_map(|service| &service.env)
            .filter(|(name, value)| value.len() >= 8 && secret_name(name))
            .map(|(_, value)| value.clone()),
    );
    app.rules = cfg.confirm.clone();
    app.sandbox = match agent::shell_sandboxed(&cfg) {
        Some(true) => app::Sandbox::Isolated,
        Some(false) => app::Sandbox::Unconfined,
        None => app::Sandbox::Unknown,
    };
    app.load_commands_with(Path::new(&workspace), &cfg.extensions);
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
    // The readiness check when the user asks for one before runs, else the cheap file checks.
    match agent::readiness_before_run(&cfg, Path::new(&workspace)).await {
        Ok(Some(checked)) => {
            for problem in checked.report.problems() {
                app.log.extend(text::block(&format!("readiness: {problem}"), "", text::Tone::Warn));
            }
            let tone = if checked.report.status == crate::readiness::Status::Ready {
                text::Tone::Ok
            } else {
                text::Tone::Warn
            };
            app.log.extend(text::block(&checked.report.summary(), "", tone));
        }
        Ok(None) => {
            let cargo_home = molt_tools::host_cargo_home().ok();
            for missing in agent::dependency_warnings(&cfg, Path::new(&workspace), cargo_home.as_deref()) {
                app.log.extend(text::block(&format!("dependency preflight: {missing}"), "", text::Tone::Warn));
            }
        }
        Err(e) => app.log.extend(text::block(&format!("readiness: could not check: {e:#}"), "", text::Tone::Warn)),
    }
    let data_dir = cfg.kernel.data_dir.clone();
    let (lister, root) = (tx.clone(), PathBuf::from(&workspace));
    let mut saved_session = None;
    let mut backend = backend::Backend::start(cfg.clone(), workspace.clone(), tx, app.backend_generation).await?;
    // What stopped runs there are to go on with: read from their checkpoints, with no model call.
    backend.send(Action::ListRuns).await?;
    backend.send(Action::StartSession).await?;
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
                    backend.send(Action::StartSession).await?;
                }
                Action::ShowLog => {
                    let (tx, path) = (lister.clone(), data_dir.join("tui.log"));
                    tokio::task::spawn_blocking(move || {
                        let result = log::tail(&path).map_err(|e| e.to_string());
                        let _ = tx.send(Msg::ServiceLog(result));
                    });
                }
                Action::ListFiles => {
                    let (tx, root) = (lister.clone(), root.clone());
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(list_files(&root));
                    });
                }
                Action::RunHistory => {
                    let (tx, data_dir) = (lister.clone(), data_dir.clone());
                    tokio::task::spawn_blocking(move || {
                        let listed = crate::runs::list(&data_dir).map(|mut runs| {
                            runs.drain(..runs.len().saturating_sub(app::HISTORY));
                            runs
                        });
                        let _ = tx.send(Msg::RunHistory(listed.map_err(|e| format!("{e:#}"))));
                    });
                }
                Action::ReadAudit { trace, cursor } => {
                    let (tx, log) = (lister.clone(), data_dir.join("audit.jsonl"));
                    tokio::task::spawn_blocking(move || {
                        let result = read_audit(&log, &trace, cursor);
                        let _ = tx.send(Msg::Audit { trace, result });
                    });
                }
                Action::Changes => {
                    let (tx, root) = (lister.clone(), root.clone());
                    tokio::task::spawn_blocking(move || {
                        let result = crate::git::Repo::open(&root).and_then(|repo| repo.changes());
                        let _ = tx.send(Msg::Changes(result.map_err(|e| format!("{e:#}"))));
                    });
                }
                Action::FileDiff { change } => {
                    let (tx, root) = (lister.clone(), root.clone());
                    tokio::task::spawn_blocking(move || {
                        let result = crate::git::Repo::open(&root).and_then(|repo| repo.file_diff(&change));
                        let _ =
                            tx.send(Msg::FileDiff { path: change.path, result: result.map_err(|e| format!("{e:#}")) });
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
                action => backend.send(action).await?,
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

/// The interface's own server (`molt ui-backend`): the config the interface
/// resolved comes on its private pipe first, then the client protocol.
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

/// `text` as the user leaves it in `$VISUAL` or `$EDITOR` (see
/// [`pick_editor`]), run on the terminal while the interface steps aside.
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

/// The editors tried, in order, when neither `$VISUAL` nor `$EDITOR` is set.
const FALLBACK_EDITORS: [&str; 4] = ["vi", "nvim", "vim", "nano"];

/// The editor to run: `$VISUAL`, else `$EDITOR`, else the first of
/// [`FALLBACK_EDITORS`] that `installed` finds.
fn pick_editor(var: impl Fn(&str) -> Option<String>, installed: impl Fn(&str) -> bool) -> Result<String, String> {
    if let Some(editor) = ["VISUAL", "EDITOR"].into_iter().find_map(|v| var(v).filter(|e| !e.trim().is_empty())) {
        return Ok(editor);
    }
    FALLBACK_EDITORS.into_iter().find(|e| installed(e)).map(str::to_owned).ok_or_else(|| {
        format!(
            "no editor found: set $EDITOR, for example EDITOR=nano (none of {} is on PATH)",
            FALLBACK_EDITORS.join(", ")
        )
    })
}

/// Whether an executable file `name` is in a directory of `PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Open `path`, a file or a directory, in `$VISUAL` or `$EDITOR` (else the
/// first of vi, nvim, vim and nano there is) on the terminal while the
/// interface steps aside.
fn step_aside(terminal: &mut DefaultTerminal, pause: &Pause, path: &Path) -> Result<(), String> {
    let editor = pick_editor(|v| std::env::var(v).ok(), on_path)?;
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
        // What `sh` exits with when it cannot find the command.
        Ok(s) if s.code() == Some(127) => {
            Err(format!("{editor} was not found ({s}); set $EDITOR to an editor you have"))
        }
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
    fn without_visual_or_editor_the_first_installed_editor_opens_or_the_user_is_told_to_set_one() {
        let unset = |_: &str| None;
        assert_eq!(pick_editor(unset, |e| e == "vim" || e == "nano").as_deref(), Ok("vim"));
        assert_eq!(pick_editor(unset, |e| e == "nvim" || e == "vi").as_deref(), Ok("vi"));
        assert_eq!(pick_editor(unset, |e| e == "nano").as_deref(), Ok("nano"));
        let err = pick_editor(unset, |_| false).unwrap_err();
        assert!(err.contains("set $EDITOR") && err.contains("vi, nvim, vim, nano"), "{err}");
        let set = |v: &str| (v == "EDITOR").then(|| "code --wait".to_owned());
        assert_eq!(pick_editor(set, |_| false).as_deref(), Ok("code --wait"));
        let both = |v: &str| Some(if v == "VISUAL" { " " } else { "hx" }.to_owned());
        assert_eq!(pick_editor(both, |_| true).as_deref(), Ok("hx"), "a blank $VISUAL counts as unset");
        assert!(on_path("sh") && !on_path("molt-no-such-editor"));
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

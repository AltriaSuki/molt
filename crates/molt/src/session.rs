//! The agent services kept up for many tasks, as the TUI uses them: one
//! kernel, the services of a config, and the CLI's place on the bus.
//!
//! `molt do` starts everything for one task and stops it after; a session
//! starts once and serves every task the user gives it. The CLI holds the
//! same capabilities as in `molt do`, plus `planner.design` to show the
//! check before the attempts start, `planner.control` to steer a run that is
//! going, `fs.merge` and `fs.drop` to apply or discard a result the user has
//! looked at, `fs.narrow` and `shell.run` to check part of a result, or one
//! the user edited, again before it is applied, `memory.recall`, `memory.forget`, `memory.review`,
//! `memory.correct` and `memory.promote` for the memory screen, and
//! `memory.remember` for notes the user writes down.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Context;
use molt_api::fs::{
    self, DiffRequest, DiffResponse, DropRequest, DropResponse, MergeRequest, MergeResponse, NarrowRequest,
};
use molt_api::memory::{
    self, ConsolidateRequest, ConsolidateResponse, CorrectRequest, ForgetRequest, ForgetResponse, Note, NoteKind,
    PromoteRequest, PromoteResponse, RecallRequest, RecallResponse, RecallSnapshot, Recalled, RememberRequest,
    RememberResponse, RetractResponse, ReviewRequest, ReviewResponse, UndoRequest,
};
use molt_api::planner::{
    self, CheckSpec, Control, ControlRequest, ControlResponse, DesignResponse, RunRequest, RunResponse,
};
use molt_api::progress::{self, Progress};
use molt_planner::checkpoint;
use molt_proto::{Budget, ServiceId, TraceId};
use molt_sdk::{CallOpts, Service};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::agent::{self, RunForks, MEMORY, SERVICES};
use crate::{Config, Running};

/// Calls a session may make of each method. A person makes them one at a time.
const CALLS: u64 = 100_000;
/// Reply deadline for a run or a design; the planner's own limits end it sooner.
const RUN_MS: u64 = 6 * 3600 * 1000;
/// Reply deadline for merging or dropping a fork, which copy or walk whole trees.
const TREE_MS: u64 = 600_000;
/// Reply deadline for learning from a run.
const LEARN_MS: u64 = 600_000;
/// Reply deadline for listing a fork's changes while the session stops.
const DIFF_MS: u64 = 10_000;
/// Reply deadline for a change to a running run, which the planner answers at once.
const CONTROL_MS: u64 = 10_000;
/// Deadline for running a result's check again, as the planner's own default.
const CHECK_MS: u64 = 900_000;
/// Reply deadline for memory's notes.
const NOTES_MS: u64 = 60_000;

pub struct Session {
    running: Mutex<Option<Running>>,
    cli: Arc<Service>,
    workspace: String,
    /// `None` without a memory service; an error when it did not come up.
    memory: Option<Result<(), String>>,
    memory_db: Option<PathBuf>,
    forks: Mutex<Option<RunForks>>,
    /// Where the planner keeps run checkpoints: a stopped run's forks there are kept for its resume.
    checkpoints: Option<PathBuf>,
    versions: Vec<(String, String)>,
    hooks: Vec<molt_planner::hooks::Hook>,
}

/// What checking a kept result again found.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rechecked {
    /// Every change of the result as it is now, chosen or not.
    pub all: Vec<fs::Change>,
    /// The changes that were checked, and their diff.
    pub diff: DiffResponse,
    /// How the check did on them; `None` without a check command.
    pub check: Option<molt_api::shell::RunResponse>,
    /// What the review's fixed rules found in them.
    pub rules: Vec<String>,
}

impl Rechecked {
    /// The check ran and failed.
    pub fn failed(&self) -> bool {
        self.check.as_ref().is_some_and(|c| !c.success())
    }
}

/// Git settlement is retained even when memory could not be notified; retry by run id is safe.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Undone {
    pub git: crate::git::Undo,
    pub memory: Option<Result<RetractResponse, String>>,
}

impl Session {
    /// Start the kernel and the services of `cfg` for `workspace`, wait until
    /// they answer, and hand every progress event to `events`.
    pub async fn start(cfg: &Config, workspace: &str, events: mpsc::UnboundedSender<Progress>) -> anyhow::Result<Self> {
        agent::require_services(cfg)?;
        let running = crate::start(cfg).await?;
        match Self::join(cfg, &running, events).await {
            Ok((cli, memory)) => {
                let versions = versions(&running, cfg);
                Ok(Self {
                    running: Mutex::new(Some(running)),
                    cli,
                    workspace: workspace.to_owned(),
                    memory,
                    memory_db: agent::memory_db(cfg),
                    forks: Mutex::new(RunForks::before(cfg)),
                    checkpoints: agent::checkpoint_dir(cfg),
                    versions,
                    hooks: cfg.hooks.clone(),
                })
            }
            Err(e) => {
                running.shutdown().await;
                Err(e)
            }
        }
    }

    async fn join(
        cfg: &Config,
        running: &Running,
        events: mpsc::UnboundedSender<Progress>,
    ) -> anyhow::Result<(Arc<Service>, Option<Result<(), String>>)> {
        let kernel = running.kernel();
        let cli = agent::join_bus(running).await?;
        let deadline = Instant::now() + agent::STARTUP;
        for service in SERVICES {
            agent::wait_until_up(kernel, &cli, service, deadline).await?;
        }
        let memory = match cfg.service(MEMORY) {
            None => None,
            Some(_) => Some(agent::wait_until_up(kernel, &cli, MEMORY, deadline).await.map_err(|e| format!("{e:#}"))),
        };
        let budget = Budget::new(0, 0, CALLS);
        let mut targets = vec![
            planner::RUN,
            planner::DESIGN,
            planner::CONTROL,
            fs::DIFF,
            fs::VIEW,
            fs::MERGE,
            fs::DROP,
            fs::NARROW,
            molt_api::shell::RUN,
        ];
        if !cfg.hooks.is_empty() {
            targets.push(fs::FORK);
        }
        if memory.is_some() {
            targets.extend([
                memory::CONSOLIDATE,
                memory::RECALL,
                memory::FORGET,
                memory::REVIEW,
                memory::CORRECT,
                memory::REMEMBER,
                memory::PROMOTE,
                memory::UNDO,
            ]);
        }
        for target in targets {
            agent::grant(kernel, &cli, target, budget).await?;
        }
        agent::grant(kernel, &cli, &format!("topic:subscribe:{}", progress::TOPIC), budget).await?;
        cli.subscribe(progress::TOPIC).await?;
        let cli = Arc::new(cli);
        let pump = cli.clone();
        tokio::spawn(async move {
            while let Some(msg) = pump.next().await {
                // Share the CLI's gateway-origin and trace checks. Topic
                // access alone must not let a peer impersonate a model call.
                if let Some(event) = agent::progress_event(&msg) {
                    if events.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Ok((cli, memory))
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Why memory is not there, or `None` when it is up.
    pub fn memory_problem(&self) -> Option<String> {
        match &self.memory {
            None => Some("no memory service in the config".into()),
            Some(Err(e)) => Some(e.clone()),
            Some(Ok(())) => None,
        }
    }

    /// The live version of each service, by its first eight hex digits.
    pub fn versions(&self) -> &[(String, String)] {
        &self.versions
    }

    pub async fn design(&self, req: &RunRequest, trace: &TraceId) -> anyhow::Result<DesignResponse> {
        self.call(planner::DESIGN, req, RUN_MS, Some(trace)).await
    }

    pub async fn run(&self, req: &RunRequest, trace: &TraceId) -> anyhow::Result<RunResponse> {
        self.call(planner::RUN, req, RUN_MS, Some(trace)).await
    }

    /// Change the running run `run`: add to its requirements, pause it or let it go on.
    pub async fn control(&self, run: &TraceId, action: Control) -> anyhow::Result<ControlResponse> {
        // Under the run's trace, so the change is part of the run's episode.
        let req = ControlRequest { run: run.to_string(), action };
        self.call(planner::CONTROL, req, CONTROL_MS, Some(run)).await
    }

    /// Have memory learn from the run `trace`, as `molt do` does after a
    /// run, with `model`, or memory's own default without one.
    pub async fn learn(&self, trace: &TraceId, model: Option<String>) -> Result<ConsolidateResponse, String> {
        if let Some(problem) = self.memory_problem() {
            return Err(problem);
        }
        // A trace of its own, so the learning does not become part of the episode.
        let req = ConsolidateRequest { episode: trace.to_string(), workspace: self.workspace.clone(), model };
        self.call(memory::CONSOLIDATE, req, LEARN_MS, None).await.map_err(|e| format!("{e:#}"))
    }

    /// Merge a result kept in `fork` into the workspace, and drop the fork.
    pub async fn apply(&self, fork: &str) -> anyhow::Result<MergeResponse> {
        self.apply_only(fork, &[]).await
    }

    /// Merge `paths` of a result kept in `fork` into the workspace, or all
    /// of it when `paths` is empty, and drop the fork. The request names the
    /// paths, so the audit log has what was chosen.
    pub async fn apply_only(&self, fork: &str, paths: &[String]) -> anyhow::Result<MergeResponse> {
        self.validate_fork(fork).await?;
        let req = MergeRequest { fork: fork.to_owned(), drop: true, paths: paths.to_vec() };
        let result = self.call(fs::MERGE, req, TREE_MS, None).await?;
        self.notify_applied().await;
        Ok(result)
    }

    /// Check a result kept in `fork` again as it is now: `only` of its
    /// changes (all of them when `None`), after the user chose some or
    /// edited it. `check` runs in a fork of the workspace with just those
    /// changes and the check's own files, which is dropped after; the
    /// review's fixed rules look at their diff.
    pub async fn recheck(
        &self,
        fork: &str,
        only: Option<&[String]>,
        check: Option<&CheckSpec>,
    ) -> anyhow::Result<Rechecked> {
        let all = self.diff(fork).await?;
        let paths: Vec<String> = match only {
            None => all.changes.iter().map(|c| c.path.clone()).collect(),
            // A file the user put back as it was is no longer a change to apply.
            Some(only) => only.iter().filter(|p| all.changes.iter().any(|c| &c.path == *p)).cloned().collect(),
        };
        anyhow::ensure!(!paths.is_empty(), "none of the chosen files is changed in the result any more");
        let diff: DiffResponse = if paths.len() == all.changes.len() {
            all.clone()
        } else {
            self.call(fs::DIFF, DiffRequest { fork: fork.to_owned(), paths: paths.clone() }, DIFF_MS, None).await?
        };
        let check_files = check.map_or(&[][..], |c| &c.files[..]);
        let rules = molt_planner::review_rules(check_files, &diff.changes, &diff.patch);
        let ran = match check {
            None => None,
            Some(check) => {
                // The check needs its own files even where they are not applied.
                let extra =
                    check_files.iter().filter(|f| !paths.contains(f) && all.changes.iter().any(|c| &c.path == *f));
                let take: Vec<String> = paths.iter().chain(extra).cloned().collect();
                let narrowed: fs::ForkResponse =
                    self.call(fs::NARROW, NarrowRequest { fork: fork.to_owned(), paths: take }, TREE_MS, None).await?;
                let req = molt_api::shell::RunRequest {
                    workspace: narrowed.fork.clone(),
                    command: check.command.clone(),
                    timeout_ms: Some(CHECK_MS),
                };
                let ran: anyhow::Result<molt_api::shell::RunResponse> =
                    self.call(molt_api::shell::RUN, req, CHECK_MS.saturating_add(5000), None).await;
                let _: anyhow::Result<DropResponse> =
                    self.call(fs::DROP, DropRequest { fork: narrowed.fork }, TREE_MS, None).await;
                Some(ran?)
            }
        };
        Ok(Rechecked { all: all.changes, diff, check: ran, rules })
    }

    async fn notify_applied(&self) {
        let hooks: Vec<_> = self.hooks.iter().filter(|h| h.event == molt_planner::hooks::Event::AfterApply).collect();
        if hooks.is_empty() {
            return;
        }
        let fork: anyhow::Result<fs::ForkResponse> =
            self.call(fs::FORK, fs::ForkRequest { workspace: self.workspace.clone() }, TREE_MS, None).await;
        let Ok(fork) = fork else {
            tracing::warn!("after_apply hooks could not get a private fork");
            return;
        };
        for hook in hooks {
            let req = molt_api::shell::RunRequest {
                workspace: fork.fork.clone(),
                command: hook.command.clone(),
                timeout_ms: Some(hook.timeout_ms),
            };
            let result: anyhow::Result<molt_api::shell::RunResponse> =
                self.call(molt_api::shell::RUN, req, hook.timeout_ms.saturating_add(5000), None).await;
            if let Err(error) = result {
                tracing::warn!(error = %error, "after_apply notification failed");
            }
        }
        let _: anyhow::Result<fs::DropResponse> =
            self.call(fs::DROP, fs::DropRequest { fork: fork.fork }, TREE_MS, None).await;
    }

    /// The paths `fork` changed.
    pub async fn changes(&self, fork: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.diff(fork).await?.changes.into_iter().map(|c| c.path).collect())
    }

    pub async fn discard(&self, fork: &str) -> anyhow::Result<()> {
        self.validate_fork(fork).await?;
        let _: DropResponse = self.call(fs::DROP, DropRequest { fork: fork.to_owned() }, TREE_MS, None).await?;
        Ok(())
    }

    /// Read a kept result through the service, without mounting or trusting its saved path.
    pub async fn diff(&self, fork: &str) -> anyhow::Result<DiffResponse> {
        self.validate_fork(fork).await?;
        self.call(fs::DIFF, DiffRequest { fork: fork.to_owned(), paths: Vec::new() }, DIFF_MS, None).await
    }

    async fn validate_fork(&self, fork: &str) -> anyhow::Result<()> {
        let view: fs::ViewResponse =
            self.call(fs::VIEW, fs::ViewRequest { fork: fork.to_owned() }, DIFF_MS, None).await?;
        let workspace = Path::new(&self.workspace).canonicalize()?;
        anyhow::ensure!(Path::new(&view.workspace) == workspace, "the result belongs to another workspace");
        Ok(())
    }

    /// Undo as the person, using this session's memory service and audit log.
    pub async fn undo(&self, run: Option<String>) -> anyhow::Result<Undone> {
        let workspace = self.workspace.clone();
        let git =
            tokio::task::spawn_blocking(move || crate::git::Repo::open(Path::new(&workspace))?.undo(run.as_deref()))
                .await??;
        let run = git.run().to_owned();
        let memory = if self.memory.is_none() {
            None
        } else if let Some(error) = self.memory_problem() {
            Some(Err(error))
        } else {
            let req = UndoRequest { run, reason: "the user undid the run with TUI /undo".into() };
            Some(self.call(memory::UNDO, req, NOTES_MS, None).await.map_err(|e| format!("{e:#}")))
        };
        Ok(Undone { git, memory })
    }

    /// The notes about the workspace that match `query`, best first.
    pub async fn notes(&self, query: &str) -> anyhow::Result<Vec<Recalled>> {
        self.recall_notes(query, false).await
    }

    /// Inspection, like `molt memory show`, includes quarantined notes.
    /// Ordinary recall still excludes them until explicitly reviewed.
    pub async fn inspect_notes(&self, query: &str) -> anyhow::Result<Vec<Recalled>> {
        self.recall_notes(query, true).await
    }

    async fn recall_notes(&self, query: &str, include_review: bool) -> anyhow::Result<Vec<Recalled>> {
        let req = RecallRequest {
            query: query.to_owned(),
            workspace: Some(self.workspace.clone()),
            k: Some(50),
            include_review,
            ..Default::default()
        };
        let resp: RecallResponse = self.call(memory::RECALL, req, NOTES_MS, None).await?;
        Ok(resp.notes)
    }

    /// Use the CLI's audited service path and the revision the person saw.
    pub async fn change_note(&self, review: ReviewRequest, text: Option<String>) -> anyhow::Result<ReviewResponse> {
        anyhow::ensure!(review.workspace == self.workspace, "the note belongs to another project");
        match text {
            Some(text) => self.call(memory::CORRECT, CorrectRequest { review, text }, NOTES_MS, None).await,
            None => self.call(memory::REVIEW, review, NOTES_MS, None).await,
        }
    }

    /// The CLI reads these immutable records locally too; no new recall is
    /// performed and a later correction cannot replace the original context.
    pub async fn used_notes(&self, run: &str) -> anyhow::Result<Vec<RecallSnapshot>> {
        let path = self.memory_db.clone().context("memory has no database in this configuration")?;
        let workspace = self.workspace.clone();
        let run = run.to_owned();
        tokio::task::spawn_blocking(move || {
            anyhow::ensure!(path.is_file(), "memory has no recorded context yet");
            let db = molt_memory::Db::open(&path)?;
            molt_memory::recall_snapshots(&db, &workspace, &run).map_err(|e| anyhow::anyhow!(e.message))
        })
        .await?
    }

    /// Forget note `id`; false when there is none, or it is already forgotten.
    pub async fn forget(&self, id: &str, reason: &str) -> anyhow::Result<bool> {
        let req = ForgetRequest { id: id.to_owned(), reason: reason.to_owned() };
        let resp: ForgetResponse = self.call(memory::FORGET, req, NOTES_MS, None).await?;
        Ok(resp.forgotten)
    }

    /// Keep `text`, a preference of the user's, in their own layer of notes,
    /// recalled in every project.
    pub async fn remember(&self, text: &str) -> anyhow::Result<RememberResponse> {
        let req = RememberRequest {
            kind: NoteKind::Preference,
            text: text.to_owned(),
            workspace: None,
            confidence: Some(0.95),
            trace: TraceId::random().to_string(),
            events: Vec::new(),
            version: "user".into(),
        };
        self.call(memory::REMEMBER, req, NOTES_MS, None).await
    }

    /// Copy the project note `id` to the user's own layer; the copy.
    pub async fn promote(&self, id: &str, reason: &str) -> anyhow::Result<Note> {
        let req = PromoteRequest { workspace: self.workspace.clone(), id: id.to_owned(), reason: reason.to_owned() };
        let resp: PromoteResponse = self.call(memory::PROMOTE, req, NOTES_MS, None).await?;
        Ok(resp.note)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        target: &str,
        payload: impl Serialize,
        ms: u64,
        trace: Option<&TraceId>,
    ) -> anyhow::Result<T> {
        let opts = CallOpts { cap: None, budget: Budget::new(0, ms, 0), trace: trace.cloned() };
        let reply = self
            .cli
            .call(target, serde_json::to_value(payload)?, opts)
            .await
            .with_context(|| format!("{target} failed"))?;
        serde_json::from_value(reply).with_context(|| format!("the answer to {target} is not what it should be"))
    }

    /// Stop the services and the kernel, and remove the forks the session
    /// left behind, except those in `keep` and those of runs that stopped
    /// with a checkpoint, which a resume goes on in.
    pub async fn shutdown(&self, keep: &[String]) {
        self.shutdown_dropping(keep, None).await
    }

    /// [`Session::shutdown`], and the checkpoint of run `drop` is removed
    /// once the planner has stopped: that run is not to go on.
    pub async fn shutdown_dropping(&self, keep: &[String], drop: Option<&str>) {
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(running) = running {
            running.shutdown().await;
        }
        let mut keep: Vec<String> = keep.to_vec();
        if let Some(dir) = &self.checkpoints {
            if let Some(path) = drop.and_then(|run| checkpoint::path(dir, run).ok()) {
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("could not remove the checkpoint {}: {e}", path.display());
                    }
                }
            }
            keep.extend(checkpoint::list(dir).into_iter().filter_map(|(_, saved)| saved.ok()).flat_map(|s| s.forks()));
        }
        if let Some(forks) = self.forks.lock().unwrap_or_else(PoisonError::into_inner).take() {
            forks.remove_new(&keep.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }

    /// Stop the services and the kernel and remove no fork: the interface
    /// that would have said which to keep is gone, and one may hold a result
    /// it was waiting on.
    pub async fn abandon(&self) {
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(running) = running {
            running.shutdown().await;
        }
        self.forks.lock().unwrap_or_else(PoisonError::into_inner).take();
    }

    /// Drop the stopped run `run`: its forks, then its checkpoint. A fork
    /// already gone is no error. Not for a run that is going on.
    pub async fn drop_run(&self, run: &str) -> anyhow::Result<()> {
        let dir = self.checkpoints.as_deref().context("this config's planner keeps no checkpoints")?;
        let path = checkpoint::path(dir, run).map_err(|e| anyhow::anyhow!(e))?;
        // A damaged checkpoint names no fork to drop; it goes all the same.
        if let Ok(saved) = checkpoint::load(dir, run) {
            for fork in saved.forks() {
                if Path::new(&fork).exists() {
                    self.discard(&fork).await.with_context(|| format!("dropping {fork}"))?;
                }
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

fn versions(running: &Running, cfg: &Config) -> Vec<(String, String)> {
    let registry = running.kernel().registry();
    cfg.services
        .iter()
        .filter_map(|s| {
            let id = ServiceId::new(s.name.as_str()).ok()?;
            let version = registry.live(&id)?.to_string();
            let short = version.rsplit(':').next().unwrap_or(&version).chars().take(8).collect();
            Some((s.name.to_string(), short))
        })
        .collect()
}

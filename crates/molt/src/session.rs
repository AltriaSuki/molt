//! The agent services kept up for many tasks, as the TUI uses them: one
//! kernel, the services of a config, and the CLI's place on the bus.
//!
//! `molt do` starts everything for one task and stops it after; a session
//! starts once and serves every task the user gives it. The CLI holds the
//! same capabilities as in `molt do`, plus `planner.design` to show the
//! check before the attempts start, `fs.merge` and `fs.drop` to apply or
//! discard a result the user has looked at, and `memory.recall` and
//! `memory.forget` for the memory screen.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Context;
use molt_api::fs::{self, DropRequest, DropResponse, MergeRequest, MergeResponse};
use molt_api::memory::{
    self, ConsolidateRequest, ConsolidateResponse, ForgetRequest, ForgetResponse, RecallRequest, RecallResponse,
    Recalled,
};
use molt_api::planner::{self, DesignResponse, RunRequest, RunResponse};
use molt_api::progress::{self, Progress};
use molt_proto::{Budget, Kind, ServiceId, Target, TraceId};
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
/// Reply deadline for memory's notes.
const NOTES_MS: u64 = 60_000;

pub struct Session {
    running: Mutex<Option<Running>>,
    cli: Arc<Service>,
    workspace: String,
    /// `None` without a memory service; an error when it did not come up.
    memory: Option<Result<(), String>>,
    forks: Mutex<Option<RunForks>>,
    versions: Vec<(String, String)>,
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
                    forks: Mutex::new(RunForks::before(cfg)),
                    versions,
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
        let mut targets = vec![planner::RUN, planner::DESIGN, fs::MERGE, fs::DROP];
        if memory.is_some() {
            targets.extend([memory::CONSOLIDATE, memory::RECALL, memory::FORGET]);
        }
        for target in targets {
            agent::grant(kernel, &cli, target, budget).await?;
        }
        agent::grant(kernel, &cli, &format!("topic:{}", progress::TOPIC), budget).await?;
        cli.subscribe(progress::TOPIC).await?;
        let cli = Arc::new(cli);
        let pump = cli.clone();
        tokio::spawn(async move {
            while let Some(msg) = pump.next().await {
                let on_topic = matches!(&msg.to, Target::Topic { name } if name == progress::TOPIC);
                if msg.kind != Kind::Event || !on_topic {
                    continue;
                }
                if let Ok(event) = serde_json::from_value::<Progress>(msg.payload) {
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

    /// Have memory learn from the run `trace`, as `molt do` does after a run.
    pub async fn learn(&self, trace: &TraceId) -> Result<ConsolidateResponse, String> {
        if let Some(problem) = self.memory_problem() {
            return Err(problem);
        }
        // A trace of its own, so the learning does not become part of the episode.
        let req = ConsolidateRequest { episode: trace.to_string(), workspace: self.workspace.clone(), model: None };
        self.call(memory::CONSOLIDATE, req, LEARN_MS, None).await.map_err(|e| format!("{e:#}"))
    }

    /// Merge a result kept in `fork` into the workspace, and drop the fork.
    pub async fn apply(&self, fork: &str) -> anyhow::Result<MergeResponse> {
        self.call(fs::MERGE, MergeRequest { fork: fork.to_owned(), drop: true }, TREE_MS, None).await
    }

    pub async fn discard(&self, fork: &str) -> anyhow::Result<()> {
        let _: DropResponse = self.call(fs::DROP, DropRequest { fork: fork.to_owned() }, TREE_MS, None).await?;
        Ok(())
    }

    /// The notes about the workspace that match `query`, best first.
    pub async fn notes(&self, query: &str) -> anyhow::Result<Vec<Recalled>> {
        let req = RecallRequest {
            query: query.to_owned(),
            workspace: Some(self.workspace.clone()),
            k: Some(50),
            ..Default::default()
        };
        let resp: RecallResponse = self.call(memory::RECALL, req, NOTES_MS, None).await?;
        Ok(resp.notes)
    }

    /// Forget note `id`; false when there is none, or it is already forgotten.
    pub async fn forget(&self, id: &str, reason: &str) -> anyhow::Result<bool> {
        let req = ForgetRequest { id: id.to_owned(), reason: reason.to_owned() };
        let resp: ForgetResponse = self.call(memory::FORGET, req, NOTES_MS, None).await?;
        Ok(resp.forgotten)
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
    /// left behind, except those in `keep`.
    pub async fn shutdown(&self, keep: &[String]) {
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(running) = running {
            running.shutdown().await;
        }
        if let Some(forks) = self.forks.lock().unwrap_or_else(PoisonError::into_inner).take() {
            forks.remove_new(&keep.iter().map(String::as_str).collect::<Vec<_>>());
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

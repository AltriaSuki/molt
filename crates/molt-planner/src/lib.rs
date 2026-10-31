//! The planner: serves `planner.run`.
//!
//! See [`molt_api::planner`] for what a run does. The planner talks to the
//! model and tools only through a [`Bus`], so its logic can be tested with
//! scripted fakes.

mod agent;
mod ask;
mod attempt;
pub mod checkpoint;
pub mod compact;
mod control;
mod ctx;
mod designer;
mod explore;
mod fit;
mod git;
pub mod hooks;
mod instructions;
mod judge;
mod memory;
pub mod pack;
mod plan;
mod process;
mod prompts;
mod referenced;
mod registry;
mod review;
mod route;
mod runner;
pub mod schedule;
pub mod scoped;
mod select;
pub mod skills;
mod tools;
mod vet;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context};
use async_trait::async_trait;
use molt_api::planner::{Control, ControlRequest, ControlResponse, DesignResponse, RunRequest, RunResponse, Schedule};
use molt_api::progress::{self, Progress};
use molt_proto::{Budget, Envelope, ErrorCode, RemoteError, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Planner settings. [`Config::from_env`] documents the variables.
#[derive(Clone, Debug)]
pub struct Config {
    pub hooks: Vec<hooks::Hook>,
    /// Model for runs that name none; `None` leaves the choice to the gateway.
    pub default_model: Option<String>,
    pub max_turns: u32,
    pub max_check_rounds: u32,
    pub budget_usd: f64,
    /// The share of a run's budget designing its check may spend, for runs
    /// that set no `design_budget_usd`: more than 0, at most 1.
    pub design_share: f64,
    /// Offer `explore` to the attempts of every workflow whose attempts
    /// change files, as if it set `attempts.explore` (#42).
    pub explore: bool,
    /// Model for `explore`'s sub-agent when the explorer role names none;
    /// `None` uses the run's model.
    pub explore_model: Option<String>,
    /// Offer attempts the process tools, to run a command in the background
    /// while they work (#110), when the planner may call `shell.start`.
    pub processes: bool,
    /// Offer attempts `git`, to read the history of the user's repository
    /// (#111), when the planner may call `fs.git`.
    pub git: bool,
    /// `max_tokens` for each model call.
    pub max_tokens: u32,
    /// Deadline for one model call.
    pub model_timeout: Duration,
    /// Deadline for one done-check run.
    pub check_timeout: Duration,
    /// Questions the attempts of an interactive run may ask the person, in all.
    pub max_questions: u32,
    /// How long an attempt waits for the answer to its question before it
    /// decides itself.
    pub ask_timeout: Duration,
    /// Runs handled at once.
    pub max_concurrent_runs: usize,
    /// Size of the project map at the start of a run, in estimated tokens.
    pub map_tokens: u32,
    /// How long a finished run waits, while it applies the winner, for the
    /// replies to model calls that cancelled attempts left running, so their
    /// cost is counted. Calls still unanswered are reported as uncounted.
    pub late_reply_wait: Duration,
    /// The model's context window in tokens, the reply included. `None`
    /// takes what the gateway reports for the run's model once it does,
    /// else 200,000 for Claude models and 128,000 for any other.
    pub context_tokens: Option<u32>,
    /// The largest model request the bus carries, in bytes.
    pub frame_bytes: usize,
    /// Where checkpoints are kept; `None` keeps none, and a run asking for
    /// one is refused.
    pub state_dir: Option<std::path::PathBuf>,
    /// Names of the project's instruction files, looked for in the
    /// workspace root and the directories the task names.
    pub instruction_files: Vec<String>,
    /// What the model's file writes and edits do about syntax errors they
    /// introduce, for runs that do not say.
    pub syntax_check: molt_api::fs::SyntaxCheck,
    /// Workflows a run may name besides the built-ins. They come from
    /// whoever starts the planner, never from a request or a project.
    pub workflows: Vec<molt_api::workflow::Workflow>,
    /// Model for the one call that picks a workflow for a run that names
    /// `auto`; `None` uses the run's model.
    pub route_model: Option<String>,
    /// Tools services declared in their manifests (RFC 0002). They come
    /// from whoever starts the planner, never from a request or a project;
    /// a run offers only those whose method the planner holds a capability for.
    pub tools: Vec<molt_proto::tool::ProvidedTool>,
    /// The prompt pack: the system prompts, roles and workflows besides
    /// the built-ins (RFC 0003). Like the tools, it comes from whoever
    /// starts the planner, never from a request or a project. Its
    /// workflows are run by name, after those of `workflows`.
    pub pack: Arc<pack::Pack>,
    /// The user's skills directory, whose `NAME/SKILL.md` skills each run
    /// offers (issue #166). Like the pack, it comes from whoever starts the
    /// planner, never from a request or a project; a project's own skills,
    /// in its `.molt/skills`, are read as data.
    pub skills_dir: Option<std::path::PathBuf>,
    /// The `skills/` directories of the extension packs the user installed,
    /// approved and enabled (`MOLT_EXTENSION_SKILLS`), read after
    /// `skills_dir` and as the user's: like it, they come from whoever
    /// starts the planner, never from a request or a project.
    pub extension_skills: Vec<std::path::PathBuf>,
    /// Have a model write an attempt's notes on the turns its conversation
    /// leaves out to fit, beside Molt's own record of them (#108). Off by
    /// default: each fit then costs one more model call.
    pub compact_model: bool,
    /// The model each role's calls go to when its declaration names none
    /// (`MOLT_ROLE_MODELS`, #169): the reviewer on a cheaper model, say,
    /// while the attempts keep the run's. A request's `judge_model`, the
    /// explorer's `MOLT_EXPLORE_MODEL` and the router's `MOLT_ROUTE_MODEL`
    /// still win for their roles.
    pub role_models: std::collections::BTreeMap<molt_api::model::Role, String>,
    /// How runs that do not say start their attempts (`MOLT_SCHEDULE`, #13):
    /// all at once ([`Schedule::Fixed`], the default) or one at a time on
    /// evidence ([`Schedule::Adaptive`], see [`schedule`]).
    pub schedule: Schedule,
    /// The models an adaptive run escalates through, cheapest first
    /// (`MOLT_ESCALATION_MODELS`). Empty: every attempt keeps the run's model,
    /// and adaptive runs only decide how many attempts start.
    pub escalation_models: Vec<String>,
    /// The most escalations (a model up, or an attempt alongside a stuck one)
    /// one adaptive run makes (`MOLT_MAX_ESCALATIONS`); 0 for none.
    pub max_escalations: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hooks: Vec::new(),
            default_model: None,
            max_turns: 50,
            max_check_rounds: 3,
            budget_usd: 10.0,
            design_share: 0.25,
            explore: false,
            explore_model: None,
            processes: false,
            git: false,
            max_tokens: 32_000,
            model_timeout: Duration::from_secs(1200),
            check_timeout: Duration::from_secs(900),
            max_questions: 3,
            ask_timeout: Duration::from_secs(600),
            max_concurrent_runs: 4,
            map_tokens: 3_000,
            late_reply_wait: Duration::from_secs(2),
            context_tokens: None,
            frame_bytes: molt_transport::MAX_FRAME,
            state_dir: None,
            instruction_files: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            syntax_check: molt_api::fs::SyntaxCheck::Report,
            workflows: Vec::new(),
            route_model: None,
            tools: Vec::new(),
            pack: Arc::default(),
            extension_skills: Vec::new(),
            skills_dir: None,
            compact_model: false,
            role_models: Default::default(),
            schedule: Schedule::Fixed,
            escalation_models: Vec::new(),
            max_escalations: 2,
        }
    }
}

impl Config {
    /// [`Config::default`] overridden by `MOLT_PLANNER_MODEL`,
    /// `MOLT_MAX_TURNS`, `MOLT_MAX_CHECK_ROUNDS`, `MOLT_BUDGET_USD`,
    /// `MOLT_DESIGN_BUDGET_SHARE`, `MOLT_EXPLORE`, `MOLT_EXPLORE_MODEL`, `MOLT_PROCESSES`, `MOLT_GIT`, `MOLT_MAX_TOKENS`,
    /// `MOLT_MODEL_TIMEOUT_S`, `MOLT_CHECK_TIMEOUT_S`,
    /// `MOLT_MAP_TOKENS`, `MOLT_CONTEXT_TOKENS`, `MOLT_INSTRUCTION_FILES`,
    /// `MOLT_SYNTAX_CHECK`, `MOLT_ROUTE_MODEL`, `MOLT_PROMPTS_DIR`, `MOLT_SKILLS_DIR`, `MOLT_COMPACT`,
    /// `MOLT_ROLE_MODELS`, `MOLT_SCHEDULE`, `MOLT_ESCALATION_MODELS` and `MOLT_MAX_ESCALATIONS`.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `MOLT_PLANNER_MODEL` | unset (the gateway's default) |
    /// | `MOLT_MAX_TURNS` | `50` |
    /// | `MOLT_MAX_CHECK_ROUNDS` | `3` |
    /// | `MOLT_BUDGET_USD` | `10` |
    /// | `MOLT_DESIGN_BUDGET_SHARE` | `0.25`: designing the check may spend a quarter of the run's budget |
    /// | `MOLT_EXPLORE` | `off`: only workflows that set `attempts.explore` offer `explore`; `on` for every one whose attempts change files |
    /// | `MOLT_EXPLORE_MODEL` | unset: `explore`'s sub-agent uses the explorer role's model, else the run's |
    /// | `MOLT_PROCESSES` | `off`: `on` offers attempts `start_process`, `read_process`, `write_process` and `stop_process`, to run commands in the background |
    /// | `MOLT_GIT` | `off`: `on` offers attempts `git`, to read the history of the user's repository |
    /// | `MOLT_MAX_TOKENS` | `32000` |
    /// | `MOLT_MODEL_TIMEOUT_S` | `1200` |
    /// | `MOLT_CHECK_TIMEOUT_S` | `900` |
    /// | `MOLT_MAP_TOKENS` | `3000` (at most 32000) |
    /// | `MOLT_CONTEXT_TOKENS` | unset: 200000 for Claude models, 128000 for others |
    /// | `MOLT_INSTRUCTION_FILES` | `AGENTS.md,CLAUDE.md`; `none` reads none |
    /// | `MOLT_SYNTAX_CHECK` | `report`; or `reject`, or `off` |
    /// | `MOLT_ROUTE_MODEL` | unset: the run's model picks the workflow of a run that names `auto` |
    /// | `MOLT_PROMPTS_DIR` | unset: the built-in prompts; else the prompt pack to read ([`pack::Pack::load`]) |
    /// | `MOLT_PROMPT_MODULES_JSON` | unset: none; else the promoted prompt versions, under the pack's files ([`pack::Pack::with_modules`]) |
    /// | `MOLT_SKILLS_DIR` | unset: no user skills; else the directory of the user's skills ([`skills`]) |
    /// | `MOLT_EXTENSION_SKILLS` | unset: none; else the extension packs' skill directories, as a `PATH`-style list ([`skills`]) |
    /// | `MOLT_COMPACT` | `record`: Molt's own record of the turns a conversation leaves out; `model` also has the `compactor` write notes on them ([`compact`]) |
    /// | `MOLT_ROLE_MODELS` | unset: every role but the judge's uses the run's model; else a JSON object of role to model ([`molt_api::model::role_models`]) |
    /// | `MOLT_SCHEDULE` | `fixed`: a run's attempts start at once; `adaptive` starts one and adds more on evidence ([`schedule`]). A request's `schedule` wins |
    /// | `MOLT_ESCALATION_MODELS` | unset: adaptive runs keep the run's model; else the models to escalate through, cheapest first, separated by commas |
    /// | `MOLT_MAX_ESCALATIONS` | `2`; `0` makes no escalation |
    ///
    /// An empty variable counts as unset. A value that does not parse is an
    /// error, and so is a zero: none of these limits has a "no limit" value.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let number = |name: &str, default: u64| -> anyhow::Result<u64> {
            let Some(v) = get(name) else { return Ok(default) };
            let n: u64 = v.parse().with_context(|| format!("{name}={v:?} is not a number"))?;
            ensure!(n > 0, "{name}={v:?} must be at least 1");
            Ok(n)
        };
        let small = |name: &str, default: u32| -> anyhow::Result<u32> {
            u32::try_from(number(name, default.into())?).with_context(|| format!("{name} is too large"))
        };
        let d = Self::default();
        let budget_usd = match get("MOLT_BUDGET_USD") {
            Some(v) => {
                let usd: f64 = v.parse().with_context(|| format!("MOLT_BUDGET_USD={v:?} is not a number"))?;
                ensure!(usd.is_finite() && usd > 0.0, "MOLT_BUDGET_USD={v:?} must be a positive amount");
                usd
            }
            None => d.budget_usd,
        };
        let design_share = match get("MOLT_DESIGN_BUDGET_SHARE") {
            Some(v) => {
                let share: f64 =
                    v.parse().with_context(|| format!("MOLT_DESIGN_BUDGET_SHARE={v:?} is not a number"))?;
                ensure!(
                    share > 0.0 && share <= 1.0,
                    "MOLT_DESIGN_BUDGET_SHARE={v:?} must be more than 0 and at most 1, such as 0.25 for a quarter"
                );
                share
            }
            None => d.design_share,
        };
        let hooks = get("MOLT_HOOKS_JSON")
            .map(|s| serde_json::from_str::<Vec<hooks::Hook>>(&s))
            .transpose()?
            .unwrap_or_default();
        hooks::validate(&hooks)?;
        let tools = get(TOOLS_ENV)
            .map(|s| serde_json::from_str::<Vec<molt_proto::tool::ProvidedTool>>(&s))
            .transpose()
            .context("MOLT_TOOLS_JSON is not a list of tool declarations")?
            .unwrap_or_default();
        validate_tools(&tools)?;
        let mut pack = match get(pack::PROMPTS_ENV) {
            Some(dir) => pack::Pack::load(std::path::Path::new(&dir))?,
            None => pack::Pack::default(),
        };
        if let Some(json) = get(pack::MODULES_ENV) {
            let modules = serde_json::from_str(&json)
                .with_context(|| format!("{} is not a list of prompt modules", pack::MODULES_ENV))?;
            pack = pack.with_modules(modules)?;
        }
        Ok(Self {
            hooks,
            extension_skills: get(skills::EXTENSION_SKILLS_ENV)
                .map(|v| std::env::split_paths(&v).filter(|p| p.is_absolute()).collect())
                .unwrap_or_default(),
            default_model: get("MOLT_PLANNER_MODEL"),
            max_turns: small("MOLT_MAX_TURNS", d.max_turns)?,
            max_check_rounds: small("MOLT_MAX_CHECK_ROUNDS", d.max_check_rounds)?,
            budget_usd,
            design_share,
            processes: match get("MOLT_PROCESSES").as_deref() {
                None | Some("off" | "0" | "false") => false,
                Some("on" | "1" | "true") => true,
                Some(v) => anyhow::bail!("MOLT_PROCESSES={v:?}: use on or off"),
            },
            git: match get("MOLT_GIT").as_deref() {
                None | Some("off" | "0" | "false") => false,
                Some("on" | "1" | "true") => true,
                Some(v) => anyhow::bail!("MOLT_GIT={v:?}: use on or off"),
            },
            explore: match get("MOLT_EXPLORE").as_deref() {
                None | Some("off" | "0" | "false") => false,
                Some("on" | "1" | "true") => true,
                Some(v) => anyhow::bail!("MOLT_EXPLORE={v:?}: use on or off"),
            },
            explore_model: get("MOLT_EXPLORE_MODEL"),
            compact_model: match get("MOLT_COMPACT").as_deref() {
                None | Some("record") => false,
                Some("model") => true,
                Some(v) => anyhow::bail!("MOLT_COMPACT={v:?}: use record or model"),
            },
            max_tokens: small("MOLT_MAX_TOKENS", d.max_tokens)?,
            model_timeout: Duration::from_secs(number("MOLT_MODEL_TIMEOUT_S", d.model_timeout.as_secs())?),
            check_timeout: Duration::from_secs(number("MOLT_CHECK_TIMEOUT_S", d.check_timeout.as_secs())?),
            max_questions: d.max_questions,
            ask_timeout: d.ask_timeout,
            map_tokens: small("MOLT_MAP_TOKENS", d.map_tokens)?.min(32_000),
            max_concurrent_runs: d.max_concurrent_runs,
            late_reply_wait: d.late_reply_wait,
            context_tokens: get("MOLT_CONTEXT_TOKENS").map(|_| small("MOLT_CONTEXT_TOKENS", 0)).transpose()?,
            frame_bytes: d.frame_bytes,
            state_dir: d.state_dir,
            instruction_files: match get("MOLT_INSTRUCTION_FILES") {
                None => d.instruction_files,
                Some(v) if v == "none" => Vec::new(),
                Some(v) => {
                    let names: Vec<String> =
                        v.split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned).collect();
                    let plain = |n: &String| !n.contains('/') && n != "." && n != "..";
                    ensure!(
                        names.iter().all(plain),
                        "MOLT_INSTRUCTION_FILES={v:?}: give file names, without directories, separated by commas"
                    );
                    names
                }
            },
            syntax_check: match get("MOLT_SYNTAX_CHECK") {
                None => d.syntax_check,
                Some(v) => serde_json::from_value(Value::String(v.clone()))
                    .map_err(|_| anyhow::anyhow!("MOLT_SYNTAX_CHECK={v:?}: use report, reject or off"))?,
            },
            workflows: d.workflows,
            route_model: get("MOLT_ROUTE_MODEL"),
            tools,
            pack: Arc::new(pack),
            skills_dir: get(skills::SKILLS_ENV).filter(|d| !d.trim().is_empty()).map(std::path::PathBuf::from),
            role_models: match get(molt_api::model::ROLE_MODELS_ENV) {
                Some(text) => molt_api::model::role_models(&text).map_err(anyhow::Error::msg)?,
                None => d.role_models,
            },
            schedule: match get("MOLT_SCHEDULE") {
                Some(v) => v.parse().map_err(|e| anyhow::anyhow!("MOLT_SCHEDULE: {e}"))?,
                None => d.schedule,
            },
            escalation_models: match get("MOLT_ESCALATION_MODELS") {
                Some(v) => schedule::ladder(&v).map_err(|e| anyhow::anyhow!("MOLT_ESCALATION_MODELS={v:?}: {e}"))?,
                None => d.escalation_models,
            },
            max_escalations: match get("MOLT_MAX_ESCALATIONS") {
                Some(v) => v.parse().with_context(|| format!("MOLT_MAX_ESCALATIONS={v:?} is not a number"))?,
                None => d.max_escalations,
            },
        })
    }
}

/// The variable that carries [`Config::tools`] to a planner started as a
/// process, as a JSON list of [`molt_proto::tool::ProvidedTool`].
pub const TOOLS_ENV: &str = "MOLT_TOOLS_JSON";

/// What is wrong with the tools services declared for the planner, if
/// anything: each must be well formed, on a method of the service that
/// declares it, and have a name no other tool has, the planner's own
/// included.
pub fn validate_tools(tools: &[molt_proto::tool::ProvidedTool]) -> anyhow::Result<()> {
    let mut names = std::collections::BTreeMap::new();
    for tool in tools {
        let (service, name) = (&tool.service, &tool.decl.name);
        tool.decl.validate().map_err(|e| anyhow::anyhow!("service {service}: {e}"))?;
        ensure!(
            tool.decl.target.service() == Some(service),
            "service {service}: tool {name:?} targets {}, a method of another service",
            tool.decl.target
        );
        ensure!(!registry::reserved(name), "service {service}: {name:?} is the name of one of the planner's own tools");
        if let Some(other) = names.insert(name.as_str(), service) {
            anyhow::bail!("services {other} and {service} both declare a tool named {name:?}: rename one");
        }
    }
    Ok(())
}

/// What the review's fixed rules find in `changes` and their `patch`:
/// deleted tests, new skip markers and changed test configuration, leaving
/// out the check's own `check_files`. For a result changed after its run,
/// such as only part of it or one the user edited.
pub fn review_rules(check_files: &[String], changes: &[molt_api::fs::Change], patch: &str) -> Vec<String> {
    review::rules(check_files, changes, patch)
}

/// How the planner reaches other services.
#[async_trait]
pub trait Bus: Send + Sync + 'static {
    /// Call `target` (e.g. `fs.read`) and wait for the reply payload.
    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError>;
    /// Implementations may propagate cancellation; providers without support
    /// can return a late result, whose usage is still counted exactly once.
    async fn call_cancellable(
        &self,
        target: &str,
        payload: Value,
        budget: Budget,
        trace: &TraceId,
        _cancel: &CancellationToken,
    ) -> Result<Value, RemoteError> {
        self.call(target, payload, budget, trace).await
    }
    /// Publish an event; failures are ignored.
    async fn publish(&self, topic: &str, payload: Value);
    /// Whether this planner may call `target` at all. A bus that cannot
    /// tell says no, so that no tool is offered that would only be refused.
    fn holds(&self, _target: &str) -> bool {
        false
    }
}

/// [`Bus`] over a real service link.
pub struct ServiceBus(pub Arc<Service>);

#[async_trait]
impl Bus for ServiceBus {
    fn holds(&self, target: &str) -> bool {
        target.parse::<Target>().ok().and_then(|t| self.0.cap_for(&t)).is_some()
    }

    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError> {
        let opts = CallOpts { cap: None, budget, trace: Some(trace.clone()) };
        self.0.call(target, payload, opts).await.map_err(|e| match e {
            SdkError::Remote(e) => e,
            other => RemoteError { code: ErrorCode::Unavailable, message: format!("calling {target}: {other}") },
        })
    }

    async fn call_cancellable(
        &self,
        target: &str,
        payload: Value,
        budget: Budget,
        trace: &TraceId,
        cancel: &CancellationToken,
    ) -> Result<Value, RemoteError> {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let opts = CallOpts { cap: None, budget, trace: Some(trace.clone()) };
        let pending = self.0.request(target, payload, opts).await.map_err(sdk_error)?;
        let id = pending.id().clone();
        let wait = pending.wait();
        tokio::pin!(wait);
        tokio::select! {
            biased;
            result = &mut wait => result.map_err(sdk_error),
            _ = cancel.cancelled() => {
                // Dropping the wait sends kernel.cancel, but keep it alive here
                // to accept a completion that raced cancellation and its usage.
                let _ = tokio::time::timeout(Duration::from_secs(1), self.0.kernel("cancel", None, serde_json::json!({"request": id}))).await;
                match tokio::time::timeout(Duration::from_secs(2), &mut wait).await {
                    Ok(result) => result.map_err(sdk_error),
                    Err(_) => Err(cancelled()),
                }
            }
        }
    }

    async fn publish(&self, topic: &str, payload: Value) {
        if let Err(e) = self.0.publish(topic, payload).await {
            tracing::debug!(topic, error = %e, "could not publish an event");
        }
    }
}

fn cancelled() -> RemoteError {
    RemoteError {
        code: ErrorCode::Cancelled,
        message: "local call stopped; remote cancellation and settlement are unknown".into(),
    }
}
fn sdk_error(error: SdkError) -> RemoteError {
    match error {
        SdkError::Remote(e) => e,
        e => RemoteError { code: ErrorCode::Unavailable, message: e.to_string() },
    }
}

/// Carry out one run. `trace` ties every call of the run together in the
/// audit log and names the run in progress events.
pub async fn run(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<RunResponse, RemoteError> {
    runner::run(bus, cfg, req, trace).await
}

/// [`run`], stopped when `cancel` fires: no attempt starts after that, and
/// the run answers with what it had, as one its caller cancels over the bus.
pub async fn run_cancellable(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
    cancel: CancellationToken,
) -> Result<RunResponse, RemoteError> {
    runner::run_cancellable(bus, cfg, req, trace, cancel).await
}

/// Design the done-check for a run without starting it (`planner.design`).
pub async fn design(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<DesignResponse, RemoteError> {
    runner::design(bus, cfg, req, trace).await
}

/// Carry out a task as ordered steps, each its own run (`planner.plan`,
/// see [`molt_api::plan`]); with `propose`, only design the steps.
pub async fn plan(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: molt_api::plan::PlanRequest,
    trace: TraceId,
) -> Result<molt_api::plan::PlanResponse, RemoteError> {
    plan::plan(bus, cfg, req, trace, CancellationToken::new()).await
}

/// Steer the run `req.run`, going on in this process (`planner.control`).
/// A new requirement is announced on the run's progress.
pub async fn control(bus: &dyn Bus, req: ControlRequest) -> Result<ControlResponse, RemoteError> {
    let invalid = |message: String| RemoteError { code: ErrorCode::Invalid, message };
    let Some(control) = control::find(&req.run) else {
        return Err(invalid(format!("no run {} is going on here (it finished, or never started)", req.run)));
    };
    match req.action {
        Control::Constrain(constraint) => {
            let requirement = control.add(constraint).map_err(invalid)?;
            tracing::info!(run = %req.run, version = requirement.version, "requirements changed");
            let event = Progress::Required { run: req.run.clone(), requirement };
            if let Ok(v) = serde_json::to_value(&event) {
                bus.publish(progress::TOPIC, v).await;
            }
        }
        Control::Pause => control.set_paused(true),
        Control::Resume => control.set_paused(false),
    }
    Ok(control.response())
}

/// Calls to `planner.control` handled at once, besides the runs: steering a
/// run must not wait for a free run slot.
const CONTROL_SLOTS: usize = 16;

/// Serve `planner.run`, `planner.design` and `planner.control` on `svc`
/// until its link closes.
pub async fn serve(svc: Arc<Service>, cfg: Config) {
    let runs = Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_runs.max(1)));
    let max_in_flight = cfg.max_concurrent_runs.max(1) + CONTROL_SLOTS;
    let cfg = Arc::new(cfg);
    let bus: Arc<dyn Bus> = Arc::new(ServiceBus(svc.clone()));
    svc.serve_cancellable(max_in_flight, move |req, cancel| {
        let (bus, cfg, runs) = (bus.clone(), cfg.clone(), runs.clone());
        async move { handle_cancellable(bus, cfg, req, cancel, runs).await }
    })
    .await;
}

#[cfg(test)]
async fn handle(bus: Arc<dyn Bus>, cfg: Arc<Config>, req: Envelope) -> Result<Value, RemoteError> {
    let runs = Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_runs));
    handle_cancellable(bus, cfg, req, CancellationToken::new(), runs).await
}

async fn handle_cancellable(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: Envelope,
    cancel: CancellationToken,
    runs: Arc<tokio::sync::Semaphore>,
) -> Result<Value, RemoteError> {
    let method = match &req.to {
        Target::Method { method, .. } => method.as_str(),
        _ => "",
    };
    let request = |payload: Value| -> Result<RunRequest, RemoteError> {
        serde_json::from_value(payload).map_err(|e| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("bad planner.{method} request: {e}"),
        })
    };
    let encode = |response: Result<Value, serde_json::Error>| {
        response.map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the response: {e}") })
    };
    if method == "control" {
        let asked: ControlRequest = serde_json::from_value(req.payload).map_err(|e| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("bad planner.control request: {e}"),
        })?;
        return encode(serde_json::to_value(control(bus.as_ref(), asked).await?));
    }
    // A run or a design waits for a free slot.
    let _slot = tokio::select! {
        slot = runs.acquire_owned() => slot.map_err(|_| RemoteError { code: ErrorCode::Unavailable, message: "the planner is stopping".into() })?,
        _ = cancel.cancelled() => return Err(cancelled()),
    };
    match method {
        "run" => {
            let response = runner::run_cancellable(bus, cfg, request(req.payload)?, req.trace_id, cancel).await?;
            encode(serde_json::to_value(response))
        }
        "design" => {
            let response = runner::design_cancellable(bus, cfg, request(req.payload)?, req.trace_id, cancel).await?;
            encode(serde_json::to_value(response))
        }
        "plan" => {
            let asked = serde_json::from_value(req.payload).map_err(|e| RemoteError {
                code: ErrorCode::Invalid,
                message: format!("bad planner.plan request: {e}"),
            })?;
            let response = plan::plan(bus, cfg, asked, req.trace_id, cancel).await?;
            encode(serde_json::to_value(response))
        }
        other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use molt_proto::CapId;
    use serde_json::json;

    use super::*;

    fn from(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn declared_tools_come_from_the_environment_and_are_checked() {
        assert!(from(&[]).unwrap().tools.is_empty());
        let tool = |service: &str, name: &str, target: &str| {
            serde_json::json!({ "service": service, "name": name, "description": "d",
                "input_schema": { "type": "object" }, "target": target })
        };
        let vars = |tools: Vec<Value>| serde_json::to_string(&tools).unwrap();
        let cfg = from(&[(TOOLS_ENV, &vars(vec![tool("lint", "lint", "lint.check")]))]).unwrap();
        assert_eq!(cfg.tools[0].decl.name, "lint");
        for (tools, says) in [
            (vec![tool("lint", "run", "lint.check")], "planner's own tools"),
            (vec![tool("lint", "lint", "fs.write")], "a method of another service"),
            (vec![tool("lint", "lint", "lint.check"), tool("fmt", "lint", "fmt.run")], "both declare"),
        ] {
            let err = format!("{:#}", from(&[(TOOLS_ENV, &vars(tools))]).unwrap_err());
            assert!(err.contains(says), "{err}");
        }
        assert!(from(&[(TOOLS_ENV, "{")]).is_err());
    }

    #[test]
    fn env_defaults() {
        let cfg = from(&[]).unwrap();
        let d = Config::default();
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.max_turns, d.max_turns);
        assert_eq!(cfg.max_check_rounds, d.max_check_rounds);
        assert_eq!(cfg.budget_usd, d.budget_usd);
        assert_eq!(cfg.design_share, 0.25);
        assert_eq!(cfg.max_tokens, d.max_tokens);
        assert_eq!(cfg.model_timeout, d.model_timeout);
        assert_eq!(cfg.check_timeout, d.check_timeout);
        assert_eq!(cfg.max_concurrent_runs, d.max_concurrent_runs);
        assert_eq!(cfg.map_tokens, 3_000);
        assert_eq!(cfg.instruction_files, ["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(cfg.syntax_check, molt_api::fs::SyntaxCheck::Report);
    }

    #[test]
    fn the_prompt_pack_comes_from_the_directory_its_starter_names() {
        assert!(from(&[]).unwrap().pack.prompts.is_empty());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("attempt.md"), "Be brief.\n").unwrap();
        let path = dir.path().to_str().unwrap();
        let cfg = from(&[(pack::PROMPTS_ENV, path)]).unwrap();
        assert_eq!(cfg.pack.prompts["attempt"], "Be brief.");
        std::fs::write(dir.path().join("pack.toml"), "[[role]]\nname = \"x\"\nbase = \"nobody\"\n").unwrap();
        assert!(from(&[(pack::PROMPTS_ENV, path)]).is_err(), "a pack that does not read stops the planner");
        assert!(from(&[(pack::PROMPTS_ENV, "/no/such/pack")]).is_err());
    }

    #[test]
    fn env_overrides() {
        let cfg = from(&[
            ("MOLT_PLANNER_MODEL", "sonnet"),
            ("MOLT_MAX_TURNS", "7"),
            ("MOLT_MAX_CHECK_ROUNDS", "4"),
            ("MOLT_BUDGET_USD", "2.5"),
            ("MOLT_DESIGN_BUDGET_SHARE", "0.5"),
            ("MOLT_MAX_TOKENS", "1000"),
            ("MOLT_MODEL_TIMEOUT_S", "5"),
            ("MOLT_CHECK_TIMEOUT_S", "6"),
            ("MOLT_MAP_TOKENS", "99999"),
        ])
        .unwrap();
        assert_eq!(cfg.map_tokens, 32_000);
        assert_eq!(cfg.default_model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.max_turns, 7);
        assert_eq!(cfg.max_check_rounds, 4);
        assert_eq!(cfg.budget_usd, 2.5);
        assert_eq!(cfg.design_share, 0.5);
        assert_eq!(cfg.max_tokens, 1000);
        assert_eq!(cfg.model_timeout, Duration::from_secs(5));
        assert_eq!(cfg.check_timeout, Duration::from_secs(6));
        let files = |v: &str| from(&[("MOLT_INSTRUCTION_FILES", v)]).unwrap().instruction_files;
        assert_eq!(files("AGENTS.md, .cursorrules,"), ["AGENTS.md", ".cursorrules"]);
        assert!(files("none").is_empty());
        let syntax = |v: &str| from(&[("MOLT_SYNTAX_CHECK", v)]).unwrap().syntax_check;
        assert_eq!(syntax("reject"), molt_api::fs::SyntaxCheck::Reject);
        assert_eq!(syntax(" off "), molt_api::fs::SyntaxCheck::Off);
    }

    #[test]
    fn env_errors() {
        assert!(from(&[("MOLT_MAX_TURNS", "many")]).is_err());
        // Zero is never "no limit": it would fail every run.
        for name in [
            "MOLT_MAX_TURNS",
            "MOLT_MAX_CHECK_ROUNDS",
            "MOLT_MAX_TOKENS",
            "MOLT_MODEL_TIMEOUT_S",
            "MOLT_CHECK_TIMEOUT_S",
            "MOLT_MAP_TOKENS",
        ] {
            let err = from(&[(name, "0")]).unwrap_err().to_string();
            assert!(err.contains(name), "{err}");
        }
        assert!(from(&[("MOLT_MAX_TOKENS", "99999999999")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "cheap")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "-1")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "NaN")]).is_err());
        for share in ["0", "1.5", "-0.1", "NaN", "a quarter"] {
            let err = from(&[("MOLT_DESIGN_BUDGET_SHARE", share)]).unwrap_err().to_string();
            assert!(err.contains("MOLT_DESIGN_BUDGET_SHARE"), "{err}");
        }
        assert_eq!(from(&[("MOLT_DESIGN_BUDGET_SHARE", "1")]).unwrap().design_share, 1.0);
        assert!(from(&[("MOLT_CHECK_TIMEOUT_S", "1.5")]).is_err());
        assert!(from(&[("MOLT_INSTRUCTION_FILES", "docs/AGENTS.md")]).is_err());
        assert!(from(&[("MOLT_INSTRUCTION_FILES", "..")]).is_err());
        assert!(from(&[("MOLT_SYNTAX_CHECK", "strict")]).unwrap_err().to_string().contains("report, reject or off"));
        // Empty means unset.
        assert_eq!(from(&[("MOLT_PLANNER_MODEL", " ")]).unwrap().default_model, None);
    }

    struct NoBus;

    #[async_trait]
    impl Bus for NoBus {
        async fn call(&self, target: &str, _: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
            Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") })
        }

        async fn publish(&self, _: &str, _: Value) {}
    }

    fn envelope(to: &str, payload: Value) -> Envelope {
        Envelope::request(TraceId::random(), to.parse().unwrap(), CapId::random(), payload)
    }

    #[tokio::test]
    async fn unknown_methods_and_bad_payloads_are_invalid() {
        let (bus, cfg): (Arc<dyn Bus>, _) = (Arc::new(NoBus), Arc::new(Config::default()));
        let err = handle(bus.clone(), cfg.clone(), envelope("planner.steer", json!({}))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("no method \"steer\""), "{}", err.message);
        let err = handle(bus.clone(), cfg.clone(), envelope("planner.plan", json!({}))).await.unwrap_err();
        assert!(err.code == ErrorCode::Invalid && err.message.contains("bad planner.plan request"), "{}", err.message);

        let err = handle(bus.clone(), cfg.clone(), envelope("planner.run", json!({ "task": 1 }))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);

        let bad = json!({ "task": "t", "workspace": "/w", "attempts": 9 });
        let err = handle(bus.clone(), cfg.clone(), envelope("planner.run", bad)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);

        // planner.design designs: a check given, or verify off, leaves it nothing to do.
        for given in [json!({ "check": "true" }), json!({ "verify": false })] {
            let mut payload = json!({ "task": "t", "workspace": "/w" });
            payload.as_object_mut().unwrap().extend(given.as_object().unwrap().clone());
            let err = handle(bus.clone(), cfg.clone(), envelope("planner.design", payload)).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{}", err.message);
        }

        // A valid request reaches the bus; the fork fails because nothing serves `fs`.
        let ok = json!({ "task": "t", "workspace": "/w" });
        let err = handle(bus, cfg, envelope("planner.run", ok)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
    }

    #[test]
    fn processes_are_off_unless_the_planners_starter_turns_them_on() {
        assert!(!from(&[]).unwrap().processes);
        assert!(from(&[("MOLT_PROCESSES", "on")]).unwrap().processes);
        assert!(!from(&[("MOLT_PROCESSES", "off")]).unwrap().processes);
        assert!(format!("{:#}", from(&[("MOLT_PROCESSES", "yes")]).unwrap_err()).contains("use on or off"));
    }

    #[test]
    fn git_is_off_unless_the_planners_starter_turns_it_on() {
        assert!(!from(&[]).unwrap().git);
        assert!(from(&[("MOLT_GIT", "on")]).unwrap().git);
        assert!(!from(&[("MOLT_GIT", "off")]).unwrap().git);
        assert!(format!("{:#}", from(&[("MOLT_GIT", "yes")]).unwrap_err()).contains("MOLT_GIT"));
    }

    #[test]
    fn explore_is_off_unless_the_planners_starter_turns_it_on() {
        let cfg = from(&[]).unwrap();
        assert!(!cfg.explore && cfg.explore_model.is_none());
        let cfg = from(&[("MOLT_EXPLORE", "on"), ("MOLT_EXPLORE_MODEL", "haiku")]).unwrap();
        assert!(cfg.explore && cfg.explore_model.as_deref() == Some("haiku"));
        let cfg = from(&[("MOLT_ROLE_MODELS", r#"{"reviewer":"haiku"}"#)]).unwrap();
        assert_eq!(cfg.role_models.get(&molt_api::model::Role::Reviewer).map(String::as_str), Some("haiku"));
        assert!(from(&[]).unwrap().role_models.is_empty());
        assert!(from(&[("MOLT_ROLE_MODELS", r#"{"attempt":"opus"}"#)]).is_err());
        assert!(!from(&[("MOLT_EXPLORE", "off")]).unwrap().explore);
        assert!(format!("{:#}", from(&[("MOLT_EXPLORE", "yes")]).unwrap_err()).contains("use on or off"));
        // On, it is every workflow whose attempts change files, and never ask's.
        let cfg = Config { explore: true, ..Config::default() };
        assert!(runner::workflow(&cfg, None).unwrap().explores());
        assert!(runner::workflow(&cfg, Some("quick")).unwrap().explores());
        assert!(!runner::workflow(&cfg, Some("ask")).unwrap().explores());
        assert!(!runner::workflow(&Config::default(), None).unwrap().explores());
    }

    #[test]
    fn runs_are_scheduled_fixed_unless_the_planners_starter_says_adaptive() {
        let cfg = from(&[]).unwrap();
        assert_eq!((cfg.schedule, cfg.escalation_models.len(), cfg.max_escalations), (Schedule::Fixed, 0, 2));
        let cfg = from(&[
            ("MOLT_SCHEDULE", "adaptive"),
            ("MOLT_ESCALATION_MODELS", "haiku, sonnet,opus"),
            ("MOLT_MAX_ESCALATIONS", "0"),
        ])
        .unwrap();
        assert_eq!(cfg.schedule, Schedule::Adaptive);
        assert_eq!(cfg.escalation_models, ["haiku", "sonnet", "opus"]);
        assert_eq!(cfg.max_escalations, 0, "zero escalations is a setting, not no limit");
        assert!(format!("{:#}", from(&[("MOLT_SCHEDULE", "smart")]).unwrap_err()).contains("fixed or adaptive"));
        assert!(format!("{:#}", from(&[("MOLT_ESCALATION_MODELS", "a,a")]).unwrap_err()).contains("twice"));
        assert!(from(&[("MOLT_MAX_ESCALATIONS", "-1")]).is_err());
    }

    #[test]
    fn a_model_writes_compaction_notes_only_when_the_planners_starter_says_so() {
        assert!(!from(&[]).unwrap().compact_model);
        assert!(!from(&[("MOLT_COMPACT", "record")]).unwrap().compact_model);
        assert!(from(&[("MOLT_COMPACT", "model")]).unwrap().compact_model);
        assert!(format!("{:#}", from(&[("MOLT_COMPACT", "yes")]).unwrap_err()).contains("use record or model"));
    }
}

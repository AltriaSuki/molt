//! `molt improve` (issue #35, `docs/improve.md`): the improver proposes a
//! new version of one of the planner's prompts, and a gate promotes it only
//! when it beats the live one on the benchmark's regression and held-out sets.
//!
//! The pieces and who may touch what:
//!
//! - The prompt versions are data modules (`prompt.NAME`) in the registry of
//!   a kernel of their own, in the user's improvement directory ([`dir`]),
//!   whose audit log records every stored version, every decision with its
//!   evidence, and every promotion. `molt do` hands the planner the live
//!   ones ([`live_prompts`]), under the user's own prompt pack.
//! - The improver (`molt-improver`) is a candidate service: it runs in the
//!   sandbox with no files and no network, and holds only `model.complete`,
//!   with the budget this module grants. It gets the failures of the
//!   regression set (the tasks of split `dev`) and answers with a text.
//! - The gate is this module, trusted host code. Its thresholds are the
//!   user's `[improve]` configuration ([`Gate`]); the tasks, their hidden
//!   tests and the held-out set are read here and run by `molt bench`'s
//!   harness. The improver sees none of them, and its answer can carry
//!   nothing but its text and its rationale.
//! - Only prompts that do not guard acceptance can be improved (not the
//!   reviewer's, the judge's or the vetter's). Those are the low-risk tier
//!   (`mutable`), which the gate promotes by itself when the candidate passes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, ensure, Context};
use async_trait::async_trait;
use molt_api::improve::{Case, Pattern, ProposeRequest, ProposeResponse, PROPOSE};
use molt_bench::agent::{Agent, AgentRun, Arm, Job, Molt};
use molt_bench::record::Record;
use molt_bench::run::{self, Plan};
use molt_bench::task::{Select, Split, Task};
use molt_kernel::audit::Decision;
use molt_kernel::registry::Authority;
use molt_planner::pack::{self, PromptModule, MODULE_PREFIX};
use molt_proto::{Budget, CapRequest, Exec, Module, ServiceId, Tier, VersionId};
use molt_sdk::CallOpts;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Isolation, ServiceSection};

/// The variable that names the improvement directory; `none` turns the
/// promoted prompts off for `molt do`.
pub const DIR_ENV: &str = "MOLT_IMPROVE_DIR";
/// The gate's name in the registry and the audit log.
pub const GATE: &str = "gate";
/// The improver service.
pub const IMPROVER: &str = "improver";
/// The arms an evaluation compares.
pub const CURRENT: &str = "current";
pub const CANDIDATE: &str = "candidate";
/// What the improver may spend on one proposal, fixed here rather than in
/// anything it could reach: model calls, and tokens of output.
const IMPROVER_BUDGET: Budget = Budget::new(64_000, 0, 4);
/// How long one proposal may take.
const PROPOSE_MS: u64 = 20 * 60 * 1000;
/// The most failed runs of one pattern shown to the improver.
const CASES: usize = 8;

/// `[improve]`: the gate's thresholds and how it measures. The user's
/// configuration only; nothing the improver can reach.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gate {
    /// Tasks, on either set, the candidate may pass in fewer trials than
    /// the live version and still be promoted.
    pub max_regressions: u32,
    /// Runs, over both sets, the candidate must pass beyond the live version.
    pub min_gain: u32,
    /// Runs of the held-out set the candidate must pass beyond the live version.
    pub min_heldout_gain: u32,
    /// The most the candidate's runs may cost, as a multiple of the live version's.
    pub max_cost_ratio: f64,
    /// Runs of each version on each task.
    pub trials: u32,
    /// `molt do` options for every run of both versions, such as `["--attempts", "1"]`.
    pub options: Vec<String>,
    /// The improver's model; default: the gateway's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            max_regressions: 0,
            min_gain: 1,
            min_heldout_gain: 0,
            max_cost_ratio: 1.5,
            trials: 1,
            options: Vec::new(),
            model: None,
        }
    }
}

impl Gate {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.min_gain >= 1, "min_gain must be at least 1: a candidate that changes nothing is not promoted");
        ensure!(self.max_cost_ratio.is_finite() && self.max_cost_ratio > 0.0, "max_cost_ratio must be positive");
        ensure!((1..=100).contains(&self.trials), "trials must be 1 to 100");
        // The options are those of an arm, which may not set what the harness does.
        Arm::parse(&format!("x={}", self.options.join(" "))).context("options")?;
        ensure!(
            self.options.iter().all(|o| !o.chars().any(char::is_whitespace)),
            "options: give each option and value as its own string"
        );
        Ok(())
    }
}

/// The improvement directory: `MOLT_IMPROVE_DIR`, else `molt/improve` under
/// `$XDG_DATA_HOME` or `~/.local/share`. `None` when it is `none`, or there is no home.
pub fn dir(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match var(DIR_ENV).filter(|d| !d.trim().is_empty()) {
        Some(d) if d == "none" => None,
        Some(d) => Some(PathBuf::from(d)),
        None => {
            let data = var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|d| d.is_absolute())
                .or_else(|| var("HOME").filter(|h| !h.is_empty()).map(|h| Path::new(&h).join(".local/share")))?;
            Some(data.join("molt/improve"))
        }
    }
}

/// The configuration `molt improve run` takes its model gateway and its
/// `[improve]` thresholds from: the `--config` file when one is given,
/// else the user's configuration over the built-in setup. The improvement
/// directory is created, private, if it is missing.
pub fn base_config(dir: &Path, config: Option<&Path>, profile: Option<&str>) -> anyhow::Result<Config> {
    let mut dirs = std::fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(dirs.recursive(true), 0o700);
    dirs.create(dir).with_context(|| format!("creating {}", dir.display()))?;
    match config {
        Some(path) => Ok(crate::layers::Layered::file(path, profile)?.config),
        // Nothing of a project's is read: the improvement directory stands in for the workspace.
        None => Ok(crate::agent::layered(dir, dir, None, profile)?.config),
    }
}

/// The registry's module name for prompt `name`.
pub fn module_name(prompt: &str) -> String {
    format!("{MODULE_PREFIX}{prompt}")
}

/// The promoted prompts the planner of a run is to speak with: the live
/// prompt modules of the improvement registry, read without a kernel. Only
/// mutable-tier versions of built-in prompts that are not protected; none
/// when there is no registry yet.
pub fn live_prompts(var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Vec<PromptModule>> {
    let Some(dir) = dir(var) else { return Ok(Vec::new()) };
    let live = molt_kernel::registry::live_modules_in(&dir.join("registry"))
        .with_context(|| format!("reading the prompt registry in {}", dir.display()))?;
    Ok(live
        .into_iter()
        .filter(|(_, m)| m.tier == Tier::Mutable)
        .filter_map(|(version, m)| {
            let name = m.name.strip_prefix(MODULE_PREFIX)?;
            (pack::builtin(name).is_some() && !pack::protected(name)).then(|| PromptModule {
                name: name.to_owned(),
                version: version.0,
                text: m.text,
            })
        })
        .collect())
}

/// The prompt `name` may be improved: a built-in that does not guard acceptance.
pub fn improvable(name: &str) -> anyhow::Result<()> {
    let names: Vec<&str> = pack::BUILTIN.iter().map(|(n, _)| *n).filter(|n| !pack::protected(n)).collect();
    ensure!(
        pack::builtin(name).is_some(),
        "there is no built-in prompt {name:?}; the improver works on {}",
        names.join(", ")
    );
    ensure!(
        !pack::protected(name),
        "{name} guards what results are accepted, so the improver cannot change it; only you can, with `molt prompts \
         promote`"
    );
    Ok(())
}

/// What `molt improve run` is given on its command line.
pub struct RunOptions {
    pub prompt: String,
    /// The improvement directory, which holds the registry and the audit log.
    pub dir: PathBuf,
    pub tasks: PathBuf,
    pub max_usd: f64,
    pub task_usd: f64,
    pub timeout_s: u64,
    pub jobs: u32,
    pub scratch: Option<PathBuf>,
    pub no_sandbox: bool,
    /// The model of every run, as `molt bench --model`.
    pub model: Option<String>,
}

/// How `molt improve run` ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The live version failed no task of the regression set.
    NothingToImprove,
    Promoted {
        version: VersionId,
    },
    NotPromoted {
        version: VersionId,
    },
}

/// One candidate's whole cycle: measure the live version on the regression
/// set, have the improver propose a text from its failures, measure both on
/// the regression and the held-out sets, decide, and promote when the
/// candidate passes the gate. The decision and the promotion are in the
/// audit log of the improvement directory.
pub async fn run(opts: &RunOptions, gate: &Gate, base: &Config, out: &mut dyn std::io::Write) -> anyhow::Result<Ended> {
    improvable(&opts.prompt)?;
    gate.validate()?;
    if let Some(dir) = crate::prompts::dir(&|n| std::env::var(n).ok()) {
        ensure!(
            !dir.join(format!("{}.md", opts.prompt)).exists(),
            "your prompt pack's {}.md replaces the prompt, so a promoted version would never be used: the improver \
             works only on prompts your pack leaves to molt",
            opts.prompt
        );
    }
    let tasks = Select::default().load(&opts.tasks)?;
    let (dev, heldout): (Vec<Task>, Vec<Task>) = tasks.into_iter().partition(|t| t.spec.split == Split::Dev);
    ensure!(!dev.is_empty(), "{} has no task of split dev, the regression set", opts.tasks.display());
    ensure!(
        !heldout.is_empty(),
        "{} has no task of split heldout: the gate needs a held-out set",
        opts.tasks.display()
    );

    let cfg = improve_config(base, &opts.dir)?;
    let running = crate::start(&cfg).await?;
    let result = cycle(opts, gate, &running, dev, heldout, out).await;
    running.shutdown().await;
    result
}

/// The kernel of the improvement directory: the model gateway of `base`,
/// and the improver in the sandbox with only `model.complete`.
fn improve_config(base: &Config, dir: &Path) -> anyhow::Result<Config> {
    let model = base.service("model").context("the configuration has no model service")?.clone();
    let mut cfg = Config::default();
    cfg.kernel.data_dir = dir.to_path_buf();
    cfg.services = vec![model, improver_service(&crate::agent::bin_dir()?)?];
    Ok(cfg)
}

/// The improver's section: a candidate in the sandbox whose one capability
/// is `model.complete` with [`IMPROVER_BUDGET`], and no settings of its own.
pub fn improver_service(bin_dir: &Path) -> anyhow::Result<ServiceSection> {
    let exe = bin_dir.join("molt-improver");
    let mut svc: ServiceSection = toml::from_str(&format!("name = \"{IMPROVER}\""))?;
    svc.tier = Tier::Mutable;
    svc.isolation = Isolation::Sandbox;
    svc.exec =
        Some(Exec { command: exe.to_str().context("the molt-improver path is not UTF-8")?.into(), args: vec![] });
    svc.provides = vec![format!("{IMPROVER}.*").parse()?];
    svc.requests = vec![CapRequest { target: molt_api::model::COMPLETE.parse()?, budget: IMPROVER_BUDGET }];
    svc.memory_mb = Some(512);
    svc.max_restarts = 2;
    Ok(svc)
}

async fn cycle(
    opts: &RunOptions,
    gate: &Gate,
    running: &crate::Running,
    dev: Vec<Task>,
    heldout: Vec<Task>,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<Ended> {
    let kernel = running.kernel();
    let module = module_name(&opts.prompt);
    let live = kernel.registry().live_module(&module);
    let current = match &live {
        Some(v) => kernel.registry().module(v).context("the live version is missing from the registry")?.text,
        None => pack::builtin(&opts.prompt).expect("checked").to_owned(),
    };
    let short = |v: &Option<VersionId>| v.as_ref().map_or("the built-in".to_owned(), |v| short_id(v).to_owned());
    writeln!(out, "{}: the live version is {}", opts.prompt, short(&live))?;

    let eval = opts.dir.join("evaluations").join(format!("{}-{}", run::now_ms(), std::process::id()));
    std::fs::create_dir_all(&eval).with_context(|| format!("creating {}", eval.display()))?;
    let results = eval.join("results.jsonl");
    let mut spent = 0.0;

    // The live version on the regression set: the failures the improver learns from.
    let arms = Arms::new(&eval, &opts.prompt, &current, None)?;
    let plan = Plan { tasks: dev.clone(), arms: vec![arm(CURRENT, gate)], trials: gate.trials };
    spent += evaluate(opts, gate, &plan, &results, &arms, opts.max_usd).await?;
    let records = molt_bench::record::load(&results)?;
    let patterns = patterns(&records, &dev, &eval.join("results.logs"));
    if patterns.is_empty() {
        writeln!(out, "{}: the live version passes every task of the regression set; nothing to improve", opts.prompt)?;
        return Ok(Ended::NothingToImprove);
    }
    let failed: usize = patterns.iter().map(|p| p.cases.len()).sum();
    writeln!(out, "{}: {failed} failed runs on the regression set, in {} patterns", opts.prompt, patterns.len())?;

    let ask =
        ProposeRequest { prompt: opts.prompt.clone(), current: current.clone(), patterns, model: gate.model.clone() };
    let proposal = propose(running, &ask).await?;
    let text = pack::normalize(&proposal.text).to_owned();
    ensure!(text != current, "the improver proposed the text that is live now");
    let candidate = kernel
        .store_module(
            Module { name: module.clone(), tier: Tier::Mutable, parent: live.clone(), text: text.clone() },
            IMPROVER,
        )
        .await?;
    writeln!(out, "{}: the improver proposed {}: {}", opts.prompt, short_id(&candidate), proposal.rationale.trim())?;

    // Both versions on both sets, the runs of the live version above kept.
    let arms = Arms::new(&eval, &opts.prompt, &current, Some(&text))?;
    let mut tasks = dev.clone();
    tasks.extend(heldout.iter().cloned());
    let plan = Plan { tasks, arms: vec![arm(CURRENT, gate), arm(CANDIDATE, gate)], trials: gate.trials };
    spent += evaluate(opts, gate, &plan, &results, &arms, opts.max_usd - spent).await?;
    let records = molt_bench::record::load(&results)?;

    let verdict = decide(gate, &records, &dev, &heldout);
    let mut evidence = verdict.evidence.clone();
    evidence["results"] = json!(results.display().to_string());
    evidence["results_sha256"] = json!(sha256_file(&results)?);
    evidence["spent_usd"] = json!(spent);
    evidence["rationale"] = json!(proposal.rationale);
    kernel
        .record_decision(Decision {
            module: module.clone(),
            candidate: candidate.clone(),
            baseline: live.clone(),
            by: Authority::Gate(ServiceId::new(GATE)?).to_string(),
            promote: verdict.promote,
            reasons: verdict.reasons.clone(),
            evidence,
        })
        .await?;
    for reason in &verdict.reasons {
        writeln!(out, "  {reason}")?;
    }
    if !verdict.promote {
        writeln!(out, "{}: {} is not promoted; {} stays live", opts.prompt, short_id(&candidate), short(&live))?;
        return Ok(Ended::NotPromoted { version: candidate });
    }
    let gate_id = Authority::Gate(ServiceId::new(GATE)?);
    kernel.promote_module(&module, live.as_ref(), Some(&candidate), &gate_id).await?;
    writeln!(
        out,
        "{}: promoted {}; runs from now on use it. `molt improve rollback --prompt {}` returns to {}",
        opts.prompt,
        short_id(&candidate),
        opts.prompt,
        short(&live)
    )?;
    Ok(Ended::Promoted { version: candidate })
}

fn arm(name: &str, gate: &Gate) -> Arm {
    Arm { name: name.into(), args: gate.options.clone() }
}

fn short_id(v: &VersionId) -> &str {
    let hex = v.0.strip_prefix("sha256:").unwrap_or(&v.0);
    &hex[..hex.len().min(12)]
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(std::fs::read(path)?)))
}

/// Ask the improver for a text, and take nothing from its answer but the
/// text and the rationale.
async fn propose(running: &crate::Running, ask: &ProposeRequest) -> anyhow::Result<ProposeResponse> {
    let kernel = running.kernel();
    let cli = crate::agent::join_bus(running).await?;
    crate::agent::wait_until_up(kernel, &cli, IMPROVER, Instant::now() + crate::agent::STARTUP).await?;
    crate::agent::grant(kernel, &cli, PROPOSE, Budget::new(0, 0, 1)).await?;
    let opts = CallOpts { cap: None, budget: Budget::new(0, PROPOSE_MS, 0), trace: None };
    let reply = cli.call(PROPOSE, serde_json::to_value(ask)?, opts).await.context("the improver failed")?;
    let proposal: ProposeResponse =
        serde_json::from_value(reply).context("the improver's answer is not a text and a rationale alone")?;
    ensure!(
        !proposal.text.trim().is_empty() && proposal.text.len() <= molt_api::improve::MAX_TEXT,
        "the improver's text is empty or larger than 64 KiB"
    );
    Ok(proposal)
}

/// Run `plan` with `molt bench`'s harness: each run in a fresh workspace,
/// home and data dir, graded by the task's hidden tests, with each arm's
/// prompt pack. Returns what it spent.
async fn evaluate(
    opts: &RunOptions,
    gate: &Gate,
    plan: &Plan,
    results: &Path,
    arms: &Arms,
    max_usd: f64,
) -> anyhow::Result<f64> {
    ensure!(max_usd > 0.0, "the evaluation reached --max-usd before it finished; nothing is promoted");
    if let Some(scratch) = &opts.scratch {
        std::fs::create_dir_all(scratch).with_context(|| format!("creating {}", scratch.display()))?;
    }
    let args = molt_bench::cli::RunArgs {
        tasks: molt_bench::cli::TaskArgs { tasks: opts.tasks.clone(), ids: Vec::new(), split: None, language: None },
        arms: plan.arms.clone(),
        trials: gate.trials,
        results: results.to_path_buf(),
        logs: None,
        max_usd: Some(max_usd),
        task_usd: opts.task_usd,
        timeout_s: opts.timeout_s,
        jobs: opts.jobs,
        model: opts.model.clone(),
        effort: None,
        max_turns: None,
        env: Vec::new(),
        scratch: opts.scratch.clone(),
        keep_audit: true,
        no_sandbox: opts.no_sandbox,
        retry_errors: false,
        dry_run: false,
        molt: None,
    };
    let prepared = molt_bench::cli::prepare(&args, plan).await?;
    prepared.write_policy()?;
    let agent = Arc::new(arms.with(&prepared.molt));
    let summary = run::run(plan, &prepared.settings, results, agent, CancellationToken::new(), |e| {
        if let run::Event::Finished { record: r, n, of, .. } = e {
            let how = if r.error.is_some() {
                "error"
            } else if r.passed {
                "passed"
            } else {
                "failed"
            };
            eprintln!("[{n}/{of}] {} · {} · trial {}: {how}", r.task, r.arm, r.trial + 1);
        }
    })
    .await?;
    if let Some(stop) = summary.stop {
        bail!("the evaluation stopped before it finished ({stop:?}); nothing is promoted");
    }
    Ok(summary.spent_usd)
}

/// The arms of an evaluation: `molt do` with a prompt pack that holds the
/// version under test, one directory per arm, outside any workspace.
struct Arms {
    packs: Vec<(String, PathBuf)>,
    molt: Option<BTreeMap<String, Molt>>,
}

impl Arms {
    fn new(eval: &Path, prompt: &str, current: &str, candidate: Option<&str>) -> anyhow::Result<Self> {
        let mut packs = Vec::new();
        for (arm, text) in [(CURRENT, Some(current)), (CANDIDATE, candidate)] {
            let Some(text) = text else { continue };
            let dir = eval.join("packs").join(arm);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(format!("{prompt}.md")), text)?;
            packs.push((arm.to_owned(), dir));
        }
        Ok(Self { packs, molt: None })
    }

    fn with(&self, base: &Molt) -> Arms {
        let molt = self
            .packs
            .iter()
            .map(|(arm, dir)| {
                let mut env = base.env.clone();
                env.push((pack::PROMPTS_ENV.into(), dir.display().to_string()));
                let molt =
                    Molt { exe: base.exe.clone(), options: base.options.clone(), env, keep_audit: base.keep_audit };
                (arm.clone(), molt)
            })
            .collect();
        Arms { packs: self.packs.clone(), molt: Some(molt) }
    }
}

#[async_trait]
impl Agent for Arms {
    async fn run(&self, job: Job<'_>) -> AgentRun {
        match self.molt.as_ref().and_then(|m| m.get(&job.arm.name)) {
            Some(molt) => molt.run(job).await,
            None => {
                let mut run = AgentRun::default();
                run.set_error(molt_bench::agent::ErrorKind::Harness, format!("no arm {}", job.arm.name));
                run
            }
        }
    }
}

/// The failures of the live version on the regression set, grouped by how
/// they ended, most common first. Runs that ended in an error (an API
/// outage, a harness failure) say nothing about the prompt and are left out.
pub fn patterns(records: &[Record], dev: &[Task], logs: &Path) -> Vec<Pattern> {
    let tickets: BTreeMap<&str, &str> = dev.iter().map(|t| (t.id.as_str(), t.spec.prompt.as_str())).collect();
    let mut groups: BTreeMap<&'static str, Vec<Case>> = BTreeMap::new();
    for r in records.iter().filter(|r| r.arm == CURRENT && !r.passed && r.error.is_none()) {
        // Never a held-out task: only the regression set's tickets are known here.
        let Some(ticket) = tickets.get(r.task.as_str()) else { continue };
        let what = if r.timed_out {
            "ran out of time"
        } else if r.claimed_done {
            "said it was done, but the hidden tests failed"
        } else {
            "did not finish with a result its check passed"
        };
        let dir = logs.join(&r.task).join(format!("{}-{}", r.arm, r.trial));
        groups.entry(what).or_default().push(Case {
            task: r.task.clone(),
            ticket: (*ticket).to_owned(),
            evidence: evidence(r, &dir),
        });
    }
    let mut patterns: Vec<Pattern> = groups
        .into_iter()
        .map(|(what, mut cases)| {
            cases.truncate(CASES);
            Pattern { what: what.to_owned(), cases }
        })
        .collect();
    patterns.sort_by_key(|p| std::cmp::Reverse(p.cases.len()));
    patterns
}

/// A failed run's changes, the agent's last words from its audit log, and
/// the hidden tests' output.
fn evidence(r: &Record, logs: &Path) -> String {
    let mut out = String::new();
    if let Ok(diff) = std::fs::read_to_string(logs.join("changes.diff")) {
        out.push_str(&format!("<changes>\n{}\n</changes>\n", head(&diff, 3000)));
    }
    if let Some(words) = last_words(&logs.join("audit.jsonl")) {
        out.push_str(&format!("<last_words>\n{}\n</last_words>\n", head(&words, 1500)));
    }
    let outcome = r.outcome.map(|o| format!("{o:?}").to_lowercase()).unwrap_or_else(|| "none".into());
    out.push_str(&format!("<outcome>{outcome}</outcome>\n"));
    let grade = r.grade.output.trim();
    let tail: String = grade.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
    out.push_str(&format!(
        "<hidden_tests exit=\"{}\">\n{tail}\n</hidden_tests>",
        r.grade.exit_code.map_or("none".into(), |c| c.to_string())
    ));
    out
}

fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// The text of the last model reply in an audit log.
fn last_words(audit: &Path) -> Option<String> {
    let text = std::fs::read_to_string(audit).ok()?;
    let mut last = None;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let envelope = &entry["event"]["envelope"];
        if entry["event"]["type"] != "message" || envelope["kind"] != "reply" || envelope["from"] != "model" {
            continue;
        }
        let words: Vec<&str> = envelope["payload"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        if !words.is_empty() {
            last = Some(words.join("\n"));
        }
    }
    last
}

/// What the gate decided and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub promote: bool,
    /// One line per rule, `ok:` or `failed:`.
    pub reasons: Vec<String>,
    pub evidence: Value,
}

/// One set's runs, per version.
#[derive(Clone, Debug, Default, Serialize)]
struct Tally {
    tasks: usize,
    current_passed: u32,
    candidate_passed: u32,
    current_cost_usd: f64,
    candidate_cost_usd: f64,
    /// Tasks the candidate passed in fewer trials than the live version.
    regressions: Vec<String>,
    /// Tasks the candidate passed in more trials.
    fixed: Vec<String>,
}

/// The gate: compare the two versions' runs on the regression set (`dev`)
/// and the held-out set under `gate`'s thresholds. Every run must be there
/// and none may have ended in an error or with an unknown cost: evidence
/// that is not complete never passes.
pub fn decide(gate: &Gate, records: &[Record], dev: &[Task], heldout: &[Task]) -> Verdict {
    let mut reasons = Vec::new();
    let mut ok = true;
    let mut check = |held: bool, line: String| {
        ok &= held;
        reasons.push(format!("{}: {line}", if held { "ok" } else { "failed" }));
    };
    let tally = |tasks: &[Task]| -> (Tally, Vec<String>) {
        let mut t = Tally { tasks: tasks.len(), ..Tally::default() };
        let mut missing = Vec::new();
        for task in tasks {
            let mut passed = [0u32; 2];
            for (i, arm) in [CURRENT, CANDIDATE].into_iter().enumerate() {
                for trial in 0..gate.trials {
                    // A later record of the same run replaces an earlier one.
                    let r = records
                        .iter()
                        .rev()
                        .find(|r| r.task == task.id && r.arm == arm && r.trial == trial && r.task_hash == task.hash);
                    match r {
                        None => missing.push(format!("{} · {arm} · trial {} has no result", task.id, trial + 1)),
                        Some(r) if r.error.is_some() => missing.push(format!(
                            "{} · {arm} · trial {} ended in an error: {}",
                            task.id,
                            trial + 1,
                            r.error.as_deref().unwrap_or_default()
                        )),
                        Some(r) => {
                            passed[i] += u32::from(r.passed);
                            if i == 0 {
                                t.current_cost_usd += r.cost_usd;
                            } else {
                                t.candidate_cost_usd += r.cost_usd;
                            }
                        }
                    }
                }
            }
            t.current_passed += passed[0];
            t.candidate_passed += passed[1];
            if passed[1] < passed[0] {
                t.regressions.push(task.id.clone());
            } else if passed[1] > passed[0] {
                t.fixed.push(task.id.clone());
            }
        }
        (t, missing)
    };
    let (reg, mut missing) = tally(dev);
    let (held, more) = tally(heldout);
    missing.extend(more);
    let runs = |t: &Tally| t.tasks as u32 * gate.trials;

    check(
        missing.is_empty(),
        if missing.is_empty() {
            "every run of both versions has a result".to_owned()
        } else {
            format!("the evidence is incomplete: {}", missing.join("; "))
        },
    );
    let regressions: Vec<&String> = reg.regressions.iter().chain(&held.regressions).collect();
    check(
        regressions.len() as u32 <= gate.max_regressions,
        format!(
            "{} regressions (tasks it passes less often: {}), at most {} allowed",
            regressions.len(),
            if regressions.is_empty() { "none".into() } else { join(&regressions) },
            gate.max_regressions
        ),
    );
    check(
        reg.candidate_passed >= reg.current_passed,
        format!(
            "regression set: {}/{} runs passed, the live version {}/{}",
            reg.candidate_passed,
            runs(&reg),
            reg.current_passed,
            runs(&reg)
        ),
    );
    check(
        held.candidate_passed >= held.current_passed + gate.min_heldout_gain,
        format!(
            "held-out set: {}/{} runs passed, the live version {}/{}, at least {} more needed",
            held.candidate_passed,
            runs(&held),
            held.current_passed,
            runs(&held),
            gate.min_heldout_gain
        ),
    );
    let (now, then) = (reg.candidate_passed + held.candidate_passed, reg.current_passed + held.current_passed);
    check(
        now >= then + gate.min_gain,
        format!("{now} runs passed in all, the live version {then}, at least {} more needed", gate.min_gain),
    );
    let (cost, base) = (reg.candidate_cost_usd + held.candidate_cost_usd, reg.current_cost_usd + held.current_cost_usd);
    check(
        cost <= base * gate.max_cost_ratio + 1e-9,
        format!("it cost ${cost:.4}, the live version ${base:.4}, at most {}× allowed", gate.max_cost_ratio),
    );
    let evidence = json!({
        "thresholds": gate,
        "regression_set": reg,
        "heldout_set": held,
    });
    Verdict { promote: ok, reasons, evidence }
}

fn join(items: &[&String]) -> String {
    items.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

/// `molt improve rollback`: in one step, make the live version's parent
/// live again (the built-in, for the first promoted version), as a person.
pub async fn rollback(dir: &Path, prompt: &str, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    improvable(prompt)?;
    let module = module_name(prompt);
    let mut cfg = Config::default();
    cfg.kernel.data_dir = dir.to_path_buf();
    let running = crate::start(&cfg).await?;
    let result = async {
        let kernel = running.kernel();
        let live = kernel
            .registry()
            .live_module(&module)
            .with_context(|| format!("{prompt} has no promoted version: runs already use the built-in prompt"))?;
        let parent = kernel.registry().module(&live).context("the live version is missing from the registry")?.parent;
        kernel.promote_module(&module, Some(&live), parent.as_ref(), &Authority::Human).await?;
        let to = parent.as_ref().map_or("the built-in prompt".to_owned(), |p| short_id(p).to_owned());
        writeln!(out, "{prompt}: rolled back from {} to {to}", short_id(&live))?;
        anyhow::Ok(())
    }
    .await;
    running.shutdown().await;
    result
}

/// `molt improve status`: the live version of each prompt the registry has
/// one for, and the gate's decisions in the audit log, newest last.
pub async fn status(dir: &Path, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    let live = molt_kernel::registry::live_modules_in(&dir.join("registry"))?;
    if live.is_empty() {
        writeln!(out, "no promoted prompt: runs use the built-in prompts")?;
    }
    for (version, m) in &live {
        let parent = m.parent.as_ref().map_or("the built-in".to_owned(), |p| short_id(p).to_owned());
        writeln!(out, "{}: {} is live, derived from {parent}", m.name, short_id(version))?;
    }
    let audit = dir.join("audit.jsonl");
    if audit.exists() {
        for e in molt_kernel::audit::read_all(&audit).await? {
            if let molt_kernel::audit::AuditEvent::GateDecision(d) = e.event {
                let how = if d.promote { "promoted" } else { "not promoted" };
                writeln!(out, "decision #{}: {} {} {how}", e.seq, d.module, short_id(&d.candidate))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_bench::grade::Grade;

    fn task(dir: &Path, id: &str, split: &str) -> Task {
        let t = dir.join(id);
        std::fs::create_dir_all(t.join("repo")).unwrap();
        std::fs::create_dir_all(t.join("hidden")).unwrap();
        std::fs::write(t.join("repo/README.md"), "# t\n").unwrap();
        std::fs::write(t.join("hidden/test_bench_hidden.sh"), "true\n").unwrap();
        std::fs::create_dir_all(t.join("solution")).unwrap();
        std::fs::write(t.join("solution/x"), "x\n").unwrap();
        std::fs::write(
            t.join("task.toml"),
            format!(
                "title = \"t\"\nlanguage = \"python\"\nkind = \"feature\"\ndifficulty = \"easy\"\nsplit = \"{split}\"\n\
                 tests = \"true\"\ncheck = \"true\"\nprompt = \"Do {id}.\"\n"
            ),
        )
        .unwrap();
        Task::load(&t).unwrap()
    }

    fn record(task: &Task, arm: &str, trial: u32, passed: bool) -> Record {
        let line = json!({
            "format": molt_bench::record::FORMAT, "run": "r", "started_ms": 0, "task": task.id, "task_hash": task.hash,
            "language": "python", "kind": "feature", "difficulty": "easy", "split": task.spec.split,
            "arm": arm, "arm_args": [], "trial": trial,
            "setup": { "molt_build": "b", "model": null, "effort": null, "max_turns": null, "task_usd": 1.0,
                       "timeout_s": 1, "env": [] },
            "molt_version": "0", "git_commit": null, "passed": passed, "claimed_done": true, "wall_ms": 1,
            "cost_usd": 0.01, "reported_cost_usd": 0.01, "usage": {}, "model_calls": 2, "turns": 2, "attempts": 1,
            "uncounted_calls": 0, "exit_code": 0, "signal": null, "timed_out": false, "outcome": "unverified",
            "applied": true, "error": null, "grade": Grade::ungraded(String::new()),
        });
        serde_json::from_value(line).unwrap()
    }

    struct Sets {
        _dir: tempfile::TempDir,
        dev: Vec<Task>,
        heldout: Vec<Task>,
    }

    fn sets() -> Sets {
        let dir = tempfile::tempdir().unwrap();
        let dev = vec![task(dir.path(), "py-a", "dev"), task(dir.path(), "py-b", "dev")];
        let heldout = vec![task(dir.path(), "py-c", "heldout")];
        Sets { _dir: dir, dev, heldout }
    }

    /// Records for (task, current passed, candidate passed).
    fn runs(s: &Sets, outcomes: &[(usize, bool, bool)]) -> Vec<Record> {
        let all: Vec<&Task> = s.dev.iter().chain(&s.heldout).collect();
        outcomes
            .iter()
            .flat_map(|&(i, cur, cand)| [record(all[i], CURRENT, 0, cur), record(all[i], CANDIDATE, 0, cand)])
            .collect()
    }

    #[test]
    fn a_candidate_that_fixes_runs_and_breaks_none_passes_the_gate() {
        let s = sets();
        let v = decide(
            &Gate::default(),
            &runs(&s, &[(0, false, true), (1, true, true), (2, false, true)]),
            &s.dev,
            &s.heldout,
        );
        assert!(v.promote, "{:#?}", v.reasons);
        assert!(v.reasons.iter().all(|r| r.starts_with("ok:")));
        assert_eq!(v.evidence["heldout_set"]["fixed"], json!(["py-c"]));
    }

    #[test]
    fn a_candidate_that_regresses_on_either_set_or_gains_nothing_is_not_promoted() {
        let s = sets();
        let gate = Gate::default();
        // Fixes the regression set and breaks the held-out task.
        let v = decide(&gate, &runs(&s, &[(0, false, true), (1, false, true), (2, true, false)]), &s.dev, &s.heldout);
        assert!(!v.promote);
        assert!(
            v.reasons.iter().any(|r| r.starts_with("failed: 1 regressions (tasks it passes less often: py-c)")),
            "{:#?}",
            v.reasons
        );
        assert!(v.reasons.iter().any(|r| r.starts_with("failed: held-out set")), "{:#?}", v.reasons);
        // The same as the live version.
        let v = decide(&gate, &runs(&s, &[(0, true, true), (1, false, false), (2, true, true)]), &s.dev, &s.heldout);
        assert!(
            !v.promote && v.reasons.iter().any(|r| r.contains("at least 1 more needed") && r.starts_with("failed"))
        );
        // A stricter held-out threshold of the user's.
        let strict = Gate { min_heldout_gain: 1, ..Gate::default() };
        let v = decide(&strict, &runs(&s, &[(0, false, true), (1, true, true), (2, true, true)]), &s.dev, &s.heldout);
        assert!(!v.promote, "{:#?}", v.reasons);
    }

    #[test]
    fn missing_runs_errors_and_unknown_costs_never_pass() {
        let s = sets();
        let gate = Gate::default();
        let mut records = runs(&s, &[(0, false, true), (1, false, true), (2, false, true)]);
        records.pop();
        let v = decide(&gate, &records, &s.dev, &s.heldout);
        assert!(!v.promote && v.reasons[0].contains("py-c · candidate · trial 1 has no result"), "{:#?}", v.reasons);
        let mut records = runs(&s, &[(0, false, true), (1, false, true), (2, false, true)]);
        records[3].error = Some("2 model calls had no price, so the run's cost is unknown".into());
        let v = decide(&gate, &records, &s.dev, &s.heldout);
        assert!(!v.promote && v.reasons[0].contains("had no price"), "{:#?}", v.reasons);
        // A run of an earlier version of a task does not count for this one.
        let mut records = runs(&s, &[(0, false, true), (1, false, true), (2, false, true)]);
        records[0].task_hash = "old".into();
        assert!(!decide(&gate, &records, &s.dev, &s.heldout).promote);
        // Nor does a candidate that costs much more.
        let mut records = runs(&s, &[(0, false, true), (1, false, true), (2, false, true)]);
        for r in records.iter_mut().filter(|r| r.arm == CANDIDATE) {
            r.cost_usd = 1.0;
        }
        let v = decide(&gate, &records, &s.dev, &s.heldout);
        assert!(!v.promote && v.reasons.last().unwrap().starts_with("failed: it cost"), "{:#?}", v.reasons);
    }

    #[test]
    fn the_improver_learns_only_from_the_regression_sets_failures() {
        let s = sets();
        let mut records = runs(&s, &[(0, false, false), (1, true, true), (2, false, false)]);
        records.push({
            let mut r = record(&s.dev[1], CURRENT, 1, false);
            r.error = Some("the API was down".into());
            r
        });
        let found = patterns(&records, &s.dev, Path::new("/nonexistent"));
        let cases: Vec<&str> = found.iter().flat_map(|p| &p.cases).map(|c| c.task.as_str()).collect();
        assert_eq!(cases, ["py-a"], "not the held-out py-c, the passing run or the one that ended in an error");
        assert_eq!(found[0].what, "said it was done, but the hidden tests failed");
        assert_eq!(found[0].cases[0].ticket, "Do py-a.");
    }

    #[test]
    fn the_improver_runs_sandboxed_with_only_a_budgeted_model_capability() {
        let svc = improver_service(Path::new("/opt/molt/bin")).unwrap();
        assert_eq!(svc.isolation, Isolation::Sandbox);
        assert_eq!(svc.tier, Tier::Mutable);
        assert_eq!(svc.requests, [CapRequest { target: "model.complete".parse().unwrap(), budget: IMPROVER_BUDGET }]);
        assert!(svc.env.is_empty() && svc.pass_env.is_empty());
        assert_eq!(svc.exec.as_ref().unwrap().command, "/opt/molt/bin/molt-improver");
        svc.validate_isolation().unwrap();
    }

    #[test]
    fn only_prompts_that_do_not_guard_acceptance_can_be_improved() {
        for name in ["attempt", "designer", "explorer", "router"] {
            improvable(name).unwrap();
        }
        for name in ["reviewer", "judge", "vetter"] {
            assert!(improvable(name).unwrap_err().to_string().contains("guards what results are accepted"), "{name}");
        }
        assert!(improvable("mine").is_err());
    }

    #[test]
    fn the_gates_thresholds_are_checked_and_its_options_cannot_set_the_harness() {
        assert!(Gate::default().validate().is_ok());
        assert!(Gate { min_gain: 0, ..Gate::default() }.validate().is_err());
        assert!(Gate { trials: 0, ..Gate::default() }.validate().is_err());
        let err = Gate { options: vec!["--no-sandbox".into()], ..Gate::default() }.validate().unwrap_err();
        assert!(format!("{err:#}").contains("--no-sandbox"), "{err:#}");
        assert!(Gate { options: vec!["--attempts".into(), "1".into()], ..Gate::default() }.validate().is_ok());
    }

    #[test]
    fn the_improvement_directory_is_the_users_and_can_be_turned_off() {
        let var = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
        };
        assert_eq!(dir(&var(&[("HOME", "/home/u")])), Some(PathBuf::from("/home/u/.local/share/molt/improve")));
        assert_eq!(dir(&var(&[("XDG_DATA_HOME", "/d"), ("HOME", "/h")])), Some(PathBuf::from("/d/molt/improve")));
        assert_eq!(dir(&var(&[(DIR_ENV, "none"), ("HOME", "/h")])), None);
        assert!(live_prompts(&var(&[(DIR_ENV, "/nonexistent/improve")])).unwrap().is_empty());
    }
}

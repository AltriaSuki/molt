//! Running a benchmark: every arm on every task, a number of times, each run
//! in a fresh copy of the task's repo and graded by the task's hidden tests.
//! Results are appended as runs finish, so an interrupted benchmark picks up
//! where it stopped when it is started again with the same results file.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentRun, Arm, ErrorKind, Job};
use crate::grade::{self, Grade};
use crate::record::{Record, Results, Setup, FORMAT};
use crate::task::{self, Task};

/// Runs in a row that end in an error before reaching the model, after
/// which the benchmark stops: something is wrong with every run, such as a
/// missing API key.
const FAILING_IN_A_ROW: u32 = 3;
/// The most of a run's changes kept in its logs.
const DIFF_KEPT: usize = 1024 * 1024;
/// The longest socket path molt accepts, and the room its sockets take
/// inside a data dir (`/sock/` and a service name).
const MAX_SOCKET_PATH: usize = 107;
const SOCKET_ROOM: usize = "/sock/".len() + 16 + ".sock".len();

/// What to run.
pub struct Plan {
    pub tasks: Vec<Task>,
    pub arms: Vec<Arm>,
    /// How many times each arm does each task.
    pub trials: u32,
}

/// One run of a plan: indexes into its tasks and arms, and the trial.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Slot {
    pub task: usize,
    pub arm: usize,
    pub trial: u32,
}

impl Plan {
    /// Every run, in the order to start them: trial by trial and task by
    /// task, so a benchmark stopped early has covered the tasks evenly, and
    /// within a task the arms in an order that rotates, so that no arm
    /// always goes first.
    pub fn slots(&self) -> Vec<Slot> {
        let n = self.arms.len();
        let mut slots = Vec::with_capacity(self.tasks.len() * n * self.trials as usize);
        for trial in 0..self.trials {
            for task in 0..self.tasks.len() {
                for i in 0..n {
                    slots.push(Slot { task, arm: (i + task + trial as usize) % n, trial });
                }
            }
        }
        slots
    }
}

/// How to run a plan.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Names this invocation in its records.
    pub run_id: String,
    pub setup: Setup,
    /// Start no run that could take the spending of this invocation past
    /// this, counting each run in flight at its full `setup.task_usd`.
    pub max_usd: f64,
    /// Runs at a time.
    pub jobs: usize,
    /// Where each run's workspace and data dir are made, and removed after.
    pub scratch: PathBuf,
    /// Where each run's logs are kept.
    pub logs: PathBuf,
    pub molt_version: String,
    pub git_commit: Option<String>,
    /// Run again the runs whose records say they ended in an error.
    pub retry_errors: bool,
}

/// What happened, for progress output.
#[derive(Clone, Debug)]
pub enum Event {
    /// Before anything runs: how many runs the plan has, and how many of
    /// them the results file already holds.
    Planned {
        total: usize,
        done: usize,
    },
    Started {
        task: String,
        arm: String,
        trial: u32,
    },
    /// A run finished and was recorded: the `n`th of `of` to do in this invocation.
    Finished {
        record: Box<Record>,
        n: usize,
        of: usize,
        spent_usd: f64,
    },
    /// A run stopped by an interruption; it is not recorded.
    Dropped {
        task: String,
        arm: String,
        trial: u32,
        cost_usd: f64,
    },
}

/// Why a benchmark stopped before every run was done.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// Starting another run could pass `max_usd`; this many runs were not started.
    Budget {
        left: usize,
    },
    Interrupted,
    /// Several runs in a row failed before reaching the model, or on its API.
    Failing(String),
    /// A run made model calls the gateway has no price for, so no budget
    /// can hold the benchmark back.
    CostUnknown(String),
}

/// How a benchmark went.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub total: usize,
    /// Runs the results file held already.
    pub done_before: usize,
    pub recorded: usize,
    /// What this invocation's runs cost, those not recorded included.
    pub spent_usd: f64,
    pub stop: Option<Stop>,
}

/// Run `plan`, appending a record to `results` as each run finishes. Runs
/// the file already holds are skipped. The file must have been made with
/// the same setup, the same version of each task and the same options for
/// each arm, or nothing runs.
pub async fn run(
    plan: &Plan,
    settings: &Settings,
    results: &Path,
    agent: Arc<dyn Agent>,
    cancel: CancellationToken,
    mut on: impl FnMut(Event),
) -> anyhow::Result<Summary> {
    ensure!(!plan.tasks.is_empty() && !plan.arms.is_empty() && plan.trials > 0, "there is nothing to run");
    ensure!(settings.jobs > 0, "jobs must be at least 1");
    let mut names = HashSet::new();
    for arm in &plan.arms {
        ensure!(names.insert(&arm.name), "arm {} is given twice", arm.name);
    }
    let scratch = check_scratch(&settings.scratch)?;
    let (mut file, old) = Results::open(results)?;
    let done = already_done(plan, settings, &old)?;

    let slots = plan.slots();
    let total = slots.len();
    let mut queue: VecDeque<Slot> = slots.into_iter().filter(|s| !done.contains(&key(plan, *s))).collect();
    let done_before = total - queue.len();
    let of = queue.len();
    on(Event::Planned { total, done: done_before });

    let task_usd = settings.setup.task_usd;
    let mut set: JoinSet<Finished> = JoinSet::new();
    let mut spent = 0.0;
    let mut recorded = 0;
    let mut failing = 0;
    let mut stop = None;
    loop {
        while stop.is_none() && set.len() < settings.jobs && !cancel.is_cancelled() {
            let Some(&slot) = queue.front() else { break };
            // Each run in flight may still spend up to its whole budget.
            let committed = spent + (set.len() + 1) as f64 * task_usd;
            if committed > settings.max_usd + 1e-9 {
                if set.is_empty() {
                    stop = Some(Stop::Budget { left: queue.len() });
                }
                break;
            }
            queue.pop_front();
            let (task, arm) = (plan.tasks[slot.task].clone(), plan.arms[slot.arm].clone());
            on(Event::Started { task: task.id.clone(), arm: arm.name.clone(), trial: slot.trial });
            let one = One { task, arm, trial: slot.trial, settings: settings.clone(), scratch: scratch.clone() };
            set.spawn(one.run(agent.clone(), cancel.clone()));
        }
        let Some(joined) = set.join_next().await else { break };
        let finished = match joined {
            Ok(finished) => finished,
            Err(e) => {
                cancel.cancel();
                return Err(anyhow::anyhow!("a run panicked: {e}"));
            }
        };
        spent += finished.cost_usd();
        match finished {
            Finished::Recorded(record) => {
                file.append(&record)?;
                recorded += 1;
                let broken =
                    record.error.is_some() && (record.model_calls == 0 || record.error_kind == Some(ErrorKind::Api));
                failing = if broken { failing + 1 } else { 0 };
                let why = || record.error.clone().unwrap_or_default();
                if record.unpriced_calls > 0 && stop.is_none() {
                    stop = Some(Stop::CostUnknown(why()));
                    cancel.cancel();
                } else if failing >= FAILING_IN_A_ROW && stop.is_none() {
                    stop = Some(Stop::Failing(why()));
                }
                on(Event::Finished { record, n: recorded, of, spent_usd: spent });
            }
            Finished::Dropped { task, arm, trial, cost_usd } => on(Event::Dropped { task, arm, trial, cost_usd }),
        }
    }
    if cancel.is_cancelled() && stop.is_none() {
        stop = Some(Stop::Interrupted);
    }
    Ok(Summary { total, done_before, recorded, spent_usd: spent, stop })
}

/// The key of a run in a results file: task, arm and trial.
pub type Key = (String, String, u32);

pub fn key(plan: &Plan, slot: Slot) -> Key {
    (plan.tasks[slot.task].id.clone(), plan.arms[slot.arm].name.clone(), slot.trial)
}

/// The runs `records` already holds, after checking that they were made the
/// way this plan would make them.
pub fn already_done(plan: &Plan, settings: &Settings, records: &[Record]) -> anyhow::Result<HashSet<Key>> {
    let mut done = HashSet::new();
    for r in records {
        if r.setup != settings.setup {
            bail!(
                "the results file holds runs made with another setup:\n  then: {}\n  now:  {}\n\
                 Use a new results file to compare a different setup.",
                serde_json::to_string(&r.setup)?,
                serde_json::to_string(&settings.setup)?
            );
        }
        if let Some(task) = plan.tasks.iter().find(|t| t.id == r.task) {
            if task.hash != r.task_hash {
                bail!(
                    "the results file holds runs of an earlier version of task {}; \
                     use a new results file, or leave that task out",
                    r.task
                );
            }
        }
        if let Some(arm) = plan.arms.iter().find(|a| a.name == r.arm) {
            if arm.args != r.arm_args {
                bail!(
                    "the results file holds runs of arm {} with the options {:?}, not {:?}; \
                     give this arm another name",
                    r.arm,
                    r.arm_args.join(" "),
                    arm.args.join(" ")
                );
            }
        }
        let key = (r.task.clone(), r.arm.clone(), r.trial);
        if settings.retry_errors && r.retryable() {
            // A later record of the same run replaces this one.
            done.remove(&key);
        } else {
            done.insert(key);
        }
    }
    Ok(done)
}

/// Make `scratch` if needed and check that runs can use it: molt's forks
/// must not find an enclosing git repository or Cargo workspace, and the
/// sockets of a data dir inside it must fit in a socket address.
fn check_scratch(scratch: &Path) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let scratch = scratch.canonicalize()?;
    for dir in scratch.ancestors() {
        for marker in [".git", "Cargo.toml", "go.mod", "package.json", "pyproject.toml"] {
            if dir.join(marker).exists() {
                bail!(
                    "the scratch directory {} is inside the project at {}, which tools run in a \
                     task's workspace would find; give --scratch a directory outside it",
                    scratch.display(),
                    dir.display()
                );
            }
        }
    }
    // The data dir of a run is <scratch>/mb-XXXXXX/d.
    let longest = scratch.as_os_str().len() + "/mb-XXXXXX/d".len() + SOCKET_ROOM;
    ensure!(
        longest <= MAX_SOCKET_PATH,
        "the scratch directory {} is too long a path for the sockets of a run; give --scratch a shorter one",
        scratch.display()
    );
    Ok(scratch)
}

enum Finished {
    Recorded(Box<Record>),
    Dropped { task: String, arm: String, trial: u32, cost_usd: f64 },
}

impl Finished {
    fn cost_usd(&self) -> f64 {
        match self {
            Finished::Recorded(r) => r.cost_usd,
            Finished::Dropped { cost_usd, .. } => *cost_usd,
        }
    }
}

/// One run, with what it needs to run on its own.
struct One {
    task: Task,
    arm: Arm,
    trial: u32,
    settings: Settings,
    scratch: PathBuf,
}

impl One {
    async fn run(self, agent: Arc<dyn Agent>, cancel: CancellationToken) -> Finished {
        let started_ms = now_ms();
        let logs = self.settings.logs.join(&self.task.id).join(format!("{}-{}", self.arm.name, self.trial));
        let prepared = (|| -> anyhow::Result<tempfile::TempDir> {
            if logs.exists() {
                fs::remove_dir_all(&logs)?;
            }
            fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
            let tmp = tempfile::Builder::new().prefix("mb-").tempdir_in(&self.scratch)?;
            task::copy_tree(&self.task.repo(), &tmp.path().join("w"))?;
            fs::create_dir(tmp.path().join("h"))?;
            Ok(tmp)
        })();
        let tmp = match prepared {
            Ok(tmp) => tmp,
            Err(e) => {
                let mut run = AgentRun::default();
                run.set_error(ErrorKind::Harness, format!("preparing the run: {e:#}"));
                return Finished::Recorded(Box::new(self.record(started_ms, 0, run, Grade::ungraded(String::new()))));
            }
        };
        let (workspace, data_dir, home) = (tmp.path().join("w"), tmp.path().join("d"), tmp.path().join("h"));

        let clock = Instant::now();
        let job = Job {
            task: &self.task,
            arm: &self.arm,
            workspace: &workspace,
            data_dir: &data_dir,
            home: &home,
            logs: &logs,
            budget_usd: self.settings.setup.task_usd,
            timeout: std::time::Duration::from_secs(self.settings.setup.timeout_s),
            cancel: cancel.clone(),
        };
        let run = agent.run(job).await;
        let wall_ms = clock.elapsed().as_millis() as u64;
        if run.interrupted {
            return Finished::Dropped {
                task: self.task.id.clone(),
                arm: self.arm.name.clone(),
                trial: self.trial,
                cost_usd: run.cost_usd,
            };
        }

        save_diff(&self.task.repo(), &workspace, &logs.join("changes.diff")).await;
        let mut run = run;
        let grade = match grade::grade(&self.task, &workspace).await {
            Ok(grade) => grade,
            Err(e) => {
                run.set_error(ErrorKind::Harness, format!("grading: {e:#}"));
                Grade::ungraded(format!("{e:#}"))
            }
        };
        let _ = fs::write(logs.join("grade.txt"), &grade.output);
        drop(tmp);
        Finished::Recorded(Box::new(self.record(started_ms, wall_ms, run, grade)))
    }

    fn record(&self, started_ms: u64, wall_ms: u64, run: AgentRun, grade: Grade) -> Record {
        let s = &self.task.spec;
        Record {
            format: FORMAT,
            run: self.settings.run_id.clone(),
            started_ms,
            task: self.task.id.clone(),
            task_hash: self.task.hash.clone(),
            language: s.language,
            kind: s.kind,
            difficulty: s.difficulty,
            split: s.split,
            arm: self.arm.name.clone(),
            arm_args: self.arm.args.clone(),
            trial: self.trial,
            setup: self.settings.setup.clone(),
            molt_version: self.settings.molt_version.clone(),
            git_commit: self.settings.git_commit.clone(),
            passed: grade.passed,
            claimed_done: run.claimed_done(),
            wall_ms,
            cost_usd: run.cost_usd,
            reported_cost_usd: run.reported_cost_usd,
            usage: run.usage,
            model_calls: run.model_calls,
            turns: run.turns,
            attempts: run.attempts,
            uncounted_calls: run.uncounted_calls,
            unpriced_calls: run.unpriced_calls,
            exit_code: run.exit_code,
            signal: run.signal,
            timed_out: run.timed_out,
            outcome: run.outcome,
            applied: run.applied,
            error: run.error,
            error_kind: run.error_kind,
            grade,
        }
    }
}

/// Keep what the run changed, as a unified diff of the starting repo and
/// the workspace, before the grade lays the hidden files over it. Links are
/// compared as links, not followed. Best effort: no diff is kept when
/// `diff` is missing.
async fn save_diff(repo: &Path, workspace: &Path, to: &Path) {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new("diff");
    cmd.args(["-ruN", "--no-dereference"]);
    for skipped in task::SKIPPED {
        cmd.arg(format!("--exclude={skipped}"));
    }
    cmd.arg(repo)
        .arg(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else { return };
    let Some(stdout) = child.stdout.take() else { return };
    // Read no more than is kept; the rest is not even buffered.
    let mut diff = Vec::new();
    let mut stdout = stdout.take(DIFF_KEPT as u64 + 1);
    let read = stdout.read_to_end(&mut diff);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), read).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    if diff.len() > DIFF_KEPT {
        diff.truncate(DIFF_KEPT);
        diff.extend_from_slice(b"\n[... the rest of the diff is not kept ...]\n");
    }
    let _ = fs::write(to, diff);
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use molt_api::planner::Outcome;

    use super::*;
    use crate::record;
    use crate::testing::task_dir;

    /// Does each run as its arm says: `solve` writes the answer, `skip`
    /// does nothing, `boom` fails before reaching the model, `crash` fails
    /// after reaching it, `unpriced` makes calls without a price, `wait`
    /// waits to be interrupted. Every run costs `cost`.
    struct Fake {
        cost: f64,
        started: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(cost: f64) -> Arc<Fake> {
            Arc::new(Fake { cost, started: Mutex::new(Vec::new()) })
        }
    }

    #[async_trait]
    impl Agent for Fake {
        async fn run(&self, job: Job<'_>) -> AgentRun {
            self.started.lock().unwrap().push((job.task.id.clone(), job.arm.name.clone()));
            assert!(job.workspace.join("README.md").exists(), "the run starts from a copy of the repo");
            assert!(!job.data_dir.starts_with(job.workspace));
            let mut run = AgentRun {
                exit_code: Some(0),
                outcome: Some(Outcome::Passed),
                applied: Some(true),
                cost_usd: self.cost,
                model_calls: 3,
                ..AgentRun::default()
            };
            match job.arm.name.as_str() {
                "solve" => fs::write(job.workspace.join("hello.txt"), "hi\n").unwrap(),
                "skip" => {}
                "boom" => {
                    let mut run = AgentRun { exit_code: Some(1), ..AgentRun::default() };
                    run.set_error(ErrorKind::Api, "no API key".into());
                    return run;
                }
                "crash" => run.set_error(ErrorKind::Agent, "molt panicked".into()),
                "unpriced" => {
                    run.unpriced_calls = 2;
                    run.set_error(ErrorKind::Harness, "2 model calls had no price".into());
                }
                "wait" => {
                    job.cancel.cancelled().await;
                    run.interrupted = true;
                }
                other => panic!("no fake arm {other}"),
            }
            run
        }
    }

    fn arm(name: &str) -> Arm {
        Arm { name: name.into(), args: vec![format!("--{name}")] }
    }

    struct Bench {
        root: tempfile::TempDir,
        scratch: tempfile::TempDir,
        plan: Plan,
        settings: Settings,
    }

    impl Bench {
        fn new(tasks: &[&str], arms: &[&str], trials: u32) -> Bench {
            let root = tempfile::tempdir().unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let tasks = tasks.iter().map(|id| Task::load(&task_dir(&root.path().join("tasks"), id)).unwrap()).collect();
            let settings = Settings {
                run_id: "r1".into(),
                setup: record::tests::record("x", "y", 0, true).setup,
                max_usd: 100.0,
                jobs: 1,
                scratch: scratch.path().to_path_buf(),
                logs: root.path().join("logs"),
                molt_version: "0.1.0".into(),
                git_commit: None,
                retry_errors: false,
            };
            let plan = Plan { tasks, arms: arms.iter().map(|a| arm(a)).collect(), trials };
            Bench { root, scratch, plan, settings }
        }

        fn results(&self) -> PathBuf {
            self.root.path().join("results.jsonl")
        }

        async fn run(&self, agent: Arc<dyn Agent>) -> anyhow::Result<(Summary, Vec<Event>)> {
            let mut events = Vec::new();
            let summary =
                run(&self.plan, &self.settings, &self.results(), agent, CancellationToken::new(), |e| events.push(e))
                    .await?;
            Ok((summary, events))
        }
    }

    #[test]
    fn arms_take_turns_going_first() {
        let plan = Plan { tasks: vec![], arms: vec![arm("a"), arm("b")], trials: 2 };
        assert!(plan.slots().is_empty());
        let root = tempfile::tempdir().unwrap();
        let tasks: Vec<Task> =
            ["t-a", "t-b"].iter().map(|id| Task::load(&task_dir(root.path(), id)).unwrap()).collect();
        let plan = Plan { tasks, ..plan };
        let order: Vec<(usize, usize, u32)> = plan.slots().iter().map(|s| (s.task, s.arm, s.trial)).collect();
        assert_eq!(order, [(0, 0, 0), (0, 1, 0), (1, 1, 0), (1, 0, 0), (0, 1, 1), (0, 0, 1), (1, 0, 1), (1, 1, 1)]);
    }

    #[tokio::test]
    async fn every_run_is_graded_recorded_and_logged_and_a_second_start_resumes() {
        let bench = Bench::new(&["py-a", "py-b"], &["solve", "skip"], 2);
        let fake = Fake::new(0.5);
        let (summary, events) = bench.run(fake.clone()).await.unwrap();
        assert_eq!(summary, Summary { total: 8, done_before: 0, recorded: 8, spent_usd: 4.0, stop: None });
        assert!(matches!(events[0], Event::Planned { total: 8, done: 0 }));

        let records = record::load(&bench.results()).unwrap();
        assert_eq!(records.len(), 8);
        for r in &records {
            assert_eq!(r.passed, r.arm == "solve", "{r:?}");
            assert_eq!(r.arm_args, [format!("--{}", r.arm)]);
            assert!(r.claimed_done && r.error.is_none());
            assert_eq!((r.run.as_str(), r.cost_usd, r.model_calls), ("r1", 0.5, 3));
        }
        let logs = bench.root.path().join("logs/py-a/solve-1");
        assert!(fs::read_to_string(logs.join("changes.diff")).unwrap().contains("+hi"));
        assert!(logs.join("grade.txt").exists());
        assert_eq!(fs::read_dir(bench.scratch.path()).unwrap().count(), 0, "no workspace is left behind");

        // Started again, it has nothing left to do.
        let (summary, _) = bench.run(fake.clone()).await.unwrap();
        assert_eq!((summary.done_before, summary.recorded), (8, 0));
        assert_eq!(fake.started.lock().unwrap().len(), 8);

        // With a third trial, only that trial runs.
        let mut bench = bench;
        bench.plan.trials = 3;
        let (summary, _) = bench.run(fake.clone()).await.unwrap();
        assert_eq!((summary.total, summary.done_before, summary.recorded), (12, 8, 4));
    }

    #[tokio::test]
    async fn runs_stop_before_they_could_pass_the_budget() {
        let mut bench = Bench::new(&["py-a", "py-b", "py-c"], &["solve"], 1);
        bench.settings.setup.task_usd = 2.0;
        bench.settings.max_usd = 5.0;
        let (summary, _) = bench.run(Fake::new(1.5)).await.unwrap();
        // 0 + 2 <= 5, 1.5 + 2 <= 5, but 3 + 2 <= 5 too; 4.5 + 2 > 5.
        assert_eq!(summary.recorded, 3);
        assert_eq!(summary.stop, None);

        let mut bench = Bench::new(&["py-a", "py-b", "py-c"], &["solve"], 1);
        bench.settings.setup.task_usd = 2.0;
        bench.settings.max_usd = 5.0;
        bench.settings.jobs = 3;
        let (summary, _) = bench.run(Fake::new(1.9)).await.unwrap();
        // Two runs fit in flight at their full budget; the third waits, then 3.8 + 2 > 5.
        assert_eq!(summary.recorded, 2);
        assert_eq!(summary.stop, Some(Stop::Budget { left: 1 }));
        assert!(summary.spent_usd <= 5.0);
    }

    #[tokio::test]
    async fn a_benchmark_that_cannot_reach_the_model_stops() {
        let bench = Bench::new(&["py-a", "py-b", "py-c", "py-d"], &["boom"], 1);
        let (summary, _) = bench.run(Fake::new(0.0)).await.unwrap();
        assert_eq!(summary.recorded, 3);
        assert_eq!(summary.stop, Some(Stop::Failing("no API key".into())));

        // Asked to, a later start runs them again.
        let mut bench = bench;
        bench.settings.retry_errors = true;
        let (summary, _) = bench.run(Fake::new(0.0)).await.unwrap();
        assert_eq!((summary.done_before, summary.recorded), (0, 3));
    }

    #[tokio::test]
    async fn an_agent_that_crashes_is_charged_with_it() {
        let mut bench = Bench::new(&["py-a", "py-b", "py-c", "py-d"], &["crash"], 1);
        let (summary, _) = bench.run(Fake::new(0.1)).await.unwrap();
        // It reached the model, so the benchmark goes on.
        assert_eq!((summary.recorded, summary.stop), (4, None));
        let records = record::load(&bench.results()).unwrap();
        assert!(records.iter().all(|r| r.error_kind == Some(ErrorKind::Agent) && !r.passed && !r.retryable()));

        // A crash is the agent's failure, not one to run again.
        bench.settings.retry_errors = true;
        let (summary, _) = bench.run(Fake::new(0.1)).await.unwrap();
        assert_eq!((summary.done_before, summary.recorded), (4, 0));
    }

    #[tokio::test]
    async fn a_run_of_unknown_cost_stops_the_benchmark() {
        let mut bench = Bench::new(&["py-a", "py-b", "py-c"], &["unpriced"], 1);
        bench.settings.jobs = 2;
        let (summary, _) = bench.run(Fake::new(0.0)).await.unwrap();
        assert!(matches!(summary.stop, Some(Stop::CostUnknown(_))), "{summary:?}");
        assert!(summary.recorded < 3, "{summary:?}");
    }

    #[tokio::test]
    async fn an_interrupted_run_is_not_recorded() {
        let bench = Bench::new(&["py-a", "py-b"], &["wait"], 1);
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            stopper.cancel();
        });
        let mut events = Vec::new();
        let summary = run(&bench.plan, &bench.settings, &bench.results(), Fake::new(0.25), cancel, |e| events.push(e))
            .await
            .unwrap();
        assert_eq!(summary.stop, Some(Stop::Interrupted));
        assert_eq!((summary.recorded, summary.spent_usd), (0, 0.25));
        assert!(events.iter().any(|e| matches!(e, Event::Dropped { .. })));
        assert!(record::load(&bench.results()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn results_of_another_setup_task_version_or_arm_are_refused() {
        let mut bench = Bench::new(&["py-a"], &["solve"], 1);
        bench.run(Fake::new(0.1)).await.unwrap();

        bench.settings.setup.model = Some("sonnet".into());
        let err = bench.run(Fake::new(0.1)).await.unwrap_err().to_string();
        assert!(err.contains("another setup"), "{err}");

        bench.settings.setup.model = None;
        bench.plan.arms = vec![Arm { name: "solve".into(), args: vec!["--attempts".into(), "4".into()] }];
        let err = bench.run(Fake::new(0.1)).await.unwrap_err().to_string();
        assert!(err.contains("arm solve"), "{err}");

        bench.plan.arms = vec![arm("solve")];
        bench.plan.tasks[0].hash = "changed".into();
        let err = bench.run(Fake::new(0.1)).await.unwrap_err().to_string();
        assert!(err.contains("earlier version of task py-a"), "{err}");
    }

    #[test]
    fn scratch_must_be_outside_any_project_and_short() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let err = check_scratch(&root.path().join("scratch")).unwrap_err().to_string();
        assert!(err.contains("inside the project"), "{err}");
        let short = tempfile::tempdir().unwrap();
        assert!(check_scratch(&short.path().join("scratch")).is_ok());
        let long = short.path().join("x".repeat(80));
        assert!(check_scratch(&long).unwrap_err().to_string().contains("too long"));
    }
}

//! Picking up after an interface that stopped: the saved session, and the
//! runs that stopped with a checkpoint, to resume or drop (`/runs`).
use super::*;
use crate::tui::saved;
use molt_api::planner::Resume;
use molt_planner::checkpoint;

/// A stopped run of the workspace, as its checkpoint has it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Stopped {
    pub run: String,
    /// The checkpoint, its conversations left out; or why it cannot be used.
    pub saved: Result<checkpoint::Saved, String>,
    /// Forks of attempts that would go on, and are gone.
    pub gone: Vec<String>,
}

/// Whether a task run with `workflow` keeps a checkpoint: every one that
/// changes things does. Under `auto`, the planner keeps none when it picks
/// one that only reads.
pub(super) fn checkpoints(name: Option<&str>) -> bool {
    name == Some(workflow::AUTO) || workflow::builtin(name).is_some_and(|w| !w.read_only())
}

impl App {
    /// What to save of the session now.
    pub fn snapshot(&self) -> saved::Saved {
        let active = self.task.as_ref().map(|task| saved::Active {
            run: task.trace.to_string(),
            task: scrub(&task.req.task, &self.secrets),
            phase: match task.phase {
                Phase::Designing => saved::Phase::Designing,
                Phase::Confirming => saved::Phase::Confirming,
                Phase::Running => saved::Phase::Running,
                Phase::Deciding => saved::Phase::Deciding,
                Phase::Applying => saved::Phase::Applying,
            },
            version: task.version,
            checkpointed: task.phase == Phase::Running && task.req.checkpoint,
        });
        // A run the last interface stopped in stays named until it is resumed or dropped.
        let active = active.or_else(|| self.restored.clone());
        let earlier =
            self.earlier.iter().map(|e| Earlier { task: scrub(&e.task, &self.secrets), ..e.clone() }).collect();
        let pending = self.pending_result.clone().map(|mut entry| {
            entry.task = scrub(&entry.task, &self.secrets);
            entry
        });
        saved::Saved {
            version: 0,
            earlier,
            spent_usd: self.spent_usd,
            uncounted_calls: self.uncounted_calls,
            uncounted_stops: self.uncounted_stops,
            settled_runs: self.settled_runs.clone(),
            active,
            pending,
        }
    }

    /// Pick up the session an earlier interface saved. Nothing starts: a
    /// stopped run goes on only when the user says so.
    pub fn restore_session(&mut self, saved: saved::Saved) {
        self.earlier = saved.earlier;
        self.spent_usd = saved.spent_usd;
        self.uncounted_calls = saved.uncounted_calls;
        self.uncounted_stops = saved.uncounted_stops;
        self.settled_runs = saved.settled_runs;
        let mut lines = Vec::new();
        if !self.earlier.is_empty() {
            lines.push(format!(
                "the conversation goes on: {} earlier tasks are told to the next one (/new forgets them)",
                self.earlier.len()
            ));
        }
        if let Some(entry) = saved.pending {
            if !self.kept.contains(&entry.fork) && std::path::Path::new(&entry.fork).is_dir() {
                lines.push(format!("a result that waited for you is kept in {}: /kept", entry.fork));
                self.keep_entry(entry);
            }
        }
        if let Some(active) = saved.active {
            let task = clean_line(active.task.lines().next().unwrap_or_default());
            match active.phase {
                saved::Phase::Running if active.checkpointed => {
                    lines.push(format!("molt stopped while running: {task}"));
                    lines.push(format!(
                        "/runs resume {0} goes on with it; /runs discard {0} drops it and its forks",
                        active.run
                    ));
                    self.restored = Some(active);
                }
                saved::Phase::Designing | saved::Phase::Running => {
                    // Nothing was saved to go on from, and the reply that would have said what it spent never came.
                    self.uncounted_stops = self.uncounted_stops.saturating_add(1);
                    lines.push(format!(
                        "molt stopped while {}: {task}",
                        if active.phase == saved::Phase::Designing { "designing its check" } else { "it ran" }
                    ));
                    lines.push("  what it spent is not known; Enter starts it again".into());
                    self.input = Input::with(&active.task);
                }
                saved::Phase::Confirming => {
                    lines.push(format!("molt stopped while the check of this task waited for you: {task}"));
                    lines.push("  Enter designs its check again".into());
                    self.input = Input::with(&active.task);
                }
                saved::Phase::Deciding => {}
                saved::Phase::Applying => {
                    lines.push(format!("molt stopped while it applied the result of: {task}"));
                    lines.push(
                        "  the apply finishes or is undone on its own; look at the workspace before going on".into(),
                    );
                }
            }
        }
        if !lines.is_empty() {
            self.push(Line::from(span("↻ picked up where the last interface stopped", Tone::Warn)));
            for line in lines {
                self.push(Line::from(span(format!("  {}", clean_line(&scrub(&line, &self.secrets))), Tone::Dim)));
            }
        }
    }

    /// `/runs`, `/runs resume [RUN]` and `/runs discard [RUN]`.
    pub(super) fn runs_command(&mut self, arg: &str) -> Vec<Action> {
        let (command, run) = arg.split_once(' ').map_or((arg, ""), |(c, r)| (c, r.trim()));
        match command {
            "" => {
                self.runs_asked = true;
                vec![Action::ListRuns]
            }
            "resume" => self.resume_run(run),
            "discard" => self.discard_run(run),
            _ => {
                self.flash = Some("/runs [resume RUN|discard RUN]".into());
                Vec::new()
            }
        }
    }

    /// The stopped run `run`, or the one to pick by default: the run the last
    /// interface stopped in, then the newest.
    fn stopped(&self, run: &str) -> Result<&Stopped, String> {
        let wanted = if run.is_empty() { self.restored.as_ref().map(|a| a.run.as_str()) } else { Some(run) };
        let found = match wanted {
            Some(run) => self.stopped_runs.iter().find(|s| s.run == run),
            None => self.stopped_runs.iter().find(|s| s.saved.is_ok()),
        };
        found.ok_or_else(|| match wanted {
            Some(run) => {
                format!("no stopped run {run} in this workspace (it finished, or was dropped): /runs lists them")
            }
            None => "no stopped run in this workspace".into(),
        })
    }

    fn resume_run(&mut self, run: &str) -> Vec<Action> {
        if let Some(why) = self.cannot_start() {
            self.flash = Some(why);
            return Vec::new();
        }
        let stopped = match self.stopped(run) {
            Ok(stopped) => stopped.clone(),
            Err(e) => {
                self.flash = Some(e);
                return Vec::new();
            }
        };
        let saved = match &stopped.saved {
            Ok(saved) => saved,
            Err(e) => {
                let why = format!("run {} cannot be resumed: {e}; /runs discard {} drops it", stopped.run, stopped.run);
                self.error("not resumed", &why);
                return Vec::new();
            }
        };
        if !stopped.gone.is_empty() {
            let why = format!(
                "run {} cannot go on: its attempts worked in {}, which is gone; /runs discard {} drops it",
                stopped.run,
                stopped.gone.join(", "),
                stopped.run
            );
            self.error("not resumed", &why);
            return Vec::new();
        }
        let mut req = saved.request.clone();
        req.workspace = self.workspace_path.clone();
        req.resume = Some(Resume { run: stopped.run.clone(), retry: false });
        req.checkpoint = true;
        req.interactive = true;
        req.keep_cancelled_forks = true;
        req.apply = self.mode == Mode::Auto;
        req.stream = self.settings.stream;
        let trace = TraceId::from_raw(&stopped.run);
        let (counted_usd, counted_calls) = self.settled_runs.get(&stopped.run).copied().unwrap_or_default();
        let mut task = Task::new(trace.clone(), req.clone(), Phase::Running, self.mode, self.settings.clone());
        task.version = saved.requirements.iter().map(|r| r.version).max().unwrap_or(0);
        task.check_shown = true;
        task.carried = (
            (saved.spend.cost_usd - counted_usd).max(0.0),
            saved.spend.uncounted_calls.saturating_add(saved.open_calls).saturating_sub(counted_calls),
        );
        self.gap();
        self.echo(&req.task);
        self.push(Line::from(span(
            format!("  resuming run {} · ${:.2} spent before", stopped.run, saved.spend.cost_usd),
            Tone::Dim,
        )));
        self.restored = None;
        self.last_run = Some(stopped.run.clone());
        self.task = Some(task);
        self.zoom = None;
        self.starting(trace.clone(), Action::Run { req, trace })
    }

    fn discard_run(&mut self, run: &str) -> Vec<Action> {
        if self.task.is_some() || self.fork_operations > 0 {
            self.flash = Some("wait for the current task, apply or discard".into());
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            return Vec::new();
        }
        let run = match self.stopped(run) {
            Ok(stopped) => stopped.run.clone(),
            Err(e) => {
                self.flash = Some(e);
                return Vec::new();
            }
        };
        self.fork_operations += 1;
        vec![Action::DropRun { run }]
    }

    /// Why a task cannot start now, if it cannot.
    fn cannot_start(&self) -> Option<String> {
        if self.task.is_some() {
            Some("a task is waiting; Esc stops it".into())
        } else if self.fork_operations > 0 || self.git_busy {
            Some("wait for the current apply, discard or commit".into())
        } else if !matches!(self.session, SessionState::Up { .. }) {
            Some("the services are not up".into())
        } else {
            None
        }
    }

    pub(super) fn listed_runs(&mut self, runs: Vec<Stopped>) {
        self.stopped_runs = runs;
        if let Some(run) = self.restored.as_ref().map(|a| a.run.clone()) {
            if !self.stopped_runs.iter().any(|s| s.run == run) {
                // The run finished after all, or its checkpoint is gone: its reply never came.
                let line =
                    format!("  run {run} left no checkpoint to go on from; what it spent and did is in its audit log");
                self.push(Line::from(span(clean_line(&line), Tone::Warn)));
                self.uncounted_stops = self.uncounted_stops.saturating_add(1);
                self.restored = None;
            }
        }
        let asked = std::mem::take(&mut self.runs_asked);
        if !asked {
            let restored = self.restored.as_ref().map(|a| a.run.as_str());
            let others = self.stopped_runs.iter().filter(|s| Some(s.run.as_str()) != restored).count();
            if others > 0 {
                let line = format!("  {others} stopped runs in this workspace can go on: /runs lists them");
                self.push(Line::from(span(line, Tone::Dim)));
            }
            return;
        }
        if self.stopped_runs.is_empty() {
            self.push(Line::from(span("no stopped run in this workspace", Tone::Dim)));
            return;
        }
        self.push(Line::from(span("stopped runs, newest first: /runs resume RUN · /runs discard RUN", Tone::Dim)));
        let lines: Vec<(String, Tone)> = self
            .stopped_runs
            .iter()
            .map(|s| match &s.saved {
                Ok(saved) if s.gone.is_empty() => (agent::stopped_run(saved), Tone::Plain),
                Ok(saved) => (format!("{} · forks gone: {}", agent::stopped_run(saved), s.gone.join(", ")), Tone::Warn),
                Err(e) => (format!("{}: {e}", s.run), Tone::Warn),
            })
            .collect();
        for (line, tone) in lines {
            self.push(Line::from(span(format!("  {}", clean_line(&scrub(&line, &self.secrets))), tone)));
        }
    }

    pub(super) fn run_dropped(&mut self, run: String, result: Result<(), String>) -> Vec<Action> {
        match result {
            Ok(()) => {
                // What it spent counts in the session total; no reply will say so now.
                let (counted_usd, counted_calls) = self.settled_runs.remove(&run).unwrap_or_default();
                if let Some(Ok(saved)) = self.stopped_runs.iter().find(|s| s.run == run).map(|s| &s.saved) {
                    self.spent_usd += (saved.spend.cost_usd - counted_usd).max(0.0);
                    let calls = saved.spend.uncounted_calls.saturating_add(saved.open_calls);
                    self.uncounted_calls = self.uncounted_calls.saturating_add(calls.saturating_sub(counted_calls));
                }
                self.stopped_runs.retain(|s| s.run != run);
                if self.restored.as_ref().is_some_and(|a| a.run == run) {
                    self.restored = None;
                }
                self.push(Line::from(span(format!("  dropped run {run} and its forks"), Tone::Ok)));
            }
            Err(e) => self.error(&format!("run {run} was not dropped"), &scrub(&e, &self.secrets)),
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn a_task_cut_off_by_a_crash_goes_on_from_its_checkpoint_only_when_asked_and_its_cost_is_counted_once() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.checkpoint, "a task that changes things keeps a checkpoint");
        app.update(finished(&app, Some(0), "a", Some(0.1)));
        let snapshot = app.snapshot();
        let active = snapshot.active.clone().unwrap();
        assert_eq!(
            (active.run.as_str(), active.phase, active.checkpointed),
            (trace.as_str(), saved::Phase::Running, true)
        );

        // The process is killed; a new interface picks the session up and starts nothing.
        let mut app = App::new("/w", None);
        app.restore_session(snapshot);
        assert!(log(&app).contains("molt stopped while running: implement it"), "{}", log(&app));
        up(&mut app);
        assert!(app.update(Msg::Runs(vec![stopped_run(trace.as_str(), req, 0.25, 1)])).is_empty());
        assert!(app.task.is_none());
        assert_eq!(app.snapshot().active, Some(active), "named until it is resumed or dropped");

        let actions = type_in(&mut app, "/runs resume");
        let [Action::Run { req: resumed, trace: again }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(again, trace, "it goes on under its own id");
        assert_eq!(resumed.resume, Some(Resume { run: trace.to_string(), retry: false }));
        assert!(resumed.checkpoint && resumed.check.as_deref() == Some("true"));
        assert_eq!(app.task.as_ref().unwrap().version, 1, "its requirements are the checkpoint's");
        let mut resp = passed(true, None);
        resp.cost_usd = 0.4;
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(resp)) });
        assert!((app.spent_usd - 0.4).abs() < 1e-9, "the reply counts what it spent before: {}", app.spent_usd);
        assert_eq!(app.earlier.len(), 1);
        assert!(app.snapshot().active.is_none());
    }

    #[test]
    fn finished_summaries_and_the_session_total_come_back_and_a_new_task_is_a_new_run() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "first");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(true, None))) });
        let snapshot = app.snapshot();
        assert!(snapshot.active.is_none());

        let mut app = App::new("/w", None);
        app.restore_session(snapshot);
        up(&mut app);
        assert!((app.spent_usd - 0.3).abs() < 1e-9);
        assert!(log(&app).contains("1 earlier tasks are told to the next one"), "{}", log(&app));
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "second");
        let [Action::Run { req, trace: next }] = actions.as_slice() else { panic!() };
        assert_ne!(next, trace);
        assert_eq!((req.resume.as_ref(), req.earlier.len(), req.earlier[0].task.as_str()), (None, 1, "first"));
    }

    #[test]
    fn stopping_a_checkpointed_task_drops_its_checkpoint_and_dropping_a_stopped_run_counts_what_it_spent() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, req }] = actions.as_slice() else { panic!() };
        let actions = stop(&mut app);
        let [Action::Interrupt { drop_run, .. }] = actions.as_slice() else { panic!() };
        assert_eq!(drop_run.as_deref(), Some(trace.as_str()));
        assert!(app.snapshot().active.is_none(), "stopped by the user, it is not picked up again");
        app.update(Msg::Stopped { kept: vec![], notes: vec![] });
        up(&mut app);

        let listed = || vec![stopped_run("trace_cli", req, 0.5, 2)];
        app.update(Msg::Runs(listed()));
        assert!(log(&app).contains("1 stopped runs in this workspace can go on"), "{}", log(&app));
        assert_eq!(type_in(&mut app, "/runs"), [Action::ListRuns]);
        app.update(Msg::Runs(listed()));
        assert!(log(&app).contains("  trace_cli  "), "{}", log(&app));
        assert_eq!(type_in(&mut app, "/runs discard trace_cli"), [Action::DropRun { run: "trace_cli".into() }]);
        assert!(type_in(&mut app, "/runs resume trace_cli").is_empty(), "not while it is being dropped");
        app.update(Msg::RunDropped { run: "trace_cli".into(), result: Ok(()) });
        assert!((app.spent_usd - 0.5).abs() < 1e-9, "{}", app.spent_usd);
        assert_eq!(app.uncounted_calls, 2, "calls unanswered when it stopped are not free");
        assert!(type_in(&mut app, "/runs resume trace_cli").is_empty());
        assert!(app.flash.as_deref().is_some_and(|f| f.contains("no stopped run trace_cli")), "{:?}", app.flash);
    }

    #[test]
    fn a_damaged_checkpoint_or_a_lost_fork_is_refused_with_why_and_starts_nothing() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let req = RunRequest::new("t", "/w");
        let mut lost = stopped_run("trace_lost", &req, 0.1, 0);
        lost.gone = vec!["/d/work/fork-x".into()];
        let damaged = Stopped { run: "trace_bad".into(), saved: Err("it is damaged (EOF)".into()), gone: vec![] };
        app.update(Msg::Runs(vec![lost, damaged]));
        assert!(type_in(&mut app, "/runs resume trace_bad").is_empty());
        assert!(log(&app).contains("cannot be resumed: it is damaged"), "{}", log(&app));
        assert!(type_in(&mut app, "/runs resume trace_lost").is_empty());
        assert!(log(&app).contains("/d/work/fork-x, which is gone"), "{}", log(&app));
        assert!(app.task.is_none());
    }

    #[test]
    fn what_cannot_go_on_exactly_is_offered_again_and_its_unknown_cost_is_not_free() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "/ask what is this");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(!req.checkpoint, "an answer in one go keeps no checkpoint");
        let mut snapshot = app.snapshot();
        snapshot.active.as_mut().unwrap().phase = saved::Phase::Designing;
        let tmp = tempfile::tempdir().unwrap();
        let fork = tmp.path().join("fork-1");
        std::fs::create_dir(&fork).unwrap();
        snapshot.pending = Some(crate::tui::kept::Entry::new(fork.to_str().unwrap().into(), "earlier", "passed"));

        let mut app = App::new("/w", None);
        app.restore_session(snapshot);
        assert_eq!(app.input.full(), "what is this", "Enter starts it again");
        assert_eq!(app.uncounted_stops, 1);
        assert_eq!(app.kept, [fork.to_str().unwrap()], "the result that waited is kept");
        assert!(app.snapshot().active.is_none());
    }
}

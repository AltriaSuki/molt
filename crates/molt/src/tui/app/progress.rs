//! What a running task reports, its attempts' lanes, and its result.
use super::*;

impl App {
    pub(super) fn progress(&mut self, event: Progress) {
        let Some(task) = &mut self.task else { return };
        if event.run() != task.trace.as_str() {
            return;
        }
        match event {
            Progress::Routed { workflow, reason, .. } => {
                task.req.workflow = Some(workflow.clone());
                let line = agent::routed_line(&workflow, &reason, "/verify");
                self.log.push(Line::from(vec![span("◆ ", Tone::Shell), span(clean_line(&line), Tone::Dim)]));
            }
            Progress::CheckReady { command, files, designed, rubric, .. } => {
                if task.check_shown {
                    return;
                }
                task.check_shown = true;
                let mut lines = vec![check_head(designed)];
                match command {
                    Some(c) => lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(&c), Tone::Plain)])),
                    None if !rubric.is_empty() => lines.extend(rubric_lines(&rubric)),
                    None => {
                        lines.push(Line::from(span("  no automated check fits; the result is unverified", Tone::Dim)))
                    }
                }
                for f in files {
                    lines.push(Line::from(span(format!("  + {}", clean_line(&f)), Tone::Dim)));
                }
                self.log.extend(lines);
            }
            Progress::Recalled { notes, map_tokens, .. } => {
                if task.recalled_shown {
                    return;
                }
                task.recalled_shown = true;
                let mut head = vec![span("◇ memory", Tone::Mem)];
                let mut parts = Vec::new();
                if let Some(t) = map_tokens {
                    parts.push(format!("map {t} tokens"));
                }
                parts.push(match notes.len() {
                    1 => "1 note".to_owned(),
                    n => format!("{n} notes"),
                });
                head.push(span(format!("  {}", parts.join(" · ")), Tone::Dim));
                self.log.push(Line::from(head));
                self.log.push(Line::from(span(
                    format!("  /memory-used {} · recorded recall context", task.trace),
                    Tone::Dim,
                )));
                for n in &notes {
                    self.log.push(Line::from(vec![
                        span(format!("  {:.2} ", n.confidence), Tone::Mem),
                        span(clean_line(&n.text), Tone::Plain),
                    ]));
                }
            }
            event @ (Progress::ModelStarted { .. } | Progress::ModelText { .. } | Progress::ModelFinished { .. }) => {
                let context = event.model_call().expect("a model event has a context");
                let attempt = context.attempt.filter(|&a| a < task.req.attempts && a < 8);
                let settles = matches!(event, Progress::ModelFinished { .. });
                if let Some(attempt) = attempt.filter(|_| task.req.stream || settles) {
                    if !task.lanes.iter().any(|l| l.index == attempt) {
                        task.lanes.push(Lane::new(attempt, None));
                        task.lanes.sort_by_key(|l| l.index);
                    }
                }
                if matches!(event, Progress::ModelStarted { .. }) && !task.counted.contains(&context.call) {
                    task.open_calls.insert(context.call.clone());
                }
                // Counted even when previews are off or the lane has ended:
                // a cancelled attempt's last call settles after it stops.
                if let Progress::ModelFinished { cost_usd, .. } = &event {
                    task.open_calls.remove(&context.call);
                    if task.counted.insert(context.call.clone()) {
                        task.unsettled.add(*cost_usd);
                        if !cost_usd.is_some_and(|c| c.is_finite() && c >= 0.0) {
                            task.unsettled_unknown += 1;
                        }
                        match (context.attempt, attempt) {
                            (None, _) => task.design_spend.add(*cost_usd),
                            (_, Some(attempt)) => {
                                lane(task, attempt).expect("the lane was created").spend.add(*cost_usd)
                            }
                            _ => {}
                        }
                    }
                }
                if !task.req.stream {
                    return;
                }
                match context.attempt {
                    Some(attempt) if attempt < task.req.attempts && attempt < 8 => {
                        let lane = lane(task, attempt).expect("the lane was created");
                        if context.turn < lane.turn || lane.status.is_some() {
                            return;
                        }
                        lane.turn = context.turn;
                        for line in lane.preview.feed(&event) {
                            lane.push(line);
                        }
                    }
                    None => self.log.extend(task.designer_preview.feed(&event)),
                    _ => {}
                }
            }
            Progress::Note { message, .. } => {
                if task.notes_seen.insert(message.clone()) {
                    self.log.push(Line::from(span(format!("  {}", clean_line(&message)), Tone::Dim)));
                }
            }
            Progress::AttemptStarted { attempt, fork, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.fork = lane.fork.take().or(fork);
                } else {
                    task.lanes.push(Lane::new(attempt, fork));
                    task.lanes.sort_by_key(|l| l.index);
                }
            }
            Progress::ToolCall { attempt, turn, detail, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.turn = turn;
                    lane.push(Line::from(vec![
                        span(format!("{turn:>2} "), Tone::Dim),
                        span(clean_line(&detail), Tone::Plain),
                    ]));
                }
            }
            Progress::CheckRan { attempt, passed, exit_code, tail, judged, .. } => {
                let tail = scrub(&tail, &self.secrets);
                if let Some(lane) = lane(task, attempt) {
                    let line = match (passed, exit_code) {
                        (true, _) if judged => span("   judge: criteria met", Tone::Ok),
                        (false, _) if judged => span("   judge: criteria not met", Tone::Bad),
                        (true, _) => span("   check passed", Tone::Ok),
                        (false, Some(code)) => span(format!("   check failed (exit {code})"), Tone::Bad),
                        (false, None) => span("   check failed (killed or timed out)", Tone::Bad),
                    };
                    lane.push(Line::from(line));
                    for l in last_lines(&tail, CHECK_TAIL_LINES) {
                        lane.push(Line::from(span(format!("   {l}"), Tone::Dim)));
                    }
                }
            }
            Progress::AttemptFinished { attempt, status, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    if let Some(line) = lane.preview.finish("ended; final preview event unavailable", true) {
                        lane.push(line);
                    }
                    lane.status = Some(status);
                    lane.ended.get_or_insert_with(Instant::now);
                }
            }
            Progress::Required { requirement, .. } => {
                task.version = task.version.max(requirement.version);
                task.asked.retain(|(q, _)| Some(*q) != requirement.constraint.answers);
            }
            Progress::Adopted { attempt, version, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.push(Line::from(span(format!("   now follows v{version}"), Tone::Warn)));
                }
            }
            Progress::State { state, waiting, .. } => {
                task.state = state;
                task.waiting = waiting;
            }
            Progress::Asked { attempt, question, text, options, .. } => {
                if task.asked.iter().any(|(q, _)| *q == question) {
                    return;
                }
                let words = scrub(&text, &self.secrets);
                for (i, l) in text::clean(&words).into_iter().enumerate() {
                    let head = if i == 0 { format!("? attempt {attempt} asks  ") } else { "  ".to_owned() };
                    self.log.push(Line::from(vec![span(head, Tone::Warn), span(l, Tone::Plain)]));
                }
                for (i, option) in options.iter().enumerate() {
                    let option = scrub(option, &self.secrets);
                    self.log.push(Line::from(span(format!("  {}. {}", i + 1, clean_line(&option)), Tone::Plain)));
                }
                let how = if options.is_empty() { "type the answer" } else { "type an option's number or an answer" };
                self.log.push(Line::from(span(format!("  {how}; every attempt follows it"), Tone::Dim)));
                task.asked.push((question, options));
            }
        }
    }

    pub(super) fn ran(&mut self, trace: TraceId, result: Result<RunResponse, String>) -> Vec<Action> {
        let Some(task) = &self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        let mut task = self.task.take().expect("checked above");
        task.settle();
        self.zoom = None;
        let now = Instant::now();
        for lane in &mut task.lanes {
            lane.ended.get_or_insert(now);
        }
        self.last_lanes = std::mem::take(&mut task.lanes);
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                self.ended = Some("molt: run failed".to_owned());
                self.error("the run failed", &e);
                return Vec::new();
            }
        };
        // Progress can be dropped by a busy transport or suspended interface.
        // The final attempt reports settle the archived lane totals as well.
        for lane in &mut self.last_lanes {
            if let Some(attempt) = resp.attempts.iter().find(|a| a.index == lane.index) {
                lane.spend.usd = attempt.cost_usd;
                lane.spend.unknown |= resp.uncounted_calls > 0;
                lane.status = Some(attempt.status);
                lane.turn = attempt.turns;
            }
        }
        // A resumed run's reply counts what it spent before the resume too; the total may have counted some.
        let (counted_usd, counted_calls) = self.settled_runs.remove(trace.as_str()).unwrap_or_default();
        self.spent_usd += (resp.cost_usd - counted_usd).max(0.0);
        if resp.checkpoint.is_some() {
            self.settled_runs.insert(trace.to_string(), (resp.cost_usd, resp.uncounted_calls));
        }
        self.ended = Some(match resp.outcome {
            Outcome::Passed => match resp.changes.len() {
                1 => "molt: passed · 1 file".to_owned(),
                n => format!("molt: passed · {n} files"),
            },
            Outcome::Judged => "molt: passed · judged by a model".to_owned(),
            Outcome::Unverified => "molt: done, unverified".to_owned(),
            Outcome::Failed => "molt: failed".to_owned(),
        });
        self.remember(&task.req.task, &resp);
        // A kept result the task went on from is merged with it when it is applied.
        if let Some(kept) = task.req.continues.as_ref().and_then(|c| c.fork.clone()).filter(|_| resp.applied) {
            self.kept.retain(|old| *old != kept);
            self.kept_entries.retain(|entry| entry.fork != kept);
            self.kept_changes.push(crate::tui::kept::Change::Remove(kept));
        }
        self.continuation = (!resp.conversation.is_empty()).then(|| Continuation {
            conversation: resp.conversation.clone(),
            fork: resp.fork.clone().filter(|_| !resp.applied && !resp.changes.is_empty()),
        });
        let learns = workflow::builtin(resp.workflow.as_deref().or(Some(workflow::VERIFY)))
            .is_none_or(|w| w.stages.learn.is_some());
        self.uncounted_calls = self.uncounted_calls.saturating_add(resp.uncounted_calls.saturating_sub(counted_calls));
        let mut actions = Vec::new();
        if let Some(run) = &resp.checkpoint {
            self.push(Line::from(span(
                format!("  stopped before it finished; /runs resume {} goes on with it", clean_line(run)),
                Tone::Warn,
            )));
            actions.push(Action::ListRuns);
        }
        let memory_model = agent::memory_model(task.settings.memory_model.as_deref(), task.req.model.as_deref());
        let spent_all = task_budget(&task.settings).is_some_and(|b| resp.cost_usd >= b);

        for a in &resp.attempts {
            let (mark, tone) = match a.status {
                AttemptStatus::Passed => ("✓", Tone::Ok),
                AttemptStatus::Failed => ("✗", Tone::Bad),
                AttemptStatus::Cancelled => ("–", Tone::Dim),
                AttemptStatus::Error => ("!", Tone::Warn),
            };
            let status = match a.status {
                _ if a.dispute.is_some() => "disputed the check",
                AttemptStatus::Passed => "passed",
                AttemptStatus::Failed => "failed",
                AttemptStatus::Cancelled => "cancelled",
                AttemptStatus::Error => "error",
            };
            let checks = match a.check_runs {
                0 => String::new(),
                1 => " · 1 check".to_owned(),
                n => format!(" · {n} checks"),
            };
            self.push(Line::from(vec![
                span(format!("  {mark} attempt {} ", a.index + 1), tone),
                span(format!("{status} · {} turns{checks} · ${:.2}", a.turns, a.cost_usd), Tone::Dim),
            ]));
            if matches!(a.status, AttemptStatus::Failed | AttemptStatus::Error) {
                for l in last_lines(&scrub(&a.note, &self.secrets), NOTE_LINES) {
                    self.push(Line::from(span(format!("      {l}"), Tone::Dim)));
                }
            }
        }

        if let Some(selection) = &resp.selection {
            let line = format!("  picked among {} passing attempts:", selection.candidates.len());
            self.push(Line::from(span(line, Tone::Dim)));
            for c in &selection.candidates {
                let (mark, tone) = if resp.winner == Some(c.attempt) { ("*", Tone::Ok) } else { (" ", Tone::Dim) };
                self.push(Line::from(vec![
                    span(format!("    {mark} attempt {} ", c.attempt + 1), tone),
                    span(agent::candidate_scores(c), Tone::Dim),
                ]));
            }
        }

        let (added, removed) = text::diff_counts(&resp.patch);
        let files = match resp.changes.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let size = vec![
            span(format!("  {files}  "), Tone::Dim),
            span(format!("+{added}"), Tone::Ok),
            span(" ", Tone::Plain),
            span(format!("−{removed}"), Tone::Bad),
        ];
        let summary = scrub(&resp.summary, &self.secrets);
        let (summary, cut) = bounded(summary.trim(), SUMMARY_BYTES);
        let cut_note = cut.then(|| {
            let note = format!("  (cut at 1 MB; the whole text is in the audit log, trace {trace})");
            Line::from(span(note, Tone::Warn))
        });
        match resp.outcome {
            Outcome::Failed => {
                self.push(Line::from(span("✗ no attempt passed", Tone::Bad)));
                self.log.extend(block(summary, "  ", Tone::Dim));
                self.log.extend(cut_note);
            }
            Outcome::Passed | Outcome::Unverified | Outcome::Judged => {
                let tone = if resp.outcome == Outcome::Passed { Tone::Ok } else { Tone::Warn };
                let label = match resp.workflow.as_deref() {
                    Some(workflow::ASK) if resp.outcome == Outcome::Unverified => "unverified answer",
                    _ => outcome_name(resp.outcome),
                };
                let mut head = vec![span(format!("✓ {label}"), tone)];
                if !resp.changes.is_empty() {
                    head.extend(size);
                }
                self.push(Line::from(head));
                self.log.extend(block(summary, "  ", Tone::Plain));
                self.log.extend(cut_note);
                for c in &resp.changes {
                    let kind = match c.kind {
                        ChangeKind::Added => "added   ",
                        ChangeKind::Modified => "modified",
                        ChangeKind::Deleted => "deleted ",
                    };
                    self.push(Line::from(span(format!("  {kind} {}", clean_line(&c.path)), Tone::Dim)));
                }
                if let Some(line) = resp.left_out.as_ref().and_then(agent::left_out_line) {
                    self.push(Line::from(span(format!("  {}", clean_line(&line)), Tone::Dim)));
                }
                for f in &resp.judgement {
                    let (mark, tone) = if f.met { ("✓", Tone::Ok) } else { ("✗", Tone::Bad) };
                    self.push(Line::from(vec![
                        span(format!("  {mark} "), tone),
                        span(clean_line(&f.criterion), Tone::Dim),
                    ]));
                }
            }
        }
        if resp.uncounted_calls > 0 {
            let line = format!("  {} model calls of cancelled attempts are not in the cost", resp.uncounted_calls);
            self.push(Line::from(span(line, Tone::Dim)));
        }

        if !resp.patch.is_empty() {
            self.last = Some(LastDiff { patch: resp.patch.clone(), truncated: resp.patch_truncated });
        }
        let mut deciding = false;
        let mut applying = false;
        self.to_commit =
            (task.settings.commit && resp.outcome != Outcome::Failed && !resp.changes.is_empty()).then(|| {
                Action::Commit { trace: trace.clone(), task: task.req.task.clone(), resp: Box::new(resp.clone()) }
            });
        self.pending_result = resp.fork.as_ref().filter(|_| !resp.applied).map(|fork| {
            let mut entry = crate::tui::kept::Entry::new(fork.clone(), &task.req.task, outcome_name(resp.outcome));
            entry.diff(resp.changes.len(), &resp.patch);
            entry
        });
        if resp.applied {
            self.push(Line::from(span("  applied", Tone::Ok)));
            actions.extend(self.to_commit.take());
        } else if let Some(fork) = resp.fork.clone() {
            if resp.changes.is_empty() {
                actions.push(Action::Discard { fork });
            } else if task.mode == Mode::Confirm && !task.req.apply && !matches!(resp.outcome, Outcome::Failed) {
                let stops: Vec<_> =
                    apply_stops(&resp, &self.rules).into_iter().map(|line| scrub(&line, &self.secrets)).collect();
                if stops.is_empty() && !self.rules.always_ask {
                    applying = true;
                    actions.push(Action::Apply { fork });
                } else {
                    deciding = true;
                    task.stops = stops;
                    self.prompt = Some(Prompt::Apply { resp: Box::new(resp.clone()), fork, part: Part::default() });
                }
            } else {
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                if let Some(entry) = self.pending_result.take() {
                    self.keep_entry(entry);
                }
            }
        }
        if !(deciding || applying) {
            self.to_commit = None;
        }
        if deciding {
            self.task = Some(Task { phase: Phase::Deciding, lanes: Vec::new(), ..task });
        } else if applying {
            self.task = Some(Task { phase: Phase::Applying, lanes: Vec::new(), stops: Vec::new(), ..task });
        } else if let Some(d) = disputed_check(&task.req, &resp) {
            // The check needs the user's look before anything runs to it again: stop at its card.
            let mut req = task.req.clone();
            req.check = None;
            req.check_files = Vec::new();
            req.suite = None;
            let next = Task::new(TraceId::random(), req, Phase::Confirming, task.mode, task.settings.clone());
            self.task = Some(Task { stops: vec![DISPUTED.into()], ..next });
            self.prompt = Some(Prompt::Check(d));
        } else if resp.outcome == Outcome::Failed && !resp.attempts.is_empty() {
            // Only a check known in full can be run again without designing it anew.
            let mut req = task.req;
            if req.check.is_none() {
                if let Some(spec) = resp.check.as_ref().filter(|c| c.files.is_empty()) {
                    req.check = Some(spec.command.clone());
                }
            }
            if req.check.is_some() {
                let designed = resp.check.as_ref().is_some_and(|c| c.designed);
                self.prompt = Some(Prompt::Retry {
                    req: Box::new(req),
                    designed,
                    stopped: false,
                    attempts: resp.attempts.clone(),
                    mode: task.mode,
                    settings: Box::new(task.settings.clone()),
                });
            }
        }

        // Memory learns from failures too, in a workflow that learns; the run's budget covers it.
        if self.memory_up() && !spent_all && learns {
            self.learning.insert(trace.to_string());
            actions.push(Action::Learn { trace, model: memory_model });
        }
        actions
    }
}

impl Lane {
    fn new(index: u32, fork: Option<String>) -> Self {
        Self {
            index,
            fork,
            turn: 0,
            status: None,
            lines: Vec::new(),
            preview: Preview::default(),
            spend: Spend::default(),
            started: Instant::now(),
            ended: None,
        }
    }

    /// How long the attempt has run, or ran.
    pub fn elapsed(&self) -> Duration {
        self.ended.unwrap_or_else(Instant::now).saturating_duration_since(self.started)
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        if self.lines.len() > LANE_LINES {
            self.lines.remove(0);
        }
    }
}

fn lane(task: &mut Task, attempt: u32) -> Option<&mut Lane> {
    task.lanes.iter_mut().find(|l| l.index == attempt)
}

/// Each lane's lines under its attempt's name, for the viewer.
pub(super) fn lane_lines(lanes: &[Lane]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for lane in lanes {
        lines.push(Line::from(vec![
            span(format!("attempt {} · tool calls", lane.index + 1), Tone::Shell),
            span(format!(" · {} · {}", text::duration(lane.elapsed()), lane.spend.label()), Tone::Dim),
        ]));
        lines.extend(lane.lines.iter().cloned());
        lines.push(Line::default());
    }
    lines
}

/// The last `n` non-empty lines of `text`, made safe to show.
fn last_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<String> = text::clean(text.trim_end()).into_iter().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// `text` cut to at most `max` bytes at a character boundary, and whether it was cut.
pub(super) fn bounded(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn streamed_model_text_updates_the_frame_and_folds_without_becoming_a_result() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Run { req, .. } if req.stream));
        let context = model_context(&app, Some(0), 0);
        app.update(Msg::Progress(Progress::ModelStarted { context }));
        let context = model_context(&app, Some(0), 1);
        assert!(app
            .update(Msg::Progress(Progress::ModelText { context, text: "latest streamed text\x1b\x07".into() }))
            .is_empty());
        let shown = frame_text(&mut app);
        assert!(shown.contains("model preview") && shown.contains("latest streamed text"), "{shown}");
        assert_eq!(app.spent_usd, 0.0, "preview events cannot settle spending");
        let context = model_context(&app, Some(0), 3);
        app.update(Msg::Progress(Progress::ModelText { context, text: " after a gap".into() }));
        assert!(frame_text(&mut app).contains("gaps in preview"));
        let context = model_context(&app, Some(0), 4);
        app.update(Msg::Progress(Progress::ModelFinished {
            context,
            usage: Some(Usage::default()),
            cost_usd: Some(0.02),
            error: None,
        }));
        let shown = frame_text(&mut app);
        assert!(shown.contains("model preview finished · gaps"), "{shown}");
        assert!(app.task.as_ref().unwrap().lanes[0].preview.lines(100, 4).is_empty());
        let context = model_context(&app, Some(0), 5);
        app.update(Msg::Progress(Progress::ModelText { context, text: "late forged result".into() }));
        assert!(!frame_text(&mut app).contains("forged result"));
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Ran { trace, result: Err("the model refused".into()) }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
        assert!(log(&app).contains("the model refused"));
    }

    #[test]
    fn each_lane_shows_what_its_finished_calls_cost_and_unknown_cost_is_a_lower_bound() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.settings.budget_usd = Some(1.0);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        // Each call counts once.
        app.update(finished(&app, Some(0), "a", Some(0.3)));
        app.update(finished(&app, Some(0), "a", Some(0.3)));
        app.update(finished(&app, Some(1), "b", None));
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.lanes[0].spend.label(), "$0.30");
        assert_eq!(task.lanes[1].spend.label(), "≥ $0.00");
        assert_eq!(task.spend().label(), "≥ $0.30");
        let shown = frame_text(&mut app);
        assert!(shown.contains("turn 1 · 0s · $0.30"), "{shown}");
        assert!(shown.contains("task ≥ $0.30 / $1.00"), "{shown}");
        assert_eq!(app.spent_usd, 0.0, "progress events do not settle spending");

        // An attempt stopped by the winner still settles its last call.
        app.update(Msg::Progress(Progress::AttemptFinished {
            run: trace.to_string(),
            attempt: 1,
            status: AttemptStatus::Cancelled,
        }));
        app.update(finished(&app, Some(1), "c", Some(0.05)));
        assert_eq!(app.task.as_ref().unwrap().lanes[1].spend.label(), "≥ $0.05");

        app.update(Msg::Ran { trace, result: Ok(passed(false, Some("fork"))) });
        assert!((app.spent_usd - 0.3).abs() < 1e-9, "the reply settles the run once: {}", app.spent_usd);
    }

    #[test]
    fn a_fork_kept_because_its_changes_could_not_be_listed_says_why() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        let run = app.task.as_ref().unwrap().trace.to_string();
        app.update(Msg::Progress(Progress::AttemptStarted { run, attempt: 0, fork: Some("/d/work/fork-a".into()) }));
        let actions = stop(&mut app);
        let [Action::Interrupt { attempts, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(attempts, &["/d/work/fork-a"]);
        let note =
            "the changes in /d/work/fork-a could not be listed, so it is kept in case it holds work: fs.diff failed";
        app.update(Msg::Stopped { kept: vec!["/d/work/fork-a".into()], notes: vec![note.into()] });
        assert_eq!(app.kept, ["/d/work/fork-a"]);
        let log = log(&app);
        assert!(log.contains(note) && log.ends_with("  kept 1 · interrupted\n  kept in /d/work/fork-a"), "{log}");
    }

    #[test]
    fn final_reports_settle_lane_cost_when_progress_was_lost() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(Msg::Progress(Progress::AttemptStarted { run: trace.to_string(), attempt: 1, fork: None }));
        let resp = passed(false, Some("fork"));
        let cost = resp.attempts.iter().find(|a| a.index == 1).unwrap().cost_usd;
        app.update(Msg::Ran { trace, result: Ok(resp) });
        assert_eq!(app.last_lanes.iter().find(|a| a.index == 1).unwrap().spend.usd, cost);
        assert!(cost > 0.0);
    }

    #[test]
    fn narrow_lanes_keep_their_status_on_one_row() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.settings.attempts = Some(4);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        for attempt in 0..4 {
            app.update(Msg::Progress(Progress::AttemptStarted { run: trace.to_string(), attempt, fork: None }));
            app.update(Msg::Progress(Progress::ToolCall {
                run: trace.to_string(),
                attempt,
                turn: 12,
                tool: "read_file".into(),
                detail: "read".into(),
            }));
            app.update(finished(&app, Some(attempt), &format!("call {attempt}"), Some(0.25)));
        }
        let rows = frame_rows(&mut app, 79);
        let titles = rows.iter().find(|r| r.contains("turn 12")).expect("the lane titles");
        assert_eq!(titles.matches("turn 12").count(), 4, "{titles}");
        let wide = frame_rows(&mut app, 200);
        let titles = wide.iter().find(|r| r.contains("turn 12")).expect("the lane titles");
        assert_eq!(titles.matches("turn 12 · 0s · $0.25").count(), 4, "{titles}");
        assert_eq!(titles.matches("attempt").count(), 4, "{titles}");
    }

    #[test]
    fn previews_can_be_disabled_and_cancellation_cannot_revive_them() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "/stream off");
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Run { req, .. } if !req.stream));
        let context = model_context(&app, Some(0), 1);
        app.update(Msg::Progress(Progress::ModelText { context, text: "hidden preview".into() }));
        assert!(app.task.as_ref().unwrap().lanes.is_empty());
        assert!(!frame_text(&mut app).contains("hidden preview"));
        stop(&mut app);
        up(&mut app);
        type_in(&mut app, "/stream on");
        type_in(&mut app, "implement it again");
        let context = model_context(&app, Some(0), 1);
        app.update(Msg::Progress(Progress::ModelText { context: context.clone(), text: "provisional".into() }));
        assert!(frame_text(&mut app).contains("provisional"));
        assert!(matches!(stop(&mut app)[0], Action::Interrupt { .. }));
        // Only the offer to start again with the same check is left.
        assert!(app.task.is_none() && matches!(app.prompt, Some(Prompt::Retry { stopped: true, .. })));
        app.update(Msg::Progress(Progress::ModelText { context, text: "late result".into() }));
        assert!(!frame_text(&mut app).contains("late result"));
    }

    #[test]
    fn designer_previews_fold_on_errors_without_creating_a_check() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Design { req, .. } if req.stream));
        let context = model_context(&app, None, 0);
        app.update(Msg::Progress(Progress::ModelStarted { context }));
        let context = model_context(&app, None, 1);
        app.update(Msg::Progress(Progress::ModelText { context, text: "designing stream".into() }));
        assert!(frame_text(&mut app).contains("designing stream"));
        let context = model_context(&app, None, 2);
        app.update(Msg::Progress(Progress::ModelFinished {
            context,
            usage: None,
            cost_usd: None,
            error: Some("cancelled".into()),
        }));
        assert!(log(&app).contains("model preview stopped (cancelled)"));
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Designed { trace, result: Err("cancelled".into()), existing: vec![] }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
    }

    #[test]
    fn auto_mode_runs_straight_through_and_plan_mode_keeps_the_result() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.mode, Mode::Auto);
        let actions = type_in(&mut app, "fix it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.apply && req.check.is_none() && req.verify);
        let trace = trace.clone();
        app.update(Msg::Progress(Progress::CheckReady {
            run: trace.to_string(),
            command: Some("make test".into()),
            files: vec![],
            designed: true,
            rubric: vec![],
        }));
        app.update(Msg::Ran { trace, result: Ok(passed(true, None)) });
        assert!(app.prompt.is_none() && app.task.is_none());
        assert!(log(&app).contains("  $ make test") && log(&app).ends_with("  applied"), "{}", log(&app));

        app.update(key(KeyCode::BackTab));
        assert_eq!(app.mode, Mode::Plan);
        let actions = type_in(&mut app, "again");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(!req.apply);
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-2"))) });
        assert!(app.prompt.is_none());
        assert_eq!(app.kept, ["/d/work/fork-2"]);
    }

    #[test]
    fn the_log_shows_long_summaries_every_change_and_every_note_in_full() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "/check true");
        assert!(actions.is_empty());
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let trace = trace.clone();
        let notes = (0..8)
            .map(|i| RecalledNote { id: format!("note_{i}"), text: format!("fact {i}"), confidence: 0.5 })
            .collect();
        app.update(Msg::Progress(Progress::Recalled { run: trace.to_string(), notes, map_tokens: None }));
        let mut resp = passed(false, Some("/d/work/fork-3"));
        resp.summary = (0..200).map(|i| format!("summary line {i}")).collect::<Vec<_>>().join("\n");
        resp.changes = (0..30).map(|i| Change { path: format!("src/f{i}.rs"), kind: ChangeKind::Modified }).collect();
        app.update(Msg::Ran { trace, result: Ok(resp) });
        let log = log(&app);
        for i in 0..8 {
            assert!(log.contains(&format!("0.50 fact {i}")), "{log}");
        }
        for i in 0..200 {
            assert!(log.contains(&format!("  summary line {i}\n")), "{log}");
        }
        for i in 0..30 {
            assert!(log.contains(&format!("  modified src/f{i}.rs")), "{log}");
        }
        assert!(!log.contains("more lines") && !log.contains(" more\n"), "{log}");
    }

    #[test]
    fn a_failed_summary_is_shown_in_full_and_only_a_runaway_one_is_cut() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let mut resp = passed(false, None);
        resp.outcome = Outcome::Failed;
        resp.winner = None;
        resp.changes = vec![];
        resp.summary = (0..200).map(|i| format!("why {i}")).collect::<Vec<_>>().join("\n");
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        assert!(log(&app).contains("  why 0\n") && log(&app).contains("  why 199"), "{}", log(&app));
        assert!(!log(&app).contains("cut at"), "{}", log(&app));

        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let mut resp = passed(false, Some("/d/work/fork-4"));
        resp.summary = "é".repeat(SUMMARY_BYTES);
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        let note = format!("  (cut at 1 MB; the whole text is in the audit log, trace {trace})");
        assert!(log(&app).contains(&note), "the cut is said, with where to find the rest");
    }
}

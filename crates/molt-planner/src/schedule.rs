//! The adaptive schedule (#13): a run starts one attempt, on the cheapest
//! model its escalation list allows, and starts another only on evidence
//! that it needs one, while the budget allows it.
//!
//! The rules ([`ADAPTIVE_POLICY`]):
//!
//! - **Start.** One attempt. Its model is the request's when the request
//!   names one, else the first of `MOLT_ESCALATION_MODELS`, else the run's.
//! - **A failed check** is fed back to the attempt as always: it goes on.
//! - **No progress** (its check fails twice in a row with the same output) or
//!   **stuck** (every tool call failed for [`TOOL_ERROR_TURNS`] turns in a
//!   row), while it is the only attempt going: another attempt starts
//!   alongside it, on the next model up when there is one (`parallel`, an
//!   escalation).
//! - **An attempt failed or erred** and none is going: a new attempt on the
//!   next model up (`escalate`, an escalation), or, at the top or with the
//!   escalation cap reached, on the same model with another approach
//!   (`retry`). Either is told what the failed attempts did.
//! - **A failure no new attempt can fix** (#228): an attempt that ended on
//!   something missing from the environment, or out of disk or memory, holds
//!   back every new attempt, since each would meet the same; one the model
//!   endpoint refused (a credential, a model name) holds back new attempts on
//!   that model, and escalates to the next model up when there is one. A
//!   code failure (a compiler error, a failing test) goes on as above.
//! - **Nothing new starts** once an attempt passed, the run was stopped, the
//!   budget is spent, the run's attempts are all started, or what is left of
//!   the budget is unknown or less than a new attempt is assumed to cost
//!   after holding back for the calls in flight (`hold`, with the reason).
//!
//! Evidence from an attempt that has ended, or that comes after an attempt
//! passed, is stale and decides nothing. Each decision goes out as
//! [`molt_api::progress::Progress::Scheduled`], and so into the audit log,
//! with its reason, the price it assumed and the budget it saw.

use std::collections::{BTreeMap, BTreeSet};

use molt_api::model::attempt_name;
use molt_api::planner::{
    AttemptStatus, BudgetState, Failure, FailureAction, PriceAssumption, ScheduleAction, Scheduling, ADAPTIVE_POLICY,
};

use crate::branch::Branch;
use crate::checkpoint::SavedAttempt;

/// Turns in a row whose tool calls all failed before an attempt counts as stuck.
pub(crate) const TOOL_ERROR_TURNS: u32 = 3;
/// Characters of a failed attempt's note told to the attempts after it,
/// when it left no [`Branch`] (a checkpoint from before #264).
const PRIOR_NOTE: usize = 600;

/// What an attempt of an adaptive run reports while it works.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Signal {
    /// Its done-check failed and goes back to it; `repeated`: with the same
    /// output as its run before, so its last change made no progress.
    CheckFailed { attempt: u32, repeated: bool },
    /// Every tool call of its last `turns` turns failed.
    ToolErrors { attempt: u32, turns: u32 },
}

/// What the schedule decides on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Evidence {
    Heard(Signal),
    /// An attempt ended: how, what it cost, and why.
    Ended {
        attempt: u32,
        status: AttemptStatus,
        cost_usd: f64,
        note: String,
        /// What kind of failure ended it, when the planner could tell.
        failure: Option<Failure>,
        /// What it leaves the attempts after it, when it failed or erred;
        /// `None` from a checkpoint that predates it.
        branch: Option<Box<Branch>>,
    },
}

/// The run as the schedule sees it when evidence comes in.
#[derive(Clone, Debug)]
pub(crate) struct View {
    pub budget: BudgetState,
    /// Why nothing may start any more: the run was stopped or its budget spent.
    pub stopped: Option<String>,
    /// An attempt has passed.
    pub won: bool,
    /// What the run's attempts have spent so far, the ones still going included.
    pub attempts_spent: f64,
}

/// An attempt to start.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Launch {
    pub index: u32,
    /// Its model, when the escalation list decides it; `None` keeps the run's.
    pub model: Option<String>,
    /// What the attempts before it did, for its first message.
    pub prior: Option<String>,
}

/// `MOLT_ESCALATION_MODELS`: model names separated by commas, cheapest first.
pub fn ladder(text: &str) -> Result<Vec<String>, String> {
    let models: Vec<String> = text.split(',').map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned).collect();
    if models.is_empty() {
        return Err("name the models to escalate through, cheapest first, separated by commas".into());
    }
    for (i, m) in models.iter().enumerate() {
        if models[..i].contains(m) {
            return Err(format!("{m} is named twice"));
        }
    }
    Ok(models)
}

/// The state of one adaptive run's schedule.
#[derive(Debug)]
pub(crate) struct Adaptive {
    ladder: Vec<String>,
    /// The rung of `ladder` the last attempt started on; `None` when the
    /// ladder does not decide the model, and every attempt has the run's.
    rung: Option<usize>,
    /// The run's model, for the record when the ladder does not decide.
    run_model: Option<String>,
    max_attempts: u32,
    started: u32,
    max_escalations: u32,
    escalations: u32,
    /// The first attempt is still to start.
    to_start: bool,
    running: BTreeSet<u32>,
    models: BTreeMap<u32, Option<String>>,
    /// What each attempt that ended cost.
    costs: BTreeMap<u32, f64>,
    /// One line for each attempt that failed or erred.
    failures: Vec<String>,
    /// Attempts whose lack of progress was acted on (or held) once already.
    stuck: BTreeSet<u32>,
    /// Models (`None`: the run's) whose endpoint refused an attempt in a way
    /// only the user can fix: no new attempt starts on them.
    refused: BTreeSet<Option<String>>,
    /// The evidence of each classified failure, and the first attempt it ended.
    seen: BTreeMap<String, u32>,
}

impl Adaptive {
    /// The schedule of a run that may start `max_attempts`. `asked` is the
    /// request's model, `run_model` the run's (the request's, else the
    /// planner's default); `pinned`: the attempts' role names its own model,
    /// which wins over any other. Also says how the ladder applies, when it
    /// does not as configured.
    pub fn new(
        ladder: &[String],
        max_escalations: u32,
        asked: Option<&str>,
        run_model: Option<&str>,
        pinned: bool,
        max_attempts: u32,
    ) -> (Self, Option<String>) {
        let (rung, note) = match (ladder.is_empty(), pinned, asked) {
            (true, _, _) => (None, None),
            (false, true, _) => (
                None,
                Some("the attempts' role names its model, so the adaptive schedule does not change it".to_owned()),
            ),
            (false, false, Some(asked)) => match ladder.iter().position(|m| m == asked) {
                Some(at) => (Some(at), None),
                None => (
                    None,
                    Some(format!(
                        "the run's model {asked} is not among the escalation models ({}), so every attempt keeps it",
                        ladder.join(", ")
                    )),
                ),
            },
            (false, false, None) => (Some(0), None),
        };
        let schedule = Self {
            ladder: ladder.to_vec(),
            rung,
            run_model: run_model.map(str::to_owned),
            max_attempts,
            started: 0,
            max_escalations,
            escalations: 0,
            to_start: true,
            running: BTreeSet::new(),
            models: BTreeMap::new(),
            costs: BTreeMap::new(),
            failures: Vec::new(),
            stuck: BTreeSet::new(),
            refused: BTreeSet::new(),
            seen: BTreeMap::new(),
        };
        (schedule, note)
    }

    /// Go on from a checkpoint's attempts and escalations. A first attempt
    /// starts only when the checkpoint has none, or for a `retry`.
    pub fn restore(&mut self, attempts: &[SavedAttempt], escalations: u32, retry: bool) {
        self.escalations = escalations;
        self.to_start = retry || attempts.is_empty();
        for saved in attempts {
            self.started = self.started.max(saved.index + 1);
            self.models.insert(saved.index, saved.model.clone());
            if let Some(at) = saved.model.as_ref().and_then(|m| self.ladder.iter().position(|l| l == m)) {
                self.rung = Some(self.rung.map_or(at, |r| r.max(at)));
            }
            match &saved.end {
                Some(end) => self.ended(
                    saved.index,
                    end.status,
                    saved.cost_usd,
                    &end.note,
                    end.failure.as_ref(),
                    end.branch.as_deref(),
                ),
                // Left as it was for a retry: it does not go on.
                None if retry => {}
                None => {
                    self.running.insert(saved.index);
                }
            }
        }
    }

    /// The escalations made so far.
    pub fn escalations(&self) -> u32 {
        self.escalations
    }

    /// The model of an attempt this schedule started.
    pub fn model_of(&self, index: u32) -> Option<String> {
        self.models.get(&index).cloned().flatten()
    }

    /// The run's first attempt, unless it has one already or may start none.
    pub fn start(&mut self, view: &View) -> Option<(Option<Launch>, Scheduling)> {
        if !std::mem::take(&mut self.to_start) || view.won {
            return None;
        }
        let model = self.rung.map(|r| self.ladder[r].clone());
        let price = self.price(view, model.as_deref());
        if let Some(why) = view.stopped.clone().or_else(|| self.used_up()) {
            return Some((None, self.decision(ScheduleAction::Hold, None, None, why, price, view)));
        }
        let why = match &model {
            Some(m) => format!("an adaptive run starts with one attempt, on {m}, the first model it may escalate from"),
            None => "an adaptive run starts with one attempt, on the run's model".to_owned(),
        };
        Some(self.launch(ScheduleAction::Start, None, model, why, price, view))
    }

    /// What `evidence` decides: `None` for nothing to say (the attempt goes
    /// on, or the evidence is stale), else the decision and the attempt it
    /// starts, if any.
    pub fn decide(&mut self, evidence: Evidence, view: &View) -> Option<(Option<Launch>, Scheduling)> {
        match evidence {
            Evidence::Ended { attempt, status, cost_usd, note, failure, branch } => {
                let was_running = self.running.contains(&attempt);
                self.ended(attempt, status, cost_usd, &note, failure.as_ref(), branch.as_deref());
                let failed = matches!(status, AttemptStatus::Failed | AttemptStatus::Error);
                // Another attempt is still going, or it ended with the run (cancelled), or one has passed.
                if !was_running || !failed || view.won || !self.running.is_empty() {
                    return None;
                }
                let how = if status == AttemptStatus::Error { "erred" } else { "failed" };
                let why = format!("{} {how}: {}", attempt_name(attempt), first_line(&note));
                let up = self.next_rung();
                if let Some(f) = failure.as_ref().filter(|f| f.needs_user()) {
                    let on = self.model_of(attempt).or_else(|| self.run_model.clone());
                    let price = self.price(view, on.as_deref());
                    if f.action != FailureAction::FixConfig {
                        let why = format!(
                            "{} {how} on the {}, not the code ({}): another attempt would meet the same, so \
                             nothing starts: provide what is missing, then retry the run",
                            attempt_name(attempt),
                            f.class.name(),
                            f.evidence
                        );
                        return Some((
                            None,
                            self.decision(ScheduleAction::Hold, None, Some(attempt), why, price, view),
                        ));
                    }
                    if up.is_none() || self.escalations >= self.max_escalations {
                        let model = on.as_deref().unwrap_or("the run's model");
                        let why = format!(
                            "{why}; the model endpoint refused it ({}), and a new attempt on {model} would be refused \
                             the same way, so nothing starts: fix the configuration, then retry the run",
                            f.evidence
                        );
                        return Some((
                            None,
                            self.decision(ScheduleAction::Hold, None, Some(attempt), why, price, view),
                        ));
                    }
                    let model = up.map(|r| self.ladder[r].clone());
                    let why =
                        format!("{why}; the model endpoint refused it, so the next attempt runs on the next model up");
                    return Some(self.gated(ScheduleAction::Escalate, attempt, model, why, view));
                }
                if up.is_some() && self.escalations < self.max_escalations {
                    let model = up.map(|r| self.ladder[r].clone());
                    let why = format!("{why}; the next attempt runs on the next model up");
                    return Some(self.gated(ScheduleAction::Escalate, attempt, model, why, view));
                }
                let model = self.rung.map(|r| self.ladder[r].clone());
                let why = match up {
                    Some(_) => format!(
                        "{why}; the escalation cap ({}) is reached, so the next attempt takes another approach on \
                         the same model",
                        self.max_escalations
                    ),
                    None => format!("{why}; the next attempt takes another approach on the same model"),
                };
                Some(self.gated(ScheduleAction::Retry, attempt, model, why, view))
            }
            Evidence::Heard(Signal::CheckFailed { repeated: false, .. }) => None,
            Evidence::Heard(signal) => {
                let (attempt, why) = match signal {
                    Signal::CheckFailed { attempt, .. } => (
                        attempt,
                        format!(
                            "{}'s done-check failed twice in a row with the same output: it is making no progress",
                            attempt_name(attempt)
                        ),
                    ),
                    Signal::ToolErrors { attempt, turns } => {
                        (attempt, format!("every tool call of {}'s last {turns} turns failed", attempt_name(attempt)))
                    }
                };
                // Stale, already acted on, or not alone: nothing to add.
                if view.won
                    || !self.running.contains(&attempt)
                    || self.running.len() > 1
                    || self.stuck.contains(&attempt)
                {
                    return None;
                }
                self.stuck.insert(attempt);
                let up = self.next_rung();
                let model = up.or(self.rung).map(|r| self.ladder[r].clone());
                let why = match up {
                    Some(_) => format!("{why}; another attempt starts alongside it, on the next model up"),
                    None => format!("{why}; another attempt starts alongside it, with another approach"),
                };
                if self.escalations >= self.max_escalations {
                    let why = format!("{why}, but the escalation cap ({}) is reached", self.max_escalations);
                    let price = self.price(view, model.as_deref());
                    return Some((None, self.decision(ScheduleAction::Hold, None, Some(attempt), why, price, view)));
                }
                Some(self.gated(ScheduleAction::Parallel, attempt, model, why, view))
            }
        }
    }

    fn ended(
        &mut self,
        attempt: u32,
        status: AttemptStatus,
        cost_usd: f64,
        note: &str,
        failure: Option<&Failure>,
        branch: Option<&Branch>,
    ) {
        self.running.remove(&attempt);
        self.costs.insert(attempt, cost_usd);
        if matches!(status, AttemptStatus::Failed | AttemptStatus::Error) {
            let model = self.models.get(&attempt).cloned().flatten();
            if failure.is_some_and(|f| f.action == FailureAction::FixConfig) {
                self.refused.insert(model.clone());
            }
            let on = match model.or_else(|| self.run_model.clone()) {
                Some(m) => format!(" (on {m})"),
                None => String::new(),
            };
            // The same classified failure again is said once, not once for each attempt it ended.
            let known =
                failure.filter(|f| !f.evidence.is_empty() && f.class != molt_api::planner::FailureClass::Unknown);
            let head = format!("- {}{on}: {}", attempt_name(attempt), status_name(status));
            if let Some(f) = known {
                if let Some(first) = self.seen.get(&f.evidence) {
                    // What it changed still tells the next attempt a change not to make again.
                    let mut same = format!("{head}: the same as {}", attempt_name(*first));
                    if let Some(b) = branch {
                        same.push_str(&format!("\n{}", b.render(false)));
                    }
                    self.failures.push(escaped(&same));
                    return;
                }
                self.seen.insert(f.evidence.clone(), attempt);
            }
            let told = match branch {
                Some(b) => format!("{head}: {}\n{}", b.reason, b.render(true)),
                None => {
                    let note: String = note.chars().take(PRIOR_NOTE).collect();
                    format!("{head}: {}", note.trim())
                }
            };
            self.failures.push(escaped(&told));
        }
    }

    fn next_rung(&self) -> Option<usize> {
        self.rung.map(|r| r + 1).filter(|r| *r < self.ladder.len())
    }

    fn used_up(&self) -> Option<String> {
        (self.started >= self.max_attempts)
            .then(|| format!("the run may start {} attempts and has started them all", self.max_attempts))
    }

    /// Start an attempt for `action`, unless the run or its budget holds it back.
    fn gated(
        &mut self,
        action: ScheduleAction,
        after: u32,
        model: Option<String>,
        why: String,
        view: &View,
    ) -> (Option<Launch>, Scheduling) {
        let price = self.price(view, model.as_deref());
        let refused = (model.is_some() && self.refused.contains(&model)).then(|| {
            format!(
                "the model endpoint already refused {} in a way only the user can fix",
                model.as_deref().unwrap_or("")
            )
        });
        let held =
            view.stopped.clone().or(refused).or_else(|| self.used_up()).or_else(|| match view.budget.remaining_usd {
                None => Some(format!(
                    "the cost of {} calls is not known, so neither is what is left of the budget",
                    view.budget.unknown_calls
                )),
                Some(left) if left <= 0.0 || price.estimate_usd.is_some_and(|e| left < e) => Some(format!(
                    "${left:.2} of the budget is left after holding ${:.2} back for {} calls in flight, less than the \
                 ${:.2} a new attempt is assumed to cost",
                    view.budget.reserved_usd,
                    view.budget.in_flight_calls,
                    price.estimate_usd.unwrap_or(0.0)
                )),
                Some(_) => None,
            });
        match held {
            Some(held) => {
                let why = format!("{why}, but nothing starts: {held}");
                (None, self.decision(ScheduleAction::Hold, None, Some(after), why, price, view))
            }
            None => self.launch(action, Some(after), model, why, price, view),
        }
    }

    fn launch(
        &mut self,
        action: ScheduleAction,
        after: Option<u32>,
        model: Option<String>,
        why: String,
        price: PriceAssumption,
        view: &View,
    ) -> (Option<Launch>, Scheduling) {
        if matches!(action, ScheduleAction::Parallel | ScheduleAction::Escalate) {
            self.escalations += 1;
        }
        if let Some(at) = model.as_ref().and_then(|m| self.ladder.iter().position(|l| l == m)) {
            self.rung = Some(at);
        }
        let index = self.started;
        self.started += 1;
        self.running.insert(index);
        self.models.insert(index, model.clone());
        let prior = (!self.failures.is_empty()).then(|| {
            format!(
                "<earlier_attempts>\nEarlier attempts at this task did not pass. Each is listed with what it tried, \
                 the files its own copy changed (yours starts without those changes) and why it failed. Take another \
                 approach than theirs, and do not make a change that already failed again:\n{}\n</earlier_attempts>",
                self.failures.join("\n")
            )
        });
        let decision = self.decision(action, Some(index), after, why, price, view);
        (Some(Launch { index, model, prior }), decision)
    }

    fn decision(
        &self,
        action: ScheduleAction,
        attempt: Option<u32>,
        after: Option<u32>,
        reason: String,
        price: PriceAssumption,
        view: &View,
    ) -> Scheduling {
        let model = attempt.and_then(|a| self.models.get(&a).cloned().flatten()).or_else(|| self.run_model.clone());
        Scheduling {
            policy: ADAPTIVE_POLICY.to_owned(),
            action,
            attempt,
            after,
            model: attempt.and(model),
            reason,
            price,
            budget: view.budget.clone(),
            escalations: self.escalations,
            max_escalations: self.max_escalations,
        }
    }

    /// What a new attempt on `model` is assumed to cost: the most an attempt
    /// of this run cost, or what its attempts spent per attempt started,
    /// whichever is more.
    fn price(&self, view: &View, model: Option<&str>) -> PriceAssumption {
        let most = self.costs.values().copied().reduce(f64::max);
        let per_started =
            (self.started > 0 && view.attempts_spent > 0.0).then(|| view.attempts_spent / f64::from(self.started));
        let (estimate, mut basis) = match (most, per_started) {
            (Some(most), per) if most >= per.unwrap_or(0.0) => {
                (Some(most), "the most an attempt of this run has cost so far".to_owned())
            }
            (_, Some(per)) => (Some(per), "what this run's attempts have spent per attempt started".to_owned()),
            (most, None) => (most, "no attempt of this run has spent anything yet".to_owned()),
        };
        let ran = |m: &str| self.models.values().any(|v| v.as_deref() == Some(m));
        if let Some(m) = model.filter(|m| self.started > 0 && !ran(m)) {
            basis.push_str(&format!(
                ", on other models; {m} has not run in this run and the planner does not know its price, so an \
                 attempt on it may cost more"
            ));
        }
        basis.push_str(
            ". The gateway's costs are estimates, and the budget is checked before each call: no exact \
                        ceiling is guaranteed",
        );
        PriceAssumption { estimate_usd: estimate, basis }
    }
}

/// `text` that cannot close the block it is told in.
fn escaped(text: &str) -> String {
    text.replace("</earlier_attempts", "<\\/earlier_attempts")
}

fn first_line(note: &str) -> &str {
    let line = note.lines().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim();
    match line.char_indices().nth(200) {
        Some((at, _)) => &line[..at],
        None => line,
    }
}

fn status_name(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Passed => "passed",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn budget(remaining: Option<f64>) -> BudgetState {
        BudgetState {
            limit_usd: 10.0,
            spent_usd: 1.0,
            reserved_usd: 0.0,
            in_flight_calls: 0,
            unknown_calls: if remaining.is_none() { 1 } else { 0 },
            remaining_usd: remaining,
        }
    }

    fn view(remaining: Option<f64>) -> View {
        View { budget: budget(remaining), stopped: None, won: false, attempts_spent: 1.0 }
    }

    fn failed(attempt: u32, cost_usd: f64) -> Evidence {
        Evidence::Ended {
            attempt,
            status: AttemptStatus::Failed,
            cost_usd,
            note: "the done-check still failed".into(),
            failure: None,
            branch: None,
        }
    }

    fn classified(
        attempt: u32,
        status: AttemptStatus,
        class: FailureClass,
        action: FailureAction,
        evidence: &str,
    ) -> Evidence {
        let failure = Failure {
            class,
            source: molt_api::planner::FailureSource::Check,
            action,
            evidence: evidence.into(),
            exit_code: None,
        };
        Evidence::Ended {
            attempt,
            status,
            cost_usd: 0.25,
            note: format!("it failed: {evidence}"),
            failure: Some(failure),
            branch: None,
        }
    }

    use molt_api::planner::FailureClass;

    #[test]
    fn a_missing_dependency_holds_back_every_new_attempt_and_a_retry_of_the_run_starts_one() {
        let (mut s, _) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, None, false, 3);
        s.start(&view(Some(9.0)));
        let env = || {
            classified(
                0,
                AttemptStatus::Failed,
                FailureClass::Environment,
                FailureAction::FixEnvironment,
                "sh: 1: pytest: not found",
            )
        };
        let (launch, d) = s.decide(env(), &view(Some(9.0))).unwrap();
        assert!(launch.is_none(), "neither a bigger model nor another approach installs pytest");
        assert_eq!((d.action, d.after), (ScheduleAction::Hold, Some(0)));
        assert!(d.reason.contains("the environment, not the code (sh: 1: pytest: not found)"), "{}", d.reason);
        assert!(d.reason.contains("retry the run"), "{}", d.reason);
        assert_eq!(s.escalations(), 0);

        // The user installed it and retries the run: the checkpoint's failure does not stop that.
        let (mut s, _) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, None, false, 3);
        let failure = match env() {
            Evidence::Ended { failure, .. } => failure,
            _ => None,
        };
        let end = serde_json::json!({ "status": "failed", "note": "it failed", "failure": failure });
        let saved: SavedAttempt =
            serde_json::from_value(serde_json::json!({ "index": 0, "model": "haiku", "end": end })).unwrap();
        assert_eq!(saved.end.as_ref().unwrap().failure, failure, "the class survives the checkpoint");
        s.restore(&[saved], 0, true);
        let (launch, d) = s.start(&view(Some(9.0))).unwrap();
        assert_eq!((d.action, launch.unwrap().index), (ScheduleAction::Start, 1));
    }

    #[test]
    fn a_rejected_credential_is_not_retried_on_the_same_model_but_may_escalate_past_it() {
        let auth = |attempt| {
            classified(
                attempt,
                AttemptStatus::Error,
                FailureClass::Endpoint,
                FailureAction::FixConfig,
                "Denied: the Messages API returned 401: authentication_error: invalid x-api-key",
            )
        };
        // No ladder: every attempt would be on the same model and the same credential.
        let (mut s, _) = Adaptive::new(&[], 2, None, Some("sonnet"), false, 3);
        s.start(&view(Some(9.0)));
        let (launch, d) = s.decide(auth(0), &view(Some(9.0))).unwrap();
        assert!(launch.is_none());
        assert_eq!(d.action, ScheduleAction::Hold);
        assert!(d.reason.contains("a new attempt on sonnet would be refused the same way"), "{}", d.reason);

        // With a model up the ladder, which may be another endpoint: one escalation, and never back down.
        let (mut s, _) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, None, false, 4);
        s.start(&view(Some(9.0)));
        let (launch, d) = s.decide(auth(0), &view(Some(9.0))).unwrap();
        assert_eq!((d.action, launch.unwrap().model.as_deref()), (ScheduleAction::Escalate, Some("sonnet")));
        let (launch, d) = s.decide(auth(1), &view(Some(9.0))).unwrap();
        assert!(launch.is_none() && d.action == ScheduleAction::Hold, "{}", d.reason);
        assert!(d.reason.contains("on sonnet would be refused"), "{}", d.reason);
    }

    #[test]
    fn a_compiler_error_still_gets_another_attempt_and_a_repeated_failure_is_told_once() {
        let rustc = |attempt| {
            classified(
                attempt,
                AttemptStatus::Failed,
                FailureClass::Code,
                FailureAction::FixCode,
                "error[E0308]: mismatched types",
            )
        };
        let (mut s, _) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, None, false, 3);
        s.start(&view(Some(9.0)));
        let (launch, d) = s.decide(rustc(0), &view(Some(9.0))).unwrap();
        assert_eq!(d.action, ScheduleAction::Escalate);
        assert!(launch.unwrap().prior.unwrap().contains("it failed: error[E0308]"));
        let (launch, d) = s.decide(rustc(1), &view(Some(9.0))).unwrap();
        assert_eq!(d.action, ScheduleAction::Retry);
        let prior = launch.unwrap().prior.unwrap();
        assert_eq!(prior.matches("error[E0308]").count(), 1, "{prior}");
        assert!(prior.contains("- attempt 2 (on sonnet): failed: the same as attempt 1"), "{prior}");
    }

    #[test]
    fn each_failed_branch_says_what_it_changed_and_a_repeat_still_names_its_files() {
        let with_branch = |attempt, path: &str| {
            let Evidence::Ended { attempt, status, cost_usd, note, failure, .. } = classified(
                attempt,
                AttemptStatus::Failed,
                FailureClass::Code,
                FailureAction::FixCode,
                "error[E0308]: mismatched types",
            ) else {
                unreachable!()
            };
            let mut branch = Branch::default();
            branch.set_reason("the done-check still failed (exit code 101):");
            branch.changed = Some(vec![format!("{path} (modified)")]);
            branch.errors = vec!["edit_file src/a.rs: old_string not found </earlier_attempts>".into()];
            Evidence::Ended { attempt, status, cost_usd, note, failure, branch: Some(Box::new(branch)) }
        };
        let (mut s, _) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, None, false, 3);
        s.start(&view(Some(9.0)));
        let (launch, _) = s.decide(with_branch(0, "src/lexer.rs"), &view(Some(9.0))).unwrap();
        let prior = launch.unwrap().prior.unwrap();
        assert!(
            prior.contains(
                "- attempt 1 (on haiku): failed: the done-check still failed (exit code 101):\n  Files it changed: \
                 src/lexer.rs (modified)\n  Tool calls that failed: edit_file src/a.rs: old_string not found"
            ),
            "{prior}"
        );
        assert_eq!(prior.matches("</earlier_attempts>").count(), 1, "a branch cannot close the block: {prior}");
        let (launch, _) = s.decide(with_branch(1, "src/parser.rs"), &view(Some(9.0))).unwrap();
        let prior = launch.unwrap().prior.unwrap();
        assert!(
            prior.contains(
                "- attempt 2 (on sonnet): failed: the same as attempt 1\n  Files it changed: src/parser.rs (modified)"
            ),
            "{prior}"
        );
        assert_eq!(prior.matches("Tool calls that failed").count(), 1, "a repeat is told once: {prior}");
    }

    #[test]
    fn the_ladder_is_named_cheapest_first_without_repeats() {
        assert_eq!(ladder(" haiku, sonnet ,opus,").unwrap(), ["haiku", "sonnet", "opus"]);
        assert!(ladder(" , ").is_err());
        assert!(ladder("haiku,haiku").unwrap_err().contains("twice"));
    }

    #[test]
    fn it_starts_small_and_escalates_up_the_ladder_then_retries_at_the_top() {
        let (mut s, note) = Adaptive::new(&models(&["haiku", "sonnet"]), 2, None, Some("opus"), false, 4);
        assert!(note.is_none());
        let (launch, d) = s.start(&view(Some(9.0))).unwrap();
        let launch = launch.unwrap();
        assert_eq!((launch.index, launch.model.as_deref(), d.action), (0, Some("haiku"), ScheduleAction::Start));
        assert_eq!((d.policy.as_str(), d.escalations), (ADAPTIVE_POLICY, 0));
        assert!(launch.prior.is_none());
        assert!(s.start(&view(Some(9.0))).is_none(), "it starts once");

        let (launch, d) = s.decide(failed(0, 1.0), &view(Some(9.0))).unwrap();
        let launch = launch.unwrap();
        assert_eq!((d.action, d.after, d.attempt), (ScheduleAction::Escalate, Some(0), Some(1)));
        assert_eq!((launch.model.as_deref(), d.model.as_deref(), d.escalations), (Some("sonnet"), Some("sonnet"), 1));
        assert!(d.reason.contains("attempt 1 failed: the done-check still failed"), "{}", d.reason);
        assert_eq!(d.price.estimate_usd, Some(1.0));
        assert!(d.price.basis.contains("sonnet has not run"), "{}", d.price.basis);
        let prior = launch.prior.unwrap();
        assert!(prior.contains("- attempt 1 (on haiku): failed: the done-check still failed"), "{prior}");

        // At the top of the ladder: the same model, another approach, no escalation.
        let (launch, d) = s.decide(failed(1, 2.0), &view(Some(7.0))).unwrap();
        assert_eq!(
            (d.action, launch.unwrap().model.as_deref(), d.escalations),
            (ScheduleAction::Retry, Some("sonnet"), 1)
        );
        assert_eq!(d.price.estimate_usd, Some(2.0));
    }

    #[test]
    fn escalations_stop_at_the_cap_and_attempts_at_the_runs_count() {
        let (mut s, _) = Adaptive::new(&models(&["a", "b", "c"]), 1, None, None, false, 3);
        s.start(&view(Some(9.0)));
        let (_, d) = s.decide(failed(0, 0.5), &view(Some(9.0))).unwrap();
        assert_eq!((d.action, d.model.as_deref()), (ScheduleAction::Escalate, Some("b")));
        let (launch, d) = s.decide(failed(1, 0.5), &view(Some(9.0))).unwrap();
        assert_eq!((d.action, launch.unwrap().model.as_deref()), (ScheduleAction::Retry, Some("b")));
        assert!(d.reason.contains("escalation cap (1) is reached"), "{}", d.reason);
        let (launch, d) = s.decide(failed(2, 0.5), &view(Some(9.0))).unwrap();
        assert!(launch.is_none());
        assert_eq!((d.action, d.after), (ScheduleAction::Hold, Some(2)));
        assert!(d.reason.contains("may start 3 attempts and has started them all"), "{}", d.reason);
    }

    #[test]
    fn no_progress_adds_one_attempt_alongside_and_stale_evidence_decides_nothing() {
        let (mut s, _) = Adaptive::new(&models(&["a", "b"]), 2, None, None, false, 3);
        s.start(&view(Some(9.0)));
        let first = Evidence::Heard(Signal::CheckFailed { attempt: 0, repeated: false });
        assert!(s.decide(first, &view(Some(9.0))).is_none(), "a failed check goes back to the attempt");
        let stuck = Evidence::Heard(Signal::CheckFailed { attempt: 0, repeated: true });
        let (launch, d) = s.decide(stuck.clone(), &view(Some(9.0))).unwrap();
        assert_eq!((d.action, launch.unwrap().model.as_deref()), (ScheduleAction::Parallel, Some("b")));
        assert!(d.reason.contains("making no progress"), "{}", d.reason);
        assert!(s.decide(stuck, &view(Some(9.0))).is_none(), "acted on once");
        let tools = Evidence::Heard(Signal::ToolErrors { attempt: 1, turns: 3 });
        assert!(s.decide(tools, &view(Some(9.0))).is_none(), "two are going already");
        // Attempt 0 ends while attempt 1 is going: nothing starts.
        assert!(s.decide(failed(0, 1.0), &view(Some(9.0))).is_none());
        // Evidence from an attempt that ended, or after one passed, is stale.
        let late = Evidence::Heard(Signal::ToolErrors { attempt: 0, turns: 3 });
        assert!(s.decide(late, &view(Some(9.0))).is_none());
        let won = View { won: true, ..view(Some(9.0)) };
        assert!(s.decide(failed(1, 1.0), &won).is_none());
    }

    #[test]
    fn the_budget_and_a_stop_hold_back_new_attempts_and_say_why() {
        let start = |s: &mut Adaptive| s.start(&view(Some(9.0))).unwrap();
        let (mut s, _) = Adaptive::new(&[], 2, None, Some("sonnet"), false, 3);
        start(&mut s);
        let (launch, d) = s.decide(failed(0, 1.0), &view(None)).unwrap();
        assert!(launch.is_none() && d.action == ScheduleAction::Hold);
        assert!(d.reason.contains("cost of 1 calls is not known"), "{}", d.reason);
        assert_eq!((d.budget.unknown_calls, d.budget.remaining_usd), (1, None));

        let (mut s, _) = Adaptive::new(&[], 2, None, None, false, 3);
        start(&mut s);
        let tight = View {
            budget: BudgetState { reserved_usd: 0.25, in_flight_calls: 2, ..budget(Some(0.5)) },
            ..view(Some(0.5))
        };
        let (launch, d) = s.decide(failed(0, 1.0), &tight).unwrap();
        assert!(launch.is_none());
        assert!(d.reason.contains("$0.50 of the budget is left after holding $0.25 back for 2 calls"), "{}", d.reason);

        let (mut s, _) = Adaptive::new(&[], 2, None, None, false, 3);
        start(&mut s);
        let stopped = View { stopped: Some("the run was stopped".into()), ..view(Some(9.0)) };
        let (launch, d) = s.decide(failed(0, 1.0), &stopped).unwrap();
        assert!(launch.is_none() && d.reason.ends_with("nothing starts: the run was stopped"), "{}", d.reason);
        // Without a ladder there is no model to name but the run's.
        assert_eq!(d.model, None);
    }

    #[test]
    fn the_requests_model_starts_the_ladder_or_keeps_every_attempt_on_it() {
        let ladder = models(&["haiku", "sonnet", "opus"]);
        let (mut s, note) = Adaptive::new(&ladder, 2, Some("sonnet"), Some("sonnet"), false, 2);
        assert!(note.is_none());
        assert_eq!(s.start(&view(Some(9.0))).unwrap().0.unwrap().model.as_deref(), Some("sonnet"));
        let (mut s, note) = Adaptive::new(&ladder, 2, Some("gpt-x"), Some("gpt-x"), false, 2);
        assert!(note.unwrap().contains("not among the escalation models"));
        let (launch, d) = s.start(&view(Some(9.0))).unwrap();
        assert_eq!((launch.unwrap().model, d.model.as_deref()), (None, Some("gpt-x")));
        let (_, note) = Adaptive::new(&ladder, 2, None, None, true, 2);
        assert!(note.unwrap().contains("role names its model"));
    }
}

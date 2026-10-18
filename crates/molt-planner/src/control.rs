//! Steering a run while it goes (`planner.control`): the versions of what
//! it must do, and whether it may start new work.
//!
//! The task is version 0 of a run's requirements; each constraint added
//! while it runs is the next version. An attempt takes up a new version at
//! its next step, before its next model call, and says so. It can win only
//! under the latest version: an attempt whose check passed while a newer
//! one came in goes over its work again first. The first attempt to win
//! freezes the requirements, so nothing added after it can be left out of
//! the result.
//!
//! The answer to an attempt's question to the person (`ask_user`) is a
//! version like any other: one that names the question it answers.
//!
//! A paused run starts no new model call, command or check. Calls already
//! running cannot be stopped: they finish, and the run's state says which
//! they are until they have.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use molt_api::planner::{Constraint, ControlResponse, Requirement, RunState};
use molt_api::progress::Progress;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::checkpoint::Store;

/// Constraints a run takes.
const MAX_VERSIONS: usize = 50;
/// Bytes of one constraint's text.
const MAX_TEXT: usize = 8_000;
/// Paths one constraint protects.
const MAX_PROTECT: usize = 100;

pub(crate) struct Control {
    state: Mutex<State>,
    /// Bumped on every change, for [`Control::wait_go`] and the state reporter.
    changed: watch::Sender<u64>,
    store: Mutex<Option<Arc<Store>>>,
}

#[derive(Default)]
struct State {
    /// Version `n` is `versions[n - 1]`.
    versions: Vec<Requirement>,
    /// Why no constraint is taken any more, and the version a winner was
    /// found under.
    frozen: Option<(String, Option<u32>)>,
    /// The run has a done-check (a revised check needs one), once known.
    checked: Option<bool>,
    /// The check's own files, which no constraint can protect.
    check_files: Vec<String>,
    paused: bool,
    /// Questions asked so far; question `n` is the `n`th.
    questions: u32,
    busy: BTreeMap<u64, String>,
    next_busy: u64,
}

impl Control {
    pub fn new(versions: Vec<Requirement>) -> Arc<Self> {
        // A resumed run numbers its new questions after those answered before.
        let questions = versions.iter().filter_map(|r| r.constraint.answers).max().unwrap_or(0);
        Arc::new(Self {
            state: Mutex::new(State { versions, questions, ..State::default() }),
            changed: watch::Sender::new(0),
            store: Mutex::new(None),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Save every new version in the run's checkpoint.
    pub fn keep_in(&self, store: Option<Arc<Store>>) {
        *self.store.lock().unwrap_or_else(PoisonError::into_inner) = store;
    }

    /// The check is settled: whether there is one, and its files.
    pub fn settle(&self, checked: bool, files: Vec<String>) {
        let mut state = self.state();
        state.checked = Some(checked);
        state.check_files = files;
    }

    pub fn version(&self) -> u32 {
        self.state().versions.len() as u32
    }

    /// Every version after the task, in order.
    pub fn all(&self) -> Vec<Requirement> {
        self.state().versions.clone()
    }

    /// The versions after `version`, up to the latest.
    pub fn since(&self, version: u32) -> Vec<Requirement> {
        self.state().versions.iter().skip(version as usize).cloned().collect()
    }

    /// The versions up to and including `version`.
    pub fn upto(&self, version: u32) -> Vec<Requirement> {
        self.state().versions.iter().take(version as usize).cloned().collect()
    }

    /// The paths protected as of `version`, without the check's own files.
    pub fn protected(&self, version: u32) -> Vec<String> {
        let state = self.state();
        let mut paths: Vec<String> = state
            .versions
            .iter()
            .take(version as usize)
            .flat_map(|r| r.constraint.protect.iter().cloned())
            .filter(|p| !state.check_files.contains(p))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// The check command as of `version`, when a constraint replaced the run's own.
    pub fn check(&self, version: u32) -> Option<String> {
        self.state().versions.iter().take(version as usize).rev().find_map(|r| r.constraint.check.clone())
    }

    /// Take `constraint` as the next version.
    pub fn add(&self, mut constraint: Constraint) -> Result<Requirement, String> {
        constraint.text = constraint.text.trim().to_owned();
        constraint.check = constraint.check.map(|c| c.trim().to_owned());
        let mut protect = Vec::new();
        for path in &constraint.protect {
            protect.push(plain_path(path)?);
        }
        constraint.protect = protect;
        if constraint.answers.is_some() && constraint.text.is_empty() {
            return Err("the answer is empty: give its text".into());
        }
        if constraint.text.is_empty() && constraint.protect.is_empty() && constraint.check.is_none() {
            return Err("the constraint says nothing: give its text, paths to protect or a check".into());
        }
        if constraint.text.len() > MAX_TEXT {
            return Err(format!("the constraint's text is longer than {MAX_TEXT} bytes"));
        }
        if constraint.protect.len() > MAX_PROTECT {
            return Err(format!("a constraint protects at most {MAX_PROTECT} paths"));
        }
        if constraint.check.as_ref().is_some_and(|c| c.is_empty()) {
            return Err("the new check is empty".into());
        }
        let requirement = {
            let mut state = self.state();
            if let Some((why, _)) = &state.frozen {
                return Err(format!("too late to change the run: {why}"));
            }
            if state.versions.len() >= MAX_VERSIONS {
                return Err(format!("a run takes at most {MAX_VERSIONS} constraints"));
            }
            if constraint.check.is_some() && state.checked == Some(false) {
                return Err("the run has no done-check to replace: it runs unverified".into());
            }
            if let Some(path) = constraint.protect.iter().find(|p| state.check_files.contains(p)) {
                return Err(format!("{path} is a file of the run's done-check, which the run restores itself"));
            }
            if let Some(question) = constraint.answers {
                if question == 0 || question > state.questions {
                    return Err(format!("no question {question} was asked in this run"));
                }
                if state.versions.iter().any(|r| r.constraint.answers == Some(question)) {
                    return Err(format!("question {question} is answered already"));
                }
            }
            let requirement = Requirement {
                version: state.versions.len() as u32 + 1,
                constraint,
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            };
            state.versions.push(requirement.clone());
            requirement
        };
        if let Some(store) = self.store.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            let versions = self.all();
            if let Err(e) = store.update(|s| s.requirements = versions) {
                tracing::warn!(error = %e, "could not save the run's new requirements");
            }
        }
        self.bump();
        Ok(requirement)
    }

    /// May a result found under `version` be the run's? Only under the
    /// latest one. The first yes freezes the requirements; any other attempt
    /// that passed under the same version may win as well.
    pub fn claim(&self, version: u32) -> bool {
        let mut state = self.state();
        match &state.frozen {
            Some((_, at)) => *at == Some(version),
            None if version as usize == state.versions.len() => {
                state.frozen = Some(("an attempt has passed and its result is being applied".into(), Some(version)));
                true
            }
            None => false,
        }
    }

    /// Take no constraint any more.
    pub fn freeze(&self, why: &str) {
        let mut state = self.state();
        if state.frozen.is_none() {
            state.frozen = Some((why.to_owned(), None));
        }
    }

    /// Number a new question, at most `max` in the run. An error says why
    /// none can be asked: the person can no longer change the run, or the
    /// run has asked as many as it may.
    pub fn ask(&self, max: u32) -> Result<u32, String> {
        let mut state = self.state();
        if let Some((why, _)) = &state.frozen {
            return Err(format!("the run takes no answer any more: {why}"));
        }
        if state.questions >= max {
            return Err(format!("the run has asked as many questions as it may ({max})"));
        }
        state.questions += 1;
        Ok(state.questions)
    }

    /// The version that answers `question`, once there is one; `None` when
    /// `timeout` passed or `cancel` fired first.
    pub async fn answer(&self, question: u32, timeout: Duration, cancel: &CancellationToken) -> Option<Requirement> {
        let mut changed = self.changed.subscribe();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            let found = self.state().versions.iter().find(|r| r.constraint.answers == Some(question)).cloned();
            if found.is_some() {
                return found;
            }
            tokio::select! {
                _ = changed.changed() => {}
                () = &mut deadline => return None,
                _ = cancel.cancelled() => return None,
            }
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.state().paused = paused;
        self.bump();
    }

    pub fn response(&self) -> ControlResponse {
        let state = self.state();
        let (run_state, waiting) = state.status();
        ControlResponse { state: run_state, version: state.versions.len() as u32, waiting }
    }

    /// Wait while the run is paused. False when `cancel` or `also` fired first.
    pub async fn wait_go(&self, cancel: &CancellationToken, also: &CancellationToken) -> bool {
        let mut changed = self.changed.subscribe();
        loop {
            if cancel.is_cancelled() || also.is_cancelled() {
                return false;
            }
            if !self.state().paused {
                return true;
            }
            tokio::select! {
                _ = changed.changed() => {}
                _ = cancel.cancelled() => return false,
                _ = also.cancelled() => return false,
            }
        }
    }

    /// Something that runs and cannot be stopped at once, until the guard drops.
    pub fn busy(self: &Arc<Self>, what: String) -> Busy {
        let id = {
            let mut state = self.state();
            let id = state.next_busy;
            state.next_busy += 1;
            state.busy.insert(id, what);
            id
        };
        self.bump();
        Busy { control: self.clone(), id }
    }

    /// Publish the run's state each time it changes, until `stop`.
    pub fn report(self: &Arc<Self>, run: String, publish: impl Fn(Progress) + Send + 'static, stop: CancellationToken) {
        let control = self.clone();
        let mut changed = self.changed.subscribe();
        tokio::spawn(async move {
            let mut last = (RunState::Running, Vec::new());
            loop {
                let now = control.state().status();
                if now != last {
                    publish(Progress::State { run: run.clone(), state: now.0, waiting: now.1.clone() });
                    last = now;
                }
                tokio::select! {
                    _ = changed.changed() => {}
                    _ = stop.cancelled() => return,
                }
            }
        });
    }
}

impl State {
    fn status(&self) -> (RunState, Vec<String>) {
        if !self.paused {
            return (RunState::Running, Vec::new());
        }
        let waiting: Vec<String> = self.busy.values().cloned().collect();
        let state = if waiting.is_empty() { RunState::Paused } else { RunState::Pausing };
        (state, waiting)
    }
}

pub(crate) struct Busy {
    control: Arc<Control>,
    id: u64,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.control.state().busy.remove(&self.id);
        self.control.bump();
    }
}

/// A workspace-relative path without `.` or `..`, and without a trailing slash.
fn plain_path(path: &str) -> Result<String, String> {
    let trimmed = path.trim().trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if trimmed.starts_with('/') || parts.is_empty() || parts.contains(&"..") {
        return Err(format!("{path:?} is not a path inside the workspace"));
    }
    Ok(parts.join("/"))
}

/// `path` is `protected` or inside it.
pub(crate) fn covered(path: &str, protected: &str) -> bool {
    path == protected || path.strip_prefix(protected).is_some_and(|rest| rest.starts_with('/'))
}

/// The runs going on in this process, by run id.
fn runs() -> &'static Mutex<HashMap<String, Arc<Control>>> {
    static RUNS: OnceLock<Mutex<HashMap<String, Arc<Control>>>> = OnceLock::new();
    RUNS.get_or_init(Mutex::default)
}

/// Make the run `run` steerable until the guard drops.
pub(crate) fn register(run: &str, control: Arc<Control>) -> Registered {
    runs().lock().unwrap_or_else(PoisonError::into_inner).insert(run.to_owned(), control);
    Registered(run.to_owned())
}

pub(crate) fn find(run: &str) -> Option<Arc<Control>> {
    runs().lock().unwrap_or_else(PoisonError::into_inner).get(run).cloned()
}

pub(crate) struct Registered(String);

impl Drop for Registered {
    fn drop(&mut self) {
        runs().lock().unwrap_or_else(PoisonError::into_inner).remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Constraint {
        Constraint { text: t.into(), ..Constraint::default() }
    }

    #[test]
    fn versions_count_up_and_a_win_freezes_them() {
        let control = Control::new(Vec::new());
        control.settle(true, vec!["tests/t.rs".into()]);
        assert_eq!(control.version(), 0);
        let v1 = control.add(Constraint { protect: vec!["./src/api.rs".into()], ..text("leave the API alone") });
        assert_eq!(v1.unwrap().version, 1);
        assert_eq!(control.protected(1), ["src/api.rs"]);
        assert!(control.protected(0).is_empty());
        assert!(control.add(Constraint { protect: vec!["../x".into()], ..Constraint::default() }).is_err());
        assert!(control.add(Constraint { protect: vec!["tests/t.rs".into()], ..Constraint::default() }).is_err());
        assert!(control.add(Constraint::default()).is_err());
        control.add(Constraint { check: Some("make test".into()), ..Constraint::default() }).unwrap();
        assert_eq!(control.check(1), None);
        assert_eq!(control.check(2).as_deref(), Some("make test"));
        assert_eq!(control.since(1).len(), 1);

        // Found under version 1 while version 2 is the latest: not a winner.
        assert!(!control.claim(1));
        assert!(control.claim(2));
        // Another attempt that passed under the same version may win too.
        assert!(control.claim(2));
        assert!(control.add(text("too late")).unwrap_err().contains("too late"));
    }

    #[test]
    fn an_unverified_run_takes_no_new_check() {
        let control = Control::new(Vec::new());
        control.settle(false, Vec::new());
        assert!(control.add(Constraint { check: Some("true".into()), ..Constraint::default() }).is_err());
        assert!(control.add(text("answer in French")).is_ok());
    }

    #[tokio::test]
    async fn pausing_waits_for_what_runs_and_lets_go_on_resume() {
        let control = Control::new(Vec::new());
        let busy = control.busy("attempt 0: model call".into());
        control.set_paused(true);
        let r = control.response();
        assert_eq!(
            (r.state, r.waiting.as_slice()),
            (RunState::Pausing, ["attempt 0: model call".to_owned()].as_slice())
        );
        drop(busy);
        assert_eq!(control.response().state, RunState::Paused);

        let (never, cancel) = (CancellationToken::new(), CancellationToken::new());
        let waiting = {
            let control = control.clone();
            let (never, cancel) = (never.clone(), cancel.clone());
            tokio::spawn(async move { control.wait_go(&cancel, &never).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        control.set_paused(false);
        assert!(waiting.await.unwrap());

        control.set_paused(true);
        cancel.cancel();
        assert!(!control.wait_go(&cancel, &never).await);
    }

    #[tokio::test]
    async fn an_answer_is_a_version_that_names_its_question() {
        let control = Control::new(Vec::new());
        let answer = |question, t: &str| Constraint { answers: Some(question), ..text(t) };
        assert!(control.add(answer(1, "yes")).unwrap_err().contains("no question 1"));
        assert_eq!(control.ask(2), Ok(1));
        assert!(control.add(answer(1, " ")).unwrap_err().contains("empty"));

        let waiting = {
            let control = control.clone();
            tokio::spawn(async move { control.answer(1, Duration::from_secs(60), &CancellationToken::new()).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        // Steering of its own does not answer the question.
        control.add(text("answer in French")).unwrap();
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert_eq!(control.add(answer(1, "use tabs")).unwrap().version, 2);
        assert_eq!(waiting.await.unwrap().map(|r| r.constraint.text).as_deref(), Some("use tabs"));
        assert!(control.add(answer(1, "spaces")).unwrap_err().contains("answered already"));

        assert_eq!(control.ask(2), Ok(2));
        assert!(control.ask(2).unwrap_err().contains("as many questions as it may (2)"));
        assert_eq!(control.answer(2, Duration::from_millis(10), &CancellationToken::new()).await, None);
        // A resumed run numbers its questions on from the answered ones.
        assert_eq!(Control::new(control.all()).ask(5), Ok(2));
    }

    #[test]
    fn paths_cover_what_is_inside_them() {
        assert!(covered("src/api.rs", "src/api.rs"));
        assert!(covered("src/api/mod.rs", "src/api"));
        assert!(!covered("src/api2.rs", "src/api"));
    }
}

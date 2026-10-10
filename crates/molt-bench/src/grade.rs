//! Grading a workspace, and validating that a task grades correctly.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::proc::{self, Ran};
use crate::task::{self, Task};

/// Bytes of the check's output kept in a result.
pub const OUTPUT_KEPT: usize = 4000;

/// The result of grading one workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The end of the check's output.
    pub output: String,
}

impl Grade {
    /// The grade of a run that could not be graded.
    pub fn ungraded(why: String) -> Grade {
        Grade { passed: false, exit_code: None, timed_out: false, duration_ms: 0, output: why }
    }
}

impl From<Ran> for Grade {
    fn from(ran: Ran) -> Self {
        Grade {
            passed: ran.success(),
            exit_code: ran.exit_code,
            timed_out: ran.timed_out,
            duration_ms: ran.duration.as_millis() as u64,
            output: proc::tail_of(&ran.output, OUTPUT_KEPT).to_owned(),
        }
    }
}

/// Lay the task's hidden files over `workspace`, replacing what the agent
/// left at those paths, remove the compiled Python it left (which Python
/// could run in place of the hidden tests' source), and run the task's
/// check there.
pub async fn grade(task: &Task, workspace: &Path) -> anyhow::Result<Grade> {
    task::overlay(&task.hidden(), workspace).context("laying the hidden files over the workspace")?;
    remove_bytecode(workspace).context("removing compiled Python from the workspace")?;
    let ran = proc::shell(workspace, &task.spec.check, task.timeout()).await.context("running the check")?;
    Ok(ran.into())
}

/// Remove every `__pycache__` directory under `dir`, without following links.
fn remove_bytecode(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.file_name() == "__pycache__" {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            remove_bytecode(&entry.path())?;
        }
    }
    Ok(())
}

/// One finding of [`validate`].
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub ok: bool,
    pub what: String,
}

/// What [`validate`] found about one task.
#[derive(Clone, Debug)]
pub struct Validation {
    pub task: String,
    pub findings: Vec<Finding>,
}

impl Validation {
    pub fn ok(&self) -> bool {
        self.findings.iter().all(|f| f.ok)
    }

    fn found(&mut self, ok: bool, what: impl Into<String>) {
        self.findings.push(Finding { ok, what: what.into() });
    }
}

/// Check that `task` is sound: its hidden files are tests or restored copies
/// of the repo's files; the repo's own tests pass on the starting repo and
/// on the reference solution; and the check fails on the starting repo and
/// passes on the solution. Each runs in a fresh copy outside any project.
pub async fn validate(task: &Task) -> Validation {
    let mut v = Validation { task: task.id.clone(), findings: Vec::new() };
    if let Err(e) = layout(task, &mut v) {
        v.found(false, format!("{e:#}"));
        return v;
    }
    // What runs, its verbs, whether on the solution, with the hidden files, and whether it should pass.
    let runs = [
        ("the repo's own tests", ("pass", "fail"), false, false, true),
        ("the repo's own tests", ("pass", "fail"), true, false, true),
        ("the check", ("passes", "fails"), false, true, false),
        ("the check", ("passes", "fails"), true, true, true),
    ];
    for (what, (pass, fail), solved, hidden, should_pass) in runs {
        let on = if solved { "the solution" } else { "the starting repo" };
        let ran = match prepared_run(task, solved, hidden).await {
            Ok(ran) => ran,
            Err(e) => {
                v.found(false, format!("{what} on {on}: {e:#}"));
                continue;
            }
        };
        let passed = ran.success();
        let secs = format!("{:.1}s", ran.duration.as_secs_f64());
        match (should_pass, passed) {
            (true, true) => v.found(true, format!("{what} {pass} on {on} ({secs})")),
            (false, false) if !ran.timed_out => v.found(true, format!("{what} {fail} on {on} ({})", ran.ending())),
            (false, false) => v.found(false, format!("{what} timed out on {on}; it must fail, and quickly")),
            (true, false) => v.found(
                false,
                format!("{what} {fail} on {on} ({}):\n{}", ran.ending(), proc::tail_of(&ran.output, 1500).trim_end()),
            ),
            (false, true) => v.found(false, format!("{what} {pass} on {on}, so it does not detect the task")),
        }
    }
    v
}

/// Hidden files are tests named `bench_hidden`, or copies of repo files
/// that the grade restores; at least one is a test.
fn layout(task: &Task, v: &mut Validation) -> anyhow::Result<()> {
    let repo = task::files(&task.repo())?;
    let hidden = task::files(&task.hidden())?;
    let strays: Vec<String> = hidden
        .iter()
        .filter(|p| !p.to_string_lossy().contains("bench_hidden") && !repo.contains(p))
        .map(|p| task::slashed(p))
        .collect();
    if strays.is_empty() {
        v.found(true, format!("{} hidden files", hidden.len()));
    } else {
        v.found(
            false,
            format!("hidden files that are neither bench_hidden tests nor repo files: {}", strays.join(", ")),
        );
    }
    if !hidden.iter().any(|p| p.to_string_lossy().contains("bench_hidden")) {
        v.found(false, "no hidden file has bench_hidden in its name");
    }
    Ok(())
}

/// A fresh copy of the repo, with the solution and the hidden files laid
/// over it as asked, and the tests or the check run in it.
async fn prepared_run(task: &Task, solved: bool, hidden: bool) -> anyhow::Result<Ran> {
    let tmp = tempfile::Builder::new().prefix("molt-bench-validate-").tempdir()?;
    let ws = tmp.path().join("ws");
    task::copy_tree(&task.repo(), &ws)?;
    if solved {
        task::overlay(&task.solution(), &ws)?;
    }
    if hidden {
        task::overlay(&task.hidden(), &ws)?;
    }
    let command = if hidden { &task.spec.check } else { &task.spec.tests };
    Ok(proc::shell(&ws, command, task.timeout()).await?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testing::task_dir;

    #[tokio::test]
    async fn a_sound_task_validates() {
        let root = tempfile::tempdir().unwrap();
        let task = Task::load(&task_dir(root.path(), "py-hello")).unwrap();
        let v = validate(&task).await;
        assert!(v.ok(), "{:#?}", v.findings);
        assert_eq!(v.findings.len(), 5);
    }

    #[tokio::test]
    async fn a_check_that_passes_from_the_start_or_never_is_caught() {
        let root = tempfile::tempdir().unwrap();
        let dir = task_dir(root.path(), "py-always");
        fs::write(dir.join("repo/hello.txt"), "hi\n").unwrap();
        let v = validate(&Task::load(&dir).unwrap()).await;
        assert!(!v.ok());
        assert!(v.findings.iter().any(|f| !f.ok && f.what.contains("does not detect the task")), "{:#?}", v.findings);

        let dir = task_dir(root.path(), "py-never");
        fs::write(dir.join("solution/hello.txt"), "bye\n").unwrap();
        let v = validate(&Task::load(&dir).unwrap()).await;
        assert!(
            v.findings.iter().any(|f| !f.ok && f.what.starts_with("the check fails on the solution")),
            "{:#?}",
            v.findings
        );

        let dir = task_dir(root.path(), "py-stray");
        fs::write(dir.join("hidden/notes.txt"), "?").unwrap();
        let v = validate(&Task::load(&dir).unwrap()).await;
        assert!(v.findings.iter().any(|f| !f.ok && f.what.contains("notes.txt")), "{:#?}", v.findings);
    }

    #[tokio::test]
    async fn grading_overwrites_the_agents_copy_of_a_hidden_file() {
        let root = tempfile::tempdir().unwrap();
        let task = Task::load(&task_dir(root.path(), "py-hello")).unwrap();
        let ws = root.path().join("ws");
        task::copy_tree(&task.repo(), &ws).unwrap();
        // The agent wrote a "test" with the hidden test's name that always passes, but not the work.
        fs::write(ws.join("test_bench_hidden.sh"), "true\n").unwrap();
        let g = grade(&task, &ws).await.unwrap();
        assert!(!g.passed, "{g:?}");
        assert_eq!(fs::read_to_string(ws.join("test_bench_hidden.sh")).unwrap(), "grep -q hi hello.txt\n");
        fs::write(ws.join("hello.txt"), "hi\n").unwrap();
        assert!(grade(&task, &ws).await.unwrap().passed);
    }

    #[tokio::test]
    async fn compiled_python_the_agent_left_is_removed_before_grading() {
        let root = tempfile::tempdir().unwrap();
        let task = Task::load(&task_dir(root.path(), "py-hello")).unwrap();
        let ws = root.path().join("ws");
        task::copy_tree(&task.repo(), &ws).unwrap();
        crate::testing::write(&ws.join("tests/__pycache__/test_bench_hidden.cpython-311.pyc"), "planted");
        crate::testing::write(&ws.join("__pycache__/x.pyc"), "planted");
        grade(&task, &ws).await.unwrap();
        assert!(!ws.join("tests/__pycache__").exists() && !ws.join("__pycache__").exists());
        assert!(ws.join("README.md").exists());
    }
}

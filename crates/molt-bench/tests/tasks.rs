//! Every task in `bench/tasks` is sound: its hidden check fails on the
//! starting repo and passes on the reference solution, and the repo's own
//! tests pass on both. A language whose toolchain is missing here is
//! skipped, except on CI, where every task must validate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use molt_bench::grade;
use molt_bench::task::{self, Language};

fn tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/tasks")
}

fn toolchain(language: Language) -> &'static str {
    match language {
        Language::Python => "python3",
        Language::Javascript => "node",
        Language::Rust => "cargo",
        Language::Go => "go",
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[tokio::test(flavor = "multi_thread")]
async fn every_shipped_task_validates() {
    let tasks = task::load_all(&tasks_dir()).unwrap();
    let on_ci = std::env::var_os("CI").is_some();
    let limit = Arc::new(tokio::sync::Semaphore::new(4));
    let mut set = tokio::task::JoinSet::new();
    for t in tasks {
        let tool = toolchain(t.spec.language);
        if !on_path(tool) {
            assert!(!on_ci, "{}: {tool} is not installed", t.id);
            eprintln!("skipping {}: {tool} is not installed", t.id);
            continue;
        }
        let limit = limit.clone();
        set.spawn(async move {
            let _permit = limit.acquire_owned().await;
            grade::validate(&t).await
        });
    }
    let mut invalid = Vec::new();
    while let Some(v) = set.join_next().await {
        let v = v.unwrap();
        if !v.ok() {
            let found: Vec<String> = v.findings.iter().filter(|f| !f.ok).map(|f| f.what.clone()).collect();
            invalid.push(format!("{}:\n  {}", v.task, found.join("\n  ")));
        }
    }
    assert!(invalid.is_empty(), "invalid tasks:\n{}", invalid.join("\n"));
}

#[test]
fn both_splits_and_every_language_are_covered() {
    let tasks = task::load_all(&tasks_dir()).unwrap();
    for language in Language::ALL {
        assert!(tasks.iter().any(|t| t.spec.language == *language), "no {language} task");
    }
    for split in task::Split::ALL {
        assert!(tasks.iter().any(|t| t.spec.split == *split), "no {split} task");
    }
}

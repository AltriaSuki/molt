//! Every task in `bench/tasks` and `bench/smoke` is sound: its hidden check
//! fails on the starting repo and passes on the reference solution, and the
//! repo's own tests pass on both. A language whose toolchain is missing here
//! is skipped, except on CI, where every task must validate. Smoke tasks
//! whose check needs the network are validated only with
//! `MOLT_BENCH_NETWORK=1`, as the smoke workflow does before it runs them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use molt_bench::grade;
use molt_bench::task::{self, Language};

fn tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/tasks")
}

fn smoke_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/smoke")
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
    validate_all(&tasks_dir()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_smoke_task_validates() {
    validate_all(&smoke_dir()).await;
}

#[test]
fn every_smoke_task_says_what_it_asserts_and_no_benchmark_task_does() {
    for t in task::load_all(&smoke_dir()).unwrap() {
        assert!(t.spec.smoke.is_some(), "{}: a smoke task has a [smoke] table", t.id);
    }
    for t in task::load_all(&tasks_dir()).unwrap() {
        assert!(t.spec.smoke.is_none(), "{}: smoke tasks belong in bench/smoke", t.id);
    }
}

async fn validate_all(dir: &Path) {
    let tasks = task::load_all(dir).unwrap();
    let network = std::env::var_os("MOLT_BENCH_NETWORK").is_some_and(|v| v == "1");
    let on_ci = std::env::var_os("CI").is_some();
    let limit = Arc::new(tokio::sync::Semaphore::new(4));
    let mut set = tokio::task::JoinSet::new();
    for t in tasks {
        if t.network() && !network {
            eprintln!("skipping {}: its check needs the network; set MOLT_BENCH_NETWORK=1 to validate it", t.id);
            continue;
        }
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

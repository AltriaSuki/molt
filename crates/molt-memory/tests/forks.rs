//! Attempt symbols use real fs forks, file tools and shell-generated edits.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use molt_api::fs::{ForkResponse, ReadResponse};
use molt_api::memory::{ForkSymbolsResponse, SymbolsResponse};
use molt_memory::{Bus, Config, Db, Memory, Writer};
use molt_proto::{Budget, CapId, Envelope, RemoteError, TraceId};
use molt_tools::{Fs, Roots};
use serde_json::{json, Value};

struct FsBus {
    fs: Arc<Fs>,
    calls: AtomicUsize,
    edit_after_first: Mutex<Option<PathBuf>>,
}

#[async_trait]
impl Bus for FsBus {
    async fn call(&self, target: &str, payload: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
        assert_eq!(target, "fs.view");
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            if let Some(path) = self.edit_after_first.lock().unwrap().take() {
                fs::write(path, "fn changed_during_query() {}\n").unwrap();
            }
        }
        self.fs.handle("view", payload).await
    }
}

struct Setup {
    _tmp: tempfile::TempDir,
    workspace: PathBuf,
    scratch: PathBuf,
    fs: Arc<Fs>,
    bus: Arc<FsBus>,
    db: Arc<Db>,
    memory: Memory,
}

impl Setup {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        let scratch = tmp.path().join("work");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("a.rs"), "fn old_name() {}\nfn caller() { old_name(); }\n").unwrap();
        let fs = Arc::new(Fs::new(Roots { root: workspace.clone(), scratch: scratch.clone() }).unwrap());
        let scratch = scratch.canonicalize().unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let bus = Arc::new(FsBus { fs: fs.clone(), calls: AtomicUsize::new(0), edit_after_first: Mutex::new(None) });
        let db = Arc::new(Db::in_memory().unwrap());
        let cfg = Config { fork_root: Some(scratch.clone()), ..Default::default() };
        let memory = Memory::new(
            db.clone(),
            &workspace,
            cfg,
            Writer { service: "memory".into(), version: "test".into() },
            bus.clone(),
        )
        .unwrap();
        Self { _tmp: tmp, workspace, scratch, fs, bus, db, memory }
    }

    async fn fork(&self) -> String {
        let reply = self.fs.handle("fork", json!({ "workspace": self.workspace })).await.unwrap();
        serde_json::from_value::<ForkResponse>(reply).unwrap().fork
    }

    async fn ask(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        let req = Envelope::request(TraceId::random(), method.parse().unwrap(), CapId::random(), payload);
        self.memory.handle(req).await
    }

    async fn symbols(&self, fork: &str, name: &str) -> ForkSymbolsResponse {
        serde_json::from_value(
            self.ask("memory.fork_symbols", json!({ "workspace": self.workspace, "fork": fork, "name": name }))
                .await
                .unwrap(),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn rename_add_move_delete_and_shell_edits_stay_inside_the_attempt() {
    let s = Setup::new();
    s.ask("memory.index", json!({"workspace": s.workspace})).await.unwrap();
    let a = s.fork().await;
    let b = s.fork().await;
    let initial = s.symbols(&a, "old_name").await;
    assert_eq!(initial.symbols.definitions[0].line, 1);
    s.fs.handle(
        "edit",
        json!({"workspace": a, "path": "a.rs", "old": "old_name", "new": "new_name", "replace_all": true}),
    )
    .await
    .unwrap();
    let changed = s.symbols(&a, "new_name").await;
    assert_eq!(changed.symbols.definitions[0].name, "new_name");
    assert!(changed.symbols.references.iter().any(|r| r.line == 2 && r.text.contains("new_name();")));
    assert_eq!(changed.view.baseline, initial.view.baseline);
    assert_ne!(changed.view.version, initial.view.version);
    assert!(s.symbols(&a, "old_name").await.symbols.definitions.is_empty());
    assert_eq!(s.symbols(&b, "old_name").await.symbols.definitions.len(), 1);
    assert!(s.symbols(&b, "new_name").await.symbols.definitions.is_empty());

    // A code generator outside fs.write is detected, even when it preserves
    // size and restores the original mtime. Use an actual shell process.
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
    fs::File::open(Path::new(&a).join("a.rs")).unwrap().set_times(fs::FileTimes::new().set_modified(old)).unwrap();
    assert_eq!(s.symbols(&a, "new_name").await.parsed, 0);
    let status = std::process::Command::new("sh").args(["-c", "cp -p a.rs stamp; sed 's/new_name/shell_fn/g' a.rs > generated.rs; touch -r stamp generated.rs; mv generated.rs a.rs; rm stamp"])
        .current_dir(&a).status().unwrap();
    assert!(status.success());
    assert!(s.symbols(&a, "new_name").await.symbols.definitions.is_empty());
    assert_eq!(s.symbols(&a, "shell_fn").await.symbols.definitions.len(), 1);
    fs::rename(Path::new(&a).join("a.rs"), Path::new(&a).join("moved.rs")).unwrap();
    let source = fs::read_to_string(Path::new(&a).join("moved.rs")).unwrap();
    fs::write(Path::new(&a).join("moved.rs"), format!("\n\n{source}")).unwrap();
    let moved = s.symbols(&a, "shell_fn").await;
    assert_eq!((moved.symbols.definitions[0].path.as_str(), moved.symbols.definitions[0].line), ("moved.rs", 3));
    let read: ReadResponse =
        serde_json::from_value(s.fs.handle("read", json!({"workspace": a, "path": "moved.rs"})).await.unwrap())
            .unwrap();
    assert_eq!(read.content.lines().nth(2), Some("fn shell_fn() {}"));
    s.fs.handle("write", json!({"workspace": a, "path": "added.rs", "content": "fn added() {}\n"})).await.unwrap();
    assert_eq!(s.symbols(&a, "added").await.symbols.definitions.len(), 1);
    fs::remove_file(Path::new(&a).join("moved.rs")).unwrap();
    assert!(s.symbols(&a, "shell_fn").await.symbols.definitions.is_empty());
    let original: SymbolsResponse = serde_json::from_value(
        s.ask("memory.symbols", json!({"workspace": s.workspace, "name": "old_name"})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(original.definitions.len(), 1);
    assert_eq!(s.db.with(|c| c.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get::<_, i64>(0)).unwrap()), 1);
    s.fs.handle("drop", json!({"fork": a})).await.unwrap();
    let e =
        s.ask("memory.fork_symbols", json!({"workspace": s.workspace, "fork": a, "name": "added"})).await.unwrap_err();
    assert!(e.message.contains("use search/read_file"));
    assert_eq!(s.symbols(&b, "old_name").await.symbols.definitions.len(), 1);
}

#[tokio::test]
async fn unverified_wrong_workspace_and_symlink_forks_are_refused_without_widening_roots() {
    let s = Setup::new();
    let fork = s.fork().await;
    for path in [s.workspace.clone(), s.scratch.join("fork-000000000000"), s.scratch.clone()] {
        let e = s
            .ask("memory.fork_symbols", json!({"workspace": s.workspace, "fork": path, "name": "old_name"}))
            .await
            .unwrap_err();
        assert!(e.message.contains("view is unavailable"));
    }
    fs::create_dir(s.workspace.join("other")).unwrap();
    assert!(s
        .ask("memory.fork_symbols", json!({"workspace": s.workspace.join("other"), "fork": fork, "name": "old_name"}))
        .await
        .unwrap_err()
        .message
        .contains("does not belong"));
    // The ordinary memory APIs still cannot name the scratch tree.
    assert!(s.ask("memory.index", json!({"workspace": fork})).await.unwrap_err().message.contains("outside"));
    let linked = s.scratch.join("fork-111111111111");
    std::os::unix::fs::symlink(&fork, &linked).unwrap();
    assert!(s
        .ask("memory.fork_symbols", json!({"workspace": s.workspace, "fork": linked, "name": "old_name"}))
        .await
        .is_err());
}

#[tokio::test]
async fn an_edit_during_refresh_returns_no_stale_symbols() {
    let s = Setup::new();
    let fork = s.fork().await;
    *s.bus.edit_after_first.lock().unwrap() = Some(Path::new(&fork).join("a.rs"));
    let e = s
        .ask("memory.fork_symbols", json!({"workspace": s.workspace, "fork": fork, "name": "old_name"}))
        .await
        .unwrap_err();
    assert!(e.message.contains("changed while refreshing"), "{e:?}");
    assert_eq!(s.symbols(&fork, "changed_during_query").await.symbols.definitions.len(), 1);
}

fn rss() -> String {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|s| s.starts_with("VmRSS:") || s.starts_with("VmHWM:"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::test]
#[ignore = "explicit medium-repository timing and resident-memory measurement"]
async fn fork_view_benchmark() {
    let s = Setup::new();
    for n in 0..500 {
        let text: String = (0..40).map(|f| format!("pub fn symbol_{n}_{f}() -> u32 {{ {f} }}\n")).collect();
        fs::write(s.workspace.join(format!("file_{n}.rs")), text).unwrap();
    }
    let a = s.fork().await;
    let b = s.fork().await;
    println!("500 source files, 20,000 generated definitions; before views: {}", rss());
    let now = Instant::now();
    let first = s.symbols(&a, "symbol_0_0").await;
    println!("first view: {:?}, parsed {}, {}", now.elapsed(), first.parsed, rss());
    assert_eq!(first.parsed, 501);
    fs::write(Path::new(&a).join("file_0.rs"), "pub fn changed() {}\n").unwrap();
    let now = Instant::now();
    let next = s.symbols(&a, "changed").await;
    println!("one-file incremental refresh: {:?}, parsed {}, {}", now.elapsed(), next.parsed, rss());
    assert_eq!(next.parsed, 1);
    let now = Instant::now();
    let (a_result, b_result) = tokio::join!(s.symbols(&a, "changed"), s.symbols(&b, "symbol_0_0"));
    println!("parallel queries (one cached, one cold): {:?}, {}", now.elapsed(), rss());
    assert_eq!(a_result.symbols.definitions.len(), 1);
    assert_eq!(b_result.symbols.definitions.len(), 1);
    assert!(s.symbols(&a, "symbol_0_0").await.symbols.definitions.is_empty());
}

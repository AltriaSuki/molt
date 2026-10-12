//! The memory service as other services reach it: requests and events in
//! envelopes, as the kernel delivers them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use molt_api::fs::{FilesChanged, CHANGED};
use molt_api::memory::{
    ForgetResponse, IndexResponse, Layer, MapResponse, PromoteResponse, RecallResponse, RememberResponse,
    RetractResponse, SymbolsResponse,
};
use molt_memory::{Bus, Config, Db, Memory, Project, Writer};
use molt_proto::{Budget, CapId, Envelope, ErrorCode, RemoteError, ServiceId, TraceId};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tempfile::TempDir;

struct NoBus;

#[async_trait]
impl Bus for NoBus {
    async fn call(&self, target: &str, _: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
        Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") })
    }
}

/// A root with a project in `app/`, and memory over it.
struct Setup {
    _dir: TempDir,
    root: PathBuf,
    memory: Memory,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::create_dir_all(root.join(".molt/inside")).unwrap();
        std::fs::write(root.join("app/src/lib.rs"), "pub fn parse(s: &str) -> u32 {\n    s.len() as u32\n}\n").unwrap();
        let db = Arc::new(Db::in_memory().unwrap());
        let writer = Writer { service: "memory".into(), version: "v1".into() };
        let memory = Memory::new(db, &root, Config::default(), writer, Arc::new(NoBus)).unwrap();
        Self { _dir: dir, root, memory }
    }

    fn app(&self) -> String {
        self.root.join("app").to_str().unwrap().to_owned()
    }

    async fn ask(&self, from: &str, to: &str, payload: Value) -> Result<Value, RemoteError> {
        let mut msg = Envelope::request(TraceId::random(), to.parse().unwrap(), CapId::random(), payload);
        msg.from = Some(ServiceId::new(from).unwrap());
        self.memory.handle(msg).await
    }

    async fn call<T: DeserializeOwned>(&self, to: &str, payload: Value) -> T {
        serde_json::from_value(self.ask("planner", to, payload).await.unwrap()).unwrap()
    }

    async fn refused(&self, to: &str, payload: Value) -> String {
        let err = self.ask("planner", to, payload).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{err:?}");
        err.message
    }
}

/// `payload` to `memory` from the planner, which must answer.
async fn ask(memory: &Memory, to: &str, payload: Value) -> Value {
    let mut msg = Envelope::request(TraceId::random(), to.parse().unwrap(), CapId::random(), payload);
    msg.from = Some(ServiceId::new("planner").unwrap());
    memory.handle(msg).await.unwrap()
}

fn remember(workspace: &str, text: &str) -> Value {
    json!({ "kind": "fact", "text": text, "workspace": workspace, "trace": "trace_1", "version": "v7" })
}

#[tokio::test]
async fn the_sender_the_kernel_stamped_is_the_notes_author() {
    let s = Setup::new();
    let r: RememberResponse = s.call("memory.remember", remember("app", "Build with `cargo build`.")).await;
    let p = &r.note.provenance;
    assert_eq!((p.service.as_str(), p.version.as_str(), p.trace.as_str()), ("planner", "v7", "trace_1"));
    // A relative workspace is the canonical directory under the root.
    assert_eq!(r.note.workspace.as_deref(), Some(s.app().as_str()));
    // A sender cannot name another author: there is no field for it.
    let mut forged = remember("app", "Something else.");
    forged["service"] = json!("model");
    let r: RememberResponse = s.call("memory.remember", forged).await;
    assert_eq!(r.note.provenance.service, "planner");

    // No provenance, no note.
    let mut bare = remember("app", "No trace.");
    bare["trace"] = json!("");
    assert!(s.refused("memory.remember", bare).await.contains("trace"));
    let msg = Envelope::request(
        TraceId::random(),
        "memory.remember".parse().unwrap(),
        CapId::random(),
        remember("app", "Unsent."),
    );
    assert_eq!(s.memory.handle(msg).await.unwrap_err().code, ErrorCode::Invalid);
}

#[tokio::test]
async fn workspaces_stay_inside_the_root_and_out_of_the_data_dir() {
    let s = Setup::new();
    for (ws, says) in [
        ("/etc", "outside"),
        ("../", "outside"),
        (".molt/inside", ".molt"),
        ("missing", "does not exist"),
        ("app/src/lib.rs", "not a directory"),
    ] {
        let msg = s.refused("memory.remember", remember(ws, "A note.")).await;
        assert!(msg.contains(says), "{ws}: {msg}");
        let msg = s.refused("memory.index", json!({ "workspace": ws })).await;
        assert!(msg.contains(says), "{ws}: {msg}");
    }
    // A symlink out of the root is followed to where it leads, and refused.
    std::os::unix::fs::symlink("/tmp", s.root.join("app/out")).unwrap();
    assert!(s.refused("memory.map", json!({ "workspace": "app/out" })).await.contains("outside"));
    assert!(s.refused("memory.forget", json!({ "id": "note_1" })).await.contains("bad memory.forget request"));
    assert!(s.refused("memory.teach", json!({})).await.contains("unknown method"));
}

#[tokio::test]
async fn notes_are_recalled_forgotten_and_retracted() {
    let s = Setup::new();
    let first: RememberResponse = s.call("memory.remember", remember("app", "Tests run with `cargo test`.")).await;
    let again: RememberResponse = s.call("memory.remember", remember("app", "tests run with `cargo test`")).await;
    assert!(again.reinforced && again.note.id == first.note.id);
    let _: RememberResponse = s.call("memory.remember", remember("app", "The parser lives in src/lib.rs.")).await;

    let found: RecallResponse =
        s.call("memory.recall", json!({ "query": "running the tests", "workspace": "app" })).await;
    assert_eq!(found.notes.len(), 1);
    assert_eq!(found.notes[0].note.id, first.note.id);
    // Without a workspace, only notes that hold everywhere: none here.
    let global: RecallResponse = s.call("memory.recall", json!({ "query": "tests" })).await;
    assert!(global.notes.is_empty());

    let forget = json!({ "id": first.note.id, "reason": "the tests moved to make" });
    let gone: ForgetResponse = s.call("memory.forget", forget.clone()).await;
    assert!(gone.forgotten);
    let twice: ForgetResponse = s.call("memory.forget", forget).await;
    assert!(!twice.forgotten);
    let found: RecallResponse = s.call("memory.recall", json!({ "query": "tests", "workspace": "app" })).await;
    assert!(found.notes.is_empty());

    let retract = json!({ "service": "planner", "version": "v7", "reason": "v7 was rolled back" });
    let r: RetractResponse = s.call("memory.retract", retract).await;
    assert_eq!(r.retracted, 1, "the parser note; the forgotten one is already gone");
    let all: RecallResponse = s.call("memory.recall", json!({ "workspace": "app" })).await;
    assert!(all.notes.is_empty());
}

#[tokio::test]
async fn notes_about_a_project_follow_it_across_its_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for d in ["app", "clone", "other"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let db_path = root.join("memory.db");
    let open = |project: Option<Project>| {
        let db = Arc::new(Db::open(&db_path).unwrap());
        let writer = Writer { service: "memory".into(), version: "v1".into() };
        Memory::new(db, &root, Config { project, ..Config::default() }, writer, Arc::new(NoBus)).unwrap()
    };
    let texts = |found: Value| -> Vec<String> {
        let found: RecallResponse = serde_json::from_value(found).unwrap();
        found.notes.into_iter().map(|r| r.note.text).collect()
    };

    // Before projects, notes were kept by path.
    let before = open(None);
    ask(&before, "memory.remember", remember("app", "Build with make.")).await;
    drop(before);

    let project = |paths: &[&str]| Project { id: "proj_1".into(), paths: paths.iter().map(|p| root.join(p)).collect() };
    let now = open(Some(project(&["app", "clone"])));
    let r: RememberResponse =
        serde_json::from_value(ask(&now, "memory.remember", remember("clone", "Tests run with make check.")).await)
            .unwrap();
    assert_eq!(r.note.workspace.as_deref(), Some("proj_1"));
    // Both clones see both notes, the one kept by path before included.
    for ws in ["app", "clone"] {
        let mut found = texts(ask(&now, "memory.recall", json!({ "workspace": ws })).await);
        found.sort();
        assert_eq!(found, ["Build with make.", "Tests run with make check."], "{ws}");
    }
    // Another directory is another project.
    assert!(texts(ask(&now, "memory.recall", json!({ "workspace": "other" })).await).is_empty());
    drop(now);

    // The directory moves, and the registry says so.
    std::fs::rename(root.join("app"), root.join("moved")).unwrap();
    let moved = open(Some(project(&["moved", "clone"])));
    assert_eq!(texts(ask(&moved, "memory.recall", json!({ "workspace": "moved" })).await).len(), 2);
}

#[tokio::test]
async fn capture_and_user_review_require_the_kernel_identified_caller() {
    let s = Setup::new();
    let note: RememberResponse = s.call("memory.remember", remember("app", "Build with cargo.")).await;
    let payload = json!({"workspace":"app", "id":note.note.id, "expected_rev":note.note.rev, "reason":"checked", "depends_on":["src/lib.rs"], "temporary":true});
    assert!(s.refused("memory.review", payload.clone()).await.contains("CLI"));
    let reviewed: molt_api::memory::ReviewResponse =
        serde_json::from_value(s.ask("cli", "memory.review", payload).await.unwrap()).unwrap();
    assert!(reviewed.details.temporary);
    assert_eq!(reviewed.details.dependencies[0].path, "src/lib.rs");
    assert!(s
        .ask("cli", "memory.recall", json!({"workspace":"app", "capture":"context"}))
        .await
        .unwrap_err()
        .message
        .contains("planner"));
    assert!(s
        .refused("memory.recall", json!({"workspace":"app", "capture":"other"}))
        .await
        .contains("context or tool"));
    assert!(s
        .refused("memory.recall", json!({"workspace":"app", "capture":"context", "include_review":true}))
        .await
        .contains("excludes"));
    let mut changed_payload = json!({"workspace":"app", "id":note.note.id, "expected_rev":reviewed.note.rev, "reason":"checked", "depends_on":[".molt/inside"]});
    assert!(s.ask("cli", "memory.review", changed_payload.clone()).await.is_err());
    changed_payload["workspace"] = json!(".");
    changed_payload["depends_on"] = json!([]);
    assert!(s.ask("cli", "memory.review", changed_payload).await.unwrap_err().message.contains("another project"));
}

#[tokio::test]
async fn the_project_model_follows_the_files_fs_reports() {
    let s = Setup::new();
    let lookup = |name: &str| json!({ "workspace": "app", "name": name });
    // A change in a workspace with no model yet is left for its first index.
    std::fs::write(s.root.join("app/src/new.rs"), "pub fn fresh() {}\n").unwrap();
    changed(&s, &s.app(), &["src/new.rs"]).await;
    let none: SymbolsResponse = s.call("memory.symbols", lookup("fresh")).await;
    assert!(none.definitions.is_empty());

    let indexed: IndexResponse = s.call("memory.index", json!({ "workspace": "app" })).await;
    assert_eq!((indexed.files, indexed.parsed), (2, 2));
    let map: MapResponse = s.call("memory.map", json!({ "workspace": "app", "query": "parse" })).await;
    assert!(map.map.starts_with("src/lib.rs:"), "{}", map.map);

    std::fs::write(
        s.root.join("app/src/lib.rs"),
        "pub fn parse(s: &str) -> u32 {\n    tally(s)\n}\nfn tally(s: &str) -> u32 { 0 }\n",
    )
    .unwrap();
    std::fs::remove_file(s.root.join("app/src/new.rs")).unwrap();
    changed(&s, &s.app(), &["src/lib.rs", "src/new.rs"]).await;
    let tally: SymbolsResponse = s.call("memory.symbols", lookup("tally")).await;
    assert_eq!(tally.definitions.len(), 1);
    assert_eq!(tally.references.len(), 1);
    let gone: SymbolsResponse = s.call("memory.symbols", lookup("fresh")).await;
    assert!(gone.definitions.is_empty());

    // Events that are not about a workspace under the root change nothing.
    changed(&s, "/etc", &["passwd"]).await;
    let bad = Envelope::event(TraceId::random(), CHANGED, CapId::random(), json!({ "nonsense": 1 })).unwrap();
    assert_eq!(s.memory.handle(bad).await.unwrap(), Value::Null);
}

async fn changed(s: &Setup, workspace: &str, paths: &[&str]) {
    let event = FilesChanged { workspace: workspace.into(), paths: paths.iter().map(|p| p.to_string()).collect() };
    let msg =
        Envelope::event(TraceId::random(), CHANGED, CapId::random(), serde_json::to_value(event).unwrap()).unwrap();
    assert_eq!(s.memory.handle(msg).await.unwrap(), Value::Null);
}

#[tokio::test]
async fn changes_reported_while_an_index_runs_are_all_indexed() {
    let s = Setup::new();
    let _: IndexResponse = s.call("memory.index", json!({ "workspace": "app" })).await;
    for i in 0..3 {
        std::fs::write(s.root.join(format!("app/src/n{i}.rs")), format!("pub fn new{i}() {{}}\n")).unwrap();
    }
    // One handler indexes; the others leave their files to it and return.
    let app = s.app();
    tokio::join!(
        changed(&s, &app, &["src/n0.rs"]),
        changed(&s, &app, &["src/n1.rs"]),
        changed(&s, &app, &["src/n2.rs"])
    );
    for i in 0..3 {
        let found: SymbolsResponse =
            s.call("memory.symbols", json!({ "workspace": "app", "name": format!("new{i}") })).await;
        assert_eq!(found.definitions.len(), 1, "new{i}");
    }
}

/// `payload` to `memory` from `from`, in the run `trace`.
async fn ask_in(memory: &Memory, from: &str, trace: &TraceId, to: &str, payload: Value) -> Result<Value, RemoteError> {
    let mut msg = Envelope::request(trace.clone(), to.parse().unwrap(), CapId::random(), payload);
    msg.from = Some(ServiceId::new(from).unwrap());
    memory.handle(msg).await
}

#[tokio::test]
async fn the_user_layer_is_shared_by_every_project_and_only_a_person_promotes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for d in ["a", "b"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let open = |db: &str, user: Option<&Arc<Db>>| {
        let db = Arc::new(Db::open(&root.join(db)).unwrap());
        let writer = Writer { service: "memory".into(), version: "v1".into() };
        let memory = Memory::new(db, &root, Config::default(), writer, Arc::new(NoBus)).unwrap();
        match user {
            Some(user) => memory.with_user(user.clone()),
            None => memory,
        }
    };
    // A project database from before the user layer, used as it is.
    let old = open("a.db", None);
    let learned: RememberResponse =
        serde_json::from_value(ask(&old, "memory.remember", remember("a", "Build with make.")).await).unwrap();
    drop(old);

    let user = Arc::new(Db::open(&root.join("user.db")).unwrap());
    let a = open("a.db", Some(&user));
    let b = open("b.db", Some(&user));
    let trace = TraceId::random();
    let mine = json!({ "kind": "preference", "text": "Answer in Chinese.", "trace": "trace_9", "version": "user" });
    let refused = ask_in(&a, "planner", &trace, "memory.remember", mine.clone()).await.unwrap_err();
    assert!(refused.message.contains("CLI"));
    let mine: RememberResponse =
        serde_json::from_value(ask_in(&a, "cli", &trace, "memory.remember", mine).await.unwrap()).unwrap();
    assert_eq!(mine.note.workspace, None);
    let a_notes: RecallResponse =
        serde_json::from_value(ask(&a, "memory.recall", json!({ "workspace": "a" })).await).unwrap();
    assert_eq!(a_notes.notes.len(), 2, "the old project note and the user's");

    // Project B recalls it in a run, and the run's record says where it came from.
    let run = TraceId::random();
    let found =
        ask_in(&b, "planner", &run, "memory.recall", json!({ "workspace": "b", "capture": "context" })).await.unwrap();
    let found: RecallResponse = serde_json::from_value(found).unwrap();
    assert_eq!(found.notes.len(), 1);
    assert_eq!(found.notes[0].note.text, "Answer in Chinese.");
    assert_eq!(found.notes[0].layer, Layer::User);
    let b_db = Db::open(&root.join("b.db")).unwrap();
    let key = root.join("b").to_str().unwrap().to_owned();
    let snapshots = molt_memory::recall_snapshots(&b_db, &key, run.as_str()).unwrap();
    assert_eq!(snapshots[0].notes[0].layer, Layer::User);

    // Learning cannot promote; a person can, and the project note stays.
    let promote = json!({ "workspace": "a", "id": learned.note.id, "reason": "every project here uses make" });
    let refused = ask_in(&a, "planner", &trace, "memory.promote", promote.clone()).await.unwrap_err();
    assert!(refused.message.contains("CLI"), "{refused:?}");
    let promoted: PromoteResponse =
        serde_json::from_value(ask_in(&a, "cli", &trace, "memory.promote", promote).await.unwrap()).unwrap();
    assert_eq!(promoted.note.workspace, None);
    assert_eq!(promoted.note.provenance, learned.note.provenance, "the evidence goes with it");
    let in_b: RecallResponse =
        serde_json::from_value(ask(&b, "memory.recall", json!({ "workspace": "b", "query": "build" })).await).unwrap();
    assert_eq!(in_b.notes[0].note.id, promoted.note.id);
    let in_a: RecallResponse = serde_json::from_value(
        ask(&a, "memory.recall", json!({ "workspace": "a", "query": "build", "include_review": true })).await,
    )
    .unwrap();
    let original = in_a.notes.iter().find(|r| r.note.id == learned.note.id).unwrap();
    assert_eq!(original.details.promoted_to.as_deref(), Some(promoted.note.id.as_str()));

    // Rolling back the version that learned it takes the copy back too.
    let retract = json!({ "service": "planner", "version": "v7", "reason": "v7 was rolled back" });
    let r: RetractResponse = serde_json::from_value(ask(&a, "memory.retract", retract).await).unwrap();
    assert_eq!(r.retracted, 2, "the project note and its promoted copy");
    let in_b: RecallResponse =
        serde_json::from_value(ask(&b, "memory.recall", json!({ "workspace": "b" })).await).unwrap();
    assert_eq!(in_b.notes.iter().map(|r| r.note.id.as_str()).collect::<Vec<_>>(), [mine.note.id.as_str()]);

    // A user note is corrected and forgotten from any project.
    let correct = json!({ "workspace": "b", "id": mine.note.id, "expected_rev": mine.note.rev, "reason": "simplified",
        "text": "Answer in Simplified Chinese." });
    let corrected: molt_api::memory::ReviewResponse =
        serde_json::from_value(ask_in(&b, "cli", &trace, "memory.correct", correct).await.unwrap()).unwrap();
    assert_eq!(corrected.note.workspace, None);
    let forget = json!({ "id": corrected.note.id, "reason": "no longer" });
    let forgot: ForgetResponse = serde_json::from_value(ask(&b, "memory.forget", forget).await).unwrap();
    assert!(forgot.forgotten);
    let left: RecallResponse =
        serde_json::from_value(ask(&a, "memory.recall", json!({ "workspace": "a" })).await).unwrap();
    assert!(left.notes.is_empty(), "{left:?}");
}

#[tokio::test]
async fn promotion_rechecks_dependencies_changed_outside_molt() {
    let mut setup = Setup::new();
    setup.memory = setup.memory.with_user(Arc::new(Db::in_memory().unwrap()));
    let remembered: RememberResponse = setup.call("memory.remember", remember("app", "The parser uses lengths.")).await;
    let review = json!({"workspace":"app", "id":remembered.note.id, "expected_rev":remembered.note.rev,
        "reason":"checked source", "depends_on":["src/lib.rs"]});
    setup.ask("cli", "memory.review", review).await.unwrap();
    std::fs::write(setup.root.join("app/src/lib.rs"), "changed externally").unwrap();
    let error = setup
        .ask(
            "cli",
            "memory.promote",
            json!({"workspace":"app", "id":remembered.note.id,
        "reason":"applies everywhere"}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("awaits review"), "{error:?}");
}

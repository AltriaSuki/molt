//! The memory service as other services reach it: requests and events in
//! envelopes, as the kernel delivers them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use molt_api::fs::{FilesChanged, CHANGED};
use molt_api::memory::{
    ForgetResponse, IndexResponse, MapResponse, RecallResponse, RememberResponse, RetractResponse, SymbolsResponse,
};
use molt_memory::{Bus, Config, Db, Memory, Writer};
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

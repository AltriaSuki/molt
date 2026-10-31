//! Saved outputs (#238): a command's or a process's whole output, kept
//! past what a reply carries, read back by reference through
//! `shell.output` without running anything again.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use molt_api::shell::{OutputCut, OutputPage, OutputStream, ProcessState, RunResponse};
use molt_proto::{ErrorCode, RemoteError};
use molt_tools::{ExecutionPolicy, OutputLimits, Roots, Shell};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const ME: &str = "svc:planner";

struct Env {
    _tmp: TempDir,
    ws: PathBuf,
    shell: Shell,
}

impl Env {
    fn new(limits: OutputLimits) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ws")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        let roots = Roots { root: root.clone(), scratch: tmp.path().join("scratch") };
        let shell = Shell::with_policy(roots, ExecutionPolicy::Unconfined).unwrap().with_output_limits(limits).unwrap();
        Env { ws: root.join("ws").canonicalize().unwrap(), shell, _tmp: tmp }
    }

    async fn call(&self, caller: &str, method: &str, payload: Value) -> Result<Value, RemoteError> {
        self.shell.handle_from(caller, method, payload, CancellationToken::new()).await
    }

    async fn run(&self, command: &str) -> RunResponse {
        let reply = self.call(ME, "run", json!({ "workspace": "ws", "command": command })).await.unwrap();
        serde_json::from_value(reply).unwrap()
    }

    async fn output(&self, caller: &str, payload: Value) -> Result<OutputPage, RemoteError> {
        self.call(caller, "output", payload).await.map(|page| serde_json::from_value(page).unwrap())
    }
}

/// What the command in the first test prints.
fn long_log() -> String {
    let mut log = String::new();
    for n in 1..=20_000 {
        log.push_str(&format!("ok line {n}\n"));
        if n == 10_000 {
            log.push_str("error: the failure in the middle\n");
        }
    }
    log
}

#[tokio::test]
async fn a_long_logs_middle_error_is_found_by_reference_and_reading_it_runs_nothing_again() {
    let env = Env::new(OutputLimits::default());
    let command = "echo ran >> runs.txt; for n in $(seq 1 20000); do echo \"ok line $n\"; \
                   if [ $n = 10000 ]; then echo 'error: the failure in the middle'; fi; done; exit 1";
    let run = env.run(command).await;
    assert!(run.truncated, "{} bytes", run.stdout.len());
    assert!(!run.stdout.contains("the failure in the middle"), "the reply lost the middle");
    let [saved] = run.outputs.as_slice() else { panic!("{:?}", run.outputs) };
    let log = long_log();
    assert_eq!((saved.stream, saved.total, saved.stored), (OutputStream::Stdout, log.len() as u64, log.len() as u64));
    assert!(saved.whole() && !saved.open);
    assert_eq!(saved.end.and_then(|e| e.exit_code), Some(1));
    assert_eq!(saved.sha256.as_deref(), Some(hex::encode(Sha256::digest(log.as_bytes())).as_str()));
    assert_eq!(saved.command, command);

    let search = json!({ "workspace": "ws", "id": saved.id, "search": "ERROR:", "case_insensitive": true });
    for _ in 0..2 {
        let found = env.output(ME, search.clone()).await.unwrap();
        let [hit] = found.matches.as_slice() else { panic!("{:?}", found.matches) };
        assert_eq!((hit.line, hit.text.as_str()), (10_001, "error: the failure in the middle"));
        let page = env
            .output(ME, json!({ "workspace": "ws", "id": saved.id, "offset": hit.offset, "limit": 64 }))
            .await
            .unwrap();
        assert!(page.text.starts_with("error: the failure in the middle\nok line 10001\n"), "{}", page.text);
        assert_eq!(page.next, hit.offset + 64);
    }
    assert_eq!(fs::read_to_string(env.ws.join("runs.txt")).unwrap(), "ran\n", "reading ran nothing again");

    // Another caller, or the same one from another workspace, is refused.
    let read = json!({ "workspace": "ws", "id": saved.id });
    let other = env.output("svc:intruder", read).await.unwrap_err();
    assert_eq!(other.code, ErrorCode::Denied, "{}", other.message);
    let elsewhere = env.output(ME, json!({ "workspace": "other", "id": saved.id })).await.unwrap_err();
    assert_eq!(elsewhere.code, ErrorCode::Denied, "{}", elsewhere.message);
}

#[tokio::test]
async fn what_was_not_saved_says_why_and_a_cancelled_command_says_it_was() {
    let small = OutputLimits { per_output: 100_000, ..OutputLimits::default() };
    let env = Env::new(small);
    let run = env.run("head -c 300000 /dev/zero | tr '\\0' x").await;
    let [saved] = run.outputs.as_slice() else { panic!("{:?}", run.outputs) };
    assert_eq!((saved.total, saved.stored, saved.cut), (300_000, 100_000, Some(OutputCut::OutputLimit)));
    let past = json!({ "workspace": "ws", "id": saved.id, "offset": 100_001 });
    assert!(env.output(ME, past).await.unwrap_err().message.contains("past the end"));

    let full_disk = OutputLimits { min_free: u64::MAX / 2, ..OutputLimits::default() };
    let env = Env::new(full_disk);
    let run = env.run("head -c 100000 /dev/zero | tr '\\0' x").await;
    let [saved] = run.outputs.as_slice() else { panic!("{:?}", run.outputs) };
    assert_eq!((saved.stored, saved.cut), (0, Some(OutputCut::DiskLow)));

    let env = Env::new(OutputLimits::default());
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        stop.cancel();
    });
    let payload = json!({ "workspace": "ws", "command": "head -c 100000 /dev/zero | tr '\\0' x; sleep 30" });
    let reply = env.shell.handle_from(ME, "run", payload, cancel).await.unwrap();
    let run: RunResponse = serde_json::from_value(reply).unwrap();
    assert!(run.cancelled);
    let [saved] = run.outputs.as_slice() else { panic!("{:?}", run.outputs) };
    assert!(saved.end.is_some_and(|end| end.cancelled) && saved.whole(), "{saved:?}");
}

#[tokio::test]
async fn a_process_that_outgrew_its_ring_is_saved_whole_and_can_be_followed() {
    let env = Env::new(OutputLimits::default());
    let command =
        "echo early-marker; head -c 1200000 /dev/zero | tr '\\0' a; echo; sleep 1; echo late-marker; sleep 30";
    let start = json!({ "workspace": "ws", "command": command, "wait_ms": 200 });
    let state: ProcessState = serde_json::from_value(env.call(ME, "start", start).await.unwrap()).unwrap();
    let mut saved = state.saved;
    for _ in 0..50 {
        if saved.is_some() {
            break;
        }
        let read = json!({ "workspace": "ws", "id": state.id, "wait_ms": 200 });
        let now: ProcessState = serde_json::from_value(env.call(ME, "read", read).await.unwrap()).unwrap();
        saved = now.saved;
    }
    let saved = saved.expect("saved once it printed more than the ring keeps");
    assert_eq!(saved.stream, OutputStream::Combined);
    assert!(saved.open);

    // The first line fell out of the ring, but not out of what was saved.
    let early = env.output(ME, json!({ "workspace": "ws", "id": saved.id, "search": "early-marker" })).await.unwrap();
    assert_eq!(early.matches.iter().map(|m| (m.line, m.offset)).collect::<Vec<_>>(), [(1, 0)]);
    let mut offset = 1_200_014;
    let mut seen = String::new();
    for _ in 0..20 {
        let follow = json!({ "workspace": "ws", "id": saved.id, "offset": offset, "wait_ms": 5000 });
        let page = env.output(ME, follow).await.unwrap();
        seen.push_str(&page.text);
        offset = page.next;
        if seen.contains("late-marker") {
            break;
        }
    }
    assert!(seen.contains("late-marker"), "{seen:?}");

    let stopped = env.call(ME, "stop", json!({ "workspace": "ws", "id": state.id })).await.unwrap();
    let stopped: molt_api::shell::StopResponse = serde_json::from_value(stopped).unwrap();
    let after = stopped.stopped[0].saved.clone().unwrap();
    assert!(!after.open && after.end.is_some_and(|end| end.cancelled) && after.sha256.is_some(), "{after:?}");
}

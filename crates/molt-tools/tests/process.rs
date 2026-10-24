//! `shell.start`, `shell.read`, `shell.write` and `shell.stop`: processes
//! that run on after the call, their output, input, bounds and cleanup.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use molt_api::shell::{ProcessState, ProcessStatus, StopResponse};
use molt_proto::{ErrorCode, RemoteError};
use molt_tools::{ExecutionPolicy, Roots, Shell};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const CALLER: &str = "planner";

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    shell: Shell,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        for ws in ["ws", "other"] {
            fs::create_dir_all(root.join(ws)).unwrap();
        }
        let shell = Shell::with_policy(
            Roots { root: root.clone(), scratch: tmp.path().join("scratch") },
            ExecutionPolicy::Unconfined,
        )
        .unwrap();
        Env { root: root.canonicalize().unwrap(), shell, _tmp: tmp }
    }

    fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    async fn call_as(&self, caller: &str, method: &str, payload: Value) -> Result<Value, RemoteError> {
        self.shell.handle_from(caller, method, payload, CancellationToken::new()).await
    }

    async fn call(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        self.call_as(CALLER, method, payload).await
    }

    async fn start(&self, payload: Value) -> ProcessState {
        serde_json::from_value(self.call("start", payload).await.unwrap()).unwrap()
    }

    async fn read(&self, id: &str, wait_ms: u64) -> ProcessState {
        let reply = self.call("read", json!({ "workspace": "ws", "id": id, "wait_ms": wait_ms })).await.unwrap();
        serde_json::from_value(reply).unwrap()
    }

    async fn stop(&self, workspace: &str, id: Option<&str>) -> StopResponse {
        serde_json::from_value(self.call("stop", json!({ "workspace": workspace, "id": id })).await.unwrap()).unwrap()
    }

    /// Read until `until` holds or a few seconds pass; everything read, and the last state.
    async fn read_until(&self, id: &str, until: impl Fn(&str, &ProcessState) -> bool) -> (String, ProcessState) {
        let mut all = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = self.read(id, 500).await;
            all.push_str(&state.output);
            if until(&all, &state) || Instant::now() > deadline {
                return (all, state);
            }
        }
    }

    async fn pid(&self, file: &str) -> i32 {
        let path = self.ws().join(file);
        let until = Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(&path).is_ok_and(|s| s.ends_with('\n')) {
            assert!(Instant::now() < until, "{file} was never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }
}

/// True once `pid` has exited (gone, or a zombie nobody reaped yet).
fn dead(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

fn wait_dead(pid: i32) -> bool {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if dead(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn a_server_runs_on_while_files_change_and_its_new_output_is_read_until_it_is_stopped() {
    let env = Env::new();
    let server = "echo $$ > server.pid; echo 'listening on :8080'; i=0; while :; do i=$((i+1)); echo \"request $i\"; sleep 0.05; done";
    let started =
        env.start(json!({ "workspace": "ws", "command": server, "ready": "listening on", "wait_ms": 5000 })).await;
    assert_eq!((started.status, started.ready), (ProcessStatus::Running, Some(true)), "{started:?}");
    assert!(started.id.starts_with("proc_") && started.output.starts_with("listening on :8080\n"), "{started:?}");
    assert_eq!(started.offset, 0);
    let pid = env.pid("server.pid").await;

    // The workspace stays usable while it runs, and later reads take only what is new.
    fs::write(env.ws().join("src.txt"), "changed").unwrap();
    let ran = env.shell.handle("run", json!({ "workspace": "ws", "command": "cat src.txt" })).await.unwrap();
    assert_eq!(ran["stdout"], "changed");
    let first = env.read(&started.id, 2000).await;
    assert_eq!(first.offset, started.next, "a read goes on where the last one ended");
    assert!(first.output.contains("request ") && !first.output.contains("listening"), "{first:?}");
    let second = env.read(&started.id, 2000).await;
    assert_eq!(second.offset, first.next);

    let stopped = env.stop("ws", Some(&started.id)).await.stopped;
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].status, ProcessStatus::Stopped);
    assert!(wait_dead(pid), "the stopped server {pid} survived");
    // Stopping again answers the same; input is refused once it ended.
    let again = env.stop("ws", Some(&started.id)).await.stopped;
    assert_eq!((again[0].status, again[0].exit_code), (ProcessStatus::Stopped, None));
    let err = env.call("write", json!({ "workspace": "ws", "id": started.id, "input": "x\n" })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains("was stopped: it takes no more input"), "{}", err.message);
}

#[tokio::test]
async fn an_interactive_program_answers_two_rounds_of_input_and_exits_when_its_input_closes() {
    let env = Env::new();
    let program = "while read line; do echo \"got $line\"; done; echo bye";
    let p = env.start(json!({ "workspace": "ws", "command": program, "wait_ms": 0 })).await;
    for word in ["first", "second"] {
        let wrote = env.call("write", json!({ "workspace": "ws", "id": p.id, "input": format!("{word}\n") })).await;
        assert!(wrote.is_ok(), "{wrote:?}");
        let want = format!("got {word}\n");
        let (output, state) = env.read_until(&p.id, |all, _| all.contains(&want)).await;
        assert_eq!(output, want);
        assert_eq!(state.status, ProcessStatus::Running);
    }
    env.call("write", json!({ "workspace": "ws", "id": p.id, "input": "", "close": true })).await.unwrap();
    let (output, state) = env.read_until(&p.id, |_, s| s.status != ProcessStatus::Running).await;
    assert_eq!((state.status, state.exit_code, output.as_str()), (ProcessStatus::Exited, Some(0), "bye\n"));
    let err = env.call("write", json!({ "workspace": "ws", "id": p.id, "input": "x" })).await.unwrap_err();
    assert!(err.message.contains("exited with code 0: it takes no more input"), "{}", err.message);
}

#[tokio::test]
async fn a_start_tells_a_ready_server_from_one_whose_command_exited() {
    let env = Env::new();
    let failed = env
        .start(json!({ "workspace": "ws", "command": "echo 'port in use' >&2; exit 3", "ready": "listening", "wait_ms": 5000 }))
        .await;
    assert_eq!((failed.status, failed.exit_code, failed.ready), (ProcessStatus::Exited, Some(3), Some(false)));
    assert_eq!(failed.output, "port in use\n");
    // Without a ready text it says it is running, with what it printed so far.
    let slow = env.start(json!({ "workspace": "ws", "command": "echo booting; sleep 30", "wait_ms": 300 })).await;
    assert_eq!((slow.status, slow.ready, slow.output.as_str()), (ProcessStatus::Running, None, "booting\n"));
    let long = "x".repeat(molt_api::shell::MAX_READY + 1);
    let err = env.call("start", json!({ "workspace": "ws", "command": "true", "ready": long })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    let err = env.call("start", json!({ "workspace": "/", "command": "true" })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid, "a process is confined to the roots as a command is");
}

#[tokio::test]
async fn output_past_the_kept_limit_is_counted_as_omitted_and_the_answer_stays_small() {
    let env = Env::new();
    let noisy = "head -c 3000000 /dev/zero | tr '\\0' x; echo; echo END; sleep 30";
    let state = env.start(json!({ "workspace": "ws", "command": noisy, "ready": "END", "wait_ms": 10000 })).await;
    assert_eq!(state.ready, Some(true));
    let all = state.next;
    assert!(all > 3_000_000, "{all}");
    assert!(state.output.len() < 33 * 1024, "{}", state.output.len());
    assert!(state.output.ends_with("END\n"));
    assert!(state.omitted > 0 && state.output.contains("bytes omitted"));
    let err = env
        .call(
            "write",
            json!({ "workspace": "ws", "id": state.id, "input": "x".repeat(molt_api::shell::MAX_INPUT + 1) }),
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("write it in smaller pieces"), "{}", err.message);
    env.stop("ws", None).await;
}

#[tokio::test]
async fn a_process_is_stopped_with_its_children_when_its_lifetime_ends() {
    let env = Env::new();
    let command = "sleep 30 & echo $! > child.pid; wait";
    let p = env.start(json!({ "workspace": "ws", "command": command, "lifetime_ms": 300, "wait_ms": 0 })).await;
    let child = env.pid("child.pid").await;
    let (_, state) = env.read_until(&p.id, |_, s| s.status != ProcessStatus::Running).await;
    assert_eq!(state.status, ProcessStatus::Expired);
    assert!(wait_dead(child), "the child {child} outlived its process");
}

#[tokio::test]
async fn how_many_run_is_bounded_per_workspace_and_for_the_service() {
    let env = Env::new();
    let start = |caller: &'static str, ws: &'static str| {
        env.call_as(caller, "start", json!({ "workspace": ws, "command": "sleep 30", "wait_ms": 0 }))
    };
    let mut first = None;
    for _ in 0..4 {
        let p: ProcessState = serde_json::from_value(start(CALLER, "ws").await.unwrap()).unwrap();
        first.get_or_insert(p.id);
    }
    let err = start(CALLER, "ws").await.unwrap_err();
    assert!(err.message.contains("4 processes already run in this workspace"), "{}", err.message);
    for _ in 0..4 {
        start(CALLER, "other").await.unwrap();
    }
    let err = start("someone", "ws").await.unwrap_err();
    assert!(err.message.contains("already runs 8 processes"), "{}", err.message);
    // A stopped one frees its place.
    env.stop("ws", first.as_deref()).await;
    start(CALLER, "ws").await.unwrap();
    env.stop("ws", None).await;
    env.stop("other", None).await;
}

#[tokio::test]
async fn an_id_is_good_only_for_its_workspace_and_caller_and_is_forgotten_once_all_are_stopped() {
    let env = Env::new();
    let p = env.start(json!({ "workspace": "ws", "command": "echo $$ > p.pid; sleep 30", "wait_ms": 0 })).await;
    let pid = env.pid("p.pid").await;
    let read = |caller: &'static str, ws: &'static str, id: String| {
        env.call_as(caller, "read", json!({ "workspace": ws, "id": id }))
    };
    for (caller, ws) in [(CALLER, "other"), ("someone", "ws")] {
        let err = read(caller, ws, p.id.clone()).await.unwrap_err();
        assert!(err.message.contains("there is no process"), "{caller} {ws}: {}", err.message);
        let err = env.call_as(caller, "stop", json!({ "workspace": ws, "id": p.id })).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }
    // Stopping all of another workspace's, or another caller's, leaves it alone.
    env.stop("other", None).await;
    let theirs: StopResponse =
        serde_json::from_value(env.call_as("someone", "stop", json!({ "workspace": "ws" })).await.unwrap()).unwrap();
    assert!(theirs.stopped.is_empty());
    assert_eq!(env.read(&p.id, 0).await.status, ProcessStatus::Running);

    let stopped = env.stop("ws", None).await.stopped;
    assert_eq!(stopped.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [p.id.as_str()]);
    assert!(wait_dead(pid));
    let err = read(CALLER, "ws", p.id.clone()).await.unwrap_err();
    assert!(err.message.contains("there is no process"), "{}", err.message);
    let err = read(CALLER, "ws", "proc_0".into()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[tokio::test]
async fn a_process_stops_when_its_workspace_is_removed() {
    let env = Env::new();
    fs::create_dir(env.root.join("fork")).unwrap();
    let pidfile = env.root.join("fork.pid");
    let command = format!("echo $$ > {}; sleep 30", pidfile.display());
    let p = env.start(json!({ "workspace": "fork", "command": command, "wait_ms": 0 })).await;
    let until = Instant::now() + Duration::from_secs(3);
    while !fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n')) {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    fs::remove_dir(env.root.join("fork")).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !dead(pid) {
        assert!(Instant::now() < until, "a process outlived its workspace");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Still found by the name it was started with, so its caller can read how it ended.
    let state: ProcessState = serde_json::from_value(
        env.call("read", json!({ "workspace": "fork", "id": p.id, "wait_ms": 2000 })).await.unwrap(),
    )
    .unwrap();
    assert_eq!(state.status, ProcessStatus::Stopped);
}

#[tokio::test]
async fn a_cancelled_start_stops_its_process_and_shutdown_stops_the_rest() {
    let env = Env::new();
    let cancel = CancellationToken::new();
    let start = env.shell.handle_from(
        CALLER,
        "start",
        json!({ "workspace": "ws", "command": "echo $$ > never.pid; sleep 30", "ready": "never", "wait_ms": 10000 }),
        cancel.clone(),
    );
    let stop = async {
        env.pid("never.pid").await;
        cancel.cancel();
    };
    let (started, ()) = tokio::join!(start, stop);
    assert_eq!(started.unwrap_err().code, ErrorCode::Cancelled);
    assert!(wait_dead(env.pid("never.pid").await), "a cancelled start left its process");

    let kept = env.start(json!({ "workspace": "ws", "command": "echo $$ > kept.pid; sleep 30", "wait_ms": 0 })).await;
    let pid = env.pid("kept.pid").await;
    env.shell.shutdown().await;
    assert!(wait_dead(pid), "shutdown left a process running");
    assert_eq!(env.read(&kept.id, 2000).await.status, ProcessStatus::Stopped);
    let err = env.call("start", json!({ "workspace": "ws", "command": "true" })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
}

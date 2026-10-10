//! `shell.run`: output, exit status, timeouts, process groups and the
//! environment commands see.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use molt_api::shell::RunResponse;
use molt_proto::{ErrorCode, RemoteError};
use molt_tools::{ExecutionPolicy, Roots, Shell};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    ws: PathBuf,
    shell: Shell,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ws")).unwrap();
        let shell = Shell::with_policy(
            Roots { root: root.clone(), scratch: tmp.path().join("scratch") },
            ExecutionPolicy::Unconfined,
        )
        .unwrap();
        Env { ws: root.join("ws").canonicalize().unwrap(), shell, _tmp: tmp }
    }

    async fn call(&self, payload: Value) -> Result<Value, RemoteError> {
        self.shell.handle("run", payload).await
    }

    async fn run(&self, command: &str, timeout_ms: Option<u64>) -> RunResponse {
        let mut payload = json!({ "workspace": "ws", "command": command });
        if let Some(t) = timeout_ms {
            payload["timeout_ms"] = json!(t);
        }
        serde_json::from_value(self.call(payload).await.unwrap()).unwrap()
    }
}

#[tokio::test]
async fn shutdown_refuses_commands_during_grace_and_after_it_returns() {
    let env = Env::new();
    let shutdown = env.shell.shutdown();
    tokio::pin!(shutdown);
    // Poll shutdown into its grace period before submitting another command.
    tokio::select! {
        biased;
        () = &mut shutdown => panic!("shutdown should wait for graceful termination"),
        () = tokio::task::yield_now() => {}
    }
    for marker in ["during-shutdown", "after-shutdown"] {
        let err = env
            .call(json!({"workspace":"ws", "command":format!("touch {marker}")}))
            .await
            .expect_err("shutdown must close admission before sending TERM");
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(!env.ws.join(marker).exists());
        if marker == "during-shutdown" {
            (&mut shutdown).await;
        }
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn dropping_a_request_closes_its_output_readers() {
    struct Writer(i32);
    impl Drop for Writer {
        fn drop(&mut self) {
            if !dead(self.0) {
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
    }

    let env = Env::new();
    let ws = env.ws.clone();
    let shell = std::sync::Arc::new(env.shell);
    let task = tokio::spawn(async move {
        shell.handle("run", json!({"workspace":"ws", "command":
            "setsid sh -c 'trap \"echo stopped > writer.stopped; exit 0\" PIPE; echo ready > writer.ready; while :; do printf x || exit; sleep 0.02; done' & echo $! > writer.pid; wait"
        })).await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ws.join("writer.pid").exists() || !ws.join("writer.ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let _writer = Writer(fs::read_to_string(ws.join("writer.pid")).unwrap().trim().parse().unwrap());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The escaped writer remains alive while detached drain tasks own its
    // output pipe. Closing those readers makes its next write receive PIPE.
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ws.join("writer.stopped").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request drop left output readers running");
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
async fn captures_output_and_exit_status() {
    let env = Env::new();
    let r = env.run("echo out; echo err >&2; exit 3", None).await;
    assert_eq!((r.exit_code, r.signal, r.timed_out, r.truncated), (Some(3), None, false, false));
    assert_eq!((r.stdout.as_str(), r.stderr.as_str()), ("out\n", "err\n"));
    assert!(!r.success());

    let r = env.run("pwd", None).await;
    assert!(r.success());
    assert_eq!(r.stdout.trim_end(), env.ws.to_str().unwrap());

    let r = env.run("read line; echo \"got[$line]\"", None).await;
    assert_eq!(r.stdout, "got[]\n", "stdin is empty");

    let r = env.run("kill -9 $$", None).await;
    assert_eq!((r.exit_code, r.signal), (None, Some(9)));
}

#[tokio::test]
async fn a_timeout_kills_the_whole_group_quickly() {
    let env = Env::new();
    let started = Instant::now();
    let r = env.run("sleep 30 & echo $! > bg.pid; echo before; sleep 30", Some(300)).await;
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert!(r.timed_out && !r.success());
    assert_eq!(r.exit_code, None);
    assert!(matches!(r.signal, Some(9 | 15)));
    assert_eq!(r.stdout, "before\n");
    let pid: i32 = fs::read_to_string(env.ws.join("bg.pid")).unwrap().trim().parse().unwrap();
    assert!(wait_dead(pid), "the background sleep {pid} survived");
}

#[tokio::test]
async fn a_background_child_does_not_hang_the_call() {
    let env = Env::new();
    let started = Instant::now();
    let r = env.run("sleep 30 & echo $! > bg.pid; echo started", None).await;
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert!(r.success() && !r.timed_out);
    assert_eq!(r.stdout, "started\n");
    let pid: i32 = fs::read_to_string(env.ws.join("bg.pid")).unwrap().trim().parse().unwrap();
    assert!(wait_dead(pid), "the background sleep {pid} survived");
}

#[tokio::test]
async fn kill_all_stops_running_commands_and_their_children() {
    let env = Env::new();
    let pidfile = env.ws.join("bg.pid");
    let started = Instant::now();
    let stop = async {
        while !fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        env.shell.kill_all();
    };
    let (r, ()) = tokio::join!(env.run("sleep 30 & echo $! > bg.pid; wait", None), stop);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert_eq!((r.exit_code, r.signal, r.timed_out), (None, Some(9), false));
    let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(wait_dead(pid), "the background sleep {pid} survived");
    // Nothing is left to kill, and later commands run as usual.
    env.shell.kill_all();
    assert!(env.run("true", None).await.success());
}

#[tokio::test]
async fn secrets_are_not_passed_to_commands() {
    // Only this test sets these, and std serializes environment access.
    std::env::set_var("MOLT_SECRET", "bus-secret");
    std::env::set_var("ANTHROPIC_API_KEY", "sk-test");
    std::env::set_var("MOLT_TOOLS_TEST_VISIBLE", "no");
    std::env::set_var("TOOLS_TEST_VISIBLE", "yes");
    let env = Env::new();
    let r = env
        .run(
            "echo \"${MOLT_SECRET:-unset} ${ANTHROPIC_API_KEY:-unset} ${MOLT_TOOLS_TEST_VISIBLE:-unset} \
             $TOOLS_TEST_VISIBLE $TERM $NO_COLOR $CI $GIT_TERMINAL_PROMPT $PAGER $GIT_PAGER\"; env | grep -c -E '^(MOLT|ANTHROPIC)_'",
            None,
        )
        .await;
    assert_eq!(r.stdout, "unset unset unset yes dumb 1 1 0 cat cat\n0\n");
}

#[tokio::test]
async fn long_output_keeps_its_head_and_tail() {
    let env = Env::new();
    let r = env.run("printf START; head -c 100000 /dev/zero | tr '\\0' x; printf END; echo small >&2", None).await;
    assert!(r.success() && r.truncated);
    assert!(r.stdout.starts_with("STARTxxx") && r.stdout.ends_with("xxxEND"), "{}", &r.stdout[..20]);
    assert!(r.stdout.contains("[... 67240 bytes omitted ...]"), "{}", &r.stdout[8180..8240]);
    assert!(r.stdout.len() < 33 * 1024);
    assert_eq!(r.stderr, "small\n");
}

#[tokio::test]
async fn workspaces_are_confined() {
    let env = Env::new();
    fs::create_dir(env.ws.parent().unwrap().join(".molt")).unwrap();
    for ws in ["..", "/", "/tmp", "missing", ".molt"] {
        let err = env.call(json!({ "workspace": ws, "command": "true" })).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{ws}: {err}");
    }
    let err = env.shell.handle("exec", json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    let err = env.call(json!({ "command": "true" })).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    let r: RunResponse = serde_json::from_value(
        env.call(json!({ "workspace": env.ws, "command": "true", "timeout_ms": u64::MAX })).await.unwrap(),
    )
    .unwrap();
    assert!(r.success());
}

#[tokio::test]
async fn hangup_and_quit_stop_the_service_too() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        let stop = molt_tools::stop_signal().unwrap();
        // SAFETY: kill only sends a signal, which stop_signal now handles.
        assert_eq!(unsafe { libc::kill(libc::getpid(), sig) }, 0);
        tokio::time::timeout(Duration::from_secs(5), stop).await.unwrap_or_else(|_| panic!("signal {sig} was missed"));
    }
}

#[tokio::test]
async fn cancellation_terminates_children_and_escalates_after_grace() {
    use tokio_util::sync::CancellationToken;
    let env = Env::new();
    let cancel = CancellationToken::new();
    let command = "trap 'echo graceful > term' TERM; (trap '' TERM; while :; do echo x >> writes; sleep 0.02; done) & echo $! > writer.pid; wait";
    let work = env.shell.handle_cancellable("run", json!({"workspace":"ws", "command": command}), cancel.clone());
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => panic!("finished before cancellation: {result:?}"),
        _ = async { while !env.ws.join("writer.pid").exists() { tokio::time::sleep(Duration::from_millis(10)).await; } } => {}
    }
    cancel.cancel();
    let response: RunResponse = serde_json::from_value(work.await.unwrap()).unwrap();
    assert!(response.cancelled && !response.success() && !response.timed_out);
    assert!(env.ws.join("term").exists(), "TERM handler ran before KILL");
    let pid = fs::read_to_string(env.ws.join("writer.pid")).unwrap().trim().parse().unwrap();
    assert!(wait_dead(pid));
    let contents = fs::read(env.ws.join("writes")).unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(contents, fs::read(env.ws.join("writes")).unwrap());
    let response = env
        .shell
        .handle_cancellable("run", json!({"workspace":"ws", "command":"touch forbidden"}), cancel)
        .await
        .unwrap_err();
    assert_eq!(response.code, ErrorCode::Cancelled);
    assert!(!env.ws.join("forbidden").exists());
}

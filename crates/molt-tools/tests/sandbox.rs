//! Real namespace boundary tests. CI requires the backend; restricted local
//! executors still exercise policy validation and fail-closed probing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use molt_api::fs::ForkResponse;
use molt_api::shell::RunResponse;
use molt_tools::{ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct Env {
    _tmp: TempDir,
    roots: Roots,
    fork: String,
    shell: Shell,
}
impl Env {
    async fn new(policy: SandboxPolicy) -> Option<Self> {
        if let Err(error) = policy.probe() {
            assert!(
                std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(),
                "required sandbox unavailable: {error:#}"
            );
            eprintln!("namespace integration test unavailable in this executor: {error:#}");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("original"), "host").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        fs::create_dir(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/value"), "dependency").unwrap();
        fs::write(tmp.path().join("host-secret"), "outside").unwrap();
        let roots = Roots { root: root.clone(), scratch: tmp.path().join("scratch") };
        let files = Fs::new(roots.clone()).unwrap();
        let fork: ForkResponse =
            serde_json::from_value(files.handle("fork", json!({"workspace":root})).await.unwrap()).unwrap();
        let shell = Shell::with_policy(roots.clone(), ExecutionPolicy::Configured(policy)).unwrap();
        Some(Self { _tmp: tmp, roots, fork: fork.fork, shell })
    }
    async fn run(&self, command: &str) -> RunResponse {
        serde_json::from_value(
            self.shell.handle("run", json!({"workspace":self.fork,"command":command})).await.unwrap(),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn mounts_confine_writes_including_symlinks_and_children() {
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let base = env.roots.root.to_str().unwrap();
    let secret = env._tmp.path().join("host-secret");
    let command = format!("test ! -e '{}' && ! cat '{}' && ! sh -c 'echo changed > {base}/original' && ! sh -c 'echo changed > ignored/value' && ln -s {base}/original escape && ! sh -c 'echo changed > escape' && echo fork > local && test \"$(cat ignored/value)\" = dependency && test \"$HOME\" = /home/molt", secret.display(), secret.display());
    let response = env.run(&command).await;
    assert!(response.success(), "{}", response.stderr);
    assert_eq!(fs::read_to_string(env.roots.root.join("original")).unwrap(), "host");
    assert_eq!(fs::read_to_string(env.roots.root.join("ignored/value")).unwrap(), "dependency");
    assert_eq!(fs::read_to_string(PathBuf::from(&env.fork).join("local")).unwrap(), "fork\n");
    let compiled = env.run("printf 'int main(void) { return 0; }\\n' > tiny.c && cc tiny.c -o tiny && ./tiny").await;
    assert!(compiled.success(), "C build failed: {compiled:?}");
    assert!(env.shell.handle("run", json!({"workspace":base, "command":"touch must-not-exist"})).await.is_err());
    assert!(!env.roots.root.join("must-not-exist").exists());
}

#[tokio::test]
async fn network_policy_and_read_only_dependencies_are_enforced() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let Some(denied) = Env::new(SandboxPolicy::default()).await else { return };
    assert!(!denied.run(&format!("echo x > /dev/tcp/127.0.0.1/{port}")).await.success());
    // AF_UNIX is denied even with host networking: a read-only mounted
    // service socket must not become an escape into the host service.
    let policy = SandboxPolicy { network: true, read_only: vec![denied.roots.root.clone()], ..Default::default() };
    let Some(allowed) = Env::new(policy).await else { return };
    assert!(allowed.run(&format!("echo x > /dev/tcp/127.0.0.1/{port}")).await.success());
    assert!(allowed
        .run(&format!(
            "cat '{}' && ! sh -c 'echo x > {}'",
            denied.roots.root.join("original").display(),
            denied.roots.root.join("original").display()
        ))
        .await
        .success());
    let response = allowed.run("python3 -c 'import socket; socket.socket(socket.AF_UNIX)' ").await;
    assert!(!response.success() && response.stderr.contains("Operation not permitted"), "{}", response.stderr);
    // Child processes still start: Rust's and libuv's use socket pairs.
    let spawn =
        "python3 -c 'import socket; socket.socketpair(); socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)'";
    assert!(allowed.run(spawn).await.success());
    assert!(!allowed.run("unshare -Ur /bin/true").await.success());
    assert!(allowed.run("test $(ulimit -n) = 256 && test $(ulimit -t) = 600").await.success());
}

#[tokio::test]
async fn cancellation_and_exit_clean_up_children_that_create_sessions() {
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let cancel = CancellationToken::new();
    let command = "trap 'echo term > graceful' TERM; setsid sh -c 'trap \"\" TERM; while :; do echo x >> writes; sleep 0.02; done' & echo ready > ready; wait";
    let work = env.shell.handle_cancellable("run", json!({"workspace":env.fork,"command":command}), cancel.clone());
    tokio::pin!(work);
    let ready = PathBuf::from(&env.fork).join("ready");
    tokio::select! {
        result = &mut work => panic!("early command completion: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), async { while !ready.exists() || !PathBuf::from(&env.fork).join("writes").exists() { tokio::time::sleep(Duration::from_millis(5)).await; }}) => { result.unwrap(); }
    }
    cancel.cancel();
    let response: RunResponse = serde_json::from_value(work.await.unwrap()).unwrap();
    assert!(response.cancelled);
    assert!(
        PathBuf::from(&env.fork).join("graceful").exists(),
        "TERM reaches the sandbox command before namespace teardown"
    );
    let writes = PathBuf::from(&env.fork).join("writes");
    let before = fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&writes).unwrap(), before, "setsid child escaped teardown");
    let response = env.run("setsid sh -c 'while :; do echo x >> after; sleep 0.02; done' & while test ! -s after; do sleep 0.01; done; exit 0").await;
    assert!(response.success());
    let after = PathBuf::from(&env.fork).join("after");
    let before = fs::read(&after).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&after).unwrap(), before, "normal completion left a daemon");
}

#[test]
fn repository_policy_and_invalid_permissions_cannot_relax_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir(&root).unwrap();
    let roots = Roots { root: root.clone(), scratch: tmp.path().join("scratch") };
    fs::write(root.join("policy.json"), "{\"network\":true}").unwrap();
    assert!(SandboxPolicy::load(&root.join("policy.json"), &roots).is_err());
    let trusted = tmp.path().join("policy.json");
    fs::write(&trusted, "{\"network\":true,\"environment\":[\"ANTHROPIC_API_KEY\"]}").unwrap();
    assert!(SandboxPolicy::load(&trusted, &roots).is_err());
    fs::write(&trusted, "{\"read_only\":[\"/\"]}").unwrap();
    assert!(SandboxPolicy::load(&trusted, &roots).is_err());
    fs::write(&trusted, "{\"memory_mb\":0}").unwrap();
    assert!(SandboxPolicy::load(&trusted, &roots).is_err());
    fs::write(&trusted, "{\"network\":true}").unwrap();
    assert!(SandboxPolicy::load(&trusted, &roots).unwrap().network);
}

#[tokio::test]
async fn timeout_and_service_shutdown_tear_down_the_namespace() {
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let command = "setsid sh -c 'trap \"\" TERM; while :; do echo x >> timeout-writes; sleep 0.02; done' & wait";
    let result: RunResponse = serde_json::from_value(
        env.shell
            .handle(
                "run",
                json!({
                    "workspace": env.fork, "command": command, "timeout_ms":500
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(result.timed_out);
    let writes = PathBuf::from(&env.fork).join("timeout-writes");
    let before = fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&writes).unwrap(), before);

    let command = "trap 'echo term > shutdown-grace' TERM; setsid sh -c 'trap \"\" TERM; while :; do echo x >> shutdown-writes; sleep 0.02; done' & wait";
    let work = env.shell.handle("run", json!({"workspace": env.fork, "command": command}));
    tokio::pin!(work);
    let writes = PathBuf::from(&env.fork).join("shutdown-writes");
    tokio::select! {
        result = &mut work => panic!("early completion: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), async { while !writes.exists() { tokio::time::sleep(Duration::from_millis(5)).await; }}) => { result.unwrap(); }
    }
    env.shell.shutdown().await;
    tokio::time::timeout(Duration::from_secs(2), work).await.unwrap().unwrap();
    assert!(PathBuf::from(&env.fork).join("shutdown-grace").exists());
    let before = fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&writes).unwrap(), before);
    let err =
        env.shell.handle("run", json!({"workspace":env.fork,"command":"touch after-shutdown"})).await.unwrap_err();
    assert_eq!(err.code, molt_proto::ErrorCode::Unavailable);
    assert!(!PathBuf::from(&env.fork).join("after-shutdown").exists());
}

#[tokio::test]
async fn dropping_a_request_tears_down_its_namespace_without_stopping_other_requests() {
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let fork = env.fork.clone();
    let shell = std::sync::Arc::new(env.shell);
    let request =
        tokio::spawn({
            let (fork, shell) = (fork.clone(), shell.clone());
            async move {
                shell.handle("run", json!({"workspace":fork,"command":
                "setsid sh -c 'trap \"\" TERM; while :; do echo x >> dropped-writes; sleep 0.02; done' & wait"
            })).await
            }
        });
    let writes = PathBuf::from(&fork).join("dropped-writes");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !writes.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&writes).unwrap(), before, "dropped request left its setsid child running");
    let next: RunResponse = serde_json::from_value(
        shell.handle("run", json!({"workspace":fork,"command":"echo next > unaffected"})).await.unwrap(),
    )
    .unwrap();
    assert!(next.success(), "{}", next.stderr);
}

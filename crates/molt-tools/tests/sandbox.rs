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
async fn build_output_goes_to_a_private_writable_directory_outside_the_fork_that_later_commands_reuse() {
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let response = env
        .run("test \"$PYTHONDONTWRITEBYTECODE\" = 1 && echo built > \"$CARGO_TARGET_DIR/out\" && echo cached > \"$GOCACHE/entry\" && printf '%s\\n%s\\n' \"$CARGO_TARGET_DIR\" \"$GOCACHE\"")
        .await;
    assert!(response.success(), "{response:?}");
    let build = env.roots.scratch.canonicalize().unwrap().join(".molt/build");
    let fork = PathBuf::from(&env.fork);
    let dirs: Vec<PathBuf> = response.stdout.lines().map(PathBuf::from).collect();
    assert_eq!(dirs.len(), 2, "{}", response.stdout);
    for dir in &dirs {
        assert!(dir.starts_with(&build), "{} is not in {}", dir.display(), build.display());
        assert!(!dir.starts_with(&fork) && !dir.starts_with(&env.roots.root));
    }
    assert_eq!(fs::read_to_string(dirs[0].join("out")).unwrap(), "built\n");
    assert_eq!(fs::read_to_string(dirs[1].join("entry")).unwrap(), "cached\n");
    // Kept for later commands, also in another fork of the same workspace.
    let files = Fs::new(env.roots.clone()).unwrap();
    let other: ForkResponse =
        serde_json::from_value(files.handle("fork", json!({"workspace":env.roots.root})).await.unwrap()).unwrap();
    let command = "cat \"$CARGO_TARGET_DIR/out\" \"$GOCACHE/entry\"";
    let again: RunResponse = serde_json::from_value(
        env.shell.handle("run", json!({"workspace":other.fork,"command":command})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(again.stdout, "built\ncached\n", "{again:?}");
    // Python leaves no bytecode in the fork, and nothing else of the build root is visible.
    let python = env
        .run("echo 'x = 1' > module.py && python3 -c 'import module' && test ! -e __pycache__ && ! ls \"$(dirname \"$CARGO_TARGET_DIR\")/cargo-0.lock\"")
        .await;
    assert!(python.success(), "{python:?}");
}

#[tokio::test]
async fn variables_a_profile_sets_name_the_commands_workspace_and_build_directory() {
    use std::os::unix::fs::PermissionsExt;
    let mut policy = SandboxPolicy::default();
    policy.set_env.insert("VIRTUAL_ENV".into(), "$WORKSPACE/.venv".into());
    policy.set_env.insert("PATH".into(), "$WORKSPACE/.venv/bin:$PATH".into());
    policy.set_env.insert("CACHE".into(), "$BUILD/cache".into());
    let Some(env) = Env::new(policy).await else { return };
    let fork = PathBuf::from(&env.fork).canonicalize().unwrap();
    fs::create_dir_all(fork.join(".venv/bin")).unwrap();
    fs::write(fork.join(".venv/bin/python"), "#!/bin/sh\necho venv\n").unwrap();
    fs::set_permissions(fork.join(".venv/bin/python"), fs::Permissions::from_mode(0o755)).unwrap();
    let ran = env.run("python && printf '%s\\n%s\\n' \"$VIRTUAL_ENV\" \"$CACHE\" && command -v cat").await;
    assert!(ran.success(), "{ran:?}");
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[..2], ["venv", fork.join(".venv").to_str().unwrap()], "{ran:?}");
    assert!(PathBuf::from(lines[2]).starts_with(env.roots.scratch.canonicalize().unwrap().join(".molt/build")));
    assert!(lines[3].ends_with("/cat"), "the rest of PATH is kept: {ran:?}");
    for (name, value) in [("ANTHROPIC_API_KEY", "x"), ("HOME", "/"), ("TMPDIR", "/x"), ("A", "nul\0")] {
        let mut bad = SandboxPolicy::default();
        bad.set_env.insert(name.into(), value.into());
        assert!(bad.probe().is_err(), "{name} cannot be set");
    }
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

#[tokio::test]
async fn a_process_runs_in_the_sandbox_and_stopping_it_tears_down_children_that_create_sessions() {
    use molt_api::shell::{ProcessState, ProcessStatus, StopResponse};
    let Some(env) = Env::new(SandboxPolicy::default()).await else { return };
    let base = env.roots.root.to_str().unwrap();
    let command = format!(
        "echo \"home=$HOME\"; ! sh -c 'echo changed > {base}/original' 2>/dev/null && echo confined; setsid sh -c 'trap \"\" TERM; while :; do echo x >> daemon-writes; sleep 0.02; done' & echo ready; cat"
    );
    let started: ProcessState = serde_json::from_value(
        env.shell
            .handle("start", json!({"workspace": env.fork, "command": command, "ready": "ready", "wait_ms": 5000}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!((started.status, started.ready), (ProcessStatus::Running, Some(true)), "{started:?}");
    assert!(started.output.contains("home=/home/molt\nconfined\n"), "{started:?}");
    assert_eq!(fs::read_to_string(env.roots.root.join("original")).unwrap(), "host");
    // Its input reaches it inside the sandbox.
    let wrote = env.shell.handle("write", json!({"workspace": env.fork, "id": started.id, "input": "echoed\n"})).await;
    assert!(wrote.is_ok(), "{wrote:?}");
    let read: ProcessState = serde_json::from_value(
        env.shell.handle("read", json!({"workspace": env.fork, "id": started.id, "wait_ms": 3000})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(read.output, "echoed\n");
    let writes = PathBuf::from(&env.fork).join("daemon-writes");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !writes.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let stopped: StopResponse =
        serde_json::from_value(env.shell.handle("stop", json!({"workspace": env.fork})).await.unwrap()).unwrap();
    assert_eq!(stopped.stopped[0].status, ProcessStatus::Stopped);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = fs::read(&writes).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fs::read(&writes).unwrap(), before, "a setsid child outlived its stopped process");
    // A process, like a command, runs only in a fork.
    let err = env.shell.handle("start", json!({"workspace": base, "command": "touch must-not-exist"})).await;
    assert!(err.is_err());
    assert!(!env.roots.root.join("must-not-exist").exists());
}

#[test]
fn a_sandbox_the_host_cannot_afford_names_the_resource_and_never_starts_unconfined() {
    use molt_tools::resources::{Resource, Setup};
    use molt_tools::sandbox::Unavailable;
    if let Err(error) = SandboxPolicy::default().probe() {
        assert!(std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(), "required sandbox unavailable: {error:#}");
        eprintln!("namespace integration test unavailable in this executor: {error:#}");
        return;
    }
    // Six descriptors are too few for bubblewrap to set up a sandbox: a shortage that
    // repeats on every host, as a host out of descriptors, processes or namespaces is.
    let starved = SandboxPolicy { open_files: 6, ..SandboxPolicy::default() };
    let started = std::time::Instant::now();
    let error = starved.probe().unwrap_err();
    let waited = started.elapsed();
    let unavailable = error.downcast_ref::<Unavailable>().expect("the reason is told");
    assert_eq!(unavailable.setup, Setup::Short(Resource::OpenFiles));
    let said = format!("{error:#}");
    assert!(said.starts_with("short of host resources: open files: "), "{said}");
    assert!(said.contains("Too many open files") || said.contains("Error 24"), "{said}");
    assert!(said.contains("(tried 5 times)") && said.contains("commands get the sandbox's `open_files`, 6"), "{said}");
    assert!(!said.contains("--no-sandbox"), "a shortage is not a missing sandbox: {said}");
    // Bounded: it waited between tries, and gave up.
    assert!(waited >= Duration::from_millis(1500) && waited < Duration::from_secs(10), "{waited:?}");

    // A shell service under that policy does not start, and nothing runs unconfined instead.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("project")).unwrap();
    let roots = Roots { root: tmp.path().join("project"), scratch: tmp.path().join("scratch") };
    let refused = Shell::with_policy(roots.clone(), ExecutionPolicy::Configured(starved)).err().unwrap();
    assert!(format!("{refused:#}").starts_with("short of host resources: open files"), "{refused:#}");
    // With what it needs again, it starts.
    Shell::with_policy(roots, ExecutionPolicy::Configured(SandboxPolicy::default())).unwrap();
}

//! Candidate services in the Linux sandbox (`docs/services.md`), started by
//! `molt::start` from a configuration as a user's would be.
//!
//! The hostile candidate is this test binary itself: run by the supervisor
//! as `candidate <fixture> --exact`, `candidate_fixture` below becomes the
//! service, which tries to reach what it must not and reports what it saw.
//! CI requires the sandbox; an executor without one skips these tests
//! unless `MOLT_REQUIRE_SANDBOX_TESTS` is set.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use molt::config::Config;
use molt_kernel::audit::{self, AuditEvent};
use molt_kernel::supervisor::{Backend, RestartPolicy};
use molt_proto::{Budget, CapId, ErrorCode, Exec, Manifest, ServiceId, Target, Tier};
use molt_sdk::{CallOpts, SdkError, Service};
use molt_tools::SandboxPolicy;
use serde_json::{json, Value};

fn sid(s: &str) -> ServiceId {
    ServiceId::new(s).unwrap()
}

/// Whether the sandbox can run here; when it cannot, the test is skipped,
/// unless the sandbox is required.
fn sandbox_available() -> bool {
    match SandboxPolicy::default().probe() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(),
                "required sandbox unavailable: {error:#}"
            );
            eprintln!("no sandbox in this executor, so candidate services cannot run: {error:#}");
            false
        }
    }
}

/// Join the bus of a running molt as `name`.
async fn join(running: &molt::Running, sockets: &Path, name: &str) -> Service {
    let id = sid(name);
    let secret = running.kernel().register(&id).await.unwrap();
    let link = molt_transport::unix::UnixLink::connect(sockets, &id, &secret).await.unwrap();
    Service::new(id, Box::new(link), HashMap::new())
}

/// Call `to` until the service answers (it may still be starting or restarting).
async fn call_until_ok(client: &Service, cap: &CapId, to: &str, payload: Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let opts = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 10_000, 0), trace: None };
        match client.call(to, payload.clone(), opts).await {
            Ok(v) => return v,
            Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => panic!("{to} never answered: {e}"),
        }
    }
}

async fn call(client: &Service, cap: &CapId, to: &str, payload: Value) -> Result<Value, SdkError> {
    let opts = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 10_000, 0), trace: None };
    client.call(to, payload, opts).await
}

/// The real candidate: the example `echo` service, sandboxed through the
/// configuration, keeps a counter in its private state across a crash and
/// a restart, and answers through the bus like any service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sandboxed_echo_service_answers_on_the_bus_and_keeps_only_its_private_state() {
    if !sandbox_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let cfg = Config::parse(&format!(
        r#"
        [kernel]
        data_dir = "{data}"
        fsync = false

        [[service]]
        name = "echo"
        tier = "mutable"
        isolation = "sandbox"
        exec = {{ command = "{echo}" }}
        provides = ["echo.*"]
        max_restarts = 3
        "#,
        data = data.display(),
        echo = env!("CARGO_BIN_EXE_molt-echo"),
    ))
    .unwrap();
    let running = molt::start(&cfg).await.unwrap();
    let client = join(&running, &cfg.socket_dir(), "client").await;
    let cap =
        running.kernel().grant(client.id(), "echo.*".parse().unwrap(), Budget::new(0, 0, 100), None).await.unwrap();

    assert_eq!(call_until_ok(&client, &cap, "echo.echo", json!({ "hi": 1 })).await, json!({ "hi": 1 }));
    assert_eq!(call(&client, &cap, "echo.count", Value::Null).await.unwrap(), json!(1));
    assert_eq!(call(&client, &cap, "echo.count", Value::Null).await.unwrap(), json!(2));
    match call(&client, &cap, "echo.crash", Value::Null).await {
        Err(SdkError::Remote(e)) => assert!(matches!(e.code, ErrorCode::Unavailable | ErrorCode::Timeout), "{e}"),
        other => panic!("expected the crash to fail the call, got {other:?}"),
    }
    // Restarted, still confined and on the bus, with the state it wrote.
    assert_eq!(call_until_ok(&client, &cap, "echo.count", Value::Null).await, json!(3));
    let state = data.join("state/echo");
    assert_eq!(std::fs::read_to_string(state.join("count")).unwrap(), "3");
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
    }
    let audit_path = running.kernel().audit_path().to_owned();
    running.shutdown().await;
    let entries = audit::read_all(&audit_path).await.unwrap();
    let starts = entries
        .iter()
        .filter(|e| matches!(&e.event, AuditEvent::ServiceStarted { service, .. } if service.as_str() == "echo"))
        .count();
    assert!(starts >= 2, "expected a restart, saw {starts} starts");
}

/// Whatever the reason the sandbox cannot start, the candidate is not
/// started at all, rather than as a host process.
#[tokio::test]
async fn an_unavailable_sandbox_stops_the_candidate_instead_of_running_it_natively() {
    let tmp = tempfile::tempdir().unwrap();
    let sockets = tmp.path().join("sock");
    let transport = Arc::new(molt_transport::unix::UnixTransport::new(&sockets).unwrap());
    let mut config = molt_kernel::Config::new(tmp.path().join("data"));
    config.fsync = false;
    let kernel = molt_kernel::Kernel::start(config, transport).await.unwrap();
    kernel
        .install(Manifest {
            name: sid("echo"),
            tier: Tier::Mutable,
            parent: None,
            provides: vec!["echo.*".parse().unwrap()],
            requests: vec![],
            exec: Some(Exec { command: env!("CARGO_BIN_EXE_molt-echo").into(), args: vec![] }),
            tools: Vec::new(),
        })
        .await
        .unwrap();
    // A sandbox that cannot start: its probe fails as a missing bwrap would.
    let broken = SandboxPolicy { memory_mb: 0, ..SandboxPolicy::default() };
    let backend: Arc<dyn Backend> = Arc::new(molt::confine::LinuxSandbox::new(broken));
    let restart = RestartPolicy::default();
    let err =
        kernel.launch_confined(&sid("echo"), None, Default::default(), restart, vec![], backend).await.unwrap_err();
    assert!(err.to_string().contains("the linux-sandbox backend is unavailable"), "{err}");
    let path = kernel.audit_path().to_owned();
    kernel.shutdown().await;
    let entries = audit::read_all(&path).await.unwrap();
    assert!(!entries.iter().any(|e| matches!(&e.event, AuditEvent::ServiceStarted { .. })), "echo did not run");
}

/// What the fixture tries, each with whether it got through.
type Report = BTreeMap<String, Value>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_candidate_reaches_only_what_it_was_granted() {
    if !sandbox_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let data = tmp_path.join("data");
    let host_secret = tmp_path.join("host-secret");
    std::fs::write(&host_secret, "the user's file").unwrap();
    // What a gate outside the candidate decided about it.
    let evaluations = tmp_path.join("evaluations.json");
    std::fs::write(&evaluations, r#"{"rogue": "rejected"}"#).unwrap();
    // Another service's private state.
    std::fs::create_dir_all(data.join("state/other")).unwrap();
    std::fs::write(data.join("state/other/secret"), "other's").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let host_home = std::env::var("HOME").unwrap();
    let exe = std::env::current_exe().unwrap();
    let sockets = data.join("sock");

    let cfg = Config::parse(&format!(
        r#"
        [kernel]
        data_dir = "{data}"
        fsync = false

        [[service]]
        name = "rogue"
        tier = "mutable"
        isolation = "sandbox"
        exec = {{ command = "{exe}", args = ["candidate_fixture", "--exact", "--nocapture", "--test-threads=1"] }}
        provides = ["rogue.*"]
        requests = [{{ target = "recorder.report", budget = {{ calls = 4 }} }}]
        max_restarts = 3
        env = {{ PROBE_HOST_FILE = "{host_secret}", PROBE_HOST_HOME = "{host_home}", PROBE_EVALUATIONS = "{evaluations}", PROBE_DATA = "{data}", PROBE_SOCKET = "{socket}", PROBE_PORT = "{port}", PROBE_KERNEL_PID = "{pid}" }}
        "#,
        data = data.display(),
        exe = exe.display(),
        host_secret = host_secret.display(),
        evaluations = evaluations.display(),
        socket = sockets.join("recorder.sock").display(),
        pid = std::process::id(),
    ))
    .unwrap();
    let running = molt::start(&cfg).await.unwrap();
    let recorder = join(&running, &sockets, "recorder").await;
    let (reports_tx, mut reports) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        recorder
            .serve(|req| {
                let tx = reports_tx.clone();
                async move {
                    let _ = tx.send(req.payload.clone());
                    Ok(json!("recorded"))
                }
            })
            .await
    });
    let client = join(&running, &sockets, "client").await;
    let cap =
        running.kernel().grant(client.id(), "rogue.*".parse().unwrap(), Budget::new(0, 0, 100), None).await.unwrap();

    let report: Report =
        serde_json::from_value(call_until_ok(&client, &cap, "rogue.probe", Value::Null).await).unwrap();
    let blocked = [
        "read the user's file",
        "list the user's home",
        "read /etc/passwd",
        "read another service's state",
        "read the registry",
        "write the registry",
        "read the audit log",
        "append to the audit log",
        "rewrite the evaluation results",
        "list the sockets",
        "connect to another service's socket",
        "open a Unix socket",
        "connect to a port on the host",
        "resolve a host name",
        "see the kernel's process",
        "make a user namespace",
        "set up io_uring",
        "mount a file system",
        "write its own program",
        "store a version over the bus",
        "promote itself over the bus",
    ];
    for what in blocked {
        let outcome = report.get(what).unwrap_or_else(|| panic!("the fixture did not try to {what}: {report:#?}"));
        assert!(outcome["blocked"] == json!(true), "it could {what}: {outcome}");
    }
    for what in ["write its private state", "call the capability it was granted"] {
        assert_eq!(report[what]["blocked"], json!(false), "it could not {what}: {}", report[what]);
    }
    let env: BTreeSet<String> = serde_json::from_value(report["its environment"]["names"].clone()).unwrap();
    let expected = [
        "HOME",
        "MOLT_ADDRESS",
        "MOLT_CAPS",
        "MOLT_SECRET",
        "MOLT_SERVICE_ID",
        "MOLT_STATE_DIR",
        "MOLT_VERSION",
        "PATH",
        "PROBE_DATA",
        "PROBE_EVALUATIONS",
        "PROBE_HOST_FILE",
        "PROBE_HOST_HOME",
        "PROBE_KERNEL_PID",
        "PROBE_PORT",
        "PROBE_SOCKET",
        "PWD",
        "TMPDIR",
    ];
    assert_eq!(env, expected.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(), "nothing of the host's");
    // The granted call arrived, and nothing it tried changed the host.
    assert_eq!(reports.recv().await.unwrap(), json!({ "from": "probe" }));
    assert_eq!(std::fs::read_to_string(&host_secret).unwrap(), "the user's file");
    assert_eq!(std::fs::read_to_string(&evaluations).unwrap(), r#"{"rogue": "rejected"}"#);
    assert_eq!(std::fs::read_to_string(data.join("state/rogue/note")).unwrap(), "mine");
    assert!(listener.set_nonblocking(true).is_ok() && listener.accept().is_err(), "nothing connected to the host port");

    // Relayed calls spend its budget: four calls, the probe's included.
    assert_eq!(call(&client, &cap, "rogue.relay", Value::Null).await.unwrap(), json!("recorded"));
    // A crash is restarted; it comes back confined, with what it may do unchanged.
    assert!(call(&client, &cap, "rogue.crash", Value::Null).await.is_err());
    let again: Report = serde_json::from_value(call_until_ok(&client, &cap, "rogue.probe", Value::Null).await).unwrap();
    assert_eq!(again["read the user's file"]["blocked"], json!(true));
    assert_eq!(again["read the registry"]["blocked"], json!(true));
    assert_eq!(again["call the capability it was granted"]["blocked"], json!(false));
    assert_eq!(call(&client, &cap, "rogue.relay", Value::Null).await.unwrap(), json!("recorded"));
    assert_eq!(call(&client, &cap, "rogue.relay", Value::Null).await.unwrap(), json!("over_budget"));
    // Revoking its capability stops it.
    let held: CapId = serde_json::from_value(again["its capability"]["id"].clone()).unwrap();
    assert!(running.kernel().revoke(&held).await.unwrap());
    assert_eq!(call(&client, &cap, "rogue.relay", Value::Null).await.unwrap(), json!("denied"));
    // It cannot make itself the live version: no gate rights reach it, and
    // the registry is where it left it.
    let live = running.kernel().registry().live(&sid("rogue")).unwrap();
    running.shutdown().await;
    let registry = std::fs::read_to_string(data.join("registry/registry.json")).unwrap();
    assert!(registry.contains(&live.0));
    assert!(audit::verify(&data.join("audit.jsonl")).await.is_ok(), "the audit log is intact");
}

/// The hostile candidate. A no-op when this binary runs as a test suite.
#[test]
fn candidate_fixture() {
    if std::env::var_os("MOLT_SERVICE_ID").is_none() {
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let svc = Service::connect_from_env().await.expect("the bus");
        let svc = &svc;
        svc.serve(|req| async move {
            let method = match &req.to {
                Target::Method { method, .. } => method.clone(),
                _ => String::new(),
            };
            match method.as_str() {
                "probe" => Ok(serde_json::to_value(probe(svc).await).unwrap()),
                "relay" => {
                    Ok(match svc.call("recorder.report", json!({ "from": "relay" }), CallOpts::default()).await {
                        Ok(v) => v,
                        Err(SdkError::Remote(e)) => serde_json::to_value(e.code).unwrap(),
                        Err(e) => json!(e.to_string()),
                    })
                }
                "crash" => std::process::exit(3),
                other => Err(molt_proto::RemoteError { code: ErrorCode::Invalid, message: format!("no {other}") }),
            }
        })
        .await;
    });
    std::process::exit(0);
}

fn outcome<T, E: std::fmt::Display>(result: Result<T, E>) -> Value {
    match result {
        Ok(_) => json!({ "blocked": false }),
        Err(e) => json!({ "blocked": true, "why": e.to_string() }),
    }
}

/// The user's home is hidden when it cannot be listed, or when all it
/// shows is the way to this program: the sandbox mounts the program at its
/// own path, which in CI is a checkout under the home.
fn home_listing(home: &Path) -> Value {
    let program = std::env::current_exe().unwrap();
    match std::fs::read_dir(home) {
        Err(e) => json!({ "blocked": true, "why": e.to_string() }),
        Ok(entries) => {
            let shown: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| !program.starts_with(path))
                .map(|path| path.display().to_string())
                .collect();
            json!({ "blocked": shown.is_empty(), "shown": shown })
        }
    }
}

fn last_os_error(ret: libc::c_long) -> Result<(), std::io::Error> {
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn probe(svc: &Service) -> Report {
    let var = |name: &str| PathBuf::from(std::env::var(name).unwrap());
    let data = var("PROBE_DATA");
    let mut report = Report::new();
    let mut tried = |what: &str, value: Value| {
        report.insert(what.to_owned(), value);
    };
    tried("read the user's file", outcome(std::fs::read_to_string(var("PROBE_HOST_FILE"))));
    tried("list the user's home", home_listing(&var("PROBE_HOST_HOME")));
    tried("read /etc/passwd", outcome(std::fs::read_to_string("/etc/passwd")));
    tried("read another service's state", outcome(std::fs::read_to_string(data.join("state/other/secret"))));
    tried("read the registry", outcome(std::fs::read(data.join("registry/registry.json"))));
    tried("write the registry", outcome(std::fs::write(data.join("registry/registry.json"), "{}")));
    tried("read the audit log", outcome(std::fs::read(data.join("audit.jsonl"))));
    tried("append to the audit log", outcome(std::fs::OpenOptions::new().append(true).open(data.join("audit.jsonl"))));
    tried(
        "rewrite the evaluation results",
        outcome(std::fs::write(var("PROBE_EVALUATIONS"), r#"{"rogue": "approved"}"#)),
    );
    tried("list the sockets", outcome(std::fs::read_dir(data.join("sock"))));
    tried("connect to another service's socket", outcome(std::os::unix::net::UnixStream::connect(var("PROBE_SOCKET"))));
    tried(
        "open a Unix socket",
        outcome(last_os_error(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) } as _)),
    );
    let port: u16 = std::env::var("PROBE_PORT").unwrap().parse().unwrap();
    tried(
        "connect to a port on the host",
        outcome(std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))),
    );
    tried("resolve a host name", outcome(std::net::ToSocketAddrs::to_socket_addrs("example.com:443")));
    let kernel = format!("/proc/{}/environ", std::env::var("PROBE_KERNEL_PID").unwrap());
    tried("see the kernel's process", outcome(std::fs::read(kernel)));
    tried("make a user namespace", outcome(last_os_error(unsafe { libc::unshare(libc::CLONE_NEWUSER) } as _)));
    tried("set up io_uring", outcome(last_os_error(unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, 0) })));
    tried(
        "mount a file system",
        outcome(last_os_error(unsafe {
            libc::mount(c"none".as_ptr(), c"/tmp".as_ptr(), c"tmpfs".as_ptr(), 0, std::ptr::null())
        } as _)),
    );
    let me = std::env::current_exe().unwrap();
    tried("write its own program", outcome(std::fs::OpenOptions::new().append(true).open(me)));
    let state = PathBuf::from(std::env::var("MOLT_STATE_DIR").unwrap());
    tried(
        "write its private state",
        outcome(std::fs::write(state.join("note"), "mine").and_then(|()| std::fs::read_to_string(state.join("note")))),
    );
    let names: Vec<String> = std::env::vars().map(|(name, _)| name).collect();
    tried("its environment", json!({ "names": names }));
    let caps: HashMap<String, CapId> = serde_json::from_str(&std::env::var("MOLT_CAPS").unwrap()).unwrap();
    tried("its capability", json!({ "id": caps["recorder.report"] }));
    // Over the bus, without a right to approve versions.
    let version = std::env::var("MOLT_VERSION").unwrap();
    let mine = Manifest {
        name: sid("rogue"),
        tier: Tier::Mutable,
        parent: None,
        provides: vec!["rogue.*".parse().unwrap()],
        requests: vec![molt_proto::CapRequest { target: "kernel.*".parse().unwrap(), budget: Budget::new(0, 0, 1) }],
        exec: Some(Exec { command: "/bin/true".into(), args: vec![] }),
        tools: Vec::new(),
    };
    let proposed = svc.kernel("registry.propose", None, json!({ "manifest": mine })).await;
    tried("store a version over the bus", outcome(proposed));
    let promoted = svc
        .kernel("registry.promote", None, json!({ "service": "rogue", "expected": version, "version": version }))
        .await;
    tried("promote itself over the bus", outcome(promoted));
    let granted = svc.call("recorder.report", json!({ "from": "probe" }), CallOpts::default()).await;
    tried("call the capability it was granted", outcome(granted));
    report
}

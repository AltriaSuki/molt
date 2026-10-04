//! End to end: the kernel supervising a real service process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use molt_kernel::audit::{self, AuditEvent};
use molt_kernel::supervisor::{Limits, RestartPolicy};
use molt_kernel::{Config, Kernel};
use molt_proto::{Budget, ErrorCode, Exec, Manifest, ServiceId, Tier};
use molt_sdk::{CallOpts, SdkError, Service};
use molt_transport::{Secret, Transport};
use serde_json::json;

fn sid(s: &str) -> ServiceId {
    ServiceId::new(s).unwrap()
}

fn echo_manifest() -> Manifest {
    Manifest {
        name: sid("echo"),
        tier: Tier::Mutable,
        parent: None,
        provides: vec!["echo.*".parse().unwrap()],
        requests: vec![],
        exec: Some(Exec { command: env!("CARGO_BIN_EXE_molt-echo").into(), args: vec![] }),
    }
}

fn quick_restarts() -> RestartPolicy {
    RestartPolicy {
        max_restarts: 3,
        backoff_initial: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        drain: Duration::from_secs(2),
    }
}

/// Call until the service answers (it may still be starting or restarting).
async fn call_until_ok(client: &Service, cap: &molt_proto::CapId, payload: serde_json::Value) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let opts = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 2000, 0), trace: None };
        match client.call("echo.echo", payload.clone(), opts).await {
            Ok(v) => return v,
            Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => panic!("echo never answered: {e}"),
        }
    }
}

async fn scenario(kernel: Kernel, client: Service, echo_secret: Option<Secret>) {
    kernel.install(echo_manifest()).await.unwrap();
    let launched = kernel
        .launch(&sid("echo"), echo_secret, Limits { memory_bytes: Some(2 << 30), cpu_secs: Some(60) }, quick_restarts())
        .await
        .unwrap();
    assert!(launched.version.0.starts_with("sha256:"));
    let cap = kernel.grant(client.id(), "echo.*".parse().unwrap(), Budget::new(0, 5000, 1000), None).await.unwrap();

    assert_eq!(call_until_ok(&client, &cap, json!({ "hi": 1 })).await, json!({ "hi": 1 }));

    // The service crashes mid-request: the caller hears about it promptly,
    // well before the 5 s deadline, and the supervisor restarts it.
    let started = Instant::now();
    let opts = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 5000, 0), trace: None };
    match client.call("echo.crash", json!(null), opts).await {
        Err(SdkError::Remote(e)) => assert!(matches!(e.code, ErrorCode::Unavailable | ErrorCode::Timeout), "{e}"),
        other => panic!("expected an error, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(4), "crash took {:?} to surface", started.elapsed());
    assert_eq!(call_until_ok(&client, &cap, json!("again")).await, json!("again"));

    let path = kernel.audit_path().to_path_buf();
    kernel.shutdown().await;
    audit::verify(&path).await.unwrap();
    let entries = audit::read_all(&path).await.unwrap();
    let starts = entries.iter().filter(|e| matches!(&e.event, AuditEvent::ServiceStarted { .. })).count();
    let exits = entries.iter().filter(|e| matches!(&e.event, AuditEvent::ServiceExited { .. })).count();
    assert!(starts >= 2, "expected a restart, saw {starts} starts");
    assert!(exits >= 1);
}

#[tokio::test]
async fn unix_kernel_supervises_a_crashing_service() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("sock");
    let transport = Arc::new(molt_transport::unix::UnixTransport::new(&sock).unwrap());
    let mut config = Config::new(dir.path().join("data"));
    config.fsync = false;
    let kernel = Kernel::start(config, transport).await.unwrap();
    let secret = kernel.register(&sid("client")).await.unwrap();
    let link = molt_transport::unix::UnixLink::connect(&sock, &sid("client"), &secret).await.unwrap();
    let client = Service::new(sid("client"), Box::new(link), HashMap::new());
    scenario(kernel, client, None).await;
}

#[tokio::test]
async fn nats_kernel_supervises_a_crashing_service() {
    let Some(bin) = std::env::var_os("MOLT_NATS_SERVER").map(std::path::PathBuf::from).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("nats-server")).find(|p| p.is_file())
    }) else {
        eprintln!("skipping: nats-server not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (kernel_secret, echo_secret, client_secret) = (Secret::random(), Secret::random(), Secret::random());
    let conf = molt_transport::nats::server_config(
        molt_transport::nats::DEFAULT_PREFIX,
        &kernel_secret,
        &[(sid("echo"), echo_secret.clone()), (sid("client"), client_secret.clone())],
    );
    std::fs::write(dir.path().join("nats.conf"), conf).unwrap();
    let mut server = std::process::Command::new(bin)
        .args(["-a", "127.0.0.1", "-p", &port.to_string(), "-c"])
        .arg(dir.path().join("nats.conf"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("nats://127.0.0.1:{port}");
    let mut transport = None;
    for _ in 0..100 {
        if let Ok(t) = molt_transport::nats::NatsTransport::connect(&url, &kernel_secret, "molt").await {
            transport = Some(t);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let transport: Arc<dyn Transport> = Arc::new(transport.expect("nats-server did not start"));
    let mut config = Config::new(dir.path().join("data"));
    config.fsync = false;
    let kernel = Kernel::start(config, transport).await.unwrap();
    kernel.register_with_secret(&sid("client"), client_secret.clone()).await.unwrap();
    let link = molt_transport::nats::NatsLink::connect(&url, &sid("client"), &client_secret).await.unwrap();
    let client = Service::new(sid("client"), Box::new(link), HashMap::new());
    scenario(kernel, client, Some(echo_secret)).await;
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn audit_verify_cli_reports_a_good_and_a_tampered_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let log = audit::AuditLog::open(&path, false).await.unwrap();
        for _ in 0..3 {
            log.append(AuditEvent::ServiceStarted { service: sid("x"), pid: None }).await.unwrap();
        }
    });
    let run = |p: &std::path::Path| {
        std::process::Command::new(env!("CARGO_BIN_EXE_molt")).args(["audit", "verify"]).arg(p).output().unwrap()
    };
    let ok = run(&path);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("3 entries, chain intact"));
    let text = std::fs::read_to_string(&path).unwrap().replace("\"x\"", "\"y\"");
    std::fs::write(&path, text).unwrap();
    assert!(!run(&path).status.success());
}

#[tokio::test]
async fn molt_run_stops_its_services_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let config = dir.path().join("molt.toml");
    let echo = env!("CARGO_BIN_EXE_molt-echo");
    let text = format!(
        "[kernel]\ndata_dir = {:?}\nfsync = false\n\n[[service]]\nname = \"echo\"\nexec = {{ command = {echo:?} }}\n",
        data.to_str().unwrap()
    );
    std::fs::write(&config, text).unwrap();
    let molt = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .arg("--config")
        .arg(&config)
        .arg("run")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let echo_pid = loop {
        let entries = audit::read_all(&data.join("audit.jsonl")).await.unwrap_or_default();
        if let Some(pid) = entries.iter().find_map(|e| match &e.event {
            AuditEvent::ServiceStarted { pid, .. } => *pid,
            _ => None,
        }) {
            break pid;
        }
        assert!(Instant::now() < deadline, "echo never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::kill(molt.id().unwrap() as i32, libc::SIGTERM) }, 0);
    let out =
        tokio::time::timeout(Duration::from_secs(20), molt.wait_with_output()).await.expect("molt run hung").unwrap();
    assert!(out.status.success(), "{:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    assert!(!std::path::Path::new(&format!("/proc/{echo_pid}")).exists(), "echo outlived molt run");
}

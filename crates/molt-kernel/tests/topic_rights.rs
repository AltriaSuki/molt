//! The same topic authorization checks over both authenticated transports.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::{code, sid, target, World};
use molt_kernel::audit::{self, AuditEvent};
use molt_kernel::{Config, Kernel};
use molt_proto::{Budget, CapId, Envelope, ErrorCode, ServiceId, TraceId};
use molt_sdk::{SdkError, Service};
use molt_transport::nats::{server_config, NatsLink, NatsTransport, DEFAULT_PREFIX};
use molt_transport::Secret;
use serde_json::json;

async fn grant(kernel: &Kernel, holder: &Service, right: &str) -> CapId {
    let cap = kernel.grant(holder.id(), target(right), Budget::new(0, 0, 20), None).await.unwrap();
    holder.add_cap(right, cap.clone());
    cap
}

async fn exercise(kernel: &Kernel, publisher: &Service, listener: &Service, child: &Service, legacy: &Service) {
    let publish = grant(kernel, publisher, "topic:publish:builds").await;
    let subscribe = grant(kernel, listener, "topic:subscribe:builds").await;
    let both = grant(kernel, legacy, "topic:builds").await;
    listener.subscribe("builds").await.unwrap();
    legacy.subscribe("builds").await.unwrap();
    assert!(matches!(listener.publish("builds", json!("forged")).await, Err(SdkError::NoCapability(_))));
    assert!(matches!(publisher.subscribe("builds").await, Err(SdkError::NoCapability(_))));
    assert_eq!(code(publisher.kernel("subscribe", Some(publish), json!({"topic":"builds"})).await), ErrorCode::Denied);

    // Bypass SDK selection with the real subscription ID: the kernel must
    // deny it on the bus, record the denial, and deliver no forged event.
    let forged = Envelope::event(TraceId::random(), "builds", subscribe.clone(), json!("forged")).unwrap();
    let forged_id = forged.id.clone();
    let reply = tokio::time::timeout(Duration::from_secs(5), listener.send_request(forged)).await.unwrap().unwrap();
    assert_eq!(reply.error().unwrap().code, ErrorCode::Denied);
    let entries = audit::read_all(kernel.audit_path()).await.unwrap();
    assert!(entries.iter().any(|e| matches!(&e.event, AuditEvent::Denied { msg, from, code, .. }
        if msg == &forged_id && from == listener.id() && *code == ErrorCode::Denied)));
    for widened in ["topic:publish:builds", "topic:builds", "topic:subscribe:other"] {
        assert_eq!(
            code(
                listener.delegate(&subscribe, child.id(), widened, Budget::new(0, 0, 1), None).await.map(|c| json!(c))
            ),
            ErrorCode::Denied
        );
    }
    let child_cap =
        legacy.delegate(&both, child.id(), "topic:subscribe:builds", Budget::new(0, 0, 5), None).await.unwrap();
    child.add_cap("topic:subscribe:builds", child_cap);
    child.subscribe("builds").await.unwrap();
    publisher.publish("builds", json!("real")).await.unwrap();
    for service in [listener, child, legacy] {
        let event = tokio::time::timeout(Duration::from_secs(5), service.next()).await.unwrap().unwrap();
        assert_eq!(event.payload, json!("real"));
        assert_eq!(event.from.as_ref(), Some(publisher.id()));
    }
    // Legacy grants still publish; narrow grants never cover combined ones.
    legacy.publish("builds", json!("legacy")).await.unwrap();
    for service in [listener, child] {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), service.next()).await.unwrap().unwrap().payload,
            json!("legacy")
        );
    }
    kernel.revoke(&subscribe).await.unwrap();
    kernel.revoke(&both).await.unwrap();
    publisher.publish("builds", json!("after revocation")).await.unwrap();
    publisher.kernel("ping", None, json!(null)).await.unwrap();
    for service in [listener, child, legacy] {
        assert!(tokio::time::timeout(Duration::from_millis(100), service.next()).await.is_err());
    }
}

#[tokio::test]
async fn unix_topic_subscribers_cannot_publish_or_widen_their_rights() {
    let w = World::new().await;
    let publisher = w.join("publisher").await;
    let listener = w.join("listener").await;
    let child = w.join("child").await;
    let legacy = w.join("legacy").await;
    exercise(&w.kernel, &publisher, &listener, &child, &legacy).await;
}

fn nats_binary() -> Option<PathBuf> {
    std::env::var_os("MOLT_NATS_SERVER").map(PathBuf::from).or_else(|| {
        std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).map(|d| d.join("nats-server")).find(|p| p.is_file()))
    })
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn nats_topic_subscribers_cannot_publish_or_widen_their_rights() {
    let Some(binary) = nats_binary() else {
        assert!(
            std::env::var_os("MOLT_REQUIRE_NATS_TESTS").is_none(),
            "MOLT_REQUIRE_NATS_TESTS is set, and nats-server was not found: set MOLT_NATS_SERVER"
        );
        eprintln!("nats-server not found; CI provisions it for this test");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let users: Vec<(ServiceId, Secret)> = ["publisher", "listener", "child", "legacy"]
        .iter()
        .map(|name| (sid(name), Secret::from_raw(format!("test-{name}"))))
        .collect();
    let kernel_secret = Secret::from_raw("test-kernel");
    let path = dir.path().join("nats.conf");
    std::fs::write(&path, server_config(DEFAULT_PREFIX, &kernel_secret, &users)).unwrap();
    let _server = Server(
        Command::new(binary)
            .args(["-a", "127.0.0.1", "-p", &port.to_string(), "-c"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let url = format!("nats://127.0.0.1:{port}");
    let mut transport = None;
    for _ in 0..100 {
        if let Ok(t) = NatsTransport::connect(&url, &kernel_secret, DEFAULT_PREFIX).await {
            transport = Some(t);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut config = Config::new(dir.path().join("data"));
    config.fsync = false;
    let kernel = Kernel::start(config, Arc::new(transport.expect("nats-server did not start"))).await.unwrap();
    let mut services = Vec::new();
    for (id, secret) in users {
        kernel.register_with_secret(&id, secret.clone()).await.unwrap();
        let link = NatsLink::connect(&url, &id, &secret).await.unwrap();
        let service = Service::new(id, Box::new(link), HashMap::new());
        service.kernel("ping", None, json!(null)).await.unwrap();
        services.push(service);
    }
    exercise(&kernel, &services[0], &services[1], &services[2], &services[3]).await;
}

//! Runs the conformance suite against a real nats-server.
//!
//! Needs a `nats-server` binary: set `MOLT_NATS_SERVER` to its path or put it
//! on `PATH`. Without one these tests print a notice and pass, so plain
//! `cargo test` works anywhere; CI installs the server and runs them for real.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use molt_proto::{Envelope, ServiceId};
use molt_transport::nats::{server_config, NatsLink, NatsTransport, DEFAULT_PREFIX};
use molt_transport::testkit::{self, Harness};
use molt_transport::{Link, Secret, TransportError};

fn nats_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MOLT_NATS_SERVER") {
        return Some(p.into());
    }
    std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("nats-server")).find(|p| p.is_file()))
}

struct Nats {
    _dir: tempfile::TempDir,
    server: Child,
    url: String,
    t: NatsTransport,
}

impl Drop for Nats {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn secret_for(id: &ServiceId) -> Secret {
    Secret::from_raw(format!("secret-of-{id}"))
}

fn kernel_secret() -> Secret {
    Secret::from_raw("kernel-secret")
}

#[async_trait]
impl Harness for Nats {
    type T = NatsTransport;

    async fn setup() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let services: Vec<_> = ["a", "b"].iter().map(|s| testkit::id(s)).map(|i| (i.clone(), secret_for(&i))).collect();
        let conf = dir.path().join("nats.conf");
        std::fs::write(&conf, server_config(DEFAULT_PREFIX, &kernel_secret(), &services)).unwrap();
        let server = Command::new(nats_bin().unwrap())
            .args(["-a", "127.0.0.1", "-p", &port.to_string(), "-c"])
            .arg(&conf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nats-server");
        let url = format!("nats://127.0.0.1:{port}");
        let mut t = None;
        for _ in 0..100 {
            if let Ok(conn) = NatsTransport::connect(&url, &kernel_secret(), DEFAULT_PREFIX).await {
                t = Some(conn);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { _dir: dir, server, url, t: t.expect("nats-server did not come up") }
    }

    fn transport(&self) -> &NatsTransport {
        &self.t
    }

    fn secret(&self, id: &ServiceId) -> Secret {
        secret_for(id)
    }

    async fn link(&self, id: &ServiceId, secret: &Secret) -> Result<Box<dyn Link>, TransportError> {
        Ok(Box::new(NatsLink::connect(&self.url, id, secret).await?))
    }

    async fn impersonate(&self, attacker: &ServiceId, victim: &ServiceId, msg: &Envelope) {
        let link = NatsLink::connect(&self.url, attacker, &secret_for(attacker)).await.unwrap();
        let body = serde_json::to_vec(msg).unwrap();
        let _ = link.publish_raw(format!("{DEFAULT_PREFIX}.in.{victim}"), body).await;
    }

    async fn eavesdrop(&self, attacker: &ServiceId, victim: &ServiceId) -> Option<Box<dyn Link>> {
        let link = NatsLink::connect(&self.url, attacker, &secret_for(attacker)).await.ok()?;
        let sub = link.client().subscribe(format!("{DEFAULT_PREFIX}.svc.{victim}.inbox")).await.ok()?;
        let _ = link.client().flush().await;
        Some(Box::new(Spy { _link: link, sub: tokio::sync::Mutex::new(sub) }))
    }

    fn reports_busy(&self) -> bool {
        false
    }

    fn reports_not_connected(&self) -> bool {
        false
    }
}

struct Spy {
    _link: NatsLink,
    sub: tokio::sync::Mutex<async_nats::Subscriber>,
}

#[async_trait]
impl Link for Spy {
    async fn send(&self, _msg: &Envelope) -> Result<(), TransportError> {
        Ok(())
    }

    async fn recv(&self) -> Option<Envelope> {
        let m = self.sub.lock().await.next().await?;
        serde_json::from_slice(&m.payload).ok()
    }
}

macro_rules! nats_test {
    ($name:ident, $check:ident) => {
        #[tokio::test]
        async fn $name() {
            if nats_bin().is_none() {
                eprintln!("skipping {}: nats-server not found", stringify!($name));
                return;
            }
            testkit::$check::<Nats>().await;
        }
    };
}

nats_test!(nats_ordered_both_ways, ordered_both_ways);
nats_test!(nats_sender_is_stamped, sender_is_stamped_by_transport);
nats_test!(nats_wrong_secret_rejected, wrong_secret_is_rejected);
nats_test!(nats_cannot_write_into_another_endpoint, cannot_write_into_another_endpoint);
nats_test!(nats_cannot_read_another_inbox, cannot_read_another_inbox);
nats_test!(nats_unconnected_service, unconnected_service);

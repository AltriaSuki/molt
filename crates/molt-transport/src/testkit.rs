//! Conformance suite every [`Transport`] must pass.
//!
//! Implement [`Harness`] for a transport and call [`run_all`]. Each check
//! sets up a fresh harness, so checks cannot leak state into one another.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use molt_proto::{CapId, Envelope, ServiceId, TraceId};

use crate::{Link, Secret, Transport, TransportError};

/// How long a check waits before concluding that nothing will arrive.
pub const QUIET: Duration = Duration::from_millis(400);

#[async_trait]
pub trait Harness: Sized + Send + Sync {
    type T: Transport;

    /// A fresh transport with endpoints `a` and `b` known to it (not yet opened).
    async fn setup() -> Self;

    fn transport(&self) -> &Self::T;

    /// The secret the transport expects for `id`.
    fn secret(&self, id: &ServiceId) -> Secret;

    /// Connect a service link with the given credentials.
    async fn link(&self, id: &ServiceId, secret: &Secret) -> Result<Box<dyn Link>, TransportError>;

    /// Using only `attacker`'s own credentials, try to put `msg` into
    /// `victim`'s endpoint.
    async fn impersonate(&self, attacker: &ServiceId, victim: &ServiceId, msg: &Envelope);

    /// Using only `attacker`'s own credentials, try to read `victim`'s inbox.
    /// `None` if the transport refused outright.
    async fn eavesdrop(&self, attacker: &ServiceId, victim: &ServiceId) -> Option<Box<dyn Link>>;

    /// True when a full inbox makes `send` fail with `Busy` instead of dropping.
    fn reports_busy(&self) -> bool;

    /// True when sending to a service that is not connected fails instead of
    /// being silently dropped.
    fn reports_not_connected(&self) -> bool;
}

pub fn id(s: &str) -> ServiceId {
    ServiceId::new(s).unwrap()
}

pub fn numbered(n: u64) -> Envelope {
    Envelope::request(TraceId::from_raw("trace_test"), "x.y".parse().unwrap(), CapId::from_raw("cap_test"), n.into())
}

async fn open<H: Harness>(h: &H, name: &str) -> (ServiceId, Box<dyn Link>) {
    let sid = id(name);
    h.transport().open(&sid, &h.secret(&sid)).await.expect("open endpoint");
    let link = h.link(&sid, &h.secret(&sid)).await.expect("connect link");
    (sid, link)
}

/// Send to `to` until it succeeds; the link may still be registering.
async fn send_eventually<T: Transport>(t: &T, to: &ServiceId, msg: &Envelope) {
    for _ in 0..100 {
        if t.send(to, msg).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not send to {to}");
}

pub async fn ordered_both_ways<H: Harness>() {
    let h = H::setup().await;
    let mut inbound = h.transport().inbound().expect("inbound stream");
    let (a, link) = open(&h, "a").await;
    const N: u64 = 500;
    for n in 0..N {
        link.send(&numbered(n)).await.unwrap();
    }
    for n in 0..N {
        let m = tokio::time::timeout(Duration::from_secs(5), inbound.next()).await.expect("inbound").unwrap();
        assert_eq!(m.from, a);
        assert_eq!(m.msg.payload, serde_json::json!(n), "service to kernel out of order");
    }
    assert!(tokio::time::timeout(QUIET, inbound.next()).await.is_err(), "duplicate delivery");

    send_eventually(h.transport(), &a, &numbered(0)).await;
    for n in 1..N {
        // A full inbox is reported as Busy; the caller backs off and retries.
        loop {
            match h.transport().send(&a, &numbered(n)).await {
                Ok(()) => break,
                Err(TransportError::Busy(_)) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(e) => panic!("send failed: {e}"),
            }
        }
    }
    for n in 0..N {
        let m = tokio::time::timeout(Duration::from_secs(5), link.recv()).await.expect("recv").unwrap();
        assert_eq!(m.payload, serde_json::json!(n), "kernel to service out of order");
    }
    assert!(tokio::time::timeout(QUIET, link.recv()).await.is_err(), "duplicate delivery");
}

pub async fn sender_is_stamped_by_transport<H: Harness>() {
    let h = H::setup().await;
    let mut inbound = h.transport().inbound().unwrap();
    let (a, link) = open(&h, "a").await;
    let mut forged = numbered(1);
    forged.from = Some(id("b"));
    link.send(&forged).await.unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), inbound.next()).await.unwrap().unwrap();
    assert_eq!(m.from, a, "transport must stamp the authenticated sender");
}

pub async fn wrong_secret_is_rejected<H: Harness>() {
    let h = H::setup().await;
    let mut inbound = h.transport().inbound().unwrap();
    let a = id("a");
    h.transport().open(&a, &h.secret(&a)).await.unwrap();
    if let Ok(link) = h.link(&a, &Secret::from_raw("not-the-secret")).await {
        let _ = link.send(&numbered(1)).await;
    }
    assert!(tokio::time::timeout(QUIET, inbound.next()).await.is_err(), "message with a wrong secret got through");
}

pub async fn cannot_write_into_another_endpoint<H: Harness>() {
    let h = H::setup().await;
    let mut inbound = h.transport().inbound().unwrap();
    let (_a, _link_a) = open(&h, "a").await;
    let (b, _link_b) = open(&h, "b").await;
    h.impersonate(&b, &id("a"), &numbered(7)).await;
    if let Ok(Some(m)) = tokio::time::timeout(QUIET, inbound.next()).await {
        assert_eq!(m.from, b, "a message from b was attributed to a");
    }
}

pub async fn cannot_read_another_inbox<H: Harness>() {
    let h = H::setup().await;
    let (a, link_a) = open(&h, "a").await;
    let (b, _link_b) = open(&h, "b").await;
    let spy = h.eavesdrop(&b, &a).await;
    send_eventually(h.transport(), &a, &numbered(42)).await;
    let got = tokio::time::timeout(Duration::from_secs(5), link_a.recv()).await.unwrap().unwrap();
    assert_eq!(got.payload, serde_json::json!(42));
    if let Some(spy) = spy {
        let leaked = tokio::time::timeout(QUIET, spy.recv()).await;
        assert!(!matches!(leaked, Ok(Some(_))), "b read a message from a's inbox");
    }
}

pub async fn unconnected_service<H: Harness>() {
    let h = H::setup().await;
    let a = id("a");
    let res = h.transport().send(&a, &numbered(1)).await;
    if h.reports_not_connected() {
        assert!(matches!(res, Err(TransportError::NotConnected(_))), "got {res:?}");
    }
}

pub async fn full_inbox_is_bounded<H: Harness>() {
    let h = H::setup().await;
    if !h.reports_busy() {
        return;
    }
    let (a, _link_not_reading) = open(&h, "a").await;
    send_eventually(h.transport(), &a, &numbered(0)).await;
    let mut big = numbered(0);
    big.payload = serde_json::json!("x".repeat(4096));
    for _ in 0..100_000 {
        match h.transport().send(&a, &big).await {
            Ok(()) => tokio::task::yield_now().await,
            Err(TransportError::Busy(_)) => return,
            Err(e) => panic!("unexpected error {e}"),
        }
    }
    panic!("100k sends to a service that never reads were all accepted: the inbox is unbounded");
}

pub async fn run_all<H: Harness>() {
    ordered_both_ways::<H>().await;
    sender_is_stamped_by_transport::<H>().await;
    wrong_secret_is_rejected::<H>().await;
    cannot_write_into_another_endpoint::<H>().await;
    cannot_read_another_inbox::<H>().await;
    unconnected_service::<H>().await;
    full_inbox_is_bounded::<H>().await;
}

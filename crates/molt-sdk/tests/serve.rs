//! `Service::serve_concurrent` over an in-memory link, with the test playing the kernel.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, MsgId, RemoteError, ServiceId, TraceId};
use molt_sdk::{CallOpts, Service, QUEUE};
use molt_transport::{Link, TransportError};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

struct MemLink {
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<Envelope>>,
    sent: mpsc::UnboundedSender<Envelope>,
}

#[async_trait]
impl Link for MemLink {
    async fn send(&self, msg: &Envelope) -> Result<(), TransportError> {
        self.sent.send(msg.clone()).map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<Envelope> {
        self.inbox.lock().await.recv().await
    }
}

/// The kernel's end of the service's link.
struct Bus {
    to: mpsc::UnboundedSender<Envelope>,
    from: mpsc::UnboundedReceiver<Envelope>,
}

impl Bus {
    /// Send the service a request and return its id.
    fn request(&self, payload: Value) -> MsgId {
        self.request_within(payload, 0)
    }

    /// [`Bus::request`] with a deadline `ms` from now (0: none).
    fn request_within(&self, payload: Value, ms: u64) -> MsgId {
        let req = Envelope::request(TraceId::random(), "svc.work".parse().unwrap(), CapId::random(), payload)
            .with_budget(Budget::new(0, ms, 0));
        self.to.send(req.clone()).unwrap();
        req.id
    }

    async fn recv(&mut self) -> Envelope {
        tokio::time::timeout(Duration::from_secs(5), self.from.recv())
            .await
            .expect("the service sent nothing for 5 s")
            .expect("the link closed")
    }
}

fn service() -> (Arc<Service>, Bus) {
    let (to, inbox) = mpsc::unbounded_channel();
    let (sent, from) = mpsc::unbounded_channel();
    let link = MemLink { inbox: tokio::sync::Mutex::new(inbox), sent };
    let svc = Service::new(ServiceId::new("svc").unwrap(), Box::new(link), HashMap::new());
    (Arc::new(svc), Bus { to, from })
}

/// The answer a reply carries.
fn result(reply: &Envelope) -> Result<Value, RemoteError> {
    assert_eq!(reply.kind, Kind::Reply);
    reply.error().map_or_else(|| Ok(reply.payload.clone()), Err)
}

#[tokio::test]
async fn publishing_with_an_existing_trace_preserves_its_identity() {
    let (svc, mut bus) = service();
    let trace = TraceId::random();
    let cap = CapId::random();
    svc.add_cap("topic:progress", cap.clone());
    svc.publish_traced("progress", json!({"text":"preview"}), trace.clone()).await.unwrap();
    let event = bus.recv().await;
    assert_eq!(event.kind, Kind::Event);
    assert_eq!(event.trace_id, trace);
    assert_eq!(event.cap, Some(cap));
    assert_eq!(event.payload, json!({"text":"preview"}));
}

#[tokio::test]
async fn serve_concurrent_runs_at_most_max_in_flight_and_answers_every_request() {
    const MAX: usize = 3;
    const N: usize = 10;
    let (svc, mut bus) = service();
    let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (open, gate) = watch::channel(false);
    tokio::spawn({
        let (svc, running, peak) = (svc.clone(), running.clone(), peak.clone());
        async move {
            svc.serve_concurrent(MAX, move |req| {
                let (running, peak, mut gate) = (running.clone(), peak.clone(), gate.clone());
                async move {
                    peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let _ = gate.wait_for(|open| *open).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(req.payload)
                }
            })
            .await
        }
    });

    let ids: Vec<MsgId> = (0..N).map(|n| bus.request(json!(n))).collect();
    tokio::time::timeout(Duration::from_secs(5), async {
        while running.load(Ordering::SeqCst) < MAX {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the first requests start at once");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(running.load(Ordering::SeqCst), MAX, "the other requests wait for a slot");
    assert!(bus.from.try_recv().is_err(), "nothing is answered while the handlers wait");

    open.send(true).unwrap();
    let mut answers = HashMap::new();
    for _ in 0..N {
        let reply = bus.recv().await;
        let answered = reply.reply_to.clone().expect("a reply");
        assert!(answers.insert(answered, result(&reply)).is_none(), "a request was answered twice");
    }
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(answers[id].as_ref().unwrap(), &json!(n));
    }
    assert_eq!(peak.load(Ordering::SeqCst), MAX);
}

#[tokio::test]
async fn a_full_queue_is_answered_busy_while_in_flight_handlers_still_get_their_replies() {
    const MAX: usize = 4;
    let flood = QUEUE + 100;
    let (svc, mut bus) = service();
    svc.add_cap("store.get", CapId::random());
    // Each handler waits on a call of its own, whose reply comes back over the same link.
    tokio::spawn({
        let (svc, caller) = (svc.clone(), svc.clone());
        async move {
            svc.serve_concurrent(MAX, move |req| {
                let caller = caller.clone();
                async move {
                    let got = caller.call("store.get", req.payload, CallOpts::default()).await;
                    got.map_err(|e| RemoteError { code: ErrorCode::Failed, message: e.to_string() })
                }
            })
            .await
        }
    });

    let first: Vec<MsgId> = (0..MAX).map(|n| bus.request(json!(n))).collect();
    let mut calls = Vec::new();
    for _ in 0..MAX {
        let call = bus.recv().await;
        assert_eq!(call.kind, Kind::Request, "every slot is waiting on its call");
        calls.push(call);
    }
    let rest: HashSet<MsgId> = (MAX..MAX + flood).map(|n| bus.request(json!(n))).collect();
    // These replies reach the service behind the whole flood.
    for call in &calls {
        bus.to.send(call.reply(call.payload.clone())).unwrap();
    }

    let mut answers = HashMap::new();
    while answers.len() < MAX + flood {
        let msg = bus.recv().await;
        if msg.kind == Kind::Request {
            // A queued request got a slot and makes its call.
            bus.to.send(msg.reply(msg.payload.clone())).unwrap();
            continue;
        }
        let answered = msg.reply_to.clone().expect("a reply");
        assert!(answers.insert(answered, result(&msg)).is_none(), "a request was answered twice");
    }
    for (n, id) in first.iter().enumerate() {
        assert_eq!(answers[id].as_ref().unwrap(), &json!(n), "an in-flight handler got its reply");
    }
    let busy: Vec<&MsgId> = answers
        .iter()
        .filter(|(_, r)| r.as_ref().is_err_and(|e| e.code == ErrorCode::Busy))
        .map(|(id, _)| id)
        .collect();
    assert!(!busy.is_empty(), "requests beyond the queue are answered busy");
    assert!(busy.iter().all(|id| rest.contains(id)));
    let served = answers.values().filter(|r| r.is_ok()).count();
    assert_eq!(served + busy.len(), MAX + flood, "every other request is served");
    assert!(served > QUEUE, "the queued requests are served once slots free up");
}

#[tokio::test]
async fn a_handler_that_panics_is_answered_failed() {
    let (svc, mut bus) = service();
    tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.serve_concurrent(2, |req| {
                // Work done before the future starts, such as parsing, can panic too.
                assert_ne!(req.payload, json!("early"), "panicking before the future");
                async move {
                    assert_ne!(req.payload, json!("late"), "panicking inside the future");
                    Ok(req.payload)
                }
            })
            .await
        }
    });

    let early = bus.request(json!("early"));
    let late = bus.request(json!("late"));
    let fine = bus.request(json!("fine"));
    let mut answers = HashMap::new();
    for _ in 0..3 {
        let reply = bus.recv().await;
        answers.insert(reply.reply_to.clone().expect("a reply"), result(&reply));
    }
    for id in [&early, &late] {
        let err = answers[id].as_ref().expect_err("a panic is an error");
        assert_eq!(err.code, ErrorCode::Failed, "{}", err.message);
    }
    assert_eq!(answers[&fine].as_ref().unwrap(), &json!("fine"));
}

#[tokio::test]
async fn time_spent_waiting_for_a_slot_counts_against_the_deadline() {
    const WAIT: Duration = Duration::from_millis(300);
    let (svc, mut bus) = service();
    let (open, gate) = watch::channel(false);
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.serve_concurrent(1, move |req| {
                let (mut gate, seen) = (gate.clone(), seen_tx.clone());
                async move {
                    let _ = seen.send((req.payload.clone(), req.budget.ms));
                    let _ = gate.wait_for(|open| *open).await;
                    Ok(req.payload)
                }
            })
            .await
        }
    });

    let first = bus.request(json!("first"));
    assert_eq!(seen.recv().await, Some((json!("first"), 0)), "no deadline stays no deadline");
    let later = bus.request_within(json!("later"), 10_000);
    let expired = bus.request_within(json!("expired"), 100);
    tokio::time::sleep(WAIT).await;
    open.send(true).unwrap();

    let mut answers = HashMap::new();
    for _ in 0..3 {
        let reply = bus.recv().await;
        answers.insert(reply.reply_to.clone().expect("a reply"), result(&reply));
    }
    assert_eq!(answers[&first].as_ref().unwrap(), &json!("first"));
    assert_eq!(answers[&later].as_ref().unwrap(), &json!("later"));
    let err = answers[&expired].as_ref().expect_err("its deadline passed while it waited");
    assert_eq!(err.code, ErrorCode::Timeout, "{}", err.message);
    let (payload, ms) = seen.recv().await.unwrap();
    assert_eq!(payload, json!("later"));
    assert!(ms > 0 && ms <= 10_000 - WAIT.as_millis() as u64, "the handler saw {ms} ms left");
    assert!(seen.try_recv().is_err(), "a request past its deadline never reaches the handler");
}

#[tokio::test]
async fn cancellation_is_authenticated_and_does_not_stop_other_requests() {
    let (svc, mut bus) = service();
    let task = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.serve_concurrent(2, |req| async move {
                if req.payload == json!("slow") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Ok(req.payload)
            })
            .await;
        }
    });
    let id = bus.request(json!("slow"));
    let mut stop = Envelope::request(TraceId::random(), "svc.work".parse().unwrap(), CapId::random(), Value::Null);
    stop.kind = Kind::Cancel;
    stop.reply_to = Some(id.clone());
    stop.from = Some(ServiceId::new("rogue").unwrap());
    bus.to.send(stop.clone()).unwrap();
    let other = bus.request(json!("other"));
    let reply = bus.recv().await;
    assert_eq!(reply.reply_to, Some(other));
    assert_eq!(result(&reply).unwrap(), json!("other"));
    assert!(bus.from.try_recv().is_err());
    stop.from = Some(ServiceId::kernel());
    bus.to.send(stop).unwrap();
    let reply = bus.recv().await;
    assert_eq!(reply.reply_to, Some(id));
    assert_eq!(result(&reply).unwrap_err().code, ErrorCode::Cancelled);
    drop(bus.to);
    task.await.unwrap();
}

#[tokio::test]
async fn dropping_a_pending_call_sends_cancel_and_completed_calls_do_not() {
    let (svc, mut bus) = service();
    let pending = svc.request("kernel.ping", Value::Null, CallOpts::default()).await.unwrap();
    let request = bus.recv().await;
    assert_eq!(pending.id(), &request.id);
    drop(pending);
    let control = bus.recv().await;
    assert_eq!(control.to.to_string(), "kernel.cancel");
    assert_eq!(control.payload["request"], json!(request.id));
    let pending = svc.request("kernel.ping", Value::Null, CallOpts::default()).await.unwrap();
    let request = bus.recv().await;
    bus.to.send(request.reply(json!(true))).unwrap();
    assert_eq!(pending.wait().await.unwrap(), json!(true));
    tokio::task::yield_now().await;
    assert!(bus.from.try_recv().is_err());
}

#[tokio::test]
async fn dropping_a_service_wakes_and_cancels_its_pending_calls() {
    let (svc, mut bus) = service();
    let pending = svc.request("kernel.ping", Value::Null, CallOpts::default()).await.unwrap();
    let request = bus.recv().await;
    drop(svc);
    let error = tokio::time::timeout(Duration::from_secs(1), pending.wait())
        .await
        .expect("dropping a service must close the call's waiter")
        .unwrap_err();
    assert!(matches!(error, molt_sdk::SdkError::Closed));
    let cancel = bus.recv().await;
    assert_eq!(cancel.to.to_string(), "kernel.cancel");
    assert_eq!(cancel.payload["request"], json!(request.id));
}

#[tokio::test]
async fn link_loss_finishes_cleanup_even_when_every_slot_is_occupied() {
    struct Stopped(mpsc::UnboundedSender<()>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    let (svc, bus) = service();
    let (started, mut starts) = mpsc::unbounded_channel();
    let (stopped, mut stops) = mpsc::unbounded_channel();
    let task = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.serve_cancellable(1, move |_, cancel| {
                let (started, stopped) = (started.clone(), stopped.clone());
                async move {
                    let _stopped = Stopped(stopped);
                    started.send(()).unwrap();
                    cancel.cancelled().await;
                    // A broken cooperative handler can ignore cancellation.
                    std::future::pending::<Result<Value, RemoteError>>().await
                }
            })
            .await;
        }
    });
    bus.request(Value::Null);
    tokio::time::timeout(Duration::from_secs(1), starts.recv()).await.unwrap().unwrap();
    bus.request(json!("queued"));
    drop(bus.to);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("link loss must reach the bounded cooperative cleanup even while waiting for a slot")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), stops.recv()).await.unwrap().unwrap();
    assert!(starts.try_recv().is_err(), "queued requests must not start after link loss");
}

#[tokio::test]
async fn sequential_serving_does_not_invoke_a_cancelled_queued_handler() {
    let (svc, mut bus) = service();
    let (open, gate) = watch::channel(false);
    let (started, mut starts) = mpsc::unbounded_channel();
    let task = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.serve(move |req| {
                started.send(req.payload.clone()).unwrap();
                let mut gate = gate.clone();
                async move {
                    let _ = gate.wait_for(|open| *open).await;
                    Ok(req.payload)
                }
            })
            .await;
        }
    });
    let first = bus.request(json!("first"));
    assert_eq!(starts.recv().await, Some(json!("first")));
    let queued = bus.request(json!("cancelled"));
    let mut stop = Envelope::request(TraceId::random(), "svc.work".parse().unwrap(), CapId::random(), Value::Null);
    stop.kind = Kind::Cancel;
    stop.reply_to = Some(queued.clone());
    stop.from = Some(ServiceId::kernel());
    bus.to.send(stop).unwrap();
    // The reply is a FIFO barrier: read_loop has processed the cancellation.
    let barrier = svc.request("kernel.ping", Value::Null, CallOpts::default()).await.unwrap();
    let ping = bus.recv().await;
    bus.to.send(ping.reply(Value::Null)).unwrap();
    barrier.wait().await.unwrap();
    open.send(true).unwrap();
    let reply = bus.recv().await;
    assert_eq!(reply.reply_to, Some(first));
    assert!(result(&reply).is_ok());
    let reply = bus.recv().await;
    assert_eq!(reply.reply_to, Some(queued));
    assert_eq!(result(&reply).unwrap_err().code, ErrorCode::Cancelled);
    assert!(starts.try_recv().is_err(), "even synchronous handler code must not run");
    drop(bus.to);
    task.await.unwrap();
}

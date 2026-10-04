//! The four kernel invariants, tested through the bus the way a hostile
//! service would try to break them.

mod common;

use std::time::Duration;

use common::*;
use molt_kernel::audit::{self, AuditEvent};
use molt_kernel::registry::Authority;
use molt_kernel::supervisor::{Limits, RestartPolicy};
use molt_kernel::ServiceStatus;
use molt_proto::audit::ReadResponse;
use molt_proto::{Budget, CapId, CapRequest, Envelope, ErrorCode, Exec, Manifest, Tier, TraceId};
use molt_sdk::{CallOpts, SdkError};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Invariant 1: no service can grant itself a capability or raise its budget.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_service_cannot_use_a_capability_it_does_not_hold() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let rogue = w.join("rogue").await;
    let _memory = echo(w.join("memory").await);
    let cap = w.grant(&planner, "memory.recall", Budget::new(100, 1000, 10)).await;

    assert_eq!(planner.call("memory.recall", json!(1), with_cap(&cap)).await.unwrap(), json!(1));

    // No capability at all.
    let bare =
        Envelope { cap: None, ..Envelope::request(TraceId::random(), target("memory.recall"), cap.clone(), json!(2)) };
    assert_eq!(rogue.send_request(bare).await.unwrap().error().unwrap().code, ErrorCode::Denied);
    // A stolen capability id: the kernel checks the holder, not just the id.
    assert_eq!(code(rogue.call("memory.recall", json!(3), with_cap(&cap)).await), ErrorCode::Denied);
    // A made-up capability id.
    assert_eq!(code(rogue.call("memory.recall", json!(4), with_cap(&CapId::random())).await), ErrorCode::Denied);
    // Claiming to be the planner: the transport stamps the real sender.
    let mut spoofed = Envelope::request(TraceId::random(), target("memory.recall"), cap.clone(), json!(5));
    spoofed.from = Some(sid("planner"));
    assert_eq!(rogue.send_request(spoofed).await.unwrap().error().unwrap().code, ErrorCode::Denied);
    // A capability for one method does not cover another.
    assert_eq!(code(planner.call("memory.forget", json!(6), with_cap(&cap)).await), ErrorCode::Denied);
    // There is no way to mint a capability over the bus.
    let mint = rogue.kernel("cap.grant", None, json!({ "target": "memory.*" })).await;
    assert_eq!(code(mint), ErrorCode::Invalid);
}

#[tokio::test]
async fn delegation_can_split_a_budget_but_never_widen_it() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let worker = w.join("worker").await;
    let _memory = echo(w.join("memory").await);
    let cap = w.grant(&planner, "memory.recall", Budget::new(100, 1000, 10)).await;

    let widen = |target: &'static str, budget: Budget| {
        let planner = &planner;
        let cap = cap.clone();
        let to = worker.id().clone();
        async move { planner.delegate(&cap, &to, target, budget, None).await }
    };
    let denied = |r: Result<CapId, SdkError>| match r {
        Err(SdkError::Remote(e)) => assert_eq!(e.code, ErrorCode::Denied, "{e}"),
        other => panic!("delegation should have been refused: {other:?}"),
    };
    denied(widen("memory.*", Budget::new(1, 100, 1)).await); // wider target
    denied(widen("memory.recall", Budget::new(1000, 100, 1)).await); // more tokens than the parent has
    denied(widen("memory.recall", Budget::new(1, 100, 50)).await); // more calls than the parent has
    denied(widen("memory.recall", Budget::new(1, 5000, 1)).await); // longer deadline
                                                                   // The worker cannot delegate a capability it does not hold.
    denied(worker.delegate(&cap, worker.id(), "memory.recall", Budget::new(1, 100, 1), None).await);

    let child = planner.delegate(&cap, worker.id(), "memory.recall", Budget::new(10, 500, 2), None).await.unwrap();
    // The child's budget came out of the parent's.
    assert_eq!(w.kernel.caps().get(&cap).unwrap().remaining(), Budget::new(90, 1000, 8));
    assert!(worker.call("memory.recall", json!(1), with_cap(&child)).await.is_ok());
    assert!(worker.call("memory.recall", json!(2), with_cap(&child)).await.is_ok());
    assert_eq!(code(worker.call("memory.recall", json!(3), with_cap(&child)).await), ErrorCode::OverBudget);
    // Revoking the parent cuts off everything delegated from it.
    let child2 = planner.delegate(&cap, worker.id(), "memory.recall", Budget::new(1, 500, 1), None).await.unwrap();
    w.kernel.revoke(&cap).await.unwrap();
    assert_eq!(code(worker.call("memory.recall", json!(4), with_cap(&child2)).await), ErrorCode::Denied);
}

#[tokio::test]
async fn budgets_are_enforced_per_message() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let _memory = echo(w.join("memory").await);
    let cap = w.grant(&planner, "memory.recall", Budget::new(50, 1000, 100)).await;
    let opts = |tokens| CallOpts { cap: Some(cap.clone()), budget: Budget::new(tokens, 0, 0), trace: None };
    assert!(planner.call("memory.recall", json!(1), opts(40)).await.is_ok());
    assert_eq!(code(planner.call("memory.recall", json!(2), opts(20)).await), ErrorCode::OverBudget);
    assert!(planner.call("memory.recall", json!(3), opts(10)).await.is_ok());
    let long = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 60_000, 0), trace: None };
    assert_eq!(code(planner.call("memory.recall", json!(4), long).await), ErrorCode::Denied);
}

// ---------------------------------------------------------------------------
// Invariant 2: no service can promote a kernel or protected component.
// ---------------------------------------------------------------------------

fn manifest(name: &str, tier: Tier, requests: &[(&str, u64)]) -> Manifest {
    Manifest {
        name: sid(name),
        tier,
        parent: None,
        provides: vec![],
        requests: requests
            .iter()
            .map(|(t, n)| CapRequest { target: target(t), budget: Budget::new(*n, 0, 0) })
            .collect(),
        exec: None,
    }
}

#[tokio::test]
async fn only_humans_promote_protected_and_kernel_components() {
    let w = World::new().await;
    let gate = w.join("gate").await;
    let rogue = w.join("rogue").await;
    let propose = w.grant(&gate, "kernel.registry.propose", Budget::new(0, 0, 100)).await;
    let promote = w.grant(&gate, "kernel.registry.promote", Budget::new(0, 0, 100)).await;

    let eval_v1 = w.kernel.install(manifest("evaluator", Tier::Protected, &[])).await.unwrap();
    let bus_v1 = w.kernel.install(manifest("bus", Tier::Kernel, &[])).await.unwrap();
    let plan_v1 = w.kernel.install(manifest("planner", Tier::Mutable, &[("memory.recall", 100)])).await.unwrap();

    let store = |m: Manifest| {
        let gate = &gate;
        let cap = propose.clone();
        async move {
            let out = gate.kernel("registry.propose", Some(cap), json!({ "manifest": m })).await.unwrap();
            out["version"].as_str().unwrap().to_owned()
        }
    };
    let try_promote = |service: &'static str, expected: &molt_proto::VersionId, version: String| {
        let gate = &gate;
        let cap = promote.clone();
        let expected = expected.clone();
        async move {
            let args = json!({ "service": service, "expected": expected, "version": version });
            gate.kernel("registry.promote", Some(cap), args).await
        }
    };

    let mut eval_v2 = manifest("evaluator", Tier::Protected, &[]);
    eval_v2.parent = Some(eval_v1.clone());
    let v = store(eval_v2).await;
    assert_eq!(code(try_promote("evaluator", &eval_v1, v).await), ErrorCode::Denied, "protected");

    // Relabelling the evaluator as mutable does not help.
    let v = store(manifest("evaluator", Tier::Mutable, &[])).await;
    assert_eq!(code(try_promote("evaluator", &eval_v1, v).await), ErrorCode::Denied, "tier change");

    let v = store(manifest("bus", Tier::Kernel, &[("tools.*", 1)])).await;
    assert_eq!(code(try_promote("bus", &bus_v1, v).await), ErrorCode::Denied, "kernel tier");

    // A mutable service asking for a capability it never had needs a human.
    let v = store(manifest("planner", Tier::Mutable, &[("memory.recall", 100), ("tools.shell", 1)])).await;
    assert_eq!(code(try_promote("planner", &plan_v1, v).await), ErrorCode::Denied, "new capability");
    let v = store(manifest("planner", Tier::Mutable, &[("memory.recall", 500)])).await;
    assert_eq!(code(try_promote("planner", &plan_v1, v).await), ErrorCode::Denied, "bigger budget");

    // A narrower mutable version goes through, and a stale promotion loses.
    let v2 = store(manifest("planner", Tier::Mutable, &[("memory.recall", 50)])).await;
    try_promote("planner", &plan_v1, v2.clone()).await.unwrap();
    let v3 = store(manifest("planner", Tier::Mutable, &[("memory.recall", 10)])).await;
    assert_eq!(code(try_promote("planner", &plan_v1, v3).await), ErrorCode::Denied, "stale expected version");
    assert_eq!(w.kernel.registry().live(&sid("planner")).unwrap().0, v2);

    // No capability, no promotion.
    let args = json!({ "service": "planner", "expected": v2, "version": plan_v1 });
    assert_eq!(code(rogue.kernel("registry.promote", Some(promote.clone()), args).await), ErrorCode::Denied);

    // The evaluator is still on v1; a human can move it.
    assert_eq!(w.kernel.registry().live(&sid("evaluator")), Some(eval_v1.clone()));
    let mut eval_v2 = manifest("evaluator", Tier::Protected, &[]);
    eval_v2.parent = Some(eval_v1.clone());
    let human_v2 = w.kernel.install(eval_v2).await.unwrap();
    assert_eq!(w.kernel.registry().live(&sid("evaluator")), Some(human_v2));
    let _ = Authority::Human;
}

// ---------------------------------------------------------------------------
// Invariant 3: no service can delete or rewrite audit log entries.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_message_is_logged_before_it_is_delivered() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let memory = w.join("memory").await;
    let cap = w.grant(&planner, "memory.recall", Budget::new(0, 0, 10)).await;
    let call = tokio::spawn(async move { planner.call("memory.recall", json!("hello"), with_cap(&cap)).await });
    let req = memory.next().await.unwrap();
    let entries = audit::read_all(w.kernel.audit_path()).await.unwrap();
    let logged = entries.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope } if envelope.id == req.id));
    assert!(logged, "the request reached the service before it was on the record");
    memory.reply(&req, json!("world")).await.unwrap();
    assert_eq!(call.await.unwrap().unwrap(), json!("world"));
    assert!(audit::verify(w.kernel.audit_path()).await.unwrap() > 0);
}

#[tokio::test]
async fn the_audit_log_has_no_write_path_and_detects_tampering() {
    let w = World::new().await;
    let rogue = w.join("rogue").await;
    for method in ["audit.truncate", "audit.write", "audit.delete"] {
        assert_eq!(code(rogue.kernel(method, None, json!({})).await), ErrorCode::Invalid, "{method}");
    }
    // Let the log settle, then tamper with copies of it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let original = std::fs::read_to_string(w.kernel.audit_path()).unwrap();
    let lines: Vec<&str> = original.lines().collect();
    assert!(lines.len() >= 4);
    let dir = w.dir.path().to_path_buf();
    let check = |text: String| {
        let path = dir.join(format!("tampered-{}.jsonl", rand_suffix()));
        std::fs::write(&path, text).unwrap();
        async move { audit::verify(&path).await }
    };
    // An edited entry.
    let mut edited: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let mut entry: Value = serde_json::from_str(&edited[1]).unwrap();
    entry["ts_ms"] = json!(entry["ts_ms"].as_u64().unwrap() + 1);
    edited[1] = entry.to_string();
    assert!(check(edited.join("\n")).await.is_err(), "an edited entry went unnoticed");
    // A deleted entry.
    let mut deleted: Vec<&str> = lines.clone();
    deleted.remove(1);
    assert!(check(deleted.join("\n")).await.is_err(), "a deleted entry went unnoticed");
    // Swapped entries.
    let mut swapped: Vec<&str> = lines.clone();
    swapped.swap(1, 2);
    assert!(check(swapped.join("\n")).await.is_err(), "reordered entries went unnoticed");
}

#[tokio::test]
async fn reading_the_log_takes_a_capability_and_is_recorded_without_a_second_copy() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let memory = w.join("memory").await;
    let rogue = w.join("rogue").await;
    let _fs = echo(w.join("fs").await);
    let cap = w.grant(&planner, "fs.read", Budget::new(0, 0, 100)).await;
    let trace = TraceId::random();
    let opts = CallOpts { cap: Some(cap), budget: Budget::default(), trace: Some(trace.clone()) };
    planner.call("fs.read", json!({ "path": "a.rs" }), opts.clone()).await.unwrap();
    planner.call("fs.read", json!({ "path": "b.rs" }), opts).await.unwrap();

    let args = json!({ "trace": trace });
    assert_eq!(code(rogue.kernel("audit.read", None, args.clone()).await), ErrorCode::Denied);
    let read = w.grant(&memory, "kernel.audit.read", Budget::new(0, 0, 3)).await;
    assert_eq!(code(rogue.kernel("audit.read", Some(read.clone()), args.clone()).await), ErrorCode::Denied, "stolen");

    let page: ReadResponse =
        serde_json::from_value(memory.kernel("audit.read", Some(read.clone()), args.clone()).await.unwrap()).unwrap();
    let payloads: Vec<Value> = page.entries.iter().map(|e| e.envelope.payload.clone()).collect();
    assert_eq!(
        payloads,
        [json!({ "path": "a.rs" }), json!({ "path": "a.rs" }), json!({ "path": "b.rs" }), json!({ "path": "b.rs" })]
    );
    assert_eq!(page.next, None);

    // The read is on the record as the entries it returned, not as a copy of them.
    let entries = audit::read_all(w.kernel.audit_path()).await.unwrap();
    let copies = entries
        .iter()
        .filter(
            |e| matches!(&e.event, AuditEvent::Message { envelope } if envelope.payload == json!({ "path": "a.rs" })),
        )
        .count();
    assert_eq!(copies, 2, "the request and its reply, and no more");
    let recorded = entries.iter().any(|e| {
        matches!(&e.event, AuditEvent::AuditRead { by, entries: 4, first_seq: Some(_), .. } if by.as_str() == "memory")
    });
    assert!(recorded, "the read is not on the record");

    let bad = json!({ "trace": trace, "cursor": 1 });
    assert_eq!(code(memory.kernel("audit.read", Some(read.clone()), bad).await), ErrorCode::Invalid);
    // Every read counts against the capability's calls.
    assert!(memory.kernel("audit.read", Some(read.clone()), args.clone()).await.is_ok());
    assert_eq!(code(memory.kernel("audit.read", Some(read), args.clone()).await), ErrorCode::OverBudget);

    // A read keeps to the capability's deadline, like any call.
    let short = w.grant(&memory, "kernel.audit.read", Budget::new(0, 1_000, 3)).await;
    let long = CallOpts { cap: Some(short.clone()), budget: Budget::new(0, 5_000, 0), trace: None };
    assert_eq!(code(memory.call("kernel.audit.read", args.clone(), long).await), ErrorCode::Denied);
    let within = CallOpts { cap: Some(short), budget: Budget::new(0, 1_000, 0), trace: None };
    assert!(memory.call("kernel.audit.read", args, within).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_leaves_the_services_last_exits_on_the_record() {
    let w = World::new().await;
    let names = ["sleeper-1", "sleeper-2"];
    for name in names {
        let mut m = manifest(name, Tier::Mutable, &[]);
        m.exec = Some(Exec { command: "/bin/sh".into(), args: vec!["-c".into(), "exec sleep 30".into()] });
        w.kernel.install(m).await.unwrap();
        w.kernel.launch(&sid(name), None, Limits::default(), RestartPolicy::default()).await.unwrap();
    }
    let path = w.kernel.audit_path().to_owned();
    let World { dir: _dir, kernel, .. } = w;
    tokio::time::timeout(Duration::from_secs(20), kernel.shutdown()).await.expect("shutdown within the drain deadline");
    // Read at once, as a process exiting after shutdown would leave the file.
    let text = std::fs::read_to_string(&path).unwrap();
    for name in names {
        let stopped = text.lines().any(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            entry["event"] == json!({ "type": "service_exited", "service": name, "status": "stopped" })
        });
        assert!(stopped, "the exit of {name} is not on the record");
    }
    assert_eq!(audit::verify(&path).await.unwrap(), text.lines().count() as u64);
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Invariant 4: a failing service never takes the kernel down.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_service_that_never_replies_times_out_without_blocking_others() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let _silent = w.join("silent").await; // connected, never reads
    let _memory = echo(w.join("memory").await);
    let silent_cap = w.grant(&planner, "silent.wait", Budget::new(0, 300, 100)).await;
    let mem_cap = w.grant(&planner, "memory.recall", Budget::new(0, 0, 100)).await;
    let opts = CallOpts { cap: Some(silent_cap), budget: Budget::new(0, 300, 0), trace: None };
    let started = std::time::Instant::now();
    let slow = planner.call("silent.wait", json!(1), opts);
    let fast = planner.call("memory.recall", json!(2), with_cap(&mem_cap));
    let (slow, fast) = tokio::join!(slow, fast);
    assert_eq!(fast.unwrap(), json!(2));
    assert_eq!(code(slow), ErrorCode::Timeout);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(w.kernel.pending_count(), 0);
}

#[tokio::test]
async fn a_crashed_and_reconnected_service_works_again() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let memory = w.join("memory").await;
    let cap = w.grant(&planner, "memory.recall", Budget::new(0, 500, 100)).await;
    let opts = || CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, 500, 0), trace: None };
    let call = planner.call("memory.recall", json!(1), opts());
    let crash = async {
        let _req = memory.next().await.unwrap();
        drop(memory); // dies mid-request
    };
    let (result, ()) = tokio::join!(call, crash);
    assert!(matches!(code(result), ErrorCode::Timeout | ErrorCode::Unavailable));
    planner.kernel("ping", None, Value::Null).await.unwrap();
    let _memory = echo(w.join("memory").await);
    assert_eq!(planner.call("memory.recall", json!(2), opts()).await.unwrap(), json!(2));
}

#[tokio::test]
async fn a_service_the_supervisor_gave_up_on_is_reported_with_its_last_exit() {
    let w = World::new().await;
    let broken = sid("broken");
    let mut m = manifest("broken", Tier::Mutable, &[]);
    m.exec = Some(Exec { command: w.dir.path().join("missing").display().to_string(), args: vec![] });
    w.kernel.install(m).await.unwrap();
    let restart = RestartPolicy { max_restarts: 1, backoff_initial: Duration::from_millis(10), ..Default::default() };
    w.kernel.launch(&broken, None, Limits::default(), restart.clone()).await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = w.kernel.service_status(&broken);
            if status.gave_up.is_some() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the supervisor gives up");
    assert_eq!(status.gave_up, Some(1));
    let last = status.last_exit.unwrap_or_default();
    assert!(last.starts_with("spawn failed") && last.contains("No such file"), "{last}");

    // A new launch starts with a clean slate.
    let mut m = manifest("broken", Tier::Mutable, &[]);
    m.exec = Some(Exec { command: "/bin/sh".into(), args: vec!["-c".into(), "exec sleep 30".into()] });
    w.kernel.install(m).await.unwrap();
    w.kernel.launch(&broken, None, Limits::default(), restart).await.unwrap();
    assert_eq!(w.kernel.service_status(&broken), ServiceStatus::default());
    let World { dir: _dir, kernel, .. } = w;
    kernel.shutdown().await;
}

#[tokio::test]
async fn a_flooding_service_gets_busy_and_others_keep_working() {
    let w = World::new().await;
    let flooder = w.join("flooder").await;
    let planner = w.join("planner").await;
    let _sink = w.join("sink").await; // never reads
    let _memory = echo(w.join("memory").await);
    let sink_cap = w.grant(&flooder, "sink.take", Budget::new(0, 0, u64::MAX)).await;
    let mem_cap = w.grant(&planner, "memory.recall", Budget::new(0, 0, 10)).await;
    let link_flood = async {
        let mut busy = 0;
        let opts = CallOpts { cap: Some(sink_cap.clone()), budget: Budget::new(0, 500, 0), trace: None };
        let calls: Vec<_> = (0..2000).map(|n| flooder.call("sink.take", json!(n), opts.clone())).collect();
        for r in futures::future::join_all(calls).await {
            if let Err(SdkError::Remote(e)) = r {
                if e.code == ErrorCode::Busy {
                    busy += 1;
                }
            }
        }
        busy
    };
    let busy = tokio::time::timeout(Duration::from_secs(20), async {
        let (busy, ok) = tokio::join!(link_flood, async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            planner.call("memory.recall", json!("still here"), with_cap(&mem_cap)).await
        });
        assert_eq!(ok.unwrap(), json!("still here"));
        busy
    })
    .await
    .unwrap();
    assert!(busy > 0, "a full mailbox must push back with Busy");
}

#[tokio::test]
async fn garbage_on_the_wire_is_ignored() {
    use futures::SinkExt;
    let w = World::new().await;
    let id = sid("vandal");
    let secret = w.kernel.register(&id).await.unwrap();
    let stream = tokio::net::UnixStream::connect(w.sock.join("vandal.sock")).await.unwrap();
    let mut frames = tokio_util_codec(stream);
    frames.send(serde_json::to_vec(&json!({ "secret": secret.expose() })).unwrap().into()).await.unwrap();
    frames.send(b"this is not an envelope".to_vec().into()).await.unwrap();
    frames.send(vec![0xff; 1024].into()).await.unwrap();
    let planner = w.join("planner").await;
    planner.kernel("ping", None, Value::Null).await.unwrap();
}

fn tokio_util_codec(
    stream: tokio::net::UnixStream,
) -> tokio_util::codec::Framed<tokio::net::UnixStream, tokio_util::codec::LengthDelimitedCodec> {
    tokio_util::codec::Framed::new(stream, tokio_util::codec::LengthDelimitedCodec::new())
}

// ---------------------------------------------------------------------------
// Bus behavior.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_between_two_services_keep_their_order() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let memory = w.join("memory").await;
    let cap = w.grant(&planner, "memory.recall", Budget::new(0, 0, 1000)).await;
    const N: u64 = 200;
    let check = async {
        for n in 0..N {
            let req = memory.next().await.unwrap();
            assert_eq!(req.payload, json!(n), "out of order");
            memory.reply(&req, req.payload.clone()).await.unwrap();
        }
    };
    let send = async {
        let mut replies = Vec::new();
        for n in 0..N {
            replies.push(planner.request("memory.recall", json!(n), with_cap(&cap)).await.unwrap());
        }
        replies
    };
    let (replies, ()) = tokio::join!(send, check);
    for (n, r) in replies.into_iter().enumerate() {
        assert_eq!(r.wait().await.unwrap(), json!(n));
    }
}

#[tokio::test]
async fn events_reach_subscribers_only_with_capabilities() {
    let w = World::new().await;
    let publisher = w.join("publisher").await;
    let listener = w.join("listener").await;
    let outsider = w.join("outsider").await;
    w.grant(&publisher, "topic:builds", Budget::new(0, 0, 10)).await;
    w.grant(&listener, "topic:builds", Budget::new(0, 0, 10)).await;
    listener.subscribe("builds").await.unwrap();
    assert!(matches!(outsider.subscribe("builds").await, Err(SdkError::NoCapability(_))));
    let forged = outsider.kernel("subscribe", Some(CapId::random()), json!({ "topic": "builds" })).await;
    assert_eq!(code(forged), ErrorCode::Denied);
    publisher.publish("builds", json!({ "status": "green" })).await.unwrap();
    let ev = tokio::time::timeout(Duration::from_secs(5), listener.next()).await.unwrap().unwrap();
    assert_eq!(ev.payload["status"], "green");
    assert_eq!(ev.from, Some(sid("publisher")));
    assert!(tokio::time::timeout(Duration::from_millis(300), outsider.next()).await.is_err());
}

#[tokio::test]
async fn a_reply_only_comes_from_the_callee() {
    let w = World::new().await;
    let planner = w.join("planner").await;
    let memory = w.join("memory").await;
    let rogue = w.join("rogue").await;
    let cap = w.grant(&planner, "memory.recall", Budget::new(0, 0, 10)).await;
    let call = tokio::spawn(async move { planner.call("memory.recall", json!("q"), with_cap(&cap)).await });
    let req = memory.next().await.unwrap();
    // The rogue learns the request id and races to answer it.
    rogue.reply(&req, json!("forged answer")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    memory.reply(&req, json!("real answer")).await.unwrap();
    assert_eq!(call.await.unwrap().unwrap(), json!("real answer"));
}

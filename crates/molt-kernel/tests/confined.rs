//! The kernel's rules for confined services (`docs/services.md`): only a
//! service that runs confined may be promoted by a gate, never while its
//! backend is unavailable, and it cannot hold a right to approve versions.
//! The sandbox itself is tested with a real candidate in
//! `crates/molt/tests/candidate.rs`.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use molt_kernel::audit::{self, AuditEvent};
use molt_kernel::registry::Authority;
use molt_kernel::supervisor::{Backend, RestartPolicy};
use molt_kernel::KernelError;
use molt_proto::{Budget, CapRequest, ErrorCode, Exec, Manifest, Tier, VersionId};
use serde_json::json;

fn candidate(requests: &[&str]) -> Manifest {
    Manifest {
        name: sid("candidate"),
        tier: Tier::Mutable,
        parent: None,
        provides: vec![target("candidate.*")],
        requests: requests.iter().map(|t| CapRequest { target: target(t), budget: Budget::new(0, 0, 10) }).collect(),
        exec: None,
        tools: Vec::new(),
    }
}

/// Promote `version` of the candidate as `gate` over the bus.
async fn promote(gate: &molt_sdk::Service, cap: &molt_proto::CapId, expected: &VersionId, version: &str) -> ErrorCode {
    let args = json!({ "service": "candidate", "expected": expected, "version": version });
    match gate.kernel("registry.promote", Some(cap.clone()), args).await {
        Ok(_) => ErrorCode::Failed,
        Err(molt_sdk::SdkError::Remote(e)) => e.code,
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
async fn a_gate_promotes_only_a_confined_service_and_never_while_its_backend_is_unavailable() {
    let w = World::new().await;
    let gate = w.join("gate").await;
    let propose = w.grant(&gate, "kernel.registry.propose", Budget::new(0, 0, 100)).await;
    let promote_cap = w.grant(&gate, "kernel.registry.promote", Budget::new(0, 0, 100)).await;
    let v1 = w.kernel.install(candidate(&["memory.recall"])).await.unwrap();
    let mut next = candidate(&["memory.recall"]);
    next.parent = Some(v1.clone());
    next.provides.push(target("candidate.more"));
    let out = gate.kernel("registry.propose", Some(propose), json!({ "manifest": next })).await.unwrap();
    let v2 = out["version"].as_str().unwrap().to_owned();

    // Installed but run as a host process: a human's to change, not a gate's.
    w.kernel
        .launch_with_env(&sid("candidate"), None, Default::default(), RestartPolicy::default(), vec![])
        .await
        .unwrap();
    assert_eq!(promote(&gate, &promote_cap, &v1, &v2).await, ErrorCode::Denied, "native");

    // Confined, but the backend has gone: refused, not run some other way.
    let backend = Arc::new(FakeBackend::default());
    w.confine("candidate", &backend).await;
    backend.unavailable.store(true, Ordering::SeqCst);
    assert_eq!(promote(&gate, &promote_cap, &v1, &v2).await, ErrorCode::Denied, "backend unavailable");
    let by_gate = Authority::Gate(sid("gate"));
    let err = w.kernel.promote(&sid("candidate"), Some(&v1), &VersionId(v2.clone()), &by_gate).await.unwrap_err();
    assert!(err.to_string().contains("backend is unavailable"), "{err}");
    assert_eq!(w.kernel.registry().live(&sid("candidate")), Some(v1.clone()));

    // With the backend back, the registry's rules decide, and they let it through.
    backend.unavailable.store(false, Ordering::SeqCst);
    let args = json!({ "service": "candidate", "expected": v1, "version": v2 });
    gate.kernel("registry.promote", Some(promote_cap.clone()), args).await.unwrap();
    assert_eq!(w.kernel.registry().live(&sid("candidate")).unwrap().0, v2);

    // Relaunched natively, it is no longer a gate's to promote.
    w.kernel
        .launch_with_env(&sid("candidate"), None, Default::default(), RestartPolicy::default(), vec![])
        .await
        .unwrap();
    let args = json!({ "service": "candidate", "expected": v2, "version": v1 });
    let back = gate.kernel("registry.promote", Some(promote_cap), args).await;
    assert_eq!(code(back), ErrorCode::Denied);
}

#[tokio::test]
async fn a_confined_service_is_not_started_when_its_backend_is_unavailable() {
    let w = World::new().await;
    let mut manifest = candidate(&[]);
    manifest.exec = Some(Exec { command: "/bin/sh".into(), args: vec!["-c".into(), "exec sleep 30".into()] });
    w.kernel.install(manifest).await.unwrap();
    let backend = Arc::new(FakeBackend::default());
    backend.unavailable.store(true, Ordering::SeqCst);
    let dyn_backend: Arc<dyn Backend> = backend.clone();
    let err = w
        .kernel
        .launch_confined(&sid("candidate"), None, Default::default(), RestartPolicy::default(), vec![], dyn_backend)
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::Confinement(_)), "{err}");
    assert!(err.to_string().contains("the fake backend is unavailable"), "{err}");
    let path = w.kernel.audit_path().to_owned();
    let World { dir: _dir, kernel, .. } = w;
    kernel.shutdown().await;
    let entries = audit::read_all(&path).await.unwrap();
    assert!(
        !entries.iter().any(|e| matches!(&e.event, AuditEvent::ServiceStarted { .. })),
        "nothing ran, natively or otherwise"
    );
}

#[tokio::test]
async fn a_confined_service_cannot_hold_a_right_to_approve_versions() {
    let w = World::new().await;
    let backend = Arc::new(FakeBackend::default());
    let dyn_backend: Arc<dyn Backend> = backend.clone();
    // Asking for it in its manifest stops the launch.
    for wanted in ["kernel.registry.promote", "kernel.registry.propose", "kernel.*"] {
        w.kernel.install(candidate(&[wanted])).await.unwrap();
        let err = w
            .kernel
            .launch_confined(
                &sid("candidate"),
                None,
                Default::default(),
                RestartPolicy::default(),
                vec![],
                dyn_backend.clone(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot hold a right to approve versions"), "{wanted}: {err}");
    }
    w.kernel.install(candidate(&["kernel.audit.read"])).await.unwrap();
    w.confine("candidate", &backend).await;
    // Nor can a human grant it, or a gate hand on its own.
    let err = w
        .kernel
        .grant(&sid("candidate"), target("kernel.registry.promote"), Budget::new(0, 0, 1), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("could approve its own versions"), "{err}");
    let gate = w.join("gate").await;
    let promote = w.grant(&gate, "kernel.registry.promote", Budget::new(0, 0, 10)).await;
    let handed =
        gate.delegate(&promote, &sid("candidate"), "kernel.registry.promote", Budget::new(0, 0, 1), None).await;
    match handed {
        Err(molt_sdk::SdkError::Remote(e)) => assert_eq!(e.code, ErrorCode::Denied, "{e}"),
        other => panic!("delegation should have been refused: {other:?}"),
    }
    // A gate cannot promote itself either.
    let mut gate_manifest = candidate(&[]);
    gate_manifest.name = sid("gate");
    let gate_v1 = w.kernel.install(gate_manifest.clone()).await.unwrap();
    w.confine("gate", &backend).await;
    gate_manifest.provides.clear();
    let gate_v2 = w.kernel.registry().store(gate_manifest).unwrap();
    let by_itself = Authority::Gate(sid("gate"));
    let err = w.kernel.promote(&sid("gate"), Some(&gate_v1), &gate_v2, &by_itself).await.unwrap_err();
    assert!(err.to_string().contains("cannot promote itself"), "{err}");
}

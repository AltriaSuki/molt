//! Measure each reader in a fresh process: fixture generation and other tests
//! must not contribute to its peak RSS. Linux CI runs this with the normal suite.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

use molt_kernel::audit::{self, AuditEvent, AuditLog};
use molt_proto::ServiceId;

#[tokio::test(flavor = "current_thread")]
async fn audit_memory_probe() {
    let Some(path) = std::env::var_os("MOLT_AUDIT_RSS_PATH") else { return };
    let path = Path::new(&path);
    match std::env::var("MOLT_AUDIT_RSS_OPERATION").unwrap().as_str() {
        "verify" => assert!(audit::verify(path).await.unwrap() > 0),
        "open" => AuditLog::open(path, false).await.unwrap().close().await,
        "tail" => assert_eq!(audit::tail(path, 3).await.unwrap().len(), 3),
        operation => panic!("unknown operation: {operation}"),
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = status.lines().find_map(|line| line.strip_prefix("VmHWM:")).unwrap();
    let kib: u64 = peak.split_whitespace().next().unwrap().parse().unwrap();
    println!("AUDIT_RSS_KIB={kib}");
}

fn peak_rss(path: &Path, operation: &str) -> u64 {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "audit_memory_probe", "--nocapture"])
        .env("MOLT_AUDIT_RSS_PATH", path)
        .env("MOLT_AUDIT_RSS_OPERATION", operation)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{operation}: {stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    stdout.lines().find_map(|line| line.strip_prefix("AUDIT_RSS_KIB=")).unwrap().parse().unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn audit_readers_keep_memory_bounded_as_the_log_grows() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("small.jsonl");
    let large = dir.path().join("large.jsonl");
    // Both valid chains have the same 16 KiB payloads. Only the number of
    // records changes, from roughly 16 MiB to 128 MiB of log.
    let event = AuditEvent::ServiceExited { service: ServiceId::new("s").unwrap(), status: "x".repeat(16 * 1024) };
    for (path, count) in [(&small, 1024), (&large, 8192)] {
        let log = AuditLog::open(path, false).await.unwrap();
        for _ in 0..count {
            log.append(event.clone()).await.unwrap();
        }
        log.close().await;
    }
    let mut grew_with_log = Vec::new();
    for operation in ["verify", "open", "tail"] {
        let small_kib = peak_rss(&small, operation);
        let large_kib = peak_rss(&large, operation);
        println!("{operation}: 16 MiB log = {small_kib} KiB peak RSS; 128 MiB log = {large_kib} KiB peak RSS");
        // Allow runtime/allocator noise while rejecting even one full-file
        // allocation (the old readers grow by over 100 MiB here).
        if large_kib > small_kib + 16 * 1024 {
            grew_with_log.push(operation);
        }
    }
    assert!(grew_with_log.is_empty(), "memory grew with the log: {grew_with_log:?}");
}

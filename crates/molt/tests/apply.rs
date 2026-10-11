//! Recovery is available without a kernel socket or provider credentials.

use std::fs;
use std::process::Command;

use molt_tools::{Fs, Roots};
use serde_json::{json, Value};

#[tokio::test]
async fn the_offline_cli_inspects_resumes_and_undoes_the_same_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let scratch = tmp.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.txt"), "old\n").unwrap();
    let service = Fs::new(Roots { root: root.clone(), scratch: scratch.clone() }).unwrap();
    let fork = service.handle("fork", json!({"workspace": root})).await.unwrap();
    let fork = fork["fork"].as_str().unwrap();
    fs::write(std::path::Path::new(fork).join("file.txt"), "new\n").unwrap();
    let merged = service.handle("merge", json!({"fork": fork})).await.unwrap();
    let id = merged["operation"].as_str().unwrap();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_molt-tools"))
            .arg("apply")
            .arg("--root")
            .arg(&root)
            .arg("--scratch")
            .arg(&scratch)
            .args(args)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap()
    };
    let list = cli(&["list"]);
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["operations"][0]["id"], id);
    assert_eq!(list["operations"][0]["paths"][0]["status"], "applied");
    let inspect = cli(&["inspect", id]);
    assert!(inspect.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&inspect.stdout).unwrap(), list);
    for _ in 0..2 {
        let result = cli(&["undo", id]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), "old\n");
    }
    for _ in 0..2 {
        let result = cli(&["resume", id]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), "new\n");
    }
    fs::write(root.join("file.txt"), "user's later edit\n").unwrap();
    let refused = cli(&["undo", id]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("conflict: file.txt"));
    assert_eq!(fs::read_to_string(root.join("file.txt")).unwrap(), "user's later edit\n");
}

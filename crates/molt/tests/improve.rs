//! `molt improve` end to end (issue #35, `docs/improve.md`): the improver
//! runs in the sandbox, the gate measures with `molt bench`'s harness, and
//! every run is a real `molt do` against a fake Messages API.
//!
//! The fake agent has a habit a prompt can fix: asked to write a word into
//! hello.txt, it writes the word and a `!`, which the hidden tests refuse,
//! unless its system prompt says "Write the word exactly." The fake
//! improver proposes the texts it is handed, in order: first one with that
//! sentence, then one that also says "Shout.", which makes the agent write
//! the word in capitals and so fail every task.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;

use molt_api::improve::SUBMIT_PROMPT;
use molt_api::planner::tools::WRITE_FILE;
use molt_kernel::audit::{self, AuditEvent};
use molt_tools::SandboxPolicy;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const FIX: &str = "You carry out coding tasks in a workspace. Write the word exactly.";
const SHOUT: &str = "You carry out coding tasks in a workspace. Write the word exactly. Shout.";
/// The held-out task's word, which the improver must never see.
const HELD_OUT_WORD: &str = "zebra";

struct FakeApi {
    proposals: Mutex<VecDeque<&'static str>>,
}

impl Respond for FakeApi {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = req.body_json().unwrap();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let n = messages.len();
        let improver = body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_PROMPT));
        let tool = |name: &str, input: Value| json!({ "type": "tool_use", "id": format!("toolu_{n:03}"), "name": name, "input": input });
        let after_tools = messages
            .last()
            .and_then(|m| m["content"].as_array())
            .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
        let (content, stop) = if improver {
            let text = self.proposals.lock().unwrap().pop_front().expect("an improver call was not expected");
            (tool(SUBMIT_PROMPT, json!({ "text": text, "rationale": "The runs added a ! to the word." })), "tool_use")
        } else if after_tools {
            (json!({ "type": "text", "text": "Wrote hello.txt." }), "end_turn")
        } else {
            let system = body["system"].to_string();
            let asked = serde_json::to_string(&messages).unwrap();
            let word = asked.split("Write ").nth(1).and_then(|s| s.split(" into hello.txt").next()).unwrap_or("?");
            let written = match (system.contains("Write the word exactly."), system.contains("Shout.")) {
                (true, true) => word.to_uppercase(),
                (true, false) => word.to_owned(),
                (false, _) => format!("{word}!"),
            };
            (tool(WRITE_FILE, json!({ "path": "hello.txt", "content": format!("{written}\n") })), "tool_use")
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "id": format!("msg_{n:03}"),
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [content],
            "stop_reason": stop,
            "stop_sequence": null,
            "usage": { "input_tokens": 1000, "output_tokens": 100, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 }
        }))
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn task(tasks: &Path, id: &str, word: &str, split: &str) {
    let dir = tasks.join(id);
    write(
        &dir.join("task.toml"),
        &format!(
            "title = \"Say {word}\"\nlanguage = \"python\"\nkind = \"feature\"\ndifficulty = \"easy\"\nsplit = \"{split}\"\n\
             tests = \"true\"\ncheck = \"sh test_bench_hidden.sh\"\nprompt = \"Write {word} into hello.txt.\"\n"
        ),
    );
    write(&dir.join("repo/README.md"), "# A greeting\n");
    write(&dir.join("hidden/test_bench_hidden.sh"), &format!("grep -qx {word} hello.txt\n"));
    write(&dir.join("solution/hello.txt"), &format!("{word}\n"));
}

/// Every file under `dir` with its contents, to see that nothing changed.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else {
                files.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

struct Home {
    root: tempfile::TempDir,
    server: MockServer,
}

impl Home {
    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn molt(&self, args: &[&str]) -> Output {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molt"));
        cmd.args(args).current_dir(self.root.path());
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy().into_owned();
            if name.starts_with("MOLT_") || name.starts_with("ANTHROPIC_") || name.starts_with("XDG_") {
                cmd.env_remove(name);
            }
        }
        cmd.env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env("XDG_CACHE_HOME", self.path("home/.cache"))
            .env("ANTHROPIC_BASE_URL", self.server.uri())
            .env("ANTHROPIC_API_KEY", "test");
        cmd.output().unwrap()
    }

    fn improve(&self) -> Output {
        let (tasks, scratch) = (self.path("tasks"), self.path("scratch"));
        self.molt(&[
            "improve",
            "run",
            "--tasks",
            tasks.to_str().unwrap(),
            "--max-usd",
            "5",
            "--task-usd",
            "0.5",
            "--timeout-s",
            "120",
            "--scratch",
            scratch.to_str().unwrap(),
        ])
    }

    /// `molt do` in a fresh workspace, as a user would run it: what it wrote.
    fn hello(&self, n: u32) -> String {
        let workspace = self.path(&format!("ws{n}"));
        std::fs::create_dir_all(&workspace).unwrap();
        let out = self.molt(&[
            "do",
            "--workspace",
            workspace.to_str().unwrap(),
            "--no-check",
            "--attempts",
            "1",
            "--no-memory",
            "--no-learn",
            "--json",
            "--",
            "Write hi into hello.txt.",
        ]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        std::fs::read_to_string(workspace.join("hello.txt")).unwrap()
    }

    async fn improver_requests(&self) -> Vec<String> {
        let requests = self.server.received_requests().await.unwrap();
        requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .filter(|b| b.contains(SUBMIT_PROMPT))
            .collect()
    }
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

fn sandbox_available() -> bool {
    match SandboxPolicy::default().probe() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("MOLT_REQUIRE_SANDBOX_TESTS").is_none(),
                "required sandbox unavailable: {error:#}"
            );
            eprintln!("no sandbox in this executor, so the improver cannot run: {error:#}");
            false
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_promotes_a_prompt_that_fixes_a_failure_refuses_one_that_regresses_and_rolls_back_in_one_step() {
    if !sandbox_available() {
        return;
    }
    let server = MockServer::start().await;
    let api = FakeApi { proposals: Mutex::new(VecDeque::from([FIX, SHOUT])) };
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(api).mount(&server).await;
    let home = Home { root: tempfile::tempdir().unwrap(), server };
    let tasks = home.path("tasks");
    task(&tasks, "py-hi", "hi", "dev");
    task(&tasks, "py-bye", "bye", "dev");
    task(&tasks, "py-zebra", HELD_OUT_WORD, "heldout");
    // One no prompt fixes, so that the live version always has a failure to learn from.
    task(&tasks, "py-never", "never", "dev");
    write(&tasks.join("py-never/hidden/test_bench_hidden.sh"), "exit 1\n");
    // The gate's thresholds are the defaults; the runs are single unverified attempts, to keep the fake simple.
    let config = home.path("home/.config/molt/config.toml");
    write(&config, "[improve]\noptions = [\"--no-check\", \"--attempts\", \"1\", \"--no-memory\"]\n");
    let trusted = (snapshot(&tasks), std::fs::read(&config).unwrap());
    let improve = home.path("home/.local/share/molt/improve");

    // Before: the agent's habit fails the task.
    assert_eq!(home.hello(0), "hi!\n");

    // A candidate that fixes it is proposed, measured and promoted.
    let out = home.improve();
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("attempt: 3 failed runs on the regression set"), "{stdout}");
    assert!(stdout.contains("ok: held-out set: 1/1 runs passed, the live version 0/1"), "{stdout}");
    assert!(stdout.contains("attempt: promoted"), "{stdout}");
    let asked = home.improver_requests().await;
    assert_eq!(asked.len(), 1);
    assert!(
        asked[0].contains("hi!") && asked[0].contains("said it was done, but the hidden tests failed"),
        "{}",
        asked[0]
    );
    assert!(!asked[0].contains(HELD_OUT_WORD), "the improver saw the held-out set: {}", asked[0]);
    let first = home.molt(&["improve", "status"]);
    let status = text(&first.stdout);
    assert!(status.contains("prompt.attempt: ") && status.contains("derived from the built-in"), "{status}");
    let listed = text(&home.molt(&["prompts", "list"]).stdout);
    assert!(listed.lines().any(|l| l.starts_with("attempt") && l.ends_with("promoted by molt improve")), "{listed}");

    // After: every run speaks with the promoted prompt, and the task is done right.
    assert_eq!(home.hello(1), "hi\n");

    // A candidate that regresses on both sets is measured and not promoted.
    let out = home.improve();
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(3), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("failed: 3 regressions (tasks it passes less often: py-bye, py-hi, py-zebra)"), "{stdout}");
    assert!(stdout.contains("is not promoted"), "{stdout}");
    assert_eq!(text(&home.molt(&["improve", "status"]).stdout).lines().next(), status.lines().next());
    assert_eq!(home.hello(2), "hi\n");

    // Everything is on the record, each decision before the promotion it allowed.
    let log = audit::read_all(&improve.join("audit.jsonl")).await.unwrap();
    let stored: Vec<&str> = log
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::ModuleStored { module, by, .. } if module == "prompt.attempt" => Some(by.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(stored, ["improver", "improver"]);
    let decisions: Vec<(u64, bool, &molt_kernel::audit::Decision)> = log
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::GateDecision(d) => Some((e.seq, d.promote, d)),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.iter().map(|d| d.1).collect::<Vec<_>>(), [true, false]);
    let (decided_at, _, promoted) = decisions[0];
    assert_eq!((promoted.by.as_str(), promoted.baseline.as_ref()), ("gate:gate", None));
    assert_eq!(promoted.evidence["heldout_set"]["fixed"], json!(["py-zebra"]));
    assert!(promoted.evidence["results_sha256"].as_str().is_some_and(|h| h.len() == 64));
    let promotions: Vec<(u64, &str, Option<&molt_proto::VersionId>)> = log
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::ModulePromoted { module, by, to, .. } if module == "prompt.attempt" => {
                Some((e.seq, by.as_str(), to.as_ref()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(promotions.len(), 1, "the regressing candidate was never promoted");
    assert!(promotions[0].0 > decided_at && promotions[0].1 == "gate:gate");
    assert_eq!(promotions[0].2, Some(&promoted.candidate));

    // One step back to the built-in prompt, by a person, on the record too.
    let out = home.molt(&["improve", "rollback"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("rolled back from"), "{}", text(&out.stdout));
    assert_eq!(home.hello(3), "hi!\n");
    let log = audit::read_all(&improve.join("audit.jsonl")).await.unwrap();
    let last = log.iter().rev().find_map(|e| match &e.event {
        AuditEvent::ModulePromoted { by, from, to, .. } => Some((by.clone(), from.clone(), to.clone())),
        _ => None,
    });
    assert_eq!(last, Some(("human".into(), Some(promoted.candidate.clone()), None)));
    audit::verify(&improve.join("audit.jsonl")).await.unwrap();

    // Nothing of the gate's moved: the tasks with their hidden tests and the held-out set, and the thresholds.
    assert_eq!((snapshot(&tasks), std::fs::read(&config).unwrap()), trusted);
}

#[test]
fn the_improver_cannot_be_pointed_at_a_prompt_that_guards_acceptance() {
    let home = tempfile::tempdir().unwrap();
    for prompt in ["reviewer", "judge", "vetter"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .args(["improve", "run", "--prompt", prompt, "--max-usd", "1"])
            .env("HOME", home.path())
            .env("MOLT_IMPROVE_DIR", home.path().join("improve"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(text(&out.stderr).contains("guards what results are accepted"), "{prompt}: {}", text(&out.stderr));
    }
}

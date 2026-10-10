//! `molt bench` end to end: two tasks, both built-in arms, each run a real
//! `molt do` against a fake Messages API, graded by the tasks' hidden tests.
//!
//! The fake model always writes "hi" into hello.txt. Asked to design a
//! check, it designs one that only looks for the file. So it solves the
//! task that wants "hi" and fails the one that wants "bye", where Molt's
//! check passes and the hidden tests do not.

use std::path::Path;
use std::process::Output;

use molt_api::planner::tools::{SUBMIT_CHECK, WRITE_FILE};
use molt_api::planner::Outcome;
use molt_bench::record::{self, Record};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct FakeApi;

impl Respond for FakeApi {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = req.body_json().unwrap();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let n = messages.len();
        let designer = body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_CHECK));
        let last = messages.last().cloned().unwrap_or(Value::Null);
        let after_tools = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
        let tool = |name: &str, input: Value| json!({ "type": "tool_use", "id": format!("toolu_{n:03}"), "name": name, "input": input });
        let (content, stop) = match (designer, n == 1, after_tools) {
            (true, true, _) => {
                (tool(WRITE_FILE, json!({ "path": "check.sh", "content": "test -f hello.txt\n" })), "tool_use")
            }
            (true, false, _) => {
                let input = json!({ "command": "sh check.sh", "files": ["check.sh"], "rationale": "The file exists." });
                (tool(SUBMIT_CHECK, input), "tool_use")
            }
            (false, _, true) => (json!({ "type": "text", "text": "Wrote hello.txt." }), "end_turn"),
            (false, _, false) => (tool(WRITE_FILE, json!({ "path": "hello.txt", "content": "hi\n" })), "tool_use"),
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

/// A task asking for `word` in hello.txt.
fn task(tasks: &Path, id: &str, word: &str) {
    let dir = tasks.join(id);
    write(
        &dir.join("task.toml"),
        &format!(
            "title = \"Say {word}\"\nlanguage = \"python\"\nkind = \"feature\"\ndifficulty = \"easy\"\nsplit = \"dev\"\n\
             tests = \"true\"\ncheck = \"sh test_bench_hidden.sh\"\nprompt = \"Write {word} into hello.txt.\"\n"
        ),
    );
    write(&dir.join("repo/README.md"), "# A greeting\n");
    write(&dir.join("hidden/test_bench_hidden.sh"), &format!("grep -qx {word} hello.txt\n"));
    write(&dir.join("solution/hello.txt"), &format!("{word}\n"));
}

fn molt_bench(server: &MockServer, args: &[&str]) -> Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molt"));
    cmd.arg("bench").args(args);
    // The benchmark sets the API for its runs; nothing of the developer's own may change them.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("MOLT_") || name.starts_with("ANTHROPIC_") {
            cmd.env_remove(name);
        }
    }
    if !args.contains(&"report") {
        cmd.args(["--env", &format!("ANTHROPIC_BASE_URL={}", server.uri()), "--env", "ANTHROPIC_API_KEY=test"]);
    }
    cmd.output().unwrap()
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

#[tokio::test]
async fn both_arms_run_every_task_and_the_report_compares_them() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(FakeApi).mount(&server).await;
    let root = tempfile::tempdir().unwrap();
    let tasks = root.path().join("tasks");
    task(&tasks, "py-hi", "hi");
    task(&tasks, "py-bye", "bye");
    let scratch = tempfile::tempdir().unwrap();
    let results = root.path().join("out/results.jsonl");
    let (tasks, results_arg) = (tasks.to_str().unwrap(), results.to_str().unwrap());
    let run = [
        "run",
        "--tasks",
        tasks,
        "--results",
        results_arg,
        "--max-usd",
        "5",
        "--task-usd",
        "1",
        "--timeout-s",
        "120",
        "--scratch",
        scratch.path().to_str().unwrap(),
    ];

    let out = molt_bench(&server, &run);
    assert!(out.status.success(), "{}\n{}", text(&out.stdout), text(&out.stderr));
    let records = record::load(&results).unwrap();
    assert_eq!(records.len(), 4, "{}", text(&out.stderr));
    let find = |task: &str, arm: &str| -> &Record { records.iter().find(|r| r.task == task && r.arm == arm).unwrap() };

    let molt = find("py-hi", "molt");
    assert!(molt.passed && molt.claimed_done && molt.error.is_none(), "{molt:#?}");
    assert_eq!((molt.outcome, molt.attempts), (Some(Outcome::Passed), 2));
    // The designer's two turns and two of each attempt.
    assert_eq!(molt.model_calls, 6, "{molt:#?}");
    assert!(molt.cost_usd > 0.0 && molt.reported_cost_usd.is_some());

    let plain = find("py-hi", "plain");
    assert!(plain.passed && plain.claimed_done, "{plain:#?}");
    assert_eq!((plain.outcome, plain.attempts, plain.model_calls), (Some(Outcome::Unverified), 1, 2));
    assert_eq!(plain.arm_args, ["--no-check", "--attempts", "1", "--no-memory"]);

    // Both said they were done with "bye", and the hidden tests disagree.
    for arm in ["molt", "plain"] {
        let r = find("py-bye", arm);
        assert!(!r.passed && r.claimed_done, "{r:#?}");
        assert!(r.grade.output.is_empty() && r.grade.exit_code == Some(1), "{r:#?}");
    }
    assert!(records.iter().all(|r| r.setup.env.contains(&"ANTHROPIC_API_KEY=<redacted>".to_owned())));
    let logs = root.path().join("out/results.logs/py-hi/molt-0");
    assert!(std::fs::read_to_string(logs.join("changes.diff")).unwrap().contains("+hi"));
    assert!(logs.join("molt.json").exists() && logs.join("stderr.log").exists());
    let stdout = text(&out.stdout);
    assert!(stdout.contains("| molt | 1/2 (50%) |"), "{stdout}");

    // Started again, there is nothing left to do and the model is not called.
    let calls = server.received_requests().await.unwrap().len();
    let out = molt_bench(&server, &run);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("4 runs planned, 4 already in the results file"), "{}", text(&out.stderr));
    assert_eq!(server.received_requests().await.unwrap().len(), calls);
    assert_eq!(record::load(&results).unwrap().len(), 4);

    let out = molt_bench(&server, &["report", results_arg, "--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["arms"][0]["arm"], "molt");
    assert_eq!(report["arms"][1]["false_done"], 1);
    assert_eq!(report["pairs"][0]["both"], 1);
    assert_eq!(report["pairs"][0]["neither"], 1);
}

#[tokio::test]
async fn a_run_without_a_spending_limit_or_api_key_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let tasks = root.path().join("tasks");
    task(&tasks, "py-hi", "hi");
    let results = root.path().join("r.jsonl");
    let base = ["bench", "run", "--tasks", tasks.to_str().unwrap(), "--results", results.to_str().unwrap()];
    let bench = || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molt"));
        cmd.args(base).env_remove("ANTHROPIC_API_KEY");
        cmd
    };

    let out = bench().output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("--max-usd"));

    let out = bench().args(["--max-usd", "1"]).output().unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("ANTHROPIC_API_KEY"), "{}", text(&out.stderr));
    assert!(!results.exists());

    let out = bench().args(["--dry-run", "--trials", "3"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("6 runs (1 task × 2 arms × 3 trials)") && stdout.contains("up to $30.00"), "{stdout}");
    assert!(!results.exists());
}

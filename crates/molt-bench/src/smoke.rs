//! What a smoke run is checked for beyond its hidden tests: that its result
//! is free of build output, was applied and verified the way the task
//! expects, that memory learned from it and recalled what it was given, and
//! that the cost it reported is the cost its audit log shows, with no call
//! of unknown cost counted as free. `bench/README.md` describes `[smoke]`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::task::{self, Task, Verifier};

/// The log of a run's notes recalled, as `molt memory used --json` printed it.
pub const RECALLED_LOG: &str = "recalled.json";
/// Paths of changed files named in a failed assertion's detail.
const NAMED: usize = 5;
/// Bytes of a file looked at to tell whether it is binary.
const SNIFFED: u64 = 8192;
/// Dollars two costs may differ by and still be the same, for rounding.
const SAME_COST: f64 = 1e-6;

/// One thing checked about a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assertion {
    /// `clean`, `applied`, `verifier`, `learned`, `recalled`, `cost` or `unsettled`.
    pub name: String,
    pub ok: bool,
    /// What was found, in a sentence.
    pub detail: String,
}

impl Assertion {
    fn new(name: &str, ok: bool, detail: impl Into<String>) -> Assertion {
        Assertion { name: name.to_owned(), ok, detail: detail.into() }
    }
}

/// What a finished run left to look at, before the hidden files are laid
/// over its workspace.
pub struct Evidence<'a> {
    pub task: &'a Task,
    pub workspace: &'a Path,
    /// The workspace as the agent got it.
    pub before: &'a Snapshot,
    /// The audit log of the run's data dir.
    pub audit: &'a Path,
    /// The run's logs: `molt.json` and [`RECALLED_LOG`].
    pub logs: &'a Path,
}

/// Check a run of a smoke task. A task without `[smoke]` gets no assertions.
pub fn assess(e: &Evidence<'_>) -> Vec<Assertion> {
    let Some(smoke) = &e.task.spec.smoke else { return Vec::new() };
    let result: Option<Value> =
        fs::read(e.logs.join("molt.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let no_result = || "molt printed no result".to_owned();
    let mut out = vec![clean(e.before, e.workspace)];

    out.push(match &result {
        None => Assertion::new("applied", false, no_result()),
        Some(r) => {
            let applied = r["applied"].as_bool() == Some(true);
            let how = |r: &Value| {
                let review = r["review"]["verdict"].as_str().map(|v| format!(", review {v}")).unwrap_or_default();
                let fork = r["fork"].as_str().map(|f| format!(", kept in {f}")).unwrap_or_default();
                format!("outcome {}{review}{fork}", r["outcome"].as_str().unwrap_or("unknown"))
            };
            let ok = applied == smoke.applied;
            let said = if applied { "applied" } else { "not applied" };
            Assertion::new("applied", ok, format!("the result was {said} ({})", how(r)))
        }
    });

    if let Some(want) = smoke.verifier {
        out.push(match &result {
            None => Assertion::new("verifier", false, no_result()),
            Some(r) => {
                let outcome = r["outcome"].as_str().unwrap_or("unknown");
                let got = match outcome {
                    "passed" => Some(Verifier::Check),
                    "judged" => Some(Verifier::Rubric),
                    "unverified" => Some(Verifier::Unverified),
                    _ => None,
                };
                let how = got.map_or_else(
                    || format!("no result was verified (outcome {outcome})"),
                    |v| format!("the result was verified by {v}"),
                );
                Assertion::new("verifier", got == Some(want), format!("{how}; the task expects {want}"))
            }
        });
    }

    if smoke.learn {
        out.push(match &result {
            None => Assertion::new("learned", false, no_result()),
            Some(r) => learned(&r["learning"]),
        });
    }

    if !smoke.remember.is_empty() {
        out.push(recalled(&smoke.remember, &e.logs.join(RECALLED_LOG)));
    }

    let audit = Audit::read(e.audit);
    out.push(match (&result, &audit) {
        (None, _) => Assertion::new("cost", false, no_result()),
        (Some(_), None) => Assertion::new("cost", false, "the run left no audit log"),
        (Some(r), Some(a)) => {
            let learning = r["learning"]["cost_usd"].as_f64().unwrap_or(0.0);
            let reported = r["cost_usd"].as_f64().unwrap_or(0.0) + learning;
            let ok = (reported - a.priced_usd).abs() <= SAME_COST * (1.0 + a.priced_usd.abs());
            Assertion::new(
                "cost",
                ok,
                format!(
                    "reported ${reported:.6} (learning ${learning:.6} of it); the audit log's priced replies sum \
                     to ${:.6}",
                    a.priced_usd
                ),
            )
        }
    });
    out.push(match (&result, &audit) {
        (None, _) => Assertion::new("unsettled", false, no_result()),
        (Some(_), None) => Assertion::new("unsettled", false, "the run left no audit log"),
        (Some(r), Some(a)) => {
            let reported = r["uncounted_calls"].as_u64().unwrap_or(0);
            let ok = u64::from(a.unsettled) <= reported;
            Assertion::new(
                "unsettled",
                ok,
                format!(
                    "{} of {} model calls have no priced reply in the audit log; the run reported {reported} \
                     calls of unknown cost",
                    a.unsettled, a.requests
                ),
            )
        }
    });
    out
}

/// The files of a workspace and a hash of each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot(HashMap<PathBuf, [u8; 32]>);

impl Snapshot {
    /// Every file under `dir`. Unlike [`task::files`] it skips nothing but
    /// `.git`, since build output is what it is compared for, and it does
    /// not follow links: a link is hashed by its target's path.
    pub fn take(dir: &Path) -> std::io::Result<Snapshot> {
        fn walk(root: &Path, rel: &Path, out: &mut HashMap<PathBuf, [u8; 32]>) -> std::io::Result<()> {
            for entry in fs::read_dir(root.join(rel))? {
                let entry = entry?;
                let path = rel.join(entry.file_name());
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    if entry.file_name() != ".git" {
                        walk(root, &path, out)?;
                    }
                    continue;
                }
                let mut hash = Sha256::new();
                if kind.is_symlink() {
                    hash.update(b"link\0");
                    hash.update(fs::read_link(root.join(&path))?.as_os_str().as_encoded_bytes());
                } else {
                    std::io::copy(&mut fs::File::open(root.join(&path))?, &mut hash)?;
                }
                out.insert(path, hash.finalize().into());
            }
            Ok(())
        }
        let mut files = HashMap::new();
        walk(dir, Path::new(""), &mut files)?;
        Ok(Snapshot(files))
    }

    /// Files in `after` that are not in this snapshot, or differ, in order.
    fn changed(&self, after: &Snapshot) -> Vec<PathBuf> {
        let mut changed: Vec<PathBuf> =
            after.0.iter().filter(|(path, hash)| self.0.get(*path) != Some(hash)).map(|(p, _)| p.clone()).collect();
        changed.sort();
        changed
    }
}

/// The result holds no build output or binaries: no file the run added or
/// changed is under a build or cache directory, compiled, or binary.
fn clean(before: &Snapshot, workspace: &Path) -> Assertion {
    let changed = match Snapshot::take(workspace) {
        Ok(after) => before.changed(&after),
        Err(e) => return Assertion::new("clean", false, format!("could not look at the workspace: {e}")),
    };
    let dirty: Vec<String> =
        changed.iter().filter(|rel| built(rel) || binary(&workspace.join(rel))).map(|rel| task::slashed(rel)).collect();
    if dirty.is_empty() {
        return Assertion::new(
            "clean",
            true,
            format!("{} files added or changed, none of them build output", changed.len()),
        );
    }
    let mut named: Vec<&str> = dirty.iter().take(NAMED).map(String::as_str).collect();
    if dirty.len() > NAMED {
        named.push("…");
    }
    Assertion::new(
        "clean",
        false,
        format!(
            "{} of {} files added or changed are build output or binary: {}",
            dirty.len(),
            changed.len(),
            named.join(", ")
        ),
    )
}

/// Directories that hold build output or caches, by name or by prefix
/// (`target-done-check` is a Cargo target directory).
fn built(rel: &Path) -> bool {
    const DIRS: &[&str] = &["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", "node_modules", ".gradle"];
    const COMPILED: &[&str] = &["pyc", "pyo", "o", "obj", "a", "so", "dylib", "rlib", "rmeta", "class", "exe"];
    let mut dirs = rel.parent().into_iter().flat_map(|p| p.components()).map(|c| c.as_os_str().to_string_lossy());
    let in_build_dir = dirs
        .any(|d| DIRS.contains(&d.as_ref()) || d == "target" || d.starts_with("target-") || d.starts_with("target_"));
    let compiled = rel.extension().is_some_and(|x| COMPILED.iter().any(|c| x.eq_ignore_ascii_case(c)));
    in_build_dir || compiled
}

/// A regular file whose start holds a NUL byte. Links are not followed.
fn binary(path: &Path) -> bool {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = Vec::new();
    fs::File::open(path).and_then(|f| f.take(SNIFFED).read_to_end(&mut head)).is_ok() && head.contains(&0)
}

/// Memory learned at least one note from the run.
fn learned(learning: &Value) -> Assertion {
    if learning.is_null() {
        return Assertion::new("learned", false, "the run did not learn: its result has no learning");
    }
    if let Some(e) = learning["error"].as_str() {
        return Assertion::new("learned", false, format!("learning failed: {e}"));
    }
    let added: Vec<&str> =
        learning["added"].as_array().into_iter().flatten().filter_map(|n| n["text"].as_str()).collect();
    match added.first() {
        Some(first) => Assertion::new("learned", true, format!("learned {} notes, the first: {first}", added.len())),
        None => {
            let why = learning["skipped"].as_str().unwrap_or("no reason given");
            Assertion::new("learned", false, format!("learned no note ({why})"))
        }
    }
}

/// At least one of the notes written into memory before the run was
/// recalled during it.
fn recalled(remembered: &[String], log: &Path) -> Assertion {
    let snapshots: Value = match fs::read(log).map(|b| serde_json::from_slice(&b)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Assertion::new("recalled", false, format!("{} does not parse: {e}", log.display())),
        Err(_) => return Assertion::new("recalled", false, "memory could not say what the run recalled"),
    };
    let snapshots = snapshots.as_array().cloned().unwrap_or_default();
    let texts: HashSet<&str> = snapshots
        .iter()
        .flat_map(|s| s["notes"].as_array().into_iter().flatten())
        .filter_map(|n| n["note"]["text"].as_str())
        .collect();
    let hits = remembered.iter().filter(|r| texts.contains(r.trim())).count();
    let queries: Vec<&str> = snapshots.iter().filter_map(|s| s["query"].as_str()).take(2).collect();
    let detail = format!(
        "{hits} of {} remembered notes recalled over {} recalls{}",
        remembered.len(),
        snapshots.len(),
        if queries.is_empty() { String::new() } else { format!(" (queries: {})", queries.join(" | ")) }
    );
    Assertion::new("recalled", hits > 0, detail)
}

/// The model calls in a run's audit log.
#[derive(Debug, Default, PartialEq)]
struct Audit {
    /// Requests to the model service.
    requests: u32,
    /// Of those, the ones with no reply carrying a price: pending,
    /// cancelled, failed without usage, or of a model without a price.
    unsettled: u32,
    /// What the priced replies cost.
    priced_usd: f64,
}

impl Audit {
    /// `None` when there is no log. Lines that do not parse, such as a last
    /// one cut off when the run was killed, are skipped.
    fn read(path: &Path) -> Option<Audit> {
        let text = fs::read_to_string(path).ok()?;
        let mut requests: Vec<String> = Vec::new();
        let mut priced: HashMap<String, f64> = HashMap::new();
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
            let event = &entry["event"];
            let envelope = &event["envelope"];
            if event["type"] != "message" {
                continue;
            }
            if envelope["kind"] == "request" && envelope["to"] == molt_api::model::COMPLETE {
                if let Some(id) = envelope["id"].as_str() {
                    requests.push(id.to_owned());
                }
            } else if envelope["kind"] == "reply" && envelope["from"] == "model" {
                let payload = &envelope["payload"];
                if let (Some(to), Some(cost), true) =
                    (envelope["reply_to"].as_str(), payload["cost_usd"].as_f64(), payload.get("usage").is_some())
                {
                    priced.insert(to.to_owned(), cost);
                }
            }
        }
        let unsettled = requests.iter().filter(|id| !priced.contains_key(*id)).count() as u32;
        Some(Audit { requests: requests.len() as u32, unsettled, priced_usd: priced.values().sum() })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::{task_dir, write};

    fn smoke_task(root: &Path, smoke: &str) -> Task {
        let dir = task_dir(root, "py-smoke");
        let toml = fs::read_to_string(dir.join("task.toml")).unwrap();
        fs::write(dir.join("task.toml"), format!("{toml}\n[smoke]\n{smoke}")).unwrap();
        Task::load(&dir).unwrap()
    }

    fn entry(kind: &str, from: &str, to: &str, id: &str, reply_to: Option<&str>, payload: Value) -> String {
        json!({ "seq": 1, "event": { "type": "message", "envelope": {
            "id": id, "trace_id": "t", "from": from, "to": to, "kind": kind, "reply_to": reply_to, "payload": payload } } })
        .to_string()
    }

    struct Run {
        _root: tempfile::TempDir,
        task: Task,
        workspace: PathBuf,
        before: Snapshot,
        audit: PathBuf,
        logs: PathBuf,
    }

    impl Run {
        fn new(smoke: &str) -> Run {
            let root = tempfile::tempdir().unwrap();
            let task = smoke_task(&root.path().join("tasks"), smoke);
            let workspace = root.path().join("w");
            task::copy_tree(&task.repo(), &workspace).unwrap();
            // What a setup command's build left before the run, which is not the run's.
            write(&workspace.join("target/debug/calc"), "\0\0built before");
            let before = Snapshot::take(&workspace).unwrap();
            let (audit, logs) = (root.path().join("audit.jsonl"), root.path().join("logs"));
            fs::create_dir(&logs).unwrap();
            Run { _root: root, task, workspace, before, audit, logs }
        }

        fn result(&self, v: Value) {
            fs::write(self.logs.join("molt.json"), v.to_string()).unwrap();
        }

        fn audit(&self, lines: &[String]) {
            fs::write(&self.audit, lines.join("\n")).unwrap();
        }

        fn assess(&self) -> HashMap<String, Assertion> {
            let e = Evidence {
                task: &self.task,
                workspace: &self.workspace,
                before: &self.before,
                audit: &self.audit,
                logs: &self.logs,
            };
            assess(&e).into_iter().map(|a| (a.name.clone(), a)).collect()
        }
    }

    #[test]
    fn a_task_without_smoke_gets_no_assertions() {
        let root = tempfile::tempdir().unwrap();
        let task = Task::load(&task_dir(root.path(), "py-plain")).unwrap();
        let before = Snapshot::default();
        let e =
            Evidence { task: &task, workspace: root.path(), before: &before, audit: root.path(), logs: root.path() };
        assert!(assess(&e).is_empty());
    }

    #[test]
    fn build_output_and_binaries_in_the_result_are_named() {
        let run = Run::new("");
        write(&run.workspace.join("hello.txt"), "hi\n");
        write(&run.workspace.join("Cargo.lock"), "# a lock file is source\n");
        let found = run.assess();
        assert!(found["clean"].ok, "{:?}", found["clean"]);
        assert!(found["clean"].detail.starts_with("2 files"), "{:?}", found["clean"]);

        // What a done-check's `CARGO_TARGET_DIR=target-done-check cargo test` and Python leave behind.
        write(&run.workspace.join("target-done-check/debug/deps/libcalc-1f2e.rmeta"), "rust");
        write(&run.workspace.join("target-done-check/debug/.fingerprint/calc/lib-calc"), "1f2e");
        write(&run.workspace.join("src/__pycache__/calc.cpython-312.pyc"), "py");
        fs::write(run.workspace.join("minicalc"), b"\x7fELF\x02\x01\x01\0\0\0").unwrap();
        let clean = &run.assess()["clean"];
        assert!(!clean.ok);
        assert!(clean.detail.starts_with("4 of 6 files"), "{clean:?}");
        for path in ["minicalc", "src/__pycache__/calc.cpython-312.pyc", "target-done-check/debug/deps/"] {
            assert!(clean.detail.contains(path), "{path}: {clean:?}");
        }
        assert!(!clean.detail.contains("hello.txt") && !clean.detail.contains("Cargo.lock"), "{clean:?}");

        // Build output the run changed is its own, even where some stood before it.
        write(&run.workspace.join("target/debug/calc"), "\0\0built again");
        assert!(run.assess()["clean"].detail.contains("target/debug/calc"));
    }

    #[test]
    fn a_result_kept_by_the_review_fails_a_task_that_expects_it_applied() {
        let run = Run::new("verifier = \"check\"");
        run.result(json!({ "outcome": "passed", "applied": false, "fork": "/tmp/fork-1",
            "review": { "verdict": "concern" }, "cost_usd": 0.0 }));
        let found = run.assess();
        let applied = &found["applied"];
        assert!(!applied.ok);
        assert_eq!(applied.detail, "the result was not applied (outcome passed, review concern, kept in /tmp/fork-1)");
        assert!(found["verifier"].ok, "{:?}", found["verifier"]);

        let run = Run::new("applied = false\nverifier = \"rubric\"");
        run.result(json!({ "outcome": "passed", "applied": false, "cost_usd": 0.0 }));
        let found = run.assess();
        assert!(found["applied"].ok);
        assert!(!found["verifier"].ok);
        assert_eq!(found["verifier"].detail, "the result was verified by check; the task expects rubric");
    }

    #[test]
    fn learning_that_failed_or_found_nothing_says_why() {
        let run = Run::new("learn = true");
        let learned = |learning: Value| {
            run.result(json!({ "outcome": "passed", "applied": true, "cost_usd": 0.0, "learning": learning }));
            run.assess()["learned"].clone()
        };
        let failed = learned(json!({
            "error": "the model's answer did not parse: invalid type: sequence, expected a string at line 1 column 325"
        }));
        assert!(!failed.ok && failed.detail.contains("expected a string at line 1 column 325"), "{failed:?}");
        let nothing = learned(json!({ "added": [], "reinforced": [], "contradicted": [],
            "skipped": "nothing new", "usage": {}, "cost_usd": 0.01 }));
        assert_eq!((nothing.ok, nothing.detail.as_str()), (false, "learned no note (nothing new)"));
        assert!(!learned(Value::Null).ok);
        let one = learned(json!({ "added": [{ "text": "测试命令是 cargo test。" }], "reinforced": [],
            "contradicted": [], "usage": {}, "cost_usd": 0.01 }));
        assert!(one.ok && one.detail.ends_with("测试命令是 cargo test。"), "{one:?}");
    }

    #[test]
    fn a_remembered_note_must_be_recalled() {
        let note = "金额一律用 Decimal 计算，不能用 float。";
        let run = Run::new(&format!("remember = [{note:?}]"));
        assert_eq!(run.assess()["recalled"].detail, "memory could not say what the run recalled");

        let log = run.logs.join(RECALLED_LOG);
        fs::write(&log, json!([{ "query": "给 parse_amount 加上千分位", "notes": [] }]).to_string()).unwrap();
        let missed = &run.assess()["recalled"];
        assert!(!missed.ok);
        assert_eq!(
            missed.detail,
            "0 of 1 remembered notes recalled over 1 recalls (queries: 给 parse_amount 加上千分位)"
        );

        let hit = json!([{ "query": "q", "notes": [] }, { "query": "q2", "notes": [{ "note": { "text": note } }] }]);
        fs::write(&log, hit.to_string()).unwrap();
        assert!(run.assess()["recalled"].ok);
    }

    #[test]
    fn reported_cost_must_match_the_audit_log_and_unknown_cost_must_be_reported() {
        let run = Run::new("");
        let usage = json!({ "input_tokens": 1000, "output_tokens": 100 });
        let request = |id: &str| entry("request", "planner", "model.complete", id, None, json!({}));
        let reply =
            |to: &str, payload: Value| entry("reply", "model", "planner", &format!("r-{to}"), Some(to), payload);
        run.audit(&[
            request("m1"),
            reply("m1", json!({ "usage": usage, "cost_usd": 0.25 })),
            request("m2"),
            reply("m2", json!({ "usage": usage, "cost_usd": 0.5 })),
            // A race loser cancelled before its reply, and a call to a model without a price.
            request("m3"),
            request("m4"),
            reply("m4", json!({ "usage": usage, "cost_usd": null })),
            // Not model calls.
            entry("request", "planner", "fs.write", "f1", None, json!({})),
            entry("reply", "planner", "x", "p1", Some("m9"), json!({ "usage": usage, "cost_usd": 9.0 })),
        ]);

        run.result(json!({ "outcome": "passed", "applied": true, "cost_usd": 0.5, "uncounted_calls": 2,
            "learning": { "added": [], "cost_usd": 0.25 } }));
        let found = run.assess();
        assert!(found["cost"].ok, "{:?}", found["cost"]);
        assert!(found["unsettled"].ok, "{:?}", found["unsettled"]);

        // Unknown counted as zero: the two calls without a price are reported as settled and free.
        run.result(json!({ "outcome": "passed", "applied": true, "cost_usd": 0.5, "uncounted_calls": 0 }));
        let found = run.assess();
        assert!(!found["cost"].ok);
        assert_eq!(
            found["cost"].detail,
            "reported $0.500000 (learning $0.000000 of it); the audit log's priced replies sum to $0.750000"
        );
        assert!(!found["unsettled"].ok);
        assert_eq!(
            found["unsettled"].detail,
            "2 of 4 model calls have no priced reply in the audit log; the run reported 0 calls of unknown cost"
        );

        fs::remove_file(&run.audit).unwrap();
        assert_eq!(run.assess()["cost"].detail, "the run left no audit log");
        fs::remove_file(run.logs.join("molt.json")).unwrap();
        let found = run.assess();
        assert!(["applied", "cost", "unsettled"].iter().all(|n| found[*n].detail == "molt printed no result"));
    }
}

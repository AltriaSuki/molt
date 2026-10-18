//! `fs.*` against real directories: confinement, the file methods, and the
//! fork / diff / merge / drop cycle.

use std::fs;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use molt_api::fs::{
    ChangeKind, DiffResponse, EditResponse, FilesChanged, ForkResponse, ListResponse, MergeResponse, ReadResponse,
    SearchResponse, WriteResponse,
};
use molt_proto::{ErrorCode, RemoteError};
use molt_tools::{Fs, Roots};
use serde_json::{json, Value};
use tempfile::TempDir;

#[tokio::test]
async fn a_durable_merge_retains_preimages_for_files_links_modes_and_dropped_forks() {
    use molt_api::fs::{AppliesResponse, ApplyStatus, RecoverResponse};
    let env = Env::new();
    Env::put(&env.ws, &[("modified.txt", "old\n"), ("deleted.txt", "restore me\n"), ("run.sh", "echo hello\n")]);
    fs::set_permissions(env.ws.join("modified.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    symlink("modified.txt", env.ws.join("link")).unwrap();
    let fork = env.fork().await;
    Env::put(&fork, &[("modified.txt", "new\n"), ("new/added.txt", "added\n")]);
    fs::remove_file(fork.join("deleted.txt")).unwrap();
    fs::set_permissions(fork.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(fork.join("link")).unwrap();
    symlink("new/added.txt", fork.join("link")).unwrap();
    let merged = env.ok("merge", json!({"fork": fork, "drop": true})).await;
    let id = merged["operation"].as_str().unwrap();
    assert!(!fork.exists());
    let operations: AppliesResponse =
        serde_json::from_value(env.ok("applies", json!({"workspace": "ws"})).await).unwrap();
    assert_eq!(operations.operations[0].status, ApplyStatus::Applied);
    assert_eq!(fs::read_to_string(env.ws.join("modified.txt")).unwrap(), "new\n");
    assert!(!env.ws.join("deleted.txt").exists());
    assert_eq!(fs::read_link(env.ws.join("link")).unwrap(), Path::new("new/added.txt"));
    for _ in 0..2 {
        let (reply, changed) =
            env.fs.handle_reporting("recover", json!({"workspace": "ws", "operation": id, "action": "undo"})).await;
        let reply: RecoverResponse = serde_json::from_value(reply.unwrap()).unwrap();
        assert_eq!(reply.operation.status, ApplyStatus::Undone);
        if let Some(changed) = changed {
            assert_eq!(changed.paths.len(), 5);
        }
    }
    assert_eq!(fs::read_to_string(env.ws.join("modified.txt")).unwrap(), "old\n");
    assert_eq!(fs::read_to_string(env.ws.join("deleted.txt")).unwrap(), "restore me\n");
    assert!(!env.ws.join("new").exists());
    assert_eq!(fs::read_link(env.ws.join("link")).unwrap(), Path::new("modified.txt"));
    assert_eq!(fs::metadata(env.ws.join("modified.txt")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(env.ws.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    let record = env.scratch.join(".molt/applies").join(id);
    assert_eq!(fs::metadata(&record).unwrap().permissions().mode() & 0o777, 0o700);
    for f in fs::read_dir(record).unwrap() {
        assert_eq!(f.unwrap().metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(env
        .invalid(
            "write",
            json!({"workspace": env.scratch, "path": format!(".molt/applies/{id}/journal.json"), "content": "{}"})
        )
        .await
        .contains("Molt's own data"));
    assert!(env
        .invalid("read", json!({"workspace": env.scratch.join(".molt/applies").join(id), "path": "journal.json"}))
        .await
        .contains("Molt's own data"));
    let resumed: RecoverResponse = serde_json::from_value(
        env.ok("recover", json!({"workspace": "ws", "operation": id, "action": "resume"})).await,
    )
    .unwrap();
    assert_eq!(resumed.operation.status, ApplyStatus::Applied);
    assert_eq!(fs::read_to_string(env.ws.join("modified.txt")).unwrap(), "new\n");
}

struct Env {
    tmp: TempDir,
    scratch: PathBuf,
    /// `root/ws`, canonical.
    ws: PathBuf,
    fs: Fs,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let scratch = tmp.path().join("scratch");
        fs::create_dir_all(root.join("ws")).unwrap();
        fs::create_dir_all(tmp.path().join("outside")).unwrap();
        fs::write(tmp.path().join("outside/secret.txt"), "secret\n").unwrap();
        let fs = Fs::new(Roots { root: root.clone(), scratch: scratch.clone() }).unwrap();
        let ws = root.join("ws").canonicalize().unwrap();
        Env { scratch: scratch.canonicalize().unwrap(), ws, fs, tmp }
    }

    fn outside(&self) -> PathBuf {
        self.tmp.path().join("outside").canonicalize().unwrap()
    }

    /// Create `files` (path, content) under `dir`.
    fn put(dir: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    async fn call(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        self.fs.handle(method, payload).await
    }

    async fn ok(&self, method: &str, payload: Value) -> Value {
        match self.call(method, payload.clone()).await {
            Ok(v) => v,
            Err(e) => panic!("fs.{method} {payload} failed: {e}"),
        }
    }

    async fn err(&self, method: &str, payload: Value) -> RemoteError {
        match self.call(method, payload.clone()).await {
            Ok(v) => panic!("fs.{method} {payload} should fail, got {v}"),
            Err(e) => e,
        }
    }

    async fn invalid(&self, method: &str, payload: Value) -> String {
        let e = self.err(method, payload).await;
        assert_eq!(e.code, ErrorCode::Invalid, "{e}");
        e.message
    }

    async fn fork(&self) -> PathBuf {
        let reply: ForkResponse = serde_json::from_value(self.ok("fork", json!({ "workspace": "ws" })).await).unwrap();
        PathBuf::from(reply.fork)
    }
}

fn read_reply(v: Value) -> ReadResponse {
    serde_json::from_value(v).unwrap()
}

fn listed(v: Value) -> Vec<String> {
    let reply: ListResponse = serde_json::from_value(v).unwrap();
    reply.entries.into_iter().map(|e| e.path).collect()
}

fn found(v: Value) -> Vec<(String, u64)> {
    let reply: SearchResponse = serde_json::from_value(v).unwrap();
    reply.matches.into_iter().map(|m| (m.path, m.line)).collect()
}

#[tokio::test]
async fn paths_that_escape_the_workspace_are_refused() {
    let env = Env::new();
    Env::put(&env.ws, &[("a.txt", "a\n"), ("sub/b.txt", "b\n")]);
    let outside = env.outside();
    symlink(outside.join("secret.txt"), env.ws.join("link")).unwrap();
    symlink(&outside, env.ws.join("outdir")).unwrap();
    symlink("a.txt", env.ws.join("inner")).unwrap();

    for path in ["../outside/secret.txt", "sub/../../outside/secret.txt", "..", "/etc/passwd", "a\0b"] {
        env.invalid("read", json!({ "workspace": "ws", "path": path })).await;
        env.invalid("write", json!({ "workspace": "ws", "path": path, "content": "x" })).await;
    }
    let msg = env.invalid("read", json!({ "workspace": "ws", "path": "link" })).await;
    assert!(msg.contains("outside the workspace"), "{msg}");
    env.invalid("read", json!({ "workspace": "ws", "path": "outdir/secret.txt" })).await;
    env.invalid("list", json!({ "workspace": "ws", "path": "outdir" })).await;
    env.invalid("search", json!({ "workspace": "ws", "pattern": "secret", "path": "outdir" })).await;

    let msg = env.invalid("write", json!({ "workspace": "ws", "path": "link", "content": "pwned" })).await;
    assert!(msg.contains("symlink"), "{msg}");
    env.invalid("write", json!({ "workspace": "ws", "path": "outdir/new.txt", "content": "pwned" })).await;
    env.invalid("write", json!({ "workspace": "ws", "path": "outdir/deeper/new.txt", "content": "pwned" })).await;
    env.invalid("edit", json!({ "workspace": "ws", "path": "link", "old": "secret", "new": "pwned" })).await;
    env.invalid("write", json!({ "workspace": "ws", "path": "inner", "content": "x" })).await;
    assert_eq!(fs::read_to_string(outside.join("secret.txt")).unwrap(), "secret\n");
    assert!(!outside.join("new.txt").exists() && !outside.join("deeper").exists());

    // Search and list never follow links, so the secret is not found through them.
    assert!(found(env.ok("search", json!({ "workspace": "ws", "pattern": "secret" })).await).is_empty());

    for ws in [outside.to_str().unwrap(), "../outside", "/", "ws/a.txt", "missing"] {
        env.invalid("read", json!({ "workspace": ws, "path": "secret.txt" })).await;
    }
    // A workspace may be relative to root or absolute, and a link inside it may point inside it.
    let reply = read_reply(env.ok("read", json!({ "workspace": env.ws, "path": "inner" })).await);
    assert_eq!(reply.content, "a\n");
    assert_eq!(read_reply(env.ok("read", json!({ "workspace": "ws", "path": "./sub/b.txt" })).await).content, "b\n");
}

#[tokio::test]
async fn read_returns_windows_and_truncates() {
    let env = Env::new();
    let ten: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let many: String = (1..=12_000).map(|i| format!("{i}\n")).collect();
    let wide: String = (0..3000).map(|i| format!("{i:0>199}\n")).collect();
    Env::put(&env.ws, &[("ten.txt", &ten), ("many.txt", &many), ("wide.txt", &wide), ("last.txt", "a\nb")]);
    fs::write(env.ws.join("long.txt"), "é".repeat(200_000)).unwrap();
    fs::write(env.ws.join("bin.dat"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
    fs::write(env.ws.join("latin1.txt"), [b'c', b'a', b'f', 0xe9, b'\n']).unwrap();

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "ten.txt", "offset": 3, "limit": 2 })).await);
    assert_eq!(r.content, "line 3\nline 4\n");
    assert_eq!((r.first_line, r.lines, r.total_lines, r.truncated), (3, 2, 10, true));

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "ten.txt", "offset": 9 })).await);
    assert_eq!(r.content, "line 9\nline 10\n");
    assert_eq!((r.first_line, r.lines, r.total_lines, r.truncated), (9, 2, 10, false));

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "ten.txt", "offset": 50 })).await);
    assert_eq!((r.content.as_str(), r.lines, r.total_lines, r.truncated), ("", 0, 10, false));

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "last.txt" })).await);
    assert_eq!((r.content.as_str(), r.lines, r.total_lines, r.truncated), ("a\nb", 2, 2, false));

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "many.txt" })).await);
    assert_eq!((r.lines, r.total_lines, r.truncated), (2000, 12_000, true));
    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "many.txt", "limit": 1_000_000 })).await);
    assert_eq!((r.lines, r.truncated), (10_000, true));
    assert!(r.content.ends_with("10000\n"));

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "wide.txt" })).await);
    assert!(r.content.len() <= 256 * 1024 && r.content.ends_with('\n'));
    assert_eq!(r.content.lines().count() as u64, r.lines);
    assert!(r.lines < 3000 && r.truncated);

    let r = read_reply(env.ok("read", json!({ "workspace": "ws", "path": "long.txt" })).await);
    assert_eq!((r.lines, r.total_lines, r.truncated), (1, 1, true));
    assert!(r.content.len() <= 256 * 1024 && r.content.chars().all(|c| c == 'é'));

    for path in ["bin.dat", "latin1.txt"] {
        let msg = env.invalid("read", json!({ "workspace": "ws", "path": path })).await;
        assert!(msg.contains("binary file"), "{msg}");
    }
    let msg = env.invalid("read", json!({ "workspace": "ws", "path": "nope.txt" })).await;
    assert_eq!(msg, "nope.txt does not exist");
    env.invalid("read", json!({ "workspace": "ws", "path": "" })).await;
}

#[tokio::test]
async fn write_creates_parents_and_replaces_atomically() {
    let env = Env::new();
    let reply = env.ok("write", json!({ "workspace": "ws", "path": "a/b/c.txt", "content": "one\n" })).await;
    assert_eq!(reply, json!({ "bytes": 4, "created": true }));
    assert_eq!(fs::read_to_string(env.ws.join("a/b/c.txt")).unwrap(), "one\n");

    // Replacing renames a new file into place: a hard link to the old one keeps the old content.
    let file = env.ws.join("a/b/c.txt");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o750)).unwrap();
    fs::hard_link(&file, env.ws.join("a/b/old")).unwrap();
    let reply = env.ok("write", json!({ "workspace": "ws", "path": "a/b/c.txt", "content": "two\n" })).await;
    assert_eq!(reply, json!({ "bytes": 4, "created": false }));
    assert_eq!(fs::read_to_string(&file).unwrap(), "two\n");
    assert_eq!(fs::read_to_string(env.ws.join("a/b/old")).unwrap(), "one\n");
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o750);
    let mut names: Vec<_> = fs::read_dir(env.ws.join("a/b")).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, ["c.txt", "old"], "no temporary files are left behind");

    let big = "x".repeat(10 * 1024 * 1024 + 1);
    env.invalid("write", json!({ "workspace": "ws", "path": "big.txt", "content": big })).await;
    env.invalid("write", json!({ "workspace": "ws", "path": "a/b", "content": "x" })).await;
    env.invalid("write", json!({ "workspace": "ws", "path": "a/b/c.txt/d", "content": "x" })).await;
    assert!(!env.ws.join("big.txt").exists());
}

#[tokio::test]
async fn edit_needs_one_match_unless_replace_all() {
    let env = Env::new();
    Env::put(&env.ws, &[("f.rs", "let a = 1;\nlet b = 1;\nlet c = 1;\n")]);
    fs::set_permissions(env.ws.join("f.rs"), fs::Permissions::from_mode(0o700)).unwrap();
    let edit = |old: &str, new: &str, all: bool| json!({ "workspace": "ws", "path": "f.rs", "old": old, "new": new, "replace_all": all });

    assert_eq!(env.ok("edit", edit("let b", "let bee", false)).await, json!({ "replacements": 1 }));
    assert_eq!(fs::read_to_string(env.ws.join("f.rs")).unwrap(), "let a = 1;\nlet bee = 1;\nlet c = 1;\n");
    assert_eq!(fs::metadata(env.ws.join("f.rs")).unwrap().permissions().mode() & 0o777, 0o700);

    let msg = env.invalid("edit", edit("let z", "let y", false)).await;
    assert_eq!(msg, "old text not found in f.rs");
    let msg = env.invalid("edit", edit("= 1;", "= 2;", false)).await;
    assert!(msg.contains("3 times") && msg.contains("replace_all"), "{msg}");
    env.invalid("edit", edit("", "x", false)).await;
    env.invalid("edit", edit("let a", "let a", false)).await;
    let msg = env.invalid("edit", json!({ "workspace": "ws", "path": "g.rs", "old": "a", "new": "b" })).await;
    assert!(msg.contains("does not exist"), "{msg}");

    assert_eq!(env.ok("edit", edit("= 1;", "= 2;", true)).await, json!({ "replacements": 3 }));
    assert_eq!(fs::read_to_string(env.ws.join("f.rs")).unwrap(), "let a = 2;\nlet bee = 2;\nlet c = 2;\n");
}

#[tokio::test]
async fn writes_and_edits_report_the_syntax_errors_they_introduce_and_reject_mode_keeps_the_file() {
    let env = Env::new();
    let good = "fn main() {\n    let x = (1 + 2);\n}\n";
    Env::put(&env.ws, &[("src/main.rs", good), ("app.py", "def f(x):\n    return x\n"), ("notes.txt", "(")]);
    let edit = |path: &str, old: &str, new: &str, syntax: &str| json!({ "workspace": "ws", "path": path, "old": old, "new": new, "syntax": syntax });
    let reply = |v: Value| serde_json::from_value::<EditResponse>(v).unwrap();

    // Reported, and the edit is made.
    let r = reply(env.ok("edit", edit("src/main.rs", "(1 + 2);", "(1 + 2;", "report")).await);
    let syntax = r.syntax.expect("a new error is reported");
    assert!(!syntax.rejected);
    assert_eq!((syntax.errors[0].line, syntax.errors[0].message.as_str()), (2, "missing `)`"));
    assert!(syntax.errors[0].context.contains("2 |     let x = (1 + 2;"), "{}", syntax.errors[0].context);
    assert!(fs::read_to_string(env.ws.join("src/main.rs")).unwrap().contains("(1 + 2;"));

    // The file is broken already: an unrelated edit reports nothing.
    let r = reply(env.ok("edit", edit("src/main.rs", "fn main", "fn start", "report")).await);
    assert_eq!(r.syntax, None);

    // Rejected: the error comes back and the file stays as it was.
    let r = reply(env.ok("edit", edit("app.py", "def f(x):", "def f(x:", "reject")).await);
    assert!(r.syntax.as_ref().is_some_and(|s| s.rejected && s.errors[0].line == 1), "{r:?}");
    assert_eq!(fs::read_to_string(env.ws.join("app.py")).unwrap(), "def f(x):\n    return x\n");
    let w = json!({ "workspace": "ws", "path": "app.py", "content": "def f(:\n", "syntax": "reject" });
    let r: WriteResponse = serde_json::from_value(env.ok("write", w).await).unwrap();
    assert!(r.syntax.is_some_and(|s| s.rejected) && !r.created);
    assert_eq!(fs::read_to_string(env.ws.join("app.py")).unwrap(), "def f(x):\n    return x\n");
    let (r, changed) = env.fs.handle_reporting("edit", edit("app.py", "def f(x):", "def f(x:", "reject")).await;
    assert!(r.is_ok() && changed.is_none(), "a rejected change reports no changed file");
    let w = json!({ "workspace": "ws", "path": "new.ts", "content": "const x = f(;\n", "syntax": "reject" });
    let r: WriteResponse = serde_json::from_value(env.ok("write", w).await).unwrap();
    assert!(r.syntax.is_some_and(|s| s.rejected) && r.created);
    assert!(!env.ws.join("new.ts").exists());

    // Languages the service cannot parse, and requests that do not ask, are not checked.
    let r = reply(env.ok("edit", edit("notes.txt", "(", "((", "reject")).await);
    assert_eq!(r.syntax, None);
    assert_eq!(fs::read_to_string(env.ws.join("notes.txt")).unwrap(), "((");
    let off = json!({ "workspace": "ws", "path": "app.py", "old": "return x", "new": "return (x" });
    assert_eq!(env.ok("edit", off).await, json!({ "replacements": 1 }));
}

/// The process's peak resident memory, in bytes.
fn peak_rss() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let kb = status.lines().find_map(|l| l.strip_prefix("VmHWM:")).unwrap();
    kb.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap() * 1024
}

#[tokio::test]
async fn an_edit_over_the_size_limit_is_refused_before_it_is_built() {
    let env = Env::new();
    // Each of a million bytes becomes a thousand: a 1 GB result.
    fs::write(env.ws.join("big.txt"), "a".repeat(1_000_000)).unwrap();
    let before = peak_rss();
    let edit =
        json!({ "workspace": "ws", "path": "big.txt", "old": "a", "new": "b".repeat(1000), "replace_all": true });
    let msg = env.invalid("edit", edit).await;
    assert!(msg.contains("would be 1000000000 bytes"), "{msg}");
    let grew = peak_rss().saturating_sub(before);
    assert!(grew < 256 << 20, "refusing the edit took {} MB", grew >> 20);
    assert_eq!(fs::metadata(env.ws.join("big.txt")).unwrap().len(), 1_000_000);
}

#[tokio::test]
async fn list_and_search_follow_ignore_files_without_a_repository() {
    let env = Env::new();
    Env::put(
        &env.ws,
        &[
            (".gitignore", "target/\n*.log\n"),
            (".ignore", "private/\n"),
            (".env", "TOKEN=fn\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/deep/lib.rs", "// nothing\npub fn Lib() {}\n"),
            ("src/notes.txt", "fn in text\n"),
            ("src/trace.log", "fn trace\n"),
            ("target/debug/out.rs", "fn built() {}\n"),
            ("app.log", "fn log\n"),
            ("private/key.rs", "fn key() {}\n"),
        ],
    );
    fs::write(env.ws.join("src/blob.rs"), b"fn \0 binary").unwrap();
    assert!(!env.ws.join(".git").exists());

    let all = listed(env.ok("list", json!({ "workspace": "ws", "depth": 5 })).await);
    assert_eq!(
        all,
        [
            ".env",
            ".gitignore",
            ".ignore",
            "src",
            "src/blob.rs",
            "src/deep",
            "src/deep/lib.rs",
            "src/main.rs",
            "src/notes.txt"
        ]
    );
    let top = listed(env.ok("list", json!({ "workspace": "ws", "depth": 1 })).await);
    assert_eq!(top, [".env", ".gitignore", ".ignore", "src"]);
    let src = listed(env.ok("list", json!({ "workspace": "ws", "path": "src/" })).await);
    assert_eq!(src, ["src/blob.rs", "src/deep", "src/deep/lib.rs", "src/main.rs", "src/notes.txt"]);
    // Named on purpose, an ignored directory is listed.
    let target = listed(env.ok("list", json!({ "workspace": "ws", "path": "target" })).await);
    assert_eq!(target, ["target/debug", "target/debug/out.rs"]);

    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": r"\bfn\b" })).await);
    assert_eq!(
        hits,
        [(".env".into(), 1), ("src/deep/lib.rs".into(), 2), ("src/main.rs".into(), 1), ("src/notes.txt".into(), 1)]
    );
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "fn", "glob": "*.rs" })).await);
    assert_eq!(hits, [("src/deep/lib.rs".into(), 2), ("src/main.rs".into(), 1)]);
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "LIB", "case_insensitive": true })).await);
    assert_eq!(hits, [("src/deep/lib.rs".into(), 2)]);
    assert!(found(env.ok("search", json!({ "workspace": "ws", "pattern": "LIB" })).await).is_empty());
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "fn", "path": "src/main.rs" })).await);
    assert_eq!(hits, [("src/main.rs".into(), 1)]);
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "fn", "glob": "lib.rs" })).await);
    assert_eq!(hits, [("src/deep/lib.rs".into(), 2)]);
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "fn", "path": "src" })).await);
    assert_eq!(hits.len(), 3, "the workspace's ignore rules apply to a subdirectory: {hits:?}");

    let reply: SearchResponse =
        serde_json::from_value(env.ok("search", json!({ "workspace": "ws", "pattern": "fn", "max_results": 2 })).await)
            .unwrap();
    assert_eq!((reply.matches.len(), reply.truncated), (2, true));
    env.invalid("search", json!({ "workspace": "ws", "pattern": "(" })).await;
}

#[tokio::test]
async fn list_and_search_skip_git_and_molt() {
    let env = Env::new();
    Env::put(
        &env.ws,
        &[
            (".git/config", "needle\n"),
            (".git/HEAD", "needle\n"),
            (".molt/state", "needle\n"),
            ("a/.git", "needle\n"),
            ("a/b.txt", "x\nneedle and a very long line\n"),
        ],
    );
    let all = listed(env.ok("list", json!({ "workspace": "ws", "depth": 10 })).await);
    assert_eq!(all, ["a", "a/b.txt"]);
    let hits = found(env.ok("search", json!({ "workspace": "ws", "pattern": "needle" })).await);
    assert_eq!(hits, [("a/b.txt".into(), 2)]);

    Env::put(&env.ws, &[("long.txt", &format!("needle{}\n", "z".repeat(2000)))]);
    let reply: SearchResponse = serde_json::from_value(
        env.ok("search", json!({ "workspace": "ws", "pattern": "needle", "glob": "long*" })).await,
    )
    .unwrap();
    assert_eq!(reply.matches[0].text.chars().count(), 500);
}

/// A workspace with tracked files, an ignored dependency directory, an
/// ignored file and a git directory.
fn project(env: &Env) {
    Env::put(
        &env.ws,
        &[
            (".gitignore", "node_modules/\n*.log\n"),
            ("README.md", "# Project\n\nIntro.\n"),
            ("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
            ("run.sh", "#!/bin/sh\necho run\n"),
            ("node_modules/pkg/index.js", "module.exports = 1;\n"),
            ("debug.log", "log\n"),
            (".git/HEAD", "ref: refs/heads/main\n"),
        ],
    );
    fs::set_permissions(env.ws.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn fork_copies_tracked_files_and_links_ignored_ones() {
    let env = Env::new();
    project(&env);
    let reply: ForkResponse = serde_json::from_value(env.ok("fork", json!({ "workspace": "ws" })).await).unwrap();
    let fork = PathBuf::from(&reply.fork);
    assert_eq!(fork.parent().unwrap(), env.scratch);
    assert!(fork.file_name().unwrap().to_str().unwrap().starts_with("fork-"));
    assert_eq!(reply.files, 4, ".gitignore, README.md, run.sh, src/main.rs");

    let main = fs::symlink_metadata(fork.join("src/main.rs")).unwrap();
    assert!(main.is_file());
    assert_eq!(
        fs::read_to_string(fork.join("src/main.rs")).unwrap(),
        fs::read_to_string(env.ws.join("src/main.rs")).unwrap()
    );
    assert_eq!(fs::metadata(fork.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    for ignored in ["node_modules", "debug.log"] {
        assert!(fs::symlink_metadata(fork.join(ignored)).unwrap().file_type().is_symlink(), "{ignored}");
        assert_eq!(fs::read_link(fork.join(ignored)).unwrap(), env.ws.join(ignored));
    }
    assert_eq!(fs::read_to_string(fork.join("node_modules/pkg/index.js")).unwrap(), "module.exports = 1;\n");
    assert!(!fork.join(".git").exists() && fs::symlink_metadata(fork.join(".git")).is_err());

    let meta: Value = serde_json::from_slice(
        &fs::read(env.scratch.join(format!("{}.json", fork.file_name().unwrap().to_str().unwrap()))).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["base"], json!(env.ws));
    assert_eq!(meta["files"].as_object().unwrap().len(), 4);

    // The fork is a workspace of its own, and an untouched fork has no changes.
    let r = read_reply(env.ok("read", json!({ "workspace": fork, "path": "README.md" })).await);
    assert_eq!(r.content, "# Project\n\nIntro.\n");
    let diff: DiffResponse = serde_json::from_value(env.ok("diff", json!({ "fork": fork })).await).unwrap();
    assert!(diff.changes.is_empty() && diff.patch.is_empty(), "{diff:?}");
}

#[tokio::test]
async fn diff_and_merge_carry_changes_back() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    let in_fork = |path: &str| fork.join(path);

    env.ok("edit", json!({ "workspace": fork, "path": "src/main.rs", "old": "\"hi\"", "new": "\"hello\"" })).await;
    env.ok("write", json!({ "workspace": fork, "path": "notes/new.txt", "content": "new file\n" })).await;
    fs::remove_file(in_fork("run.sh")).unwrap();
    fs::write(in_fork("out.log"), "ignored output\n").unwrap();
    fs::write(in_fork("image.bin"), [0u8, 1, 2, 3]).unwrap();

    let diff: DiffResponse = serde_json::from_value(env.ok("diff", json!({ "fork": fork })).await).unwrap();
    let changes: Vec<_> = diff.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(
        changes,
        [
            ("image.bin", ChangeKind::Added),
            ("notes/new.txt", ChangeKind::Added),
            ("run.sh", ChangeKind::Deleted),
            ("src/main.rs", ChangeKind::Modified)
        ]
    );
    let patch = &diff.patch;
    assert!(!diff.truncated);
    assert!(patch.contains("Binary file b/image.bin added\n"), "{patch}");
    assert!(patch.contains("--- /dev/null\n+++ b/notes/new.txt\n@@ -0,0 +1 @@\n+new file\n"), "{patch}");
    assert!(patch.contains("--- a/run.sh\n+++ /dev/null\n"), "{patch}");
    assert!(patch.contains("--- a/src/main.rs\n+++ b/src/main.rs\n"), "{patch}");
    assert!(patch.contains("-    println!(\"hi\");\n+    println!(\"hello\");\n"), "{patch}");

    let reply = env.ok("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(reply["changes"], serde_json::to_value(&diff.changes).unwrap());
    assert!(fs::read_to_string(env.ws.join("src/main.rs")).unwrap().contains("\"hello\""));
    assert_eq!(fs::read_to_string(env.ws.join("notes/new.txt")).unwrap(), "new file\n");
    assert_eq!(fs::read(env.ws.join("image.bin")).unwrap(), [0u8, 1, 2, 3]);
    assert!(!env.ws.join("run.sh").exists());
    assert!(!env.ws.join("out.log").exists());
    assert!(fs::symlink_metadata(env.ws.join("node_modules")).unwrap().is_dir());
    assert!(env.ws.join("node_modules/pkg/index.js").is_file());
    assert!(env.ws.join(".git/HEAD").is_file());
    assert!(!fork.exists(), "drop: true removes the fork");
    assert!(!fork.with_extension("json").exists(), "drop removes the fork metadata");
    assert_eq!(fs::read_dir(&env.scratch).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(), [".molt"]);
    assert!(env
        .scratch
        .join(".molt/applies")
        .join(reply["operation"].as_str().unwrap())
        .join("journal.json")
        .is_file());
}

#[tokio::test]
async fn part_of_a_fork_can_be_diffed_narrowed_into_a_fork_of_its_own_and_merged_alone() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    env.ok("edit", json!({ "workspace": fork, "path": "src/main.rs", "old": "\"hi\"", "new": "\"hello\"" })).await;
    env.ok("write", json!({ "workspace": fork, "path": "notes/new.txt", "content": "new file\n" })).await;
    fs::remove_file(fork.join("run.sh")).unwrap();
    let paths = |d: &DiffResponse| d.changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>();

    let part: DiffResponse =
        serde_json::from_value(env.ok("diff", json!({ "fork": fork, "paths": ["notes/new.txt", "run.sh"] })).await)
            .unwrap();
    assert_eq!(paths(&part), ["notes/new.txt", "run.sh"]);
    assert!(!part.patch.contains("src/main.rs"), "{}", part.patch);
    let unknown = env.invalid("diff", json!({ "fork": fork, "paths": ["README.md"] })).await;
    assert!(unknown.contains("not among the fork's changes: README.md"), "{unknown}");

    // The narrowed fork has the chosen changes and the original's state of everything else.
    let narrowed: ForkResponse =
        serde_json::from_value(env.ok("narrow", json!({ "fork": fork, "paths": ["notes/new.txt", "run.sh"] })).await)
            .unwrap();
    let narrowed = PathBuf::from(narrowed.fork);
    assert_ne!(narrowed, fork);
    assert_eq!(fs::read_to_string(narrowed.join("notes/new.txt")).unwrap(), "new file\n");
    assert!(!narrowed.join("run.sh").exists());
    assert!(fs::read_to_string(narrowed.join("src/main.rs")).unwrap().contains("\"hi\""));
    assert_eq!(paths(&diff_of(&env, &narrowed).await), ["notes/new.txt", "run.sh"]);
    assert!(env.invalid("narrow", json!({ "fork": fork, "paths": ["README.md"] })).await.contains("README.md"));
    env.ok("drop", json!({ "fork": narrowed })).await;

    // Nothing is written when one path is not a change; the chosen ones merge alone.
    assert!(env.invalid("merge", json!({ "fork": fork, "paths": ["src/main.rs", "nope"] })).await.contains("nope"));
    assert!(fs::read_to_string(env.ws.join("src/main.rs")).unwrap().contains("\"hi\""));
    let reply: MergeResponse =
        serde_json::from_value(env.ok("merge", json!({ "fork": fork, "paths": ["src/main.rs"] })).await).unwrap();
    assert_eq!(reply.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["src/main.rs"]);
    assert!(fs::read_to_string(env.ws.join("src/main.rs")).unwrap().contains("\"hello\""));
    assert!(!env.ws.join("notes/new.txt").exists());
    assert!(env.ws.join("run.sh").exists());
}

#[tokio::test]
async fn build_artifacts_that_commands_leave_in_a_fork_are_reported_and_never_merged() {
    let env = Env::new();
    Env::put(&env.ws, &[("src/lib.rs", "pub fn f() {}\n"), ("vendor/prebuilt.so", "old\n")]);
    let fork = env.fork().await;
    let tag = "Signature: 8a477f597d28d172789f06886806bc55\n";
    // What a check run leaves: a target directory of its own, bytecode, a
    // tagged cache and a compiled program.
    Env::put(
        &fork,
        &[
            ("target-done-check/CACHEDIR.TAG", tag),
            ("target-done-check/debug/libcalc.rlib", "x"),
            ("__pycache__/calc.cpython-312.pyc", "x"),
            ("cache/CACHEDIR.TAG", tag),
            ("cache/entry.txt", "x"),
            ("scripts/run.sh", "#!/bin/sh\necho run\n"),
            ("src/target/mod.rs", "pub mod x;\n"),
            ("vendor/prebuilt.so", "new\n"),
        ],
    );
    fs::create_dir(fork.join("bin")).unwrap();
    fs::write(fork.join("bin/calc"), b"\x7fELF\0\0\0").unwrap();
    chmod(&fork.join("bin/calc"), 0o755);
    chmod(&fork.join("scripts/run.sh"), 0o755);
    // A file the attempt writes itself is its work, whatever its name.
    env.ok("write", json!({ "workspace": fork, "path": "plugins/ext.so", "content": "asked for\n" })).await;
    env.ok("edit", json!({ "workspace": fork, "path": "src/lib.rs", "old": "f()", "new": "g()" })).await;

    let diff = diff_of(&env, &fork).await;
    let changes: Vec<_> = diff.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(
        changes,
        [
            ("plugins/ext.so", ChangeKind::Added),
            ("scripts/run.sh", ChangeKind::Added),
            ("src/lib.rs", ChangeKind::Modified),
            ("src/target/mod.rs", ChangeKind::Added),
            ("vendor/prebuilt.so", ChangeKind::Modified),
        ]
    );
    let left_out = diff.left_out.clone().expect("artifacts are reported");
    assert_eq!(left_out.count, 6, "{left_out:?}");
    assert_eq!(
        left_out.paths,
        [
            "__pycache__/calc.cpython-312.pyc",
            "bin/calc",
            "cache/CACHEDIR.TAG",
            "cache/entry.txt",
            "target-done-check/CACHEDIR.TAG"
        ]
    );
    assert!(!diff.patch.contains("target-done-check") && !diff.patch.contains("bin/calc"), "{}", diff.patch);

    let merged: MergeResponse =
        serde_json::from_value(env.ok("merge", json!({ "fork": fork, "drop": true })).await).unwrap();
    assert_eq!(merged.changes, diff.changes);
    assert_eq!(merged.left_out, Some(left_out));
    for gone in ["target-done-check", "__pycache__", "cache", "bin"] {
        assert!(!env.ws.join(gone).exists(), "{gone} reached the workspace");
    }
    assert_eq!(fs::read_to_string(env.ws.join("plugins/ext.so")).unwrap(), "asked for\n");
    assert_eq!(fs::read_to_string(env.ws.join("src/lib.rs")).unwrap(), "pub fn g() {}\n");
    assert_eq!(fs::read_to_string(env.ws.join("vendor/prebuilt.so")).unwrap(), "new\n");
    assert!(env.ws.join("src/target/mod.rs").is_file() && env.ws.join("scripts/run.sh").is_file());
    assert_eq!(fs::read_dir(&env.scratch).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(), [".molt"]);
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

async fn diff_of(env: &Env, fork: &Path) -> DiffResponse {
    serde_json::from_value(env.ok("diff", json!({ "fork": fork })).await).unwrap()
}

#[tokio::test]
async fn making_a_script_executable_in_a_fork_is_a_change_that_merges() {
    let env = Env::new();
    Env::put(&env.ws, &[("check.sh", "#!/bin/sh\nexit 0\n"), ("both.sh", "#!/bin/sh\nexit 1\n")]);
    chmod(&env.ws.join("check.sh"), 0o644);
    chmod(&env.ws.join("both.sh"), 0o640);
    let fork = env.fork().await;
    chmod(&fork.join("check.sh"), 0o755);
    fs::write(fork.join("both.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    chmod(&fork.join("both.sh"), 0o750);

    let diff = diff_of(&env, &fork).await;
    let changes: Vec<_> = diff.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(changes, [("both.sh", ChangeKind::Modified), ("check.sh", ChangeKind::Modified)]);
    assert!(diff.patch.contains("# check.sh: made executable\n"), "{}", diff.patch);
    assert!(!diff.patch.contains("+++ b/check.sh"), "the content did not change: {}", diff.patch);
    assert!(diff.patch.contains("# both.sh: made executable\n--- a/both.sh\n+++ b/both.sh\n"), "{}", diff.patch);

    env.ok("merge", json!({ "fork": fork, "drop": true })).await;
    // Read access gains execute, as chmod +x would give it.
    assert_eq!(mode(&env.ws.join("check.sh")), 0o755);
    assert_eq!(mode(&env.ws.join("both.sh")), 0o750);
    assert_eq!(fs::read_to_string(env.ws.join("both.sh")).unwrap(), "#!/bin/sh\nexit 0\n");
    for script in ["check.sh", "both.sh"] {
        let ran = std::process::Command::new(env.ws.join(script)).status().unwrap();
        assert!(ran.success(), "{script} runs after the merge, as it did in the fork");
    }
}

#[tokio::test]
async fn removing_the_execute_bit_merges_and_other_bits_stay_the_originals() {
    let env = Env::new();
    project(&env);
    Env::put(&env.ws, &[("private.txt", "secret\n")]);
    chmod(&env.ws.join("private.txt"), 0o600);
    let fork = env.fork().await;
    chmod(&fork.join("run.sh"), 0o644);
    // Only the execute bit is carried back: the fork loosening a file's
    // read bits while editing it does not reach the original.
    fs::write(fork.join("private.txt"), "still secret\n").unwrap();
    chmod(&fork.join("private.txt"), 0o644);

    let diff = diff_of(&env, &fork).await;
    assert!(diff.patch.contains("# run.sh: no longer executable\n"), "{}", diff.patch);
    assert!(!diff.patch.contains("private.txt: "), "{}", diff.patch);
    env.ok("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(mode(&env.ws.join("run.sh")), 0o644);
    assert_eq!(mode(&env.ws.join("private.txt")), 0o600);
    assert_eq!(fs::read_to_string(env.ws.join("private.txt")).unwrap(), "still secret\n");
}

#[tokio::test]
async fn a_mode_change_in_the_original_since_the_fork_is_a_conflict() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    fs::write(fork.join("run.sh"), "#!/bin/sh\necho fork\n").unwrap();
    chmod(&env.ws.join("run.sh"), 0o644);

    let diff = diff_of(&env, &fork).await;
    assert!(diff.patch.contains("# run.sh: the original changed since the fork"), "{}", diff.patch);
    let err = env.err("merge", json!({ "fork": fork })).await;
    assert!(err.message.starts_with("conflict:") && err.message.contains("run.sh"), "{}", err.message);
    assert_eq!(fs::read_to_string(env.ws.join("run.sh")).unwrap(), "#!/bin/sh\necho run\n");
    assert_eq!(mode(&env.ws.join("run.sh")), 0o644);

    // The same mode on both sides is not a conflict.
    let fork = env.fork().await;
    chmod(&fork.join("README.md"), 0o755);
    chmod(&env.ws.join("README.md"), 0o755);
    assert!(diff_of(&env, &fork).await.patch.contains("# README.md: the original already has this change"));
    env.ok("merge", json!({ "fork": fork, "drop": true })).await;
}

#[tokio::test]
async fn a_new_file_never_brings_special_bits_from_the_fork() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    Env::put(&fork, &[("tool.sh", "#!/bin/sh\n"), ("shared", "")]);
    chmod(&fork.join("tool.sh"), 0o4755);
    fs::create_dir(fork.join("dir")).unwrap();
    symlink("../run.sh", fork.join("dir/link")).unwrap();
    chmod(&fork.join("shared"), 0o1666);
    env.ok("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(mode(&env.ws.join("tool.sh")), 0o755);
    assert_eq!(mode(&env.ws.join("shared")), 0o666);
    assert_eq!(fs::read_link(env.ws.join("dir/link")).unwrap(), Path::new("../run.sh"));
}

#[tokio::test]
async fn a_fork_made_before_modes_were_recorded_merges_as_it_did_then() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    // Rewrite the metadata as the previous version wrote it: no version,
    // content hashes without the executable mark.
    let meta_path = env.scratch.join(format!("{}.json", fork.file_name().unwrap().to_str().unwrap()));
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    let run = meta["files"]["run.sh"].as_str().unwrap().to_owned();
    assert!(run.ends_with("+x"), "{run}");
    meta["files"]["run.sh"] = json!(run.trim_end_matches("+x"));
    meta.as_object_mut().unwrap().remove("version");
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    // An executable file is not reported as changed, and a mode change is
    // not seen, as before.
    assert!(diff_of(&env, &fork).await.changes.is_empty());
    chmod(&fork.join("README.md"), 0o755);
    assert!(diff_of(&env, &fork).await.changes.is_empty());
    fs::write(fork.join("run.sh"), "#!/bin/sh\necho fork\n").unwrap();
    chmod(&fork.join("run.sh"), 0o644);
    let diff = diff_of(&env, &fork).await;
    assert_eq!(diff.changes.len(), 1, "{diff:?}");
    env.ok("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(fs::read_to_string(env.ws.join("run.sh")).unwrap(), "#!/bin/sh\necho fork\n");
    assert_eq!(mode(&env.ws.join("run.sh")), 0o755, "the original keeps its mode");
}

#[tokio::test]
async fn a_fork_from_a_newer_version_is_refused() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    let meta_path = env.scratch.join(format!("{}.json", fork.file_name().unwrap().to_str().unwrap()));
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["version"] = json!(99);
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    for method in ["diff", "merge"] {
        let err = env.err(method, json!({ "fork": fork })).await;
        assert!(err.message.contains("newer Molt"), "{}", err.message);
    }
}

#[tokio::test]
async fn changes_outside_forks_are_reported_for_the_project_model() {
    let env = Env::new();
    project(&env);
    let reported = |(reply, changed): (Result<Value, RemoteError>, Option<FilesChanged>)| {
        reply.unwrap();
        changed
    };
    let ws = env.ws.to_string_lossy().into_owned();

    let wrote = env.fs.handle_reporting("write", json!({ "workspace": "ws", "path": "./src/new.rs", "content": "" }));
    assert_eq!(reported(wrote.await), Some(FilesChanged { workspace: ws.clone(), paths: vec!["src/new.rs".into()] }));
    let edit = json!({ "workspace": "ws", "path": "src/main.rs", "old": "hi", "new": "hey" });
    assert_eq!(
        reported(env.fs.handle_reporting("edit", edit).await),
        Some(FilesChanged { workspace: ws.clone(), paths: vec!["src/main.rs".into()] })
    );
    // Reads change nothing, and failures report nothing.
    assert_eq!(
        reported(env.fs.handle_reporting("read", json!({ "workspace": "ws", "path": "README.md" })).await),
        None
    );
    let missing = json!({ "workspace": "ws", "path": "nope.rs", "old": "a", "new": "b" });
    assert_eq!(env.fs.handle_reporting("edit", missing).await.1, None);

    // A fork is private to its attempt: its writes are not reported, its merge is.
    let fork = env.fork().await;
    let in_fork = json!({ "workspace": fork, "path": "src/lib.rs", "content": "pub fn f() {}\n" });
    assert_eq!(reported(env.fs.handle_reporting("write", in_fork).await), None);
    fs::remove_file(fork.join("run.sh")).unwrap();
    let merged = env.fs.handle_reporting("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(
        reported(merged),
        Some(FilesChanged { workspace: ws, paths: vec!["run.sh".into(), "src/lib.rs".into()] })
    );
    // A merge with nothing to carry back reports nothing.
    let fork = env.fork().await;
    assert_eq!(reported(env.fs.handle_reporting("merge", json!({ "fork": fork, "drop": true })).await), None);
}

#[tokio::test]
async fn merge_refuses_when_the_original_changed_and_writes_nothing() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    fs::write(fork.join("README.md"), "# Fork\n").unwrap();
    fs::write(fork.join("added.txt"), "from the fork\n").unwrap();
    fs::write(fork.join("same.txt"), "same\n").unwrap();
    fs::write(env.ws.join("README.md"), "# Original moved on\n").unwrap();
    fs::write(env.ws.join("same.txt"), "same\n").unwrap();

    let diff: DiffResponse = serde_json::from_value(env.ok("diff", json!({ "fork": fork })).await).unwrap();
    assert!(diff.patch.contains("# README.md: the original changed since the fork"), "{}", diff.patch);

    let err = env.err("merge", json!({ "fork": fork, "drop": true })).await;
    assert_eq!(err.code, ErrorCode::Failed);
    assert!(err.message.starts_with("conflict:"), "{}", err.message);
    assert!(err.message.contains("README.md") && !err.message.contains("same.txt"), "{}", err.message);
    assert_eq!(fs::read_to_string(env.ws.join("README.md")).unwrap(), "# Original moved on\n");
    assert!(!env.ws.join("added.txt").exists(), "nothing is written on conflict");
    assert!(fork.is_dir(), "the fork survives a failed merge");

    // A file added on both sides with the same content is not a conflict.
    fs::write(fork.join("README.md"), "# Original moved on\n").unwrap();
    env.ok("merge", json!({ "fork": fork })).await;
    assert_eq!(fs::read_to_string(env.ws.join("added.txt")).unwrap(), "from the fork\n");
    assert!(fork.is_dir());
}

#[tokio::test]
async fn drop_removes_only_forks() {
    let env = Env::new();
    project(&env);
    let fork = env.fork().await;
    assert_eq!(env.ok("drop", json!({ "fork": fork })).await, json!({ "dropped": true }));
    assert!(!fork.exists());
    assert!(fs::read_dir(&env.scratch).unwrap().next().is_none());
    assert_eq!(env.ok("drop", json!({ "fork": fork })).await, json!({ "dropped": false }));
    // The original, including what the fork linked to, is untouched.
    assert!(env.ws.join("node_modules/pkg/index.js").is_file());
    assert!(env.ws.join("debug.log").is_file());

    let lookalike = env.scratch.join("fork-0123456789ab");
    fs::create_dir(&lookalike).unwrap();
    let other = env.scratch.join("other");
    fs::create_dir(&other).unwrap();
    for not_a_fork in [env.ws.to_str().unwrap(), "ws", lookalike.to_str().unwrap(), other.to_str().unwrap(), "..", "/"]
    {
        let msg = env.invalid("diff", json!({ "fork": not_a_fork })).await;
        assert!(msg.contains("not a fork"), "{msg}");
        env.invalid("merge", json!({ "fork": not_a_fork })).await;
        env.invalid("drop", json!({ "fork": not_a_fork })).await;
    }
    assert!(env.ws.join("README.md").is_file() && lookalike.is_dir() && other.is_dir());
}

#[tokio::test]
async fn special_files_never_block() {
    let env = Env::new();
    Env::put(&env.ws, &[("a.txt", "a\n")]);
    let fifo = std::ffi::CString::new(env.ws.join("pipe").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

    let msg = env.invalid("read", json!({ "workspace": "ws", "path": "pipe" })).await;
    assert!(msg.contains("not a regular file"), "{msg}");
    env.invalid("edit", json!({ "workspace": "ws", "path": "pipe", "old": "a", "new": "b" })).await;
    assert!(found(env.ok("search", json!({ "workspace": "ws", "pattern": "a" })).await).len() == 1);
    let search = env.call("search", json!({ "workspace": "ws", "pattern": "a", "path": "pipe" }));
    match tokio::time::timeout(Duration::from_secs(5), search).await {
        Ok(reply) => assert!(found(reply.unwrap()).is_empty()),
        Err(_) => {
            // Give the blocked read a writer, so the runtime can shut down.
            drop(fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(env.ws.join("pipe")));
            panic!("fs.search on a FIFO blocked");
        }
    }
    assert_eq!(listed(env.ok("list", json!({ "workspace": "ws" })).await), ["a.txt", "pipe"]);
    let fork = env.fork().await;
    assert!(fs::symlink_metadata(fork.join("pipe")).unwrap().file_type().is_symlink());
    let diff: DiffResponse = serde_json::from_value(env.ok("diff", json!({ "fork": fork })).await).unwrap();
    assert!(diff.changes.is_empty());
}

#[tokio::test]
async fn molt_data_directories_are_off_limits() {
    let env = Env::new();
    let root = env.ws.parent().unwrap().to_path_buf();
    Env::put(&root, &[(".molt/secrets/fs", "bus secret\n")]);
    Env::put(&env.ws, &[("a.txt", "a\n"), (".molt/state", "secret\n"), ("sub/.molt/state", "secret\n")]);
    symlink(".molt", env.ws.join("data")).unwrap();

    for path in [".molt/state", "./.molt/state", "sub/.molt/state", "data/state"] {
        let msg = env.invalid("read", json!({ "workspace": "ws", "path": path })).await;
        assert!(msg.contains(".molt is Molt's own data directory"), "{msg}");
        env.invalid("write", json!({ "workspace": "ws", "path": path, "content": "x" })).await;
        env.invalid("edit", json!({ "workspace": "ws", "path": path, "old": "secret", "new": "x" })).await;
    }
    for path in [".molt", "sub/.molt", "data"] {
        env.invalid("list", json!({ "workspace": "ws", "path": path })).await;
        env.invalid("search", json!({ "workspace": "ws", "pattern": "secret", "path": path })).await;
    }
    for path in [".molt/new.txt", ".molt/new/deeper.txt", "data/new.txt", "data/new/deeper.txt"] {
        env.invalid("write", json!({ "workspace": "ws", "path": path, "content": "x" })).await;
    }
    assert_eq!(fs::read_to_string(env.ws.join(".molt/state")).unwrap(), "secret\n");
    assert!(!env.ws.join(".molt/new.txt").exists() && !env.ws.join(".molt/new").exists());
    for ws in [".molt", ".molt/secrets", "ws/.molt", "ws/data", "ws/sub/.molt"] {
        let msg = env.invalid("list", json!({ "workspace": ws })).await;
        assert!(msg.contains("inside .molt"), "{ws}: {msg}");
    }
    // Only the name `.molt` is refused.
    env.ok("write", json!({ "workspace": "ws", "path": ".molt.toml", "content": "x" })).await;

    // When scratch is inside a data directory, its forks may be used; the rest of it may not.
    let fs = Fs::new(Roots { root: root.clone(), scratch: root.join(".molt/work") }).unwrap();
    let reply: ForkResponse =
        serde_json::from_value(fs.handle("fork", json!({ "workspace": "ws" })).await.unwrap()).unwrap();
    let read = fs.handle("read", json!({ "workspace": reply.fork, "path": "a.txt" })).await.unwrap();
    assert_eq!(read_reply(read).content, "a\n");
    let e = fs.handle("list", json!({ "workspace": ".molt" })).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e}");
}

#[tokio::test]
async fn scratch_and_forks_are_private() {
    let env = Env::new();
    project(&env);
    let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&env.scratch), 0o700, "scratch is created private");
    let fork = env.fork().await;
    assert_eq!(mode(&fork), 0o700);
    assert_eq!(mode(&fork.with_extension("json")), 0o600);

    // Another user who can rename what is in scratch could swap a fork's files before the merge.
    let tmp = env.tmp.path();
    let roots = |scratch: &Path| Roots { root: tmp.join("root"), scratch: scratch.to_path_buf() };
    let refused = |scratch: &Path| match Fs::new(roots(scratch)) {
        Ok(_) => panic!("{} was accepted as scratch", scratch.display()),
        Err(e) => e.to_string(),
    };
    for bits in [0o777, 0o1777, 0o770, 0o702] {
        let dir = tmp.join(format!("open-{bits:o}"));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(bits)).unwrap();
        let msg = refused(&dir);
        assert!(msg.contains("written by other users"), "{msg}");
    }
    let own = tmp.join("own");
    fs::create_dir(&own).unwrap();
    fs::set_permissions(&own, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&own, tmp.join("link")).unwrap();
    for link in [tmp.join("link"), tmp.join("link/")] {
        let msg = refused(&link);
        assert!(msg.contains("symlink"), "{msg}");
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let theirs = if unsafe { libc::geteuid() } == 0 {
        let dir = tmp.join("theirs");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::chown(&dir, Some(65534), Some(65534)).unwrap();
        dir
    } else {
        PathBuf::from("/")
    };
    let msg = refused(&theirs);
    assert!(msg.contains("another user"), "{msg}");
    assert!(Fs::new(roots(&own)).is_ok(), "a directory of our own that only we can write to is fine");
}

#[tokio::test]
async fn bad_requests_are_invalid() {
    let env = Env::new();
    env.invalid("frobnicate", json!({})).await;
    env.invalid("read", json!({ "workspace": "ws" })).await;
    env.invalid("fork", json!({ "workspace": env.outside() })).await;
    Env::put(&env.ws, &[("a.txt", "a\n")]);
    let msg = env.invalid("list", json!({ "workspace": "ws/a.txt/x" })).await;
    assert!(msg.contains("does not exist"), "{msg}");
    // The full method name is accepted too.
    env.ok("fs.list", json!({ "workspace": "ws" })).await;
}

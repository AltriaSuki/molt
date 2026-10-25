//! `fs.git` (#111) against real repositories: finding the commit that
//! introduced a regression, telling the user's changes not committed from
//! an attempt's, a baseline that holds while the user's `HEAD` moves, and
//! that no query changes the repository, runs a program its configuration
//! names, or reads outside the workspace's part of it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use molt_api::fs::{ForkResponse, GitResponse};
use molt_proto::ErrorCode;
use molt_tools::{Fs, Roots};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct Env {
    tmp: TempDir,
    /// `root/ws`, canonical: the repository, or a directory of it.
    ws: PathBuf,
    fs: Fs,
}

impl Env {
    /// A repository at `root/ws`.
    fn new() -> Self {
        let env = Self::bare_dirs();
        git(&env.ws, &["init", "-q", "-b", "main"]);
        env
    }

    /// `root/ws` without a repository.
    fn bare_dirs() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("ws")).unwrap();
        let fs = Fs::new(Roots { root: root.clone(), scratch: tmp.path().join("scratch") }).unwrap();
        let ws = root.join("ws").canonicalize().unwrap();
        Env { tmp, ws, fs }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().join("root").canonicalize().unwrap()
    }

    async fn fork_of(&self, workspace: &Path) -> String {
        let reply = self.fs.handle("fork", json!({ "workspace": workspace })).await.unwrap();
        serde_json::from_value::<ForkResponse>(reply).unwrap().fork
    }

    async fn query(&self, workspace: &str, query: Value) -> GitResponse {
        let mut payload = query.clone();
        payload["workspace"] = json!(workspace);
        match self.fs.handle("git", payload).await {
            Ok(reply) => serde_json::from_value(reply).unwrap(),
            Err(e) => panic!("fs.git {query} failed: {e}"),
        }
    }

    async fn refused(&self, workspace: &str, query: Value) -> String {
        let mut payload = query.clone();
        payload["workspace"] = json!(workspace);
        match self.fs.handle("git", payload).await {
            Ok(reply) => panic!("fs.git {query} should fail, got {reply}"),
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Invalid, "{e}");
                e.message
            }
        }
    }
}

/// Run git in `dir` as a user would, without the developer's own configuration.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=Ann", "-c", "user.email=ann@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn put(dir: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
    put(dir, files);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Everything under `dir`, `.git` included: each entry's kind, mode,
/// content and modification time. A query must leave it exactly as it was.
fn everything(dir: &Path) -> BTreeMap<PathBuf, String> {
    let mut seen = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(at) = todo.pop() {
        for entry in fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let what = if meta.is_dir() {
                todo.push(path.clone());
                format!("dir {:o} {}", meta.mode(), meta.mtime_nsec())
            } else if meta.file_type().is_symlink() {
                format!("link {}", fs::read_link(&path).unwrap().display())
            } else {
                let hash = hex::encode(Sha256::digest(fs::read(&path).unwrap()));
                format!("file {:o} {} {hash}", meta.mode(), meta.mtime_nsec())
            };
            seen.insert(path, what);
        }
    }
    seen
}

const GOOD: &str = "def add(a, b):\n    return a + b\n";
const BAD: &str = "def add(a, b):\n    return a - b\n";

#[tokio::test]
async fn the_commit_that_introduced_a_regression_is_found_shown_and_blamed() {
    let env = Env::new();
    let first = commit(&env.ws, &[("calc.py", GOOD), ("README", "calc\n")], "Add add()");
    commit(&env.ws, &[("README", "calc, a calculator\n")], "Describe the project");
    let bad = commit(&env.ws, &[("calc.py", BAD)], "Tidy up calc");
    commit(&env.ws, &[("notes.txt", "later\n")], "Add notes");
    let fork = env.fork_of(&env.ws).await;
    let before = everything(&env.ws);

    // The commit that removed the right line, by its text.
    let found = env.query(&fork, json!({ "query": "log", "search": "a + b" })).await;
    let lines: Vec<&str> = found.output.lines().collect();
    assert!(lines[0].starts_with(&bad[..7]) && lines[1].trim() == "Tidy up calc", "{}", found.output);
    assert!(found.output.contains(&first[..7]), "added there too: {}", found.output);
    assert!(!found.output.contains("Describe the project"), "{}", found.output);
    assert!(found.output.contains("M\tcalc.py"), "{}", found.output);
    // Its patch says what changed.
    let shown = env.query(&fork, json!({ "query": "show", "rev": &bad[..10] })).await;
    assert!(shown.output.starts_with(&format!("commit {bad}\nAuthor: Ann <ann@example.com>")), "{}", shown.output);
    assert!(shown.output.contains("-    return a + b\n+    return a - b\n"), "{}", shown.output);
    // Blame names it for the line, and the history of the one file holds both commits.
    let blamed = env.query(&fork, json!({ "query": "blame", "path": "calc.py", "start_line": 2, "end_line": 2 })).await;
    assert!(blamed.output.starts_with(&bad[..7]) && blamed.output.contains("return a - b"), "{}", blamed.output);
    assert_eq!(blamed.output.lines().count(), 1, "{}", blamed.output);
    let history = env.query(&fork, json!({ "query": "log", "path": "calc.py" })).await;
    assert_eq!(history.output.matches("calc.py").count(), 2, "{}", history.output);
    let between = env.query(&fork, json!({ "query": "diff", "from": first, "to": "HEAD", "path": "calc.py" })).await;
    assert!(between.output.contains("+    return a - b"), "{}", between.output);
    let one = env.query(&fork, json!({ "query": "log", "rev": format!("{}..", &first[..8]), "limit": 1 })).await;
    assert!(one.output.contains("Add notes") && !one.output.contains("Tidy"), "{}", one.output);
    assert_eq!(found.baseline.as_deref(), Some(git(&env.ws, &["rev-parse", "HEAD"]).as_str()));
    assert_eq!(before, everything(&env.ws), "a query changed the repository");
}

#[tokio::test]
async fn status_tells_the_users_changes_not_committed_from_the_attempts_own() {
    let env = Env::new();
    commit(&env.ws, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("gone.txt", "x\n"), ("run.sh", "true\n")], "Start");
    put(&env.ws, &[("a.txt", "a, edited\n"), ("staged.txt", "staged\n"), ("new.txt", "untracked\n")]);
    git(&env.ws, &["add", "staged.txt"]);
    put(&env.ws, &[("b.txt", "b, staged\n")]);
    git(&env.ws, &["add", "b.txt"]);
    fs::remove_file(env.ws.join("gone.txt")).unwrap();
    let before = everything(&env.ws);
    let fork = env.fork_of(&env.ws).await;
    let copy = Path::new(&fork);
    put(copy, &[("b.txt", "b, mine\n"), ("mine.txt", "mine\n")]);
    fs::set_permissions(copy.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();

    let status = env.query(&fork, json!({ "query": "status" })).await.output;
    let (theirs, yours) = status.split_once("Your changes").unwrap();
    for line in ["modified a.txt", "modified b.txt", "deleted  gone.txt", "added    new.txt", "added    staged.txt"] {
        assert!(theirs.contains(line), "{line}: {status}");
    }
    assert!(!theirs.contains("mine.txt") && !theirs.contains("run.sh"), "{status}");
    for line in ["modified b.txt", "added    mine.txt", "modified run.sh"] {
        assert!(yours.contains(line), "{line}: {status}");
    }
    assert!(!yours.contains("a.txt") && !yours.contains("new.txt"), "{status}");
    assert!(status.contains("on branch main") && status.contains("\"Start\" (Ann, "), "{status}");
    // From the baseline to the files now: theirs and yours together.
    let now = env.query(&fork, json!({ "query": "diff" })).await.output;
    assert!(now.contains("-b\n+b, mine\n") && now.contains("+a, edited\n"), "{now}");
    assert!(now.contains("# run.sh: made executable") && now.contains("+++ b/mine.txt"), "{now}");
    let one = env.query(&fork, json!({ "query": "diff", "path": "a.txt" })).await.output;
    assert!(one.contains("a.txt") && !one.contains("b.txt"), "{one}");
    // The workspace itself, as an ask run reads it: its own changes, nothing of the fork's.
    let plain = env.query("ws", json!({ "query": "status" })).await.output;
    assert!(plain.contains("modified b.txt") && !plain.contains("mine.txt") && !plain.contains("Your changes"));
    assert_eq!(before, everything(&env.ws), "a query changed the repository, its index or its work tree");
}

#[tokio::test]
async fn the_baseline_holds_while_the_users_head_moves_and_branches_switch() {
    let env = Env::new();
    let base = commit(&env.ws, &[("f.txt", "one\n")], "One");
    let fork = env.fork_of(&env.ws).await;
    git(&env.ws, &["checkout", "-q", "-b", "other"]);
    let other = commit(&env.ws, &[("f.txt", "two\n")], "Two");

    let ws = env.ws.clone();
    let switching = std::thread::spawn(move || {
        for i in 0..30 {
            git(&ws, &["checkout", "-q", if i % 2 == 0 { "main" } else { "other" }]);
        }
    });
    for _ in 0..10 {
        let shown = env.query(&fork, json!({ "query": "show", "rev": "HEAD" })).await;
        assert!(shown.output.contains(&format!("commit {base}")), "HEAD is the baseline: {}", shown.output);
        assert_eq!(shown.baseline.as_deref(), Some(base.as_str()));
        let log = env.query(&fork, json!({ "query": "log" })).await.output;
        assert!(log.contains("One") && !log.contains("Two"), "{log}");
    }
    switching.join().unwrap();
    git(&env.ws, &["checkout", "-q", "other"]);
    let status = env.query(&fork, json!({ "query": "status" })).await.output;
    assert!(status.starts_with("Note: the user's HEAD has moved"), "{status}");
    assert!(status.contains(&other[..12]) && status.contains(&format!("still means the baseline {}", &base[..12])));
    // The fork started from the baseline's files, so it has nothing of theirs, though the work tree changed.
    assert!(status.contains("None: the files are as the baseline has them."), "{status}");
    let later = env.query(&fork, json!({ "query": "log", "rev": "other" })).await.output;
    assert!(later.contains("Two"), "another branch, by name: {later}");
}

#[tokio::test]
async fn a_repositorys_configuration_cannot_make_a_query_run_anything() {
    let env = Env::new();
    let marker = env.tmp.path().join("ran");
    let script = env.tmp.path().join("evil.sh");
    fs::write(&script, format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let hooks = env.tmp.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    for hook in ["post-checkout", "post-index-change", "reference-transaction", "pre-auto-gc"] {
        fs::copy(&script, hooks.join(hook)).unwrap();
    }
    commit(&env.ws, &[("a.txt", "one\n"), ("b.bin", "one\n")], "One");
    commit(&env.ws, &[("a.txt", "two\n"), ("b.bin", "two\n")], "Two");
    // A signed commit on top, which a log that shows signatures would have gpg.program check.
    let (tree, parent) = (git(&env.ws, &["rev-parse", "HEAD^{tree}"]), git(&env.ws, &["rev-parse", "HEAD"]));
    let who = "Ann <ann@example.com> 1700000000 +0000";
    let signed = env.tmp.path().join("signed");
    fs::write(
        &signed,
        format!(
            "tree {tree}\nparent {parent}\nauthor {who}\ncommitter {who}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n              iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\nSigned\n"
        ),
    )
    .unwrap();
    let signed = git(&env.ws, &["hash-object", "-t", "commit", "-w", signed.to_str().unwrap()]);
    git(&env.ws, &["update-ref", "refs/heads/main", &signed]);
    let evil = script.display();
    for (key, value) in [
        ("diff.external", evil.to_string()),
        ("diff.evil.command", evil.to_string()),
        ("diff.evil.textconv", evil.to_string()),
        ("filter.evil.clean", evil.to_string()),
        ("filter.evil.smudge", evil.to_string()),
        ("filter.evil.process", evil.to_string()),
        ("core.fsmonitor", evil.to_string()),
        ("core.pager", evil.to_string()),
        ("pager.log", evil.to_string()),
        ("core.hooksPath", hooks.display().to_string()),
        ("log.showSignature", "true".into()),
        ("gpg.program", evil.to_string()),
        ("core.sshCommand", evil.to_string()),
        ("credential.helper", format!("!{evil}")),
        ("gc.auto", "1".into()),
        ("alias.log", format!("!{evil}")),
        ("remote.origin.url", format!("ext::{evil}")),
        ("remote.origin.promisor", "true".into()),
        ("extensions.partialClone", "origin".into()),
    ] {
        git(&env.ws, &["config", key, &value]);
    }
    fs::write(env.ws.join(".git/info/attributes"), "* diff=evil filter=evil\n").unwrap();
    put(&env.ws, &[(".gitattributes", "* diff=evil filter=evil\n"), ("a.txt", "dirty\n")]);
    let before = everything(&env.ws);
    let fork = env.fork_of(&env.ws).await;
    // An attempt can write attributes and a .git of its own in its copy: neither is read.
    put(Path::new(&fork), &[(".gitattributes", "* diff=evil\n"), (".git/config", "[core]\nfsmonitor = x\n")]);

    for query in [
        json!({ "query": "status" }),
        json!({ "query": "log", "search": "two" }),
        json!({ "query": "log", "path": "a.txt" }),
        json!({ "query": "show", "rev": "HEAD" }),
        json!({ "query": "show", "rev": "HEAD~1" }),
        json!({ "query": "diff", "from": "HEAD~2", "to": "HEAD" }),
        json!({ "query": "diff", "from": "HEAD~2" }),
        json!({ "query": "blame", "path": "a.txt" }),
    ] {
        let answer = env.query(&fork, query.clone()).await.output;
        assert!(!answer.is_empty(), "{query}");
        assert!(!marker.exists(), "{query} ran {}", fs::read_to_string(&marker).unwrap());
    }
    let shown = env.query(&fork, json!({ "query": "show", "rev": "HEAD~1" })).await.output;
    assert!(shown.contains("-one\n+two\n"), "the plain patch, not the textconv's: {shown}");
    assert!(!shown.contains("Good signature") && !shown.contains("gpg"), "{shown}");
    let plain = env.query("ws", json!({ "query": "status" })).await.output;
    assert!(plain.contains("modified a.txt"), "{plain}");
    assert!(!marker.exists());
    assert_eq!(before, everything(&env.ws), "a query changed the repository");

    // A configuration that moves the work tree elsewhere cannot take the queries there.
    let elsewhere = env.tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    git(&env.ws, &["config", "core.worktree", elsewhere.to_str().unwrap()]);
    let why = env.refused("ws", json!({ "query": "status" })).await;
    assert!(why.contains("does not hold the workspace"), "{why}");
}

#[tokio::test]
async fn queries_stay_in_the_workspaces_part_of_the_repository_and_name_only_commits() {
    let env = Env::new();
    let repo = env.ws.clone();
    commit(&repo, &[("app/main.rs", "fn main() {}\n"), ("secret/key.txt", "hunter2\n")], "Both");
    let other = commit(&repo, &[("secret/key.txt", "hunter3\n")], "Only the secret");
    commit(&repo, &[("app/main.rs", "fn main() { run() }\n"), ("secret/key.txt", "hunter4\n")], "Both again");
    let fake = git(&repo, &["rev-parse", "HEAD:secret/key.txt"]);
    let sub = repo.join("app");
    symlink(env.tmp.path(), sub.join("out")).unwrap();
    let fork = env.fork_of(&sub).await;

    let log = env.query(&fork, json!({ "query": "log" })).await.output;
    assert!(log.contains("Both again") && log.contains("main.rs") && !log.contains("Only the secret"), "{log}");
    assert!(!log.contains("secret/") && !log.contains("app/main.rs"), "paths relative to the workspace: {log}");
    let shown = env.query(&fork, json!({ "query": "show", "rev": "HEAD" })).await.output;
    assert!(shown.contains("+++ b/main.rs") && !shown.contains("hunter"), "{shown}");
    let status = env.query(&fork, json!({ "query": "status" })).await.output;
    assert!(status.contains("the directory app/ of a git repository"), "{status}");
    assert!(status.contains("added    out"), "a symlink is listed, not followed: {status}");
    let now = env.query(&fork, json!({ "query": "diff" })).await.output;
    assert!(now.contains(&format!("# out: added symlink to {}", env.tmp.path().display())), "{now}");
    // Nothing outside the workspace, whatever the query names.
    for (query, why) in [
        (json!({ "query": "blame", "path": "../secret/key.txt" }), "`..` is not allowed"),
        (json!({ "query": "show", "rev": "HEAD:secret/key.txt" }), "not a revision"),
        (json!({ "query": "show", "rev": fake }), "no commit"),
        (json!({ "query": "show", "rev": "--output=/tmp/x" }), "not a revision"),
        (json!({ "query": "log", "rev": "HEAD --all" }), "not a revision"),
        (json!({ "query": "diff", "from": "HEAD~1", "to": ":/secret" }), "not a revision"),
        (json!({ "query": "blame", "path": "/etc/passwd" }), "absolute paths"),
        (json!({ "query": "blame", "path": "." }), "needs the path of a file"),
        (json!({ "query": "log", "search": "a\0b" }), "without NUL"),
    ] {
        let message = env.refused(&fork, query.clone()).await;
        assert!(message.contains(why), "{query}: {message}");
    }
    let only = env.query(&fork, json!({ "query": "show", "rev": other })).await.output;
    assert!(!only.contains("hunter"), "{only}");
    // A workspace outside the roots, or not in a repository, has none.
    let outside = env.tmp.path().to_str().unwrap().to_owned();
    let e = env.fs.handle("git", json!({ "workspace": outside, "query": "status" })).await.unwrap_err();
    assert!(e.message.contains("outside the directories"), "{e}");
    let plain = Env::bare_dirs();
    let why = plain.refused("ws", json!({ "query": "log" })).await;
    assert_eq!(why, "the workspace is not in a git repository, so it has no history to read");
    let plain_fork = plain.fork_of(&plain.ws).await;
    assert!(plain.refused(&plain_fork, json!({ "query": "status" })).await.contains("not in a git repository"));
    let _ = env.root();
}

#[tokio::test]
async fn a_linked_worktree_reads_its_own_head_and_submodules_are_not_entered() {
    let env = Env::new();
    commit(&env.ws, &[("f.txt", "one\n")], "One");
    let tree = env.root().join("tree");
    git(&env.ws, &["worktree", "add", "-q", "-b", "feature", tree.to_str().unwrap()]);
    let feature = commit(&tree, &[("f.txt", "feature\n")], "On the feature");
    let gitlink = git(&env.ws, &["rev-parse", "HEAD"]);
    git(&tree, &["update-index", "--add", "--cacheinfo", &format!("160000,{gitlink},vendor/lib")]);
    git(&tree, &["commit", "-q", "-m", "Add a submodule"]);
    put(&tree, &[("vendor/lib/inside.txt", "not looked into\n")]);
    let before = everything(&env.ws);
    let fork = env.fork_of(&tree).await;

    let status = env.query(&fork, json!({ "query": "status" })).await.output;
    assert!(status.contains("on branch feature") && status.contains("\"Add a submodule\""), "{status}");
    assert!(status.contains(&format!("Submodules, not looked into: vendor/lib (at {})", &gitlink[..12])), "{status}");
    assert!(!status.contains("inside.txt"), "{status}");
    let log = env.query(&fork, json!({ "query": "log", "rev": format!("{}..", &feature[..9]) })).await.output;
    assert!(log.contains("Add a submodule") && !log.contains("On the feature"), "{log}");
    let main = env.query(&fork, json!({ "query": "log", "rev": "main" })).await.output;
    assert!(main.contains("One") && !main.contains("feature"), "{main}");
    assert_eq!(before, everything(&env.ws), "the main repository changed");
}

#[tokio::test]
async fn names_with_spaces_tabs_newlines_dashes_and_unicode_are_read_as_they_are() {
    let env = Env::new();
    let names = ["-n dash.txt", "tab\there.txt", "new\nline.txt", "ünïcødé 名前.txt", "quote\"d.txt"];
    let files: Vec<(&str, &str)> = names.iter().map(|n| (*n, "first\n")).collect();
    commit(&env.ws, &files, "Odd names");
    let files: Vec<(&str, &str)> = names.iter().map(|n| (*n, "second\n")).collect();
    commit(&env.ws, &files, "Change them");
    put(&env.ws, &[("-n dash.txt", "dirty\n")]);
    let fork = env.fork_of(&env.ws).await;
    let status = env.query(&fork, json!({ "query": "status" })).await.output;
    assert!(status.contains("modified -n dash.txt\n"), "{status}");
    assert_eq!(status.matches("modified").count(), 1, "{status}");
    for name in names {
        let blamed = env.query(&fork, json!({ "query": "blame", "path": name })).await.output;
        assert!(blamed.contains("second"), "{name:?}: {blamed}");
        let log = env.query(&fork, json!({ "query": "log", "path": name })).await.output;
        assert_eq!(log.matches("    ").count(), 2, "{name:?}: {log}");
    }
    let now = env.query(&fork, json!({ "query": "diff" })).await.output;
    assert!(now.contains("+++ b/-n dash.txt\n") && now.contains("+dirty\n"), "{now}");
    let log = env.query(&fork, json!({ "query": "log", "limit": 1 })).await.output;
    assert!(log.contains("ünïcødé 名前.txt"), "names as they are: {log}");
}

#[tokio::test]
async fn answers_are_bounded_and_a_repository_without_commits_says_so() {
    let env = Env::new();
    let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
    commit(&env.ws, &[("big.txt", &big)], "Big");
    for i in 0..120 {
        commit(&env.ws, &[("n.txt", &i.to_string())], &format!("Commit {i}"));
    }
    let fork = env.fork_of(&env.ws).await;
    let shown = env.query(&fork, json!({ "query": "show", "rev": "HEAD~120" })).await;
    assert!(shown.truncated && shown.output.len() <= 40 * 1024 && shown.output.ends_with('\n'));
    let log = env.query(&fork, json!({ "query": "log", "limit": 1000 })).await.output;
    assert_eq!(log.matches("Commit ").count(), 100, "at most 100 commits");
    let log = env.query(&fork, json!({ "query": "log" })).await.output;
    assert_eq!(log.matches("Commit ").count(), 20, "20 unless asked");

    let empty = Env::new();
    put(&empty.ws, &[("a.txt", "a\n")]);
    let fork = empty.fork_of(&empty.ws).await;
    let status = empty.query(&fork, json!({ "query": "status" })).await;
    assert!(status.output.contains("The repository had no commit when your copy was made."), "{}", status.output);
    assert!(status.output.contains("added    a.txt") && status.baseline.is_none(), "{}", status.output);
    assert!(empty.refused(&fork, json!({ "query": "log" })).await.contains("no commit to read yet"));
}

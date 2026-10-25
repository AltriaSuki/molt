//! `fs.git` (#111): the git history of the repository a workspace is in,
//! read without changing anything in it.
//!
//! A fork has no `.git`, so its history is its original's, and its
//! baseline is the commit the original's `HEAD` named when the fork was
//! made (kept in its metadata), however that `HEAD` moves later. Git only
//! ever reads objects and refs here: it is given the repository's git
//! directory as its work tree too, so it never reads the user's work tree
//! or index, and nothing that compares files on disk with a commit (which
//! can run a repository's clean filters) is asked of it. What differs
//! between a commit and the files on disk is worked out here instead, from
//! content hashes, as fork and diff do.
//!
//! A repository's own configuration cannot make git run a program, page,
//! use the network or write: the user's and the system's configuration are
//! not read, and the command line overrides every setting that would
//! (hooks, fsmonitor, external diff and textconv, signatures, pagers,
//! transports, the lazy fetches of a partial clone, automatic gc).
//! Revisions are resolved to commit ids before they reach a command, so they
//! can be neither options nor the blobs of other parts of the repository,
//! and paths are kept to the workspace's part of it. A query has a deadline
//! and its answer a size limit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use molt_api::fs::{Change, ChangeKind, GitQuery, GitRequest, GitResponse};
use molt_proto::RemoteError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::error::{failed, invalid};
use crate::files::is_binary;
use crate::fork::{self, Snapshot, DIFF_TIMEOUT, EXECUTABLE, MAX_DIFF_FILE, NOT_A_FILE};
use crate::{paths, walk, Roots};

/// Bytes of one answer; the planner's tool results hold 50 000.
const MAX_OUTPUT: usize = 40 * 1024;
/// How long one query may take, all its git processes together.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long recording a fork's baseline may take.
const BASELINE_TIMEOUT: Duration = Duration::from_secs(5);
/// Commits a log lists when the caller says nothing, and at most.
const LOG_LIMIT: u32 = 20;
const MAX_LOG_LIMIT: u32 = 100;
/// Files of a commit a status or diff compares with files on disk, at most.
const MAX_TREE_FILES: usize = 200_000;
const MAX_TREE_BYTES: usize = 64 << 20;
/// Bytes of git's error output kept for a message.
const MAX_STDERR: usize = 4096;
const MAX_REV: usize = 256;
const MAX_SEARCH: usize = 1024;
/// Content hashes of blobs remembered between queries.
const MAX_CACHED_BLOBS: usize = 200_000;
/// Paths a status lists in each part.
const MAX_LISTED: usize = 200;
/// Characters of a commit id as answers show it.
const SHORT: usize = 12;

const NOT_A_REPO: &str = "the workspace is not in a git repository, so it has no history to read";
const NO_COMMITS: &str = "the repository has no commit to read yet";

/// Variables git runs with, in an environment otherwise empty but for `PATH`.
const ENV: [(&str, &str); 13] = [
    // Neither the user's nor the system's configuration.
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    // Nothing takes a lock or refreshes the index.
    ("GIT_OPTIONAL_LOCKS", "0"),
    // A partial clone does not fetch the objects it lacks.
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    // A path is a path, never `:(glob)` or other magic.
    ("GIT_LITERAL_PATHSPECS", "1"),
    ("GIT_ADVICE", "0"),
    ("GIT_PROTOCOL_FROM_USER", "0"),
    ("LC_ALL", "C"),
];

/// Settings on the command line, which win over the repository's own.
const CONFIG: [&str; 23] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    "core.untrackedCache=false",
    "core.attributesFile=/dev/null",
    "core.quotePath=false",
    "protocol.allow=never",
    "credential.helper=",
    "gc.auto=0",
    "maintenance.auto=false",
    "log.showSignature=false",
    "log.mailmap=false",
    "mailmap.file=/dev/null",
    "mailmap.blob=",
    "color.ui=never",
    "diff.external=",
    "diff.noprefix=false",
    "diff.mnemonicPrefix=false",
    "diff.relative=false",
    "diff.orderFile=/dev/null",
    "blame.ignoreRevsFile=",
    "blame.coloring=none",
    "i18n.logOutputEncoding=UTF-8",
];

/// Options of every command that shows changes: no external diff program, text conversion or colour.
const PLAIN: [&str; 3] = ["--no-ext-diff", "--no-textconv", "--no-color"];

/// The commit a fork's history queries read by default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Baseline {
    /// The full commit id.
    pub commit: String,
    /// The branch `HEAD` was on; `None` when it was detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Content hashes of blobs by id, so a repository's unchanged files are
/// read once, not at every query.
#[derive(Default)]
pub(crate) struct Blobs(Mutex<HashMap<String, String>>);

impl Blobs {
    fn get(&self, oid: &str) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(oid).cloned()
    }

    fn insert(&self, found: Vec<(String, String)>) {
        let mut known = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if known.len() + found.len() > MAX_CACHED_BLOBS {
            known.clear();
        }
        known.extend(found);
    }
}

/// What `HEAD` of the repository `workspace` is in names now: `None`
/// outside a repository, before its first commit, or when git cannot say.
pub(crate) fn baseline(workspace: &Path) -> Option<Baseline> {
    let mut cmd = command(None, workspace);
    cmd.args(["rev-parse", "HEAD", "--symbolic-full-name", "HEAD"]);
    head(run(&mut cmd, 4096, Instant::now() + BASELINE_TIMEOUT).ok()?)
}

/// The baseline in what `rev-parse HEAD --symbolic-full-name HEAD` printed.
fn head(out: Output) -> Option<Baseline> {
    if !out.ok {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let commit = lines.next().filter(|c| is_oid(c))?.to_owned();
    let name = lines.next()?;
    let branch = name.strip_prefix("refs/heads/").map(str::to_owned);
    Some(Baseline { commit, branch })
}

/// Answer `req`.
pub(crate) fn query(roots: &Roots, blobs: &Blobs, req: GitRequest) -> Result<GitResponse, RemoteError> {
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let target = Target::open(roots, &req.workspace, deadline)?;
    let (body, cut) = match req.query {
        GitQuery::Status => (target.status(blobs)?, false),
        GitQuery::Log { rev, path, search, limit } => target.log(rev.as_deref(), path.as_deref(), search, limit)?,
        GitQuery::Show { rev, path } => target.show(&rev, path.as_deref())?,
        GitQuery::Diff { from, to, path } => target.diff(blobs, from.as_deref(), to.as_deref(), path.as_deref())?,
        GitQuery::Blame { path, rev, start_line, end_line } => {
            target.blame(&path, rev.as_deref(), start_line, end_line)?
        }
    };
    let mut output = target.moved_note();
    output.push_str(&body);
    let (output, clipped) = clip(output, MAX_OUTPUT);
    let baseline = target.baseline.as_ref().map(|b| b.commit.clone());
    Ok(GitResponse { output, truncated: cut || clipped, baseline })
}

/// The repository a workspace is in.
struct Repo {
    /// Its git directory: the linked work tree's own, for one made with `git worktree`.
    dir: PathBuf,
    /// Where the workspace is in the repository, as git writes paths: empty or ending in `/`.
    prefix: String,
    deadline: Instant,
}

impl Repo {
    /// The repository the canonical directory `workspace` is in.
    fn open(workspace: &Path, deadline: Instant) -> Result<Self, RemoteError> {
        let mut cmd = command(None, workspace);
        cmd.args(["rev-parse", "--absolute-git-dir", "--show-toplevel"]);
        let out = run(&mut cmd, 64 * 1024, deadline)?;
        if !out.ok {
            let why = out.error();
            if why.contains("not a git repository") {
                return Err(invalid(NOT_A_REPO));
            }
            return Err(invalid(format!("git cannot read the repository the workspace is in: {why}")));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let (Some(dir), Some(top)) = (lines.next(), lines.next()) else { return Err(invalid(NOT_A_REPO)) };
        // A repository's configuration can name another work tree (core.worktree): it must hold the workspace.
        let top =
            Path::new(top).canonicalize().map_err(|e| failed(format!("the repository's work tree {top}: {e}")))?;
        let Ok(rel) = workspace.strip_prefix(&top) else {
            return Err(invalid(format!(
                "the work tree of the repository the workspace is in, {}, does not hold the workspace",
                top.display()
            )));
        };
        let prefix = paths::slash(Path::new(""), rel);
        let prefix = if prefix.is_empty() { prefix } else { format!("{prefix}/") };
        Ok(Self { dir: PathBuf::from(dir), prefix, deadline })
    }

    /// Run git in the repository with `args`, keeping `limit` bytes of its output.
    fn run<I, S>(&self, args: I, limit: usize) -> Result<Output, RemoteError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = command(Some(&self.dir), &self.dir);
        cmd.args(args);
        run(&mut cmd, limit, self.deadline)
    }

    /// [`Repo::run`], an error saying what git said when it fails.
    fn read<I, S>(&self, what: &str, args: I, limit: usize) -> Result<Output, RemoteError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let out = self.run(args, limit)?;
        if !out.ok {
            return Err(invalid(format!("git {what} failed: {}", out.error())));
        }
        Ok(out)
    }

    /// What `HEAD` names now.
    fn head(&self) -> Result<Option<Baseline>, RemoteError> {
        Ok(head(self.run(["rev-parse", "HEAD", "--symbolic-full-name", "HEAD"], 4096)?))
    }

    /// The files of `commit` in the workspace's part of the repository.
    fn tree(&self, commit: &str) -> Result<Tree, RemoteError> {
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", commit];
        let scope = self.prefix.trim_end_matches('/');
        if !scope.is_empty() {
            args.extend(["--", scope]);
        }
        let out = self.read("ls-tree", args, MAX_TREE_BYTES)?;
        if out.cut {
            return Err(failed("the commit lists too many files to compare with the workspace's"));
        }
        let mut tree = Tree::default();
        for record in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let record = String::from_utf8_lossy(record);
            let Some((meta, path)) = record.split_once('\t') else { continue };
            let Some(rel) = path.strip_prefix(&self.prefix) else { continue };
            let mut meta = meta.split(' ');
            let (Some(mode), Some(kind), Some(oid)) = (meta.next(), meta.next(), meta.next()) else { continue };
            if !is_oid(oid) {
                continue;
            }
            match kind {
                "blob" => {
                    tree.files.insert(rel.to_owned(), (mode.to_owned(), oid.to_owned()));
                }
                "commit" => {
                    tree.submodules.insert(rel.to_owned(), oid.to_owned());
                }
                _ => {}
            }
            if tree.files.len() > MAX_TREE_FILES {
                return Err(failed(format!(
                    "the commit has more than {MAX_TREE_FILES} files in the workspace, too many to compare"
                )));
            }
        }
        Ok(tree)
    }

    /// Learn the content hash of every blob of `oids` that `blobs` does not know yet.
    fn hash_blobs(&self, blobs: &Blobs, oids: Vec<&str>) -> Result<(), RemoteError> {
        let mut seen = HashSet::new();
        let wanted: Vec<String> =
            oids.into_iter().filter(|o| seen.insert(*o) && blobs.get(o).is_none()).map(str::to_owned).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let mut cmd = command(Some(&self.dir), &self.dir);
        cmd.args(["cat-file", "--batch"]).stdin(Stdio::piped());
        let mut child = spawn(&mut cmd)?;
        let stderr = drain(child.stderr.take());
        let watch = Watchdog::start(&child, self.deadline);
        let list: String = wanted.iter().map(|o| format!("{o}\n")).collect();
        let writer = child.stdin.take().map(|mut stdin| std::thread::spawn(move || stdin.write_all(list.as_bytes())));
        let mut found = Vec::new();
        let read = match child.stdout.take() {
            Some(out) => read_blobs(&mut BufReader::new(out), &wanted, &mut found),
            None => Err(io::Error::other("no output")),
        };
        let ended = finish(child, watch, self.deadline);
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let errors = stderr.join().unwrap_or_default();
        ended?;
        read.map_err(|e| failed(format!("git cat-file failed: {e}: {}", first_line(&errors))))?;
        blobs.insert(found);
        Ok(())
    }

    /// The content of the blob `oid`, at most [`MAX_DIFF_FILE`] bytes of it.
    fn blob(&self, oid: &str) -> Result<(Vec<u8>, bool), RemoteError> {
        let out = self.read("cat-file", ["cat-file", "blob", oid], MAX_DIFF_FILE as usize)?;
        Ok((out.stdout, out.cut))
    }
}

/// Read `cat-file --batch`'s answer for `wanted`, in order, into `found`
/// as (id, content hash). A missing object is left out.
fn read_blobs(out: &mut impl BufRead, wanted: &[String], found: &mut Vec<(String, String)>) -> io::Result<()> {
    for oid in wanted {
        let mut header = String::new();
        if out.read_line(&mut header)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut parts = header.split_whitespace().skip(1);
        let (kind, size) = (parts.next(), parts.next().and_then(|s| s.parse::<u64>().ok()));
        let Some(size) = size else { continue };
        let mut hasher = Sha256::new();
        io::copy(&mut out.take(size), &mut hasher)?;
        let mut newline = [0u8; 1];
        out.read_exact(&mut newline)?;
        if kind == Some("blob") {
            found.push((oid.clone(), hex::encode(hasher.finalize())));
        }
    }
    Ok(())
}

/// The files of a commit below the workspace, by workspace-relative path.
#[derive(Default)]
struct Tree {
    /// (mode, blob id).
    files: BTreeMap<String, (String, String)>,
    /// Submodules, with the commit each is at.
    submodules: BTreeMap<String, String>,
}

impl Tree {
    /// True for a path in a submodule, which is not looked into.
    fn in_submodule(&self, path: &str) -> bool {
        ancestors(path).any(|a| self.submodules.contains_key(a))
    }
}

/// `path` and each directory above it.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(path), |p| p.rsplit_once('/').map(|(parent, _)| parent))
}

/// What a query reads: the repository, its baseline, and the files now.
struct Target {
    repo: Repo,
    /// The commit queries read by default, and what `HEAD` means in them.
    baseline: Option<Baseline>,
    /// What the original's `HEAD` names now, when that is not the baseline.
    moved: Option<Option<Baseline>>,
    /// The fork the query names, if it names one.
    fork: Option<Snapshot>,
    /// The directory whose files are the workspace's now.
    dir: PathBuf,
    scratch: PathBuf,
}

impl Target {
    fn open(roots: &Roots, workspace: &str, deadline: Instant) -> Result<Self, RemoteError> {
        let fork = if fork::names_fork(roots, workspace) { Some(fork::snapshot(roots, workspace)?) } else { None };
        let base = match &fork {
            Some(fork) => fork.base.clone(),
            None => roots.workspace(workspace)?,
        };
        let repo = Repo::open(&base, deadline)?;
        let now = repo.head()?;
        let baseline = match &fork {
            Some(fork) => fork.baseline.clone(),
            None => now.clone(),
        };
        let moved = (now.as_ref().map(|b| &b.commit) != baseline.as_ref().map(|b| &b.commit)).then_some(now);
        let dir = fork.as_ref().map_or(base, |f| f.dir.clone());
        Ok(Self { repo, baseline, moved, fork, dir, scratch: roots.scratch.clone() })
    }

    /// Said first in every answer when the original's `HEAD` moved since the fork was made.
    fn moved_note(&self) -> String {
        let Some(now) = &self.moved else { return String::new() };
        let baseline = match &self.baseline {
            Some(b) => format!("still means the baseline {}", short(&b.commit)),
            None => "names nothing: the repository had no commit when your copy was made".to_owned(),
        };
        let now = match now {
            Some(now) => format!("to {}{}", short(&now.commit), on_branch(now)),
            None => "to no commit".to_owned(),
        };
        format!(
            "Note: the user's HEAD has moved since your copy of the workspace was made, {now}; HEAD in your queries \
             {baseline}.\n\n"
        )
    }

    fn baseline_commit(&self) -> Result<&str, RemoteError> {
        self.baseline.as_ref().map(|b| b.commit.as_str()).ok_or_else(|| invalid(NO_COMMITS))
    }

    /// The commit `rev` names. `HEAD`, `HEAD~n` and `HEAD^` count from the baseline.
    fn commit(&self, rev: &str) -> Result<String, RemoteError> {
        let rev = rev.trim();
        check_rev(rev)?;
        let named = match rev.strip_prefix("HEAD") {
            Some(rest) if rest.is_empty() || rest.starts_with(['~', '^']) => {
                format!("{}{rest}", self.baseline_commit()?)
            }
            _ => rev.to_owned(),
        };
        let out = self.repo.run(["rev-parse", "--verify", "--quiet", &format!("{named}^{{commit}}")], 4096)?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.lines().next().filter(|oid| out.ok && is_oid(oid)) {
            Some(oid) => Ok(oid.to_owned()),
            None => Err(invalid(format!("there is no commit {rev} in the repository"))),
        }
    }

    /// `rev` as a log reads it: a commit, or a range `A..B` or `A...B`
    /// whose missing side is the baseline.
    fn range(&self, rev: Option<&str>) -> Result<String, RemoteError> {
        let Some(rev) = rev.map(str::trim).filter(|r| !r.is_empty()) else {
            return Ok(self.baseline_commit()?.to_owned());
        };
        let side = |s: &str| if s.is_empty() { Ok(self.baseline_commit()?.to_owned()) } else { self.commit(s) };
        for dots in ["...", ".."] {
            if let Some((a, b)) = rev.split_once(dots) {
                return Ok(format!("{}{dots}{}", side(a)?, side(b)?));
            }
        }
        self.commit(rev)
    }

    /// The pathspec of `path` in the repository, or of the workspace without one.
    fn pathspec(&self, path: Option<&str>) -> Result<Option<String>, RemoteError> {
        let rel = path.map(|p| paths::relative(p).map(|p| paths::slash(Path::new(""), &p))).transpose()?;
        let spec = format!("{}{}", self.repo.prefix, rel.unwrap_or_default());
        let spec = spec.trim_end_matches('/');
        Ok((!spec.is_empty()).then(|| spec.to_owned()))
    }

    /// Options that keep the paths of changes relative to the workspace, and to it.
    fn relative(&self) -> Option<String> {
        (!self.repo.prefix.is_empty()).then(|| format!("--relative={}", self.repo.prefix))
    }

    fn status(&self, blobs: &Blobs) -> Result<String, RemoteError> {
        let mut out = if self.repo.prefix.is_empty() {
            "The workspace is the top of a git repository.\n".to_owned()
        } else {
            format!("The workspace is the directory {} of a git repository; history is kept to it.\n", self.repo.prefix)
        };
        let forked = self.fork.is_some();
        let tree = match &self.baseline {
            Some(b) => {
                let about = self.repo.read(
                    "log",
                    ["log", "-1", "--date=short", "--format=%s%x00%ad%x00%an", &b.commit, "--"],
                    4096,
                )?;
                let text = String::from_utf8_lossy(&about.stdout);
                let mut parts = text.trim_end().splitn(3, '\0');
                let (subject, date, author) =
                    (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let what = if forked {
                    "the commit the user's HEAD named when your copy of the workspace was made; HEAD in your queries \
                     means it, and they read it unless you name another commit"
                } else {
                    "HEAD"
                };
                out.push_str(&format!(
                    "Baseline: {}{}, \"{subject}\" ({author}, {date}): {what}.\n",
                    short(&b.commit),
                    on_branch(b)
                ));
                self.repo.tree(&b.commit)?
            }
            None if forked => {
                out.push_str("Baseline: none. The repository had no commit when your copy was made.\n");
                Tree::default()
            }
            None => {
                out.push_str("Baseline: none. The repository has no commit yet.\n");
                Tree::default()
            }
        };
        let (files, skip) = match &self.fork {
            Some(fork) => (fork.at_fork.clone(), fork.links.clone()),
            None => (self.files_now(&tree)?, Default::default()),
        };
        let skip = |p: &str| ancestors(p).any(|a| skip.contains(a));
        let (theirs, unknown) = self.compare(blobs, &tree, &files, skip)?;
        out.push('\n');
        if forked {
            out.push_str(
                "Changes not committed that the user had when your copy was made (theirs, staged or not; your copy \
                 started from them):\n",
            );
        } else {
            out.push_str("Changes not committed in the workspace (staged or not):\n");
        }
        list(&mut out, &theirs, "  None: the files are as the baseline has them.\n");
        if let Some(fork) = &self.fork {
            out.push_str("\nYour changes since your copy was made:\n");
            list(&mut out, &fork.changes, "  None yet.\n");
        }
        if !tree.submodules.is_empty() {
            let named: Vec<String> =
                tree.submodules.iter().map(|(p, oid)| format!("{} (at {})", shown(p), short(oid))).collect();
            out.push_str(&format!("\nSubmodules, not looked into: {}.\n", named.join(", ")));
        }
        if unknown > 0 {
            out.push_str(&format!(
                "\n{unknown} files of the baseline could not be compared: the repository does not have their \
                 content (a partial clone?).\n"
            ));
        }
        Ok(out)
    }

    /// The state of each file of the workspace that is not a fork, as a
    /// fork would have recorded it, and of each file of `tree` the walk
    /// rules leave out but that is there.
    fn files_now(&self, tree: &Tree) -> Result<BTreeMap<String, String>, RemoteError> {
        let mut files = BTreeMap::new();
        let err = |e: io::Error| failed(format!("reading the workspace: {e}"));
        for item in walk::walk(&self.dir, &self.dir, None, &self.scratch) {
            let entry = item.map_err(|e| failed(format!("reading the workspace: {e}")))?;
            if entry.file_type().is_some_and(|t| t.is_file() || t.is_symlink()) {
                let rel = paths::slash(&self.dir, entry.path());
                if let Some(state) = fork::state(&self.dir, &rel).map_err(err)?.filter(|s| s != NOT_A_FILE) {
                    files.insert(rel, state);
                }
            }
        }
        let left_out: Vec<&String> =
            tree.files.keys().filter(|p| !files.contains_key(*p) && paths::relative(p).is_ok()).collect();
        for path in left_out {
            if let Some(state) = fork::state(&self.dir, path).map_err(err)?.filter(|s| s != NOT_A_FILE) {
                files.insert(path.clone(), state);
            }
        }
        Ok(files)
    }

    /// What `files` changed against `tree`, leaving out submodules and the
    /// paths `skip` names, and how many of the tree's files could not be
    /// compared since the repository lacks their content.
    fn compare(
        &self,
        blobs: &Blobs,
        tree: &Tree,
        files: &BTreeMap<String, String>,
        skip: impl Fn(&str) -> bool,
    ) -> Result<(Vec<Change>, usize), RemoteError> {
        let compared: Vec<(&String, &(String, String))> = tree.files.iter().filter(|(p, _)| !skip(p)).collect();
        self.repo.hash_blobs(blobs, compared.iter().map(|(_, (_, oid))| oid.as_str()).collect())?;
        let mut unknown = HashSet::new();
        let mut expected = BTreeMap::new();
        for (path, (mode, oid)) in compared {
            match blobs.get(oid) {
                Some(hash) => {
                    expected.insert(path.clone(), state_of(mode, &hash));
                }
                None => {
                    unknown.insert(path.as_str());
                }
            }
        }
        let files: BTreeMap<String, String> = files
            .iter()
            .filter(|(p, _)| !skip(p) && !tree.in_submodule(p) && !unknown.contains(p.as_str()))
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect();
        Ok((fork::changes(&expected, &files), unknown.len()))
    }

    fn log(
        &self,
        rev: Option<&str>,
        path: Option<&str>,
        search: Option<String>,
        limit: Option<u32>,
    ) -> Result<(String, bool), RemoteError> {
        let range = self.range(rev)?;
        let limit = limit.unwrap_or(LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
        let mut args: Vec<String> =
            ["log", "--date=short", "--format=%h %ad %an%n    %s", "--name-status"].map(str::to_owned).to_vec();
        args.extend(PLAIN.map(str::to_owned));
        args.push(format!("--max-count={limit}"));
        if let Some(search) = search.filter(|s| !s.is_empty()) {
            if search.len() > MAX_SEARCH || search.contains('\0') {
                return Err(invalid(format!("search is at most {MAX_SEARCH} bytes of text, without NUL")));
            }
            args.push(format!("-S{search}"));
        }
        args.extend(self.relative());
        args.push(range);
        args.push("--".into());
        args.extend(self.pathspec(path)?);
        let out = self.repo.read("log", args, MAX_OUTPUT)?;
        let (text, cut) = out.text();
        Ok(if text.trim().is_empty() { ("No commits match.\n".to_owned(), false) } else { (text, cut) })
    }

    fn show(&self, rev: &str, path: Option<&str>) -> Result<(String, bool), RemoteError> {
        let commit = self.commit(rev)?;
        let mut args: Vec<String> =
            ["show", "--date=iso", "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B", "--stat", "--patch"]
                .map(str::to_owned)
                .to_vec();
        args.extend(PLAIN.map(str::to_owned));
        args.extend(self.relative());
        args.push(commit);
        args.push("--".into());
        args.extend(self.pathspec(path)?);
        Ok(self.repo.read("show", args, MAX_OUTPUT)?.text())
    }

    fn diff(
        &self,
        blobs: &Blobs,
        from: Option<&str>,
        to: Option<&str>,
        path: Option<&str>,
    ) -> Result<(String, bool), RemoteError> {
        let from = match from.map(str::trim).filter(|f| !f.is_empty()) {
            Some(rev) => self.commit(rev)?,
            None => self.baseline_commit()?.to_owned(),
        };
        let Some(to) = to.map(str::trim).filter(|t| !t.is_empty()) else { return self.diff_now(blobs, &from, path) };
        let to = self.commit(to)?;
        let mut args: Vec<String> = ["diff", "--stat", "--patch"].map(str::to_owned).to_vec();
        args.extend(PLAIN.map(str::to_owned));
        args.extend(self.relative());
        args.extend([from, to, "--".into()]);
        args.extend(self.pathspec(path)?);
        let (text, cut) = self.repo.read("diff", args, MAX_OUTPUT)?.text();
        Ok(if text.trim().is_empty() { ("No differences.\n".to_owned(), false) } else { (text, cut) })
    }

    /// From the commit `from` to the workspace's files as they are now.
    fn diff_now(&self, blobs: &Blobs, from: &str, path: Option<&str>) -> Result<(String, bool), RemoteError> {
        let within = match path {
            Some(p) => paths::slash(Path::new(""), &paths::relative(p)?),
            None => String::new(),
        };
        let tree = self.repo.tree(from)?;
        let (now, links) = match &self.fork {
            Some(fork) => (fork.now.clone(), fork.links.clone()),
            None => (self.files_now(&tree)?, Default::default()),
        };
        let inside = |p: &str| within.is_empty() || ancestors(p).any(|a| a == within);
        let skip = |p: &str| !inside(p) || ancestors(p).any(|a| links.contains(a));
        let (changes, unknown) = self.compare(blobs, &tree, &now, skip)?;
        let mut out = format!(
            "From {} to your files as they are now ({}):\n",
            short(from),
            if self.fork.is_some() {
                "the user's changes not committed and yours together; status tells them apart"
            } else {
                "the changes not committed in the workspace"
            }
        );
        if changes.is_empty() {
            out.push_str("No differences.\n");
        }
        let mut cut = false;
        for change in &changes {
            let section = self.file_patch(&tree, &now, change)?;
            if out.len() + section.len() > MAX_OUTPUT {
                cut = true;
                break;
            }
            out.push_str(&section);
        }
        if unknown > 0 {
            out.push_str(&format!("[{unknown} files not compared: the repository does not have their content]\n"));
        }
        Ok((out, cut))
    }

    /// One file's patch from `tree` to the workspace now.
    fn file_patch(&self, tree: &Tree, now: &BTreeMap<String, String>, change: &Change) -> Result<String, RemoteError> {
        let path = &change.path;
        let old = match tree.files.get(path) {
            Some((mode, oid)) => {
                let (bytes, large) = self.repo.blob(oid)?;
                Side::of(bytes, large, mode == "120000")
            }
            None => Side::Missing,
        };
        let new = match change.kind {
            ChangeKind::Deleted => Side::Missing,
            _ => Side::read(&self.dir.join(path)),
        };
        let was_x = tree.files.get(path).map(|(mode, _)| mode == "100755");
        let is_x = now.get(path).map(|s| s.ends_with(EXECUTABLE));
        let mut out = match (was_x, is_x) {
            (Some(false), Some(true)) => format!("# {}: made executable\n", shown(path)),
            (Some(true), Some(false)) => format!("# {}: no longer executable\n", shown(path)),
            _ => String::new(),
        };
        out.push_str(&render(path, old, new));
        Ok(out)
    }

    fn blame(
        &self,
        path: &str,
        rev: Option<&str>,
        start: Option<u64>,
        end: Option<u64>,
    ) -> Result<(String, bool), RemoteError> {
        let commit = match rev.map(str::trim).filter(|r| !r.is_empty()) {
            Some(rev) => self.commit(rev)?,
            None => self.baseline_commit()?.to_owned(),
        };
        if paths::relative(path)?.as_os_str().is_empty() {
            return Err(invalid("blame needs the path of a file"));
        }
        let Some(spec) = self.pathspec(Some(path))? else { return Err(invalid("blame needs the path of a file")) };
        let mut args: Vec<String> = ["blame", "--date=short", "--no-textconv"].map(str::to_owned).to_vec();
        match (start, end) {
            (None, None) => {}
            (Some(0), _) | (_, Some(0)) => return Err(invalid("lines are numbered from 1")),
            (Some(s), Some(e)) if e < s => return Err(invalid("end_line comes before start_line")),
            (s, e) => args.push(format!("-L{},{}", s.unwrap_or(1), e.map(|e| e.to_string()).unwrap_or_default())),
        }
        args.extend([commit, "--".into(), spec]);
        Ok(self.repo.read("blame", args, MAX_OUTPUT)?.text())
    }
}

/// The state a fork records for a file of `mode` whose content hashes to `hash`.
fn state_of(mode: &str, hash: &str) -> String {
    match mode {
        "100755" => format!("{hash}{EXECUTABLE}"),
        "120000" => format!("link:{hash}"),
        _ => hash.to_owned(),
    }
}

/// List `changes` into `out`, or say `none`.
fn list(out: &mut String, changes: &[Change], none: &str) {
    if changes.is_empty() {
        out.push_str(none);
    }
    for change in changes.iter().take(MAX_LISTED) {
        let kind = match change.kind {
            ChangeKind::Added => "added   ",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted ",
        };
        out.push_str(&format!("  {kind} {}\n", shown(&change.path)));
    }
    if changes.len() > MAX_LISTED {
        out.push_str(&format!("  [{} more]\n", changes.len() - MAX_LISTED));
    }
}

/// One side of a file's patch.
enum Side {
    Missing,
    Text(String),
    Binary,
    Large,
    Link(String),
    Unreadable(String),
}

impl Side {
    fn of(bytes: Vec<u8>, large: bool, link: bool) -> Self {
        match () {
            _ if link => Self::Link(String::from_utf8_lossy(&bytes).into_owned()),
            _ if large => Self::Large,
            _ if is_binary(&bytes) => Self::Binary,
            _ => String::from_utf8(bytes).map_or(Self::Binary, Self::Text),
        }
    }

    /// Read `path` without following a final symlink; its parents come from a walk that follows none.
    fn read(path: &Path) -> Self {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => return Self::Unreadable(e.to_string()),
        };
        if meta.file_type().is_symlink() {
            return fs::read_link(path).map_or_else(
                |e| Self::Unreadable(e.to_string()),
                |t| Self::Link(t.as_os_str().to_string_lossy().into_owned()),
            );
        }
        if !meta.is_file() {
            return Self::Unreadable("not a regular file".into());
        }
        if meta.len() > MAX_DIFF_FILE {
            return Self::Large;
        }
        match fs::read(path) {
            Ok(bytes) => Self::of(bytes, false, false),
            Err(e) => Self::Unreadable(e.to_string()),
        }
    }
}

/// A unified diff of one file from `old` to `new`, as `fs.diff` writes one.
fn render(path: &str, old: Side, new: Side) -> String {
    let name = shown(path);
    let (a, b) = (format!("a/{name}"), format!("b/{name}"));
    match (old, new) {
        (Side::Unreadable(e), _) | (_, Side::Unreadable(e)) => format!("# {name}: cannot read: {e}\n"),
        (Side::Link(t), Side::Link(u)) => format!("# {name}: symlink to {t}, now to {u}\n"),
        (Side::Missing, Side::Link(t)) => format!("# {name}: added symlink to {t}\n"),
        (_, Side::Link(t)) => format!("# {name}: replaced by a symlink to {t}\n"),
        (Side::Link(_), Side::Missing) => format!("# {name}: deleted symlink\n"),
        (Side::Link(_), _) => format!("# {name}: symlink replaced by a file\n"),
        (Side::Large, _) | (_, Side::Large) => format!("# {name}: too large to show\n"),
        (Side::Binary, Side::Missing) => format!("Binary file {a} deleted\n"),
        (Side::Missing, Side::Binary) => format!("Binary file {b} added\n"),
        (Side::Binary, _) | (_, Side::Binary) => format!("Binary files {a} and {b} differ\n"),
        (old, new) => {
            let (old_name, old) = match old {
                Side::Text(t) => (a.as_str(), t),
                _ => ("/dev/null", String::new()),
            };
            let (new_name, new) = match new {
                Side::Text(t) => (b.as_str(), t),
                _ => ("/dev/null", String::new()),
            };
            let diff = TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(&old, &new);
            format!("--- {old_name}\n+++ {new_name}\n{}", diff.unified_diff())
        }
    }
}

/// A revision as a query may name it: a commit id, branch, tag or `HEAD`,
/// with `~`, `^` and `@{...}` suffixes. Never an option, a range (only a
/// log takes one, and splits it first) or `REV:PATH`, which would name a
/// file anywhere in the repository.
fn check_rev(rev: &str) -> Result<(), RemoteError> {
    let ok = !rev.is_empty()
        && rev.len() <= MAX_REV
        && !rev.starts_with('-')
        && !rev.contains("..")
        && rev.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./~^@{}+".contains(&b));
    if ok {
        return Ok(());
    }
    Err(invalid(format!(
        "{rev:?} is not a revision to read: name one commit, such as a commit id, a branch, a tag or HEAD~2"
    )))
}

fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(SHORT)]
}

fn on_branch(b: &Baseline) -> String {
    b.branch.as_ref().map(|name| format!(" on branch {name}")).unwrap_or_else(|| " (detached HEAD)".to_owned())
}

/// A path as an answer names it: quoted when it holds a control character,
/// so a newline in a name cannot pass for another line.
fn shown(path: &str) -> String {
    if path.chars().any(char::is_control) {
        format!("{path:?}")
    } else {
        path.to_owned()
    }
}

/// `text` cut after its last whole line within `limit` bytes, and whether it was cut.
fn clip(mut text: String, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let cut = text.as_bytes()[..limit].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    text.truncate(cut);
    (text, true)
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    line.strip_prefix("fatal: ").or_else(|| line.strip_prefix("error: ")).unwrap_or(line).to_owned()
}

/// What a git process printed, and how it ended.
struct Output {
    stdout: Vec<u8>,
    /// It printed more than the limit, and was stopped.
    cut: bool,
    /// It succeeded, or was stopped for printing too much.
    ok: bool,
    stderr: Vec<u8>,
}

impl Output {
    /// The output as text, cut at a line, and whether it was cut.
    fn text(self) -> (String, bool) {
        let text = String::from_utf8_lossy(&self.stdout).into_owned();
        if !self.cut {
            return (text, false);
        }
        (clip(text, self.stdout.len().saturating_sub(1)).0, true)
    }

    /// What git said went wrong.
    fn error(&self) -> String {
        let line = first_line(&self.stderr);
        if line.is_empty() {
            "it gave no reason".to_owned()
        } else {
            line
        }
    }
}

/// Git with everything that could run a program, page, use the network or
/// write turned off. With `dir`, in that git directory, which is its work
/// tree too, so nothing of the user's work tree or index is read; without
/// it, finding the repository `cwd` is in.
fn command(dir: Option<&Path>, cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.envs(ENV);
    if let Some(dir) = dir {
        cmd.env("GIT_DIR", dir).env("GIT_WORK_TREE", dir);
    }
    for setting in CONFIG {
        cmd.arg("-c").arg(setting);
    }
    cmd.current_dir(dir.unwrap_or(cwd)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

fn spawn(cmd: &mut Command) -> Result<Child, RemoteError> {
    cmd.spawn().map_err(|e| failed(format!("git could not be run ({e}); install git to read the history")))
}

/// Run `cmd` until it ends or `deadline`, keeping `limit` bytes of what it
/// prints; past that it is stopped.
fn run(cmd: &mut Command, limit: usize, deadline: Instant) -> Result<Output, RemoteError> {
    let mut child = spawn(cmd)?;
    let stderr = drain(child.stderr.take());
    let watch = Watchdog::start(&child, deadline);
    let mut stdout = Vec::new();
    let mut cut = false;
    if let Some(out) = child.stdout.take() {
        let _ = out.take(limit as u64 + 1).read_to_end(&mut stdout);
        if stdout.len() > limit {
            stdout.truncate(limit);
            cut = true;
            // It has more to say: stop it rather than read the rest.
            let _ = child.kill();
        }
    }
    let status = finish(child, watch, deadline);
    let stderr = stderr.join().unwrap_or_default();
    let status = status?;
    Ok(Output { stdout, cut, ok: cut || status.success(), stderr })
}

/// Wait for `child`, killed at `deadline`.
fn finish(mut child: Child, watch: Watchdog, deadline: Instant) -> Result<ExitStatus, RemoteError> {
    let timed_out = || failed(format!("git took longer than {} s and was stopped", QUERY_TIMEOUT.as_secs()));
    if watch.stop() {
        let _ = child.wait();
        return Err(timed_out());
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            Err(e) => return Err(failed(format!("waiting for git: {e}"))),
        }
    }
}

/// Read a pipe to its end on a thread of its own, keeping its first [`MAX_STDERR`] bytes.
fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = (&mut pipe).take(MAX_STDERR as u64).read_to_end(&mut kept);
            let _ = io::copy(&mut pipe, &mut io::sink());
        }
        kept
    })
}

/// Kills a child that is still running at its deadline, while its output
/// is read. Stopped before the child is waited for, so the pid it kills
/// is always the child's.
struct Watchdog {
    stopped: Arc<(Mutex<bool>, Condvar)>,
    thread: JoinHandle<bool>,
}

impl Watchdog {
    fn start(child: &Child, deadline: Instant) -> Self {
        let pid = child.id() as libc::pid_t;
        let stopped = Arc::new((Mutex::new(false), Condvar::new()));
        let state = stopped.clone();
        let thread = std::thread::spawn(move || {
            let (lock, wake) = &*state;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if *stopped {
                    return false;
                }
                let now = Instant::now();
                if now >= deadline {
                    // SAFETY: kill has no memory preconditions; the child is not reaped until `stop` returns.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    return true;
                }
                stopped = wake.wait_timeout(stopped, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
            }
        });
        Self { stopped, thread }
    }

    /// Stop watching; true when it killed the child.
    fn stop(self) -> bool {
        let (lock, wake) = &*self.stopped;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        self.thread.join().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_is_never_an_option_a_range_or_a_path() {
        for rev in ["HEAD~2", "main", "v1.0", "abc123", "HEAD^", "feature/x", "@{1}", "HEAD@{2}", "a+b"] {
            assert!(check_rev(rev).is_ok(), "{rev}");
        }
        for rev in ["", "--output=x", "-n", "a..b", "HEAD:src/lib.rs", ":/fix", "a b", "x\0", &"a".repeat(300)] {
            assert!(check_rev(rev).is_err(), "{rev:?}");
        }
    }

    #[test]
    fn a_blob_answer_is_hashed_and_a_missing_object_left_out() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let answer = format!("{a} blob 3\nhi\n\n{b} missing\n{c} tree 4\nxxxx\n");
        let mut found = Vec::new();
        read_blobs(&mut answer.as_bytes(), &[a.clone(), b, c], &mut found).unwrap();
        assert_eq!(found, [(a, hex::encode(Sha256::digest(b"hi\n")))]);
    }

    #[test]
    fn a_state_from_a_tree_matches_what_a_fork_records() {
        assert_eq!(state_of("100644", "ab"), "ab");
        assert_eq!(state_of("100755", "ab"), "ab+x");
        assert_eq!(state_of("120000", "ab"), "link:ab");
        assert_eq!(ancestors("a/b/c").collect::<Vec<_>>(), ["a/b/c", "a/b", "a"]);
        assert_eq!(shown("tab\there"), "\"tab\\there\"");
        assert_eq!(clip("one\ntwo\nthree\n".into(), 9), ("one\ntwo\n".into(), true));
    }
}

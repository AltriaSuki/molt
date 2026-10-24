//! Committing a run's applied result to git, and taking it back: what
//! `molt do --commit` and `molt undo` do.
//!
//! Git runs here, on the user's side, never in an attempt. A run's commit
//! holds exactly the files the run merged. It is built in a scratch index
//! from `HEAD`, so what the user has staged stays staged and out of it, and
//! its message ends with a [`RUN_TRAILER`] naming the run, which leads from
//! `git log` back to the run's audit trace and to the notes it recalled
//! (`molt memory used`). Undoing a run is a new commit that puts those files
//! back as they were before it, marked with an [`UNDO_TRAILER`]; history is
//! never rewritten.
//!
//! It also says what the workspace changed against `HEAD`, for the
//! interface's `/diff`: [`Repo::changes`] and [`Repo::file_diff`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context};
use molt_api::planner::{Outcome, RunResponse, Verification};

/// Names the run a commit holds the result of.
pub const RUN_TRAILER: &str = "Molt-Run";
/// Names the run a commit took back.
pub const UNDO_TRAILER: &str = "Molt-Undo";
/// The check the run's result passed.
pub const CHECK_TRAILER: &str = "Molt-Check";
/// What the run's result was held to: `command`, `judged`, `rules` or `none`.
pub const VERIFICATION_TRAILER: &str = "Molt-Verification";
/// A role of the run that spoke with a prompt of the user's pack, and that
/// prompt's version: `role=NAME prompt=NAME sha256=HEX`.
pub const PROMPT_TRAILER: &str = "Molt-Prompt";

/// Longest commit subject, in characters, before it is cut.
const SUBJECT: usize = 72;
/// Bytes of one file's diff that `/diff` shows; past this it is cut.
const FILE_DIFF_BYTES: usize = 4 << 20;
/// Bytes of an untracked file read to count its lines.
const COUNTED_BYTES: u64 = 16 << 20;
/// Options that keep `git diff` to plain text: no external diff, text
/// conversion or colour the repository's config chose.
const PLAIN_DIFF: [&str; 4] = ["--no-ext-diff", "--no-textconv", "--no-color", "--no-renames"];

/// A file the workspace changed against `HEAD`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Changed {
    /// Relative to the workspace.
    pub path: String,
    /// Lines added and removed; `None` for a binary file.
    pub lines: Option<(u64, u64)>,
    /// Not tracked: a new file git does not ignore, all of it added.
    pub untracked: bool,
}

/// One file's diff against `HEAD`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    pub patch: String,
    /// Cut at [`FILE_DIFF_BYTES`].
    pub truncated: bool,
}

/// The git work tree a workspace is in.
#[derive(Debug)]
pub struct Repo {
    /// The top of the work tree: git runs here, and paths are relative to it.
    top: PathBuf,
    /// Where the workspace is under `top`, as git writes it: empty or ending in `/`.
    prefix: String,
    filters: BTreeSet<String>,
}

/// What a commit of a run holds and says.
#[derive(Clone, Debug, Default)]
pub struct RunCommit<'a> {
    pub run: &'a str,
    /// The winning attempt's final message; the task when it is empty.
    pub summary: &'a str,
    pub task: &'a str,
    /// The check the result passed; `None` for an unverified result.
    pub check: Option<&'a str>,
    /// Its verification level; `None` when the planner did not say.
    pub verification: Option<Verification>,
    /// The files the run merged, relative to the workspace.
    pub paths: Vec<String>,
    /// Commit on this new branch, made from `HEAD` and switched to.
    pub branch: Option<&'a str>,
    /// The roles of the run and their prompts; those of the user's prompt
    /// pack get a trailer each.
    pub prompts: &'a [molt_api::role::PromptUse],
}

/// A commit made for a run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Committed {
    pub sha: String,
    pub subject: String,
    /// The branch it is on; `None` on a detached `HEAD`.
    pub branch: Option<String>,
    /// Merged files left out because git ignores them, relative to the workspace.
    pub ignored: Vec<String>,
}

/// A commit on `HEAD` that holds a run or takes one back.
struct Marked {
    sha: String,
    /// The runs whose result it holds.
    runs: Vec<String>,
    /// The runs it took back.
    undone: Vec<String>,
    subject: String,
}

/// What `molt undo` found for a run.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Undo {
    /// The commit was taken back by the new commit `sha`.
    Reverted { run: String, commit: String, sha: String },
    /// A commit marked with [`UNDO_TRAILER`] took the run back already.
    Already { run: String, sha: String },
}

impl Undo {
    pub fn run(&self) -> &str {
        match self {
            Self::Reverted { run, .. } | Self::Already { run, .. } => run,
        }
    }
}

/// A work tree as it was before a run: what `molt do --commit` and the
/// interface's `/commit on` commit the run's result in.
#[derive(Debug)]
pub struct Before {
    repo: Repo,
    /// Paths, relative to the workspace, with changes not committed then.
    dirty: BTreeSet<String>,
}

impl Before {
    /// Look at the work tree `workspace` is in, or say why there is none.
    pub fn look(workspace: &Path) -> anyhow::Result<Self> {
        let repo = Repo::open(workspace)?;
        let dirty = repo.dirty()?;
        Ok(Self { repo, dirty })
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Commit what the run `run` of `task` merged, as `resp` reports it.
    /// Refused when one of its files had changes not committed before the
    /// run: the commit would take them in, and undoing the run would take
    /// them back too.
    pub fn commit(
        &self,
        resp: &RunResponse,
        run: &str,
        task: &str,
        branch: Option<&str>,
    ) -> anyhow::Result<Option<Committed>> {
        let paths: Vec<String> = resp.changes.iter().map(|c| c.path.clone()).collect();
        let mixed: Vec<&str> = paths.iter().filter(|p| self.dirty.contains(*p)).map(String::as_str).collect();
        ensure!(
            mixed.is_empty(),
            "{} had changes of yours not committed before the run, which the commit would take in",
            mixed.join(", ")
        );
        let check = match resp.outcome {
            Outcome::Passed => resp.check.as_ref().map(|c| c.command.as_str()),
            _ => None,
        };
        let summary = resp.summary.as_str();
        let verification = resp.verification;
        let prompts = resp.prompts.as_slice();
        self.repo.commit_run(&RunCommit { run, summary, task, check, verification, paths, branch, prompts })
    }
}

impl Repo {
    /// The work tree `workspace` is in, or why there is none.
    pub fn open(workspace: &Path) -> anyhow::Result<Self> {
        let out = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree", "--show-toplevel", "--show-prefix"])
            .current_dir(workspace)
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .context("git could not be run; is it installed?")?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        if !out.status.success() || lines.next() != Some("true") {
            bail!("{} is not in a git work tree", workspace.display());
        }
        let top = PathBuf::from(lines.next().context("git did not name the top of the work tree")?);
        let prefix = lines.next().unwrap_or_default().to_owned();
        let mut repo = Self { top, prefix, filters: BTreeSet::new() };
        let out = repo.command().args(["config", "--name-only", "--get-regexp", r"^filter\."]).output()?;
        ensure!(out.status.success() || out.status.code() == Some(1), "could not inspect Git filters");
        repo.filters = String::from_utf8(out.stdout)?
            .lines()
            .filter_map(|name| name.rsplit_once('.').map(|(prefix, _)| prefix.to_owned()))
            .collect();
        Ok(repo)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        // Paths are file names, never patterns; `-c` keeps paths with unusual characters unquoted.
        cmd.current_dir(&self.top).env("GIT_LITERAL_PATHSPECS", "1").env_remove("GIT_INDEX_FILE").args([
            "-c",
            "core.quotePath=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
        // Candidate attributes can select a configured executable filter. Host
        // plumbing must not execute candidate code outside the shell sandbox.
        for name in &self.filters {
            for (setting, value) in
                [("clean", "/bin/cat"), ("smudge", "/bin/cat"), ("process", ""), ("required", "false")]
            {
                cmd.arg("-c").arg(format!("{name}.{setting}={value}"));
            }
        }
        cmd
    }

    /// Run git with `args`, in the scratch `index` when one is given and
    /// with `input` on stdin, and return what it printed.
    fn git(&self, args: &[&str], index: Option<&Path>, input: Option<&[u8]>) -> anyhow::Result<String> {
        let mut cmd = self.command();
        cmd.args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().context("git could not be run; is it installed?")?;
        if let Some(input) = input {
            child.stdin.take().expect("stdin is piped").write_all(input)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("git {} failed: {}", args.first().copied().unwrap_or_default(), err.trim());
        }
        String::from_utf8(out.stdout).context("git printed something that is not UTF-8")
    }

    /// The commit `rev` names, or `None` when it names none (an unborn `HEAD`).
    fn commit_of(&self, rev: &str) -> anyhow::Result<Option<String>> {
        let out = self.command().args(["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")]).output()?;
        Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
    }

    /// `path`, relative to the workspace, relative to the top of the work tree.
    fn top_relative(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    /// Paths, relative to the workspace, with changes not committed: staged,
    /// unstaged or untracked. Those outside the workspace are left out.
    pub fn dirty(&self) -> anyhow::Result<BTreeSet<String>> {
        let out = self.git(&["status", "--porcelain", "-z", "--untracked-files=all", "--no-renames"], None, None)?;
        Ok(out
            .split('\0')
            .filter_map(|entry| entry.get(3..))
            .filter_map(|path| path.strip_prefix(&self.prefix))
            .map(str::to_owned)
            .collect())
    }

    /// What the workspace changed against `HEAD`, by path: tracked files
    /// that differ from it, staged or not, and untracked files git does not
    /// ignore. Refused when `HEAD` has no commit yet.
    pub fn changes(&self) -> anyhow::Result<Vec<Changed>> {
        ensure!(self.commit_of("HEAD")?.is_some(), "the repository has no commit yet to compare with");
        let mut args = vec!["diff", "HEAD", "--no-relative", "--numstat", "-z"];
        args.extend(PLAIN_DIFF);
        args.extend(self.within());
        let mut changes = Vec::new();
        for entry in self.git(&args, None, None)?.split('\0').filter(|e| !e.is_empty()) {
            let mut fields = entry.splitn(3, '\t');
            let (Some(added), Some(removed), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
                bail!("git diff --numstat printed {entry:?}, which is not a count and a path");
            };
            let Some(path) = path.strip_prefix(&self.prefix) else { continue };
            let lines = added.parse().ok().zip(removed.parse().ok());
            changes.push(Changed { path: path.to_owned(), lines, untracked: false });
        }
        let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
        args.extend(self.within());
        for path in self.git(&args, None, None)?.split('\0').filter(|e| !e.is_empty()) {
            let Some(path) = path.strip_prefix(&self.prefix) else { continue };
            let lines = new_lines(&self.top.join(self.top_relative(path))).map(|n| (n, 0));
            changes.push(Changed { path: path.to_owned(), lines, untracked: true });
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    /// The diff of one file of [`Repo::changes`] against `HEAD`.
    pub fn file_diff(&self, change: &Changed) -> anyhow::Result<FileDiff> {
        let path = self.top_relative(&change.path);
        let mut cmd = self.command();
        if change.untracked {
            cmd.args(["diff", "--no-index"]).args(PLAIN_DIFF).args(["--", "/dev/null", &path]);
        } else {
            cmd.args(["diff", "HEAD", "--no-relative"]).args(PLAIN_DIFF).args(["--", &path]);
        }
        let out = cmd.stdin(Stdio::null()).output().context("git could not be run; is it installed?")?;
        // `--no-index` exits 1 when the files differ, as a new file does.
        let differ = change.untracked && out.status.code() == Some(1);
        if !out.status.success() && !differ {
            bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let truncated = out.stdout.len() > FILE_DIFF_BYTES;
        let shown = &out.stdout[..out.stdout.len().min(FILE_DIFF_BYTES)];
        Ok(FileDiff { patch: String::from_utf8_lossy(shown).into_owned(), truncated })
    }

    /// The pathspec that keeps git to the workspace, when it is below the top of the work tree.
    fn within(&self) -> Vec<&str> {
        if self.prefix.is_empty() {
            Vec::new()
        } else {
            vec!["--", &self.prefix]
        }
    }

    /// Refuse a `--branch` name git would not take, or one that exists already.
    pub fn check_new_branch(&self, name: &str) -> anyhow::Result<()> {
        let ok = self.command().args(["check-ref-format", "--branch", name]).output()?.status.success();
        ensure!(ok && !name.starts_with('-'), "{name:?} is not a branch name git accepts");
        ensure!(self.commit_of(&format!("refs/heads/{name}"))?.is_none(), "the branch {name} exists already");
        Ok(())
    }

    /// Commit exactly `c.paths` as the work tree has them, on top of `HEAD`
    /// (or on the new branch `c.branch`), and bring the index entries of
    /// those paths, and only those, up to it. `None` when the files are as
    /// `HEAD` has them already.
    pub fn commit_run(&self, c: &RunCommit) -> anyhow::Result<Option<Committed>> {
        if let Some(name) = c.branch {
            self.check_new_branch(name)?;
        }
        let head = self.commit_of("HEAD")?;
        let mut paths: Vec<String> = c.paths.iter().map(|p| self.top_relative(p)).collect();
        paths.sort();
        paths.dedup();
        let ignored = self.ignored(&paths)?;
        paths.retain(|p| !ignored.contains(p));
        let scratch = Scratch::new(self)?;
        if head.is_some() {
            self.git(&["read-tree", "HEAD"], Some(&scratch.0), None)?;
        }
        let (present, gone): (Vec<&String>, Vec<&String>) = paths.iter().partition(|p| {
            let path = self.top.join(p);
            path.symlink_metadata().is_ok()
        });
        if !present.is_empty() {
            let mut args = vec!["add", "--"];
            args.extend(present.iter().map(|p| p.as_str()));
            self.git(&args, Some(&scratch.0), None)?;
        }
        if !gone.is_empty() {
            let mut args = vec!["rm", "--cached", "-q", "--ignore-unmatch", "--"];
            args.extend(gone.iter().map(|p| p.as_str()));
            self.git(&args, Some(&scratch.0), None)?;
        }
        let tree = self.git(&["write-tree"], Some(&scratch.0), None)?.trim().to_owned();
        drop(scratch);
        if let Some(head) = &head {
            if self.git(&["rev-parse", &format!("{head}^{{tree}}")], None, None)?.trim() == tree {
                return Ok(None);
            }
        }
        let (subject, message) = run_message(c);
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(head) = &head {
            args.extend(["-p", head.as_str()]);
        }
        let sha = self.git(&args, None, Some(message.as_bytes()))?.trim().to_owned();
        let reflog = format!("molt: run {}", c.run);
        match c.branch {
            Some(name) => {
                let r = format!("refs/heads/{name}");
                self.git(&["update-ref", "-m", &reflog, &r, &sha, ""], None, None)?;
                self.git(&["symbolic-ref", "-m", &reflog, "HEAD", &r], None, None)?;
            }
            None => {
                let old = head.as_deref().unwrap_or("");
                self.git(&["update-ref", "-m", &reflog, "HEAD", &sha, old], None, None)?;
            }
        }
        self.index_from(&sha, &paths)?;
        let branch = self.git(&["symbolic-ref", "-q", "--short", "HEAD"], None, None).ok();
        let ignored = ignored.iter().map(|p| p.strip_prefix(&self.prefix).unwrap_or(p).to_owned()).collect();
        Ok(Some(Committed { sha, subject, branch: branch.map(|b| b.trim().to_owned()), ignored }))
    }

    /// Those of `paths` git ignores and does not track.
    fn ignored(&self, paths: &[String]) -> anyhow::Result<BTreeSet<String>> {
        if paths.is_empty() {
            return Ok(BTreeSet::new());
        }
        let input = paths.join("\0");
        let out = self
            .command()
            // It takes paths, not pathspecs, and refuses the literal setting.
            .env_remove("GIT_LITERAL_PATHSPECS")
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().expect("stdin is piped").write_all(input.as_bytes())?;
                child.wait_with_output()
            })?;
        // Exit status 1 means none is ignored.
        ensure!(
            out.status.code().is_some_and(|code| code <= 1),
            "git check-ignore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()).map(str::to_owned).collect())
    }

    /// Set the index entries of `paths` to what the commit `rev` (a full sha) has, removing
    /// those it does not have, and leave every other entry alone.
    fn index_from(&self, rev: &str, paths: &[String]) -> anyhow::Result<()> {
        let info = self.entries(rev, paths)?;
        let mut input = String::new();
        for path in paths {
            match info.get(path) {
                Some(entry) => input.push_str(&format!("{entry}\t{path}\0")),
                None => input.push_str(&format!("0 {}\t{path}\0", "0".repeat(rev.len()))),
            }
        }
        if !input.is_empty() {
            self.git(&["update-index", "-z", "--index-info"], None, Some(input.as_bytes()))?;
        }
        // Refresh the stat data of the entries just written, so that unchanged files do not show as modified.
        let _ = self.git(&["update-index", "-q", "--refresh"], None, None);
        Ok(())
    }

    /// `paths` as the commit `rev` has them: `mode type object` by path.
    fn entries(&self, rev: &str, paths: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
        if paths.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut args = vec!["ls-tree", "-z", "--full-tree", rev, "--"];
        args.extend(paths.iter().map(String::as_str));
        let out = self.git(&args, None, None)?;
        Ok(out
            .split('\0')
            .filter_map(|line| line.split_once('\t'))
            .map(|(entry, path)| (path.to_owned(), entry.to_owned()))
            .collect())
    }

    /// The commits on `HEAD` that hold a run or take one back, newest first.
    fn run_commits(&self) -> anyhow::Result<Vec<Marked>> {
        if self.commit_of("HEAD")?.is_none() {
            return Ok(Vec::new());
        }
        let format = format!(
            "--format=%H%x1f%(trailers:key={RUN_TRAILER},valueonly,separator=%x1e)%x1f\
             %(trailers:key={UNDO_TRAILER},valueonly,separator=%x1e)%x1f%s"
        );
        let grep = format!("--grep=^({RUN_TRAILER}|{UNDO_TRAILER}): ");
        let out = self.git(&["log", "-z", "-E", &grep, &format, "HEAD", "--"], None, None)?;
        let split = |s: &str| -> Vec<String> {
            s.split('\x1e').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect()
        };
        Ok(out
            .split('\0')
            .filter_map(|record| {
                let mut fields = record.trim_start_matches('\n').splitn(4, '\x1f');
                let sha = fields.next().filter(|s| !s.is_empty())?.to_owned();
                let runs = split(fields.next()?);
                let undone = split(fields.next()?);
                Some(Marked { sha, runs, undone, subject: fields.next().unwrap_or_default().to_owned() })
            })
            .collect())
    }

    /// Take back the commit of `run`, or of the newest run on `HEAD` not
    /// yet taken back: commit the files it changed as they were before it.
    /// Refused when one of them changed since, or has changes not committed.
    pub fn undo(&self, run: Option<&str>) -> anyhow::Result<Undo> {
        let commits = self.run_commits()?;
        let undone: BTreeMap<&str, &str> =
            commits.iter().rev().flat_map(|c| c.undone.iter().map(|r| (r.as_str(), c.sha.as_str()))).collect();
        let found = match run {
            Some(run) => {
                if let Some(sha) = undone.get(run) {
                    return Ok(Undo::Already { run: run.to_owned(), sha: (*sha).to_owned() });
                }
                commits.iter().find(|c| c.runs.iter().any(|r| r == run)).map(|c| (run, c))
            }
            None => commits
                .iter()
                .find_map(|c| c.runs.iter().find(|r| !undone.contains_key(r.as_str())).map(|r| (r.as_str(), c))),
        };
        let Some((run, Marked { sha: commit, subject, .. })) = found else {
            match run {
                Some(run) => {
                    bail!("no commit on this branch holds run {run} (none has a `{RUN_TRAILER}: {run}` trailer)")
                }
                None => bail!("no commit on this branch holds a run that is not undone yet"),
            }
        };
        let parent = self.commit_of(&format!("{commit}^"))?;
        let paths: Vec<String> = self
            .git(
                &["diff-tree", "-r", "-z", "--no-renames", "--no-commit-id", "--name-only", "--root", commit],
                None,
                None,
            )?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        let head = self.commit_of("HEAD")?.context("HEAD names no commit")?;
        let (now, then) = (self.entries(&head, &paths)?, self.entries(commit, &paths)?);
        let moved: Vec<&str> = paths.iter().filter(|p| now.get(*p) != then.get(*p)).map(String::as_str).collect();
        ensure!(
            moved.is_empty(),
            "{} changed after run {run}; take its commit back by hand with `git revert {}`",
            moved.join(", "),
            &commit[..12.min(commit.len())]
        );
        let mut args = vec!["status", "--porcelain", "-z", "--untracked-files=all", "--no-renames", "--"];
        args.extend(paths.iter().map(String::as_str));
        let dirty: Vec<String> =
            self.git(&args, None, None)?.split('\0').filter_map(|e| e.get(3..)).map(str::to_owned).collect();
        ensure!(dirty.is_empty(), "{} has changes not committed; commit or stash them first", dirty.join(", "));

        let before = match &parent {
            Some(parent) => self.entries(parent, &paths)?,
            None => BTreeMap::new(),
        };
        let zero = "0".repeat(head.len());
        let mut input = String::new();
        for path in &paths {
            match before.get(path) {
                Some(entry) => input.push_str(&format!("{entry}\t{path}\0")),
                None => input.push_str(&format!("0 {zero}\t{path}\0")),
            }
        }
        let scratch = Scratch::new(self)?;
        self.git(&["read-tree", "HEAD"], Some(&scratch.0), None)?;
        self.git(&["update-index", "-z", "--index-info"], Some(&scratch.0), Some(input.as_bytes()))?;
        let tree = self.git(&["write-tree"], Some(&scratch.0), None)?.trim().to_owned();
        drop(scratch);
        let message = format!(
            "Revert \"{subject}\"\n\nThis reverts commit {commit}, the result of run {run}.\n\n{UNDO_TRAILER}: {run}\n"
        );
        let sha = self.git(&["commit-tree", &tree, "-p", &head], None, Some(message.as_bytes()))?.trim().to_owned();
        self.git(&["update-ref", "-m", &format!("molt: undo run {run}"), "HEAD", &sha, &head], None, None)?;
        // The index and the files follow: put back what was there, remove what was not.
        self.index_from(&sha, &paths)?;
        let restore: Vec<&str> = paths.iter().filter(|p| before.contains_key(*p)).map(String::as_str).collect();
        if !restore.is_empty() {
            let mut args = vec!["checkout-index", "-f", "-q", "--"];
            args.extend(restore);
            self.git(&args, None, None)?;
        }
        for path in paths.iter().filter(|p| !before.contains_key(*p)) {
            let path = self.top.join(path);
            if path.symlink_metadata().is_ok() {
                std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
            }
        }
        Ok(Undo::Reverted { run: run.to_owned(), commit: commit.clone(), sha })
    }
}

/// A scratch index file in the git dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(repo: &Repo) -> anyhow::Result<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let name = format!("molt-index-{}-{nanos}", std::process::id());
        let path = repo.git(&["rev-parse", "--git-path", &name], None, None)?.trim().to_owned();
        Ok(Self(repo.top.join(path)))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Remove paired emphasis without changing literal code or identifier underscores.
/// The lines of a new file, as a diff would add them; `None` for one that
/// looks binary, or is too large to read for a count.
fn new_lines(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        // Git keeps a link as its target: one line.
        return Some(1);
    }
    if !meta.is_file() || meta.len() > COUNTED_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    Some(newlines + u64::from(bytes.last().is_some_and(|&b| b != b'\n')))
}

fn plain_summary(text: &str) -> String {
    fn unmark(text: &str, marker: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find(marker) {
            let prefix = &rest[..start];
            let tail = &rest[start + marker.len()..];
            let paired = tail.find(marker).filter(|&end| {
                let content = &tail[..end];
                !content.is_empty()
                    && content.trim() == content
                    && prefix.chars().next_back().is_none_or(|c| !c.is_alphanumeric() && c != '_')
                    && tail[end + marker.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_')
            });
            out.push_str(prefix);
            match paired {
                Some(end) => {
                    out.push_str(&tail[..end]);
                    rest = &tail[end + marker.len()..];
                }
                None => {
                    out.push_str(marker);
                    rest = tail;
                }
            }
        }
        out.push_str(rest);
        out
    }
    let mut fenced: Option<(u8, usize)> = None;
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start().as_bytes();
            let delimiter = trimmed.first().copied().filter(|c| *c == b'`' || *c == b'~');
            let count = delimiter.map_or(0, |c| trimmed.iter().take_while(|&&byte| byte == c).count());
            if let Some((active, length)) = fenced {
                if delimiter == Some(active) && count >= length {
                    fenced = None;
                }
                return line.to_owned();
            }
            if count >= 3 {
                fenced = Some((delimiter.unwrap(), count));
                return line.to_owned();
            }
            let mut out = String::new();
            let mut rest = line;
            while let Some(at) = rest.find('`') {
                let mut plain = rest[..at].to_owned();
                for marker in ["**", "__", "*", "_"] {
                    plain = unmark(&plain, marker);
                }
                out.push_str(&plain);
                let run = rest[at..].bytes().take_while(|&b| b == b'`').count();
                let delimiter = &rest[at..at + run];
                match rest[at + run..].find(delimiter) {
                    Some(end) => {
                        let end = at + run + end + run;
                        out.push_str(&rest[at..end]);
                        rest = &rest[end..];
                    }
                    None => {
                        out.push_str(&rest[at..]);
                        rest = "";
                    }
                }
            }
            let mut plain = rest.to_owned();
            for marker in ["**", "__", "*", "_"] {
                plain = unmark(&plain, marker);
            }
            out.push_str(&plain);
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The subject and the whole message of a run's commit: the summary's first
/// line, cut to [`SUBJECT`] characters, then the rest of it, then the trailers.
fn run_message(c: &RunCommit) -> (String, String) {
    let summary = c.summary.trim();
    let text = if summary.is_empty() { c.task.trim() } else { summary };
    let text = plain_summary(text);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default().trim().trim_start_matches('#').trim();
    let first = if first.is_empty() { "Apply the result of a molt run" } else { first };
    let mut subject: String = first.chars().take(SUBJECT).collect();
    let mut body = String::new();
    if subject.chars().count() < first.chars().count() {
        if let Some((at, _)) = subject.char_indices().rfind(|(_, c)| c.is_whitespace()) {
            if at > 0 {
                subject.truncate(at);
            }
        }
        body.push_str(first);
        body.push('\n');
    }
    let rest = lines.collect::<Vec<_>>().join("\n");
    let rest = rest.trim();
    if !rest.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(rest);
        body.push('\n');
    }
    // A trailer value is one line.
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut message = format!("{subject}\n\n");
    if !body.is_empty() {
        message.push_str(&body);
        message.push('\n');
    }
    message.push_str(&format!("{RUN_TRAILER}: {}\n", c.run));
    if let Some(level) = c.verification {
        message.push_str(&format!("{VERIFICATION_TRAILER}: {level}\n"));
    }
    match c.check {
        Some(check) => message.push_str(&format!("{CHECK_TRAILER}: {}\n", one_line(check))),
        None => message.push_str(&format!("{CHECK_TRAILER}: none (unverified)\n")),
    }
    // The built-in prompts are the planner's version's; a prompt of the user's is named by its hash.
    for used in c.prompts.iter().filter(|p| p.prompt.source == molt_api::role::Source::User) {
        let (role, prompt) = (one_line(&used.role), one_line(&used.prompt.name));
        message.push_str(&format!("{PROMPT_TRAILER}: role={role} prompt={prompt} sha256={}\n", used.prompt.sha256));
    }
    (subject, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_messages_cut_at_word_boundaries_and_remove_emphasis_without_changing_code() {
        let summary = format!("**{}ending**\n\nUse **bold** and __strong__, preserve `**literal**` and MAX_SIZE.\n```rust\nlet x = \"**code**\";\n```", "word ".repeat(15));
        let (subject, message) = run_message(&RunCommit { run: "trace_test", summary: &summary, ..Default::default() });
        assert!(subject.chars().count() <= SUBJECT && !subject.ends_with('…'));
        assert_eq!(subject, "word ".repeat(14).trim());
        assert!(message.contains("Use bold and strong, preserve `**literal**` and MAX_SIZE."), "{message}");
        assert!(message.contains("let x = \"**code**\";"), "{message}");
        assert!(message.contains("Molt-Run: trace_test"));
        let (subject, _) =
            run_message(&RunCommit { run: "trace_test", summary: &"界".repeat(90), ..Default::default() });
        assert_eq!(subject.chars().count(), SUBJECT);
    }

    #[test]
    fn a_run_commit_names_its_verification_level_in_a_trailer_after_the_run() {
        let c = RunCommit {
            run: "trace_1",
            summary: "Fix it",
            check: Some("cargo test"),
            verification: Some(Verification::Command),
            ..Default::default()
        };
        let (_, message) = run_message(&c);
        assert!(
            message.ends_with("Molt-Run: trace_1\nMolt-Verification: command\nMolt-Check: cargo test\n"),
            "{message}"
        );
        let (_, message) = run_message(&RunCommit { verification: Some(Verification::None), check: None, ..c });
        assert!(message.contains("Molt-Verification: none\nMolt-Check: none (unverified)\n"), "{message}");
    }

    #[test]
    fn a_run_commit_names_each_prompt_of_the_users_pack_by_its_hash() {
        use molt_api::role::{PromptUse, PromptVersion, Source};
        let used = |role: &str, source| PromptUse {
            role: role.into(),
            prompt: PromptVersion { name: role.into(), sha256: "ab".repeat(32), source },
        };
        let prompts = [used("designer", Source::Builtin), used("terse", Source::User)];
        let c = RunCommit {
            run: "trace_1",
            summary: "Fix it",
            check: Some("make"),
            prompts: &prompts,
            ..Default::default()
        };
        let (_, message) = run_message(&c);
        let trailer = format!("Molt-Check: make\nMolt-Prompt: role=terse prompt=terse sha256={}\n", "ab".repeat(32));
        assert!(message.ends_with(&trailer), "{message}");
        assert!(!message.contains("role=designer"), "built-in prompts get no trailer");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).env_remove("GIT_INDEX_FILE").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// A repository with one commit holding `a.txt` and `b.txt`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        git(d, &["config", "user.name", "Test"]);
        git(d, &["config", "user.email", "test@example.com"]);
        git(d, &["config", "commit.gpgsign", "false"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "start"]);
        dir
    }

    fn commit<'a>(run: &'a str, paths: &[&str]) -> RunCommit<'a> {
        RunCommit {
            run,
            summary: "Fix the parser\n\nIt now accepts empty input.",
            task: "fix the parser",
            check: Some("cargo test\n  -q"),
            verification: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            branch: None,
            prompts: &[],
        }
    }

    #[test]
    fn committing_and_undoing_do_not_execute_candidate_selected_git_programs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let d = dir.path();
        git(d, &["config", "filter.candidate.clean", "touch filter-ran; cat"]);
        git(d, &["config", "filter.candidate.smudge", "touch filter-ran; cat"]);
        git(d, &["config", "core.hooksPath", "hooks"]);
        std::fs::create_dir(d.join("hooks")).unwrap();
        let hook = d.join("hooks/reference-transaction");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let repo = Repo::open(d).unwrap();
        std::fs::write(d.join(".gitattributes"), "*.txt filter=candidate\n").unwrap();
        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        repo.commit_run(&commit("guarded", &["a.txt", ".gitattributes"])).unwrap().unwrap();
        repo.undo(Some("guarded")).unwrap();
        assert!(!d.join("filter-ran").exists());
        assert!(!d.join("hook-ran").exists());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a\n");
    }

    #[test]
    fn a_run_commit_holds_only_the_merged_files_and_leaves_what_is_staged_alone() {
        let dir = repo();
        let d = dir.path();
        // The user's own change, staged before the run.
        std::fs::write(d.join("b.txt"), "mine\n").unwrap();
        git(d, &["add", "b.txt"]);
        let repo = Repo::open(d).unwrap();
        assert_eq!(repo.dirty().unwrap(), BTreeSet::from(["b.txt".to_owned()]));
        // The run changes a.txt, adds c.txt and deletes nothing else.
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        std::fs::write(d.join("c.txt"), "c\n").unwrap();
        let done = repo.commit_run(&commit("trace_1", &["a.txt", "c.txt"])).unwrap().unwrap();
        assert_eq!(done.subject, "Fix the parser");
        assert_eq!(done.branch.as_deref(), Some("main"));
        assert_eq!(git(d, &["show", "--name-only", "--format=", "HEAD"]), "a.txt\nc.txt\n");
        let message = git(d, &["log", "-1", "--format=%B"]);
        assert_eq!(
            message,
            "Fix the parser\n\nIt now accepts empty input.\n\nMolt-Run: trace_1\nMolt-Check: cargo test -q\n\n"
        );
        assert_eq!(git(d, &["log", "-1", "--format=%(trailers:key=Molt-Run,valueonly)"]).trim(), "trace_1");
        // The staged change is still staged, and nothing else shows as changed.
        assert_eq!(git(d, &["status", "--porcelain"]), "M  b.txt\n");
    }

    #[test]
    fn undo_reverts_the_run_commit_and_restores_its_files() {
        let dir = repo();
        let d = dir.path();
        let repo = Repo::open(d).unwrap();
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        std::fs::write(d.join("c.txt"), "c\n").unwrap();
        std::fs::remove_file(d.join("b.txt")).unwrap();
        let run = repo.commit_run(&commit("trace_1", &["a.txt", "b.txt", "c.txt"])).unwrap().unwrap();
        // Something staged elsewhere stays out of the revert.
        std::fs::write(d.join("d.txt"), "d\n").unwrap();
        git(d, &["add", "d.txt"]);

        let Undo::Reverted { run: undone, commit, sha } = repo.undo(None).unwrap() else { panic!("not reverted") };
        assert_eq!((undone.as_str(), commit.as_str()), ("trace_1", run.sha.as_str()));
        assert_eq!(git(d, &["rev-parse", "HEAD"]).trim(), sha);
        assert_eq!(git(d, &["rev-parse", "HEAD^{tree}"]), git(d, &["rev-parse", "HEAD~2^{tree}"]));
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a\n");
        assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "b\n");
        assert!(!d.join("c.txt").exists());
        assert_eq!(git(d, &["status", "--porcelain"]), "A  d.txt\n");
        assert!(git(d, &["log", "-1", "--format=%B"]).contains("Molt-Undo: trace_1"));
        // Asked again, it says the run was undone already.
        assert_eq!(repo.undo(Some("trace_1")).unwrap(), Undo::Already { run: "trace_1".into(), sha: sha.clone() });
        assert!(repo.undo(None).is_err(), "no run left to undo");
    }

    #[test]
    fn undo_refuses_files_that_changed_since_the_run() {
        let dir = repo();
        let d = dir.path();
        let repo = Repo::open(d).unwrap();
        std::fs::write(d.join("a.txt"), "a2\n").unwrap();
        repo.commit_run(&commit("trace_1", &["a.txt"])).unwrap().unwrap();
        std::fs::write(d.join("a.txt"), "edited\n").unwrap();
        let err = repo.undo(Some("trace_1")).unwrap_err().to_string();
        assert!(err.contains("a.txt has changes not committed"), "{err}");
        git(d, &["commit", "-q", "-am", "later"]);
        let err = repo.undo(Some("trace_1")).unwrap_err().to_string();
        assert!(err.contains("a.txt changed after run trace_1"), "{err}");
        assert!(repo.undo(Some("trace_9")).unwrap_err().to_string().contains("no commit on this branch holds run"));
    }

    #[test]
    fn a_run_commit_can_go_on_a_new_branch_from_a_subdirectory() {
        let dir = repo();
        let d = dir.path();
        std::fs::create_dir(d.join("sub")).unwrap();
        let repo = Repo::open(&d.join("sub")).unwrap();
        std::fs::write(d.join("sub/x.txt"), "x\n").unwrap();
        let mut c = commit("trace_2", &["x.txt"]);
        c.branch = Some("molt/fix");
        assert!(repo.check_new_branch("main").is_err());
        assert!(repo.check_new_branch("no..dots").is_err());
        let done = repo.commit_run(&c).unwrap().unwrap();
        assert_eq!(done.branch.as_deref(), Some("molt/fix"));
        assert_eq!(git(d, &["show", "--name-only", "--format=", "HEAD"]), "sub/x.txt\n");
        assert_eq!(git(d, &["rev-parse", "main"]), git(d, &["rev-parse", "HEAD~1"]));
        assert_eq!(git(d, &["status", "--porcelain"]), "");
        // Committing what HEAD has already is nothing to commit.
        assert_eq!(repo.commit_run(&commit("trace_3", &["x.txt"])).unwrap(), None);
    }

    #[test]
    fn changes_against_head_list_tracked_staged_and_new_files_with_their_lines() {
        let dir = repo();
        let d = dir.path();
        std::fs::write(d.join("a.txt"), "a\nmore\n").unwrap();
        std::fs::write(d.join("b.txt"), "staged\n").unwrap();
        git(d, &["add", "b.txt"]);
        std::fs::write(d.join("new file.txt"), "one\ntwo").unwrap();
        std::fs::write(d.join("blob.bin"), b"\0\x01").unwrap();
        std::fs::write(d.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(d.join("ignored.txt"), "left out\n").unwrap();
        let repo = Repo::open(d).unwrap();
        let changes = repo.changes().unwrap();
        let listed: Vec<_> = changes.iter().map(|c| (c.path.as_str(), c.lines, c.untracked)).collect();
        assert_eq!(
            listed,
            [
                (".gitignore", Some((1, 0)), true),
                ("a.txt", Some((1, 0)), false),
                ("b.txt", Some((1, 1)), false),
                ("blob.bin", None, true),
                ("new file.txt", Some((2, 0)), true),
            ]
        );
        let diff = repo.file_diff(&changes[1]).unwrap();
        assert!(diff.patch.contains("+more") && !diff.truncated, "{}", diff.patch);
        let diff = repo.file_diff(&changes[4]).unwrap();
        assert!(diff.patch.contains("+one") && diff.patch.contains("new file mode"), "{}", diff.patch);
    }

    #[test]
    fn changes_stay_inside_a_workspace_below_the_top_and_need_a_commit() {
        let dir = repo();
        let d = dir.path();
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/inner.txt"), "x\n").unwrap();
        std::fs::write(d.join("a.txt"), "outside\n").unwrap();
        let repo = Repo::open(&d.join("sub")).unwrap();
        let changes = repo.changes().unwrap();
        assert_eq!(changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["inner.txt"]);
        assert!(repo.file_diff(&changes[0]).unwrap().patch.contains("+x"));

        let empty = tempfile::tempdir().unwrap();
        git(empty.path(), &["init", "-q"]);
        let err = Repo::open(empty.path()).unwrap().changes().unwrap_err().to_string();
        assert!(err.contains("no commit yet"), "{err}");
    }

    #[test]
    fn a_workspace_outside_git_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let err = Repo::open(dir.path()).unwrap_err().to_string();
        assert!(err.contains("is not in a git work tree"), "{err}");
    }
}

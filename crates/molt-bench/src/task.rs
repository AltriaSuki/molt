//! Benchmark tasks: loading them, hashing them, and laying their files out
//! in a workspace. `bench/README.md` describes the format.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A task's `timeout_s` when it gives none.
pub const DEFAULT_TIMEOUT_S: u64 = 120;
/// The longest `timeout_s` a task may ask for.
pub const MAX_TIMEOUT_S: u64 = 3600;
/// Names never read from a task directory: version control, and the build
/// output and caches that running a task's tests in place leaves behind.
pub const SKIPPED: &[&str] = &[".git", "__pycache__", "target", "node_modules", "Cargo.lock"];

macro_rules! names {
    ($(#[$doc:meta])* $ty:ident { $($variant:ident = $name:literal),* $(,)? }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum $ty {
            $(#[serde(rename = $name)] $variant),*
        }

        impl $ty {
            pub const ALL: &'static [$ty] = &[$($ty::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($ty::$variant => $name),*
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, String> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| {
                    let names: Vec<&str> = Self::ALL.iter().map(|v| v.as_str()).collect();
                    format!("{s:?} is not one of {}", names.join(", "))
                })
            }
        }
    };
}

names!(
    /// The language of a task's project.
    Language { Python = "python", Javascript = "javascript", Rust = "rust", Go = "go" }
);
names!(
    /// What kind of work a task asks for.
    Kind { Bugfix = "bugfix", Feature = "feature", Refactor = "refactor", Build = "build" }
);
names!(Difficulty { Easy = "easy", Medium = "medium", Hard = "hard" });
names!(
    /// Which set a task belongs to: `dev` tasks are what an improver may
    /// learn from, `heldout` tasks measure whether that generalizes.
    Split { Dev = "dev", Heldout = "heldout" }
);

/// A task's `task.toml`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub title: String,
    pub language: Language,
    pub kind: Kind,
    pub difficulty: Difficulty,
    pub split: Split,
    /// The repo's own tests, which pass before and after the task.
    pub tests: String,
    /// The grading command, run after the hidden files are laid over the workspace.
    pub check: String,
    /// Seconds one run of `tests` or `check` may take.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// What the agent is asked to do.
    pub prompt: String,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

#[derive(Clone, Debug)]
pub struct Task {
    /// The directory's name.
    pub id: String,
    pub dir: PathBuf,
    pub spec: Spec,
    /// SHA-256 of what decides a result: `task.toml`, `repo/` and `hidden/`.
    pub hash: String,
}

impl Task {
    pub fn load(dir: &Path) -> anyhow::Result<Task> {
        let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        ensure!(valid_id(&id), "{}: a task id is lowercase letters, digits and dashes", dir.display());
        let path = dir.join("task.toml");
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let spec: Spec = toml::from_str(&text).with_context(|| format!("{}", path.display()))?;
        for (field, value) in
            [("title", &spec.title), ("tests", &spec.tests), ("check", &spec.check), ("prompt", &spec.prompt)]
        {
            ensure!(!value.trim().is_empty(), "{}: {field} is empty", path.display());
        }
        ensure!(
            (1..=MAX_TIMEOUT_S).contains(&spec.timeout_s),
            "{}: timeout_s must be 1 to {MAX_TIMEOUT_S}",
            path.display()
        );
        for part in ["repo", "hidden", "solution"] {
            let sub = dir.join(part);
            ensure!(sub.is_dir(), "task {id} has no {part}/ directory");
            ensure!(!files(&sub)?.is_empty(), "task {id}: {part}/ has no files");
        }
        let hash = hash(dir)?;
        Ok(Task { id, dir: dir.to_path_buf(), spec, hash })
    }

    pub fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    pub fn hidden(&self) -> PathBuf {
        self.dir.join("hidden")
    }

    pub fn solution(&self) -> PathBuf {
        self.dir.join("solution")
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.spec.timeout_s)
    }
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.ends_with('-')
}

/// Every task under `dir`, in id order. Directories whose names start with
/// a dot are ignored.
pub fn load_all(dir: &Path) -> anyhow::Result<Vec<Task>> {
    let tasks = task_names(dir)?.iter().map(|name| Task::load(&dir.join(name))).collect::<anyhow::Result<Vec<_>>>()?;
    ensure!(!tasks.is_empty(), "there are no tasks in {}", dir.display());
    Ok(tasks)
}

/// Which tasks to use.
#[derive(Clone, Debug, Default)]
pub struct Select {
    /// Only these ids, when not empty.
    pub ids: Vec<String>,
    pub split: Option<Split>,
    pub language: Option<Language>,
}

impl Select {
    /// The selected tasks under `dir`, in id order. Tasks named by id are
    /// loaded on their own, so a broken task elsewhere does not stop them.
    pub fn load(&self, dir: &Path) -> anyhow::Result<Vec<Task>> {
        let tasks = if self.ids.is_empty() {
            load_all(dir)?
        } else {
            let mut ids = self.ids.clone();
            ids.sort();
            ids.dedup();
            let mut tasks = Vec::new();
            for id in &ids {
                let path = dir.join(id);
                if !valid_id(id) || !path.is_dir() {
                    bail!("there is no task {id:?}; the tasks are {}", task_names(dir)?.join(", "));
                }
                tasks.push(Task::load(&path)?);
            }
            tasks
        };
        let chosen: Vec<Task> = tasks
            .into_iter()
            .filter(|t| self.split.is_none_or(|s| t.spec.split == s))
            .filter(|t| self.language.is_none_or(|l| t.spec.language == l))
            .collect();
        ensure!(!chosen.is_empty(), "no task matches the selection");
        Ok(chosen)
    }
}

/// The names of the task directories under `dir`.
fn task_names(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading the task directory {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && entry.file_type()?.is_dir() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Every regular file under `root` as sorted relative paths, leaving out
/// [`SKIPPED`] names. A task holds only files and directories, so any other
/// entry, a symlink included, is an error.
pub fn files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        let dir = root.join(rel);
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            if SKIPPED.iter().any(|s| name == *s) {
                continue;
            }
            let path = rel.join(&name);
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                out.push(path);
            } else {
                bail!("{} is neither a file nor a directory", root.join(&path).display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

fn hash(dir: &Path) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    h.update(b"molt-bench task 1\n");
    let mut add = |name: &str, bytes: &[u8]| {
        h.update(name.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    add("task.toml", &fs::read(dir.join("task.toml"))?);
    for part in ["repo", "hidden"] {
        let root = dir.join(part);
        for rel in files(&root)? {
            let name = format!("{part}/{}", slashed(&rel));
            add(&name, &fs::read(root.join(&rel))?);
        }
    }
    Ok(hex::encode(h.finalize()))
}

/// `rel` with `/` between its parts.
pub fn slashed(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Copy every file [`files`] lists under `from` into `to`, creating `to`
/// and the directories on the way. Permissions are copied with the files.
pub fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for rel in files(from)? {
        let dest = to.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from.join(&rel), &dest).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

/// Lay every file under `from` over `to`, replacing whatever stands at each
/// path: a file, a link, or a whole directory. A link or file where one of
/// the path's directories belongs is replaced by a directory, so nothing is
/// written outside `to` whatever the workspace holds.
pub fn overlay(from: &Path, to: &Path) -> anyhow::Result<()> {
    for rel in files(from)? {
        let parts: Vec<Component> = rel.components().collect();
        let (name, dirs) = parts.split_last().context("an empty path")?;
        let mut at = to.to_path_buf();
        for dir in dirs {
            at.push(dir);
            match fs::symlink_metadata(&at) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    fs::remove_file(&at).with_context(|| format!("removing {}", at.display()))?;
                    fs::create_dir(&at)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&at)?,
                Err(e) => return Err(e).with_context(|| format!("looking at {}", at.display())),
            }
        }
        at.push(name);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&at)?,
            Ok(_) => fs::remove_file(&at)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("looking at {}", at.display())),
        }
        fs::copy(from.join(&rel), &at).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::testing::{task_dir, write};

    #[test]
    fn a_task_loads_with_defaults_and_a_stable_hash() {
        let root = tempfile::tempdir().unwrap();
        let dir = task_dir(root.path(), "py-hello");
        let task = Task::load(&dir).unwrap();
        assert_eq!(task.id, "py-hello");
        assert_eq!(task.spec.timeout_s, DEFAULT_TIMEOUT_S);
        assert_eq!((task.spec.language, task.spec.split), (Language::Python, Split::Dev));
        assert_eq!(Task::load(&dir).unwrap().hash, task.hash);

        // Build output does not change the hash; the repo, the hidden tests and the spec do.
        write(&dir.join("repo/__pycache__/x.pyc"), "junk");
        write(&dir.join("solution/more.txt"), "not graded");
        assert_eq!(Task::load(&dir).unwrap().hash, task.hash);
        write(&dir.join("hidden/test_bench_hidden.sh"), "grep -q hello hello.txt\n");
        assert_ne!(Task::load(&dir).unwrap().hash, task.hash);
    }

    #[test]
    fn bad_tasks_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = task_dir(root.path(), "Bad_Id");
        assert!(Task::load(&dir).unwrap_err().to_string().contains("task id"));

        let dir = task_dir(root.path(), "py-extra");
        let toml = fs::read_to_string(dir.join("task.toml")).unwrap();
        fs::write(dir.join("task.toml"), format!("{toml}colour = \"blue\"\n")).unwrap();
        assert!(format!("{:#}", Task::load(&dir).unwrap_err()).contains("colour"));

        let dir = task_dir(root.path(), "py-nohidden");
        fs::remove_dir_all(dir.join("hidden")).unwrap();
        assert!(Task::load(&dir).unwrap_err().to_string().contains("hidden/"));

        let dir = task_dir(root.path(), "py-link");
        symlink("/etc/passwd", dir.join("repo/passwd")).unwrap();
        assert!(Task::load(&dir).unwrap_err().to_string().contains("neither a file nor a directory"));
    }

    #[test]
    fn selection_filters_and_names_unknown_ids() {
        let root = tempfile::tempdir().unwrap();
        task_dir(root.path(), "py-a");
        let b = task_dir(root.path(), "py-b");
        let toml = fs::read_to_string(b.join("task.toml")).unwrap().replace("\"dev\"", "\"heldout\"");
        fs::write(b.join("task.toml"), toml).unwrap();
        fs::create_dir(root.path().join(".hidden-dir")).unwrap();
        let ids = |tasks: Vec<Task>| tasks.into_iter().map(|t| t.id).collect::<Vec<_>>();

        assert_eq!(ids(load_all(root.path()).unwrap()), ["py-a", "py-b"]);
        let held = Select { split: Some(Split::Heldout), ..Select::default() };
        assert_eq!(ids(held.load(root.path()).unwrap()), ["py-b"]);
        let err = Select { ids: vec!["py-c".into()], ..Select::default() }.load(root.path()).unwrap_err();
        assert!(err.to_string().contains("py-a, py-b"), "{err}");
        assert!(Select { ids: vec!["../py-a".into()], ..Select::default() }.load(root.path()).is_err());
        assert!(Select { language: Some(Language::Go), ..Select::default() }.load(root.path()).is_err());

        // A broken task stops a run of all tasks, but not one of the others by name.
        fs::create_dir(root.path().join("py-broken")).unwrap();
        assert!(load_all(root.path()).is_err());
        let both = Select { ids: vec!["py-b".into(), "py-a".into(), "py-a".into()], ..Select::default() };
        assert_eq!(ids(both.load(root.path()).unwrap()), ["py-a", "py-b"]);
    }

    #[test]
    fn overlay_replaces_files_links_and_directories_and_stays_inside() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("from");
        write(&from.join("tests/test_bench_hidden.py"), "hidden");
        write(&from.join("a/b/c.txt"), "deep");
        write(&from.join("top.txt"), "top");
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();

        let to = root.path().join("to");
        write(&to.join("top.txt"), "agent's");
        // An agent planted a link where a directory belongs, and a directory where a file belongs.
        symlink(&outside, to.join("tests")).unwrap();
        fs::create_dir_all(to.join("a/b/c.txt/inner")).unwrap();
        overlay(&from, &to).unwrap();

        assert_eq!(fs::read_to_string(to.join("tests/test_bench_hidden.py")).unwrap(), "hidden");
        assert!(fs::symlink_metadata(to.join("tests")).unwrap().is_dir());
        assert_eq!(fs::read_to_string(to.join("a/b/c.txt")).unwrap(), "deep");
        assert_eq!(fs::read_to_string(to.join("top.txt")).unwrap(), "top");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "nothing was written through the link");
    }
}

//! Projects as the user knows them: each has a stable id and a name, and
//! zero or more directories. Memory keeps a project's notes under its id, so
//! they outlive a move of the directory, are shared by every clone of it, and
//! a project with no directory at all (a trip plan, a paper) can have them too.
//!
//! The registry is one file in the user's data directory,
//! `$XDG_DATA_HOME/molt/projects.json` (`~/.local/share/molt/projects.json`).
//! Nothing in a project directory is ever read to decide which project it
//! is: the files there are the project's, and not to be trusted.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};

const REGISTRY: &str = "projects.json";
const LOCK: &str = "projects.lock";
/// Where a project without a directory does its work, under its own folder.
const FILES: &str = "files";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Canonical directories of the project: none for a project without a
    /// folder, several for clones of one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathBuf>,
    /// Where its kernel state, audit log and memory are.
    pub data_dir: PathBuf,
    pub created_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    projects: Vec<Project>,
}

/// The registry in the directory `dir` (see [`Registry::user`]).
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// The user's registry: in `$XDG_DATA_HOME/molt`, else `~/.local/share/molt`.
    pub fn user() -> anyhow::Result<Self> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| Path::new(&home).join(".local/share"))
            })
            .context("neither XDG_DATA_HOME nor HOME is set, so there is no project registry")?;
        Ok(Self::at(data.join("molt")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn list(&self) -> anyhow::Result<Vec<Project>> {
        self.update(|projects| Ok(projects.clone()))
    }

    /// The project with this id, or the one project with this name.
    pub fn find(&self, which: &str) -> anyhow::Result<Project> {
        let projects = self.list()?;
        find(&projects, which).cloned()
    }

    /// The project `path` (canonical) is a directory of, registered now
    /// under its directory's name when there is none, with `data_dir` as its
    /// data dir: the one molt kept for the path before projects existed, so
    /// nothing it holds is lost.
    pub fn for_path(&self, path: &Path, data_dir: impl FnOnce() -> anyhow::Result<PathBuf>) -> anyhow::Result<Project> {
        self.update(|projects| {
            if let Some(p) = projects.iter().find(|p| p.paths.iter().any(|q| q == path)) {
                return Ok(p.clone());
            }
            let name = path.file_name().map_or("root".into(), |n| n.to_string_lossy().into_owned());
            let project = Project {
                id: new_id(projects),
                name,
                paths: vec![path.to_path_buf()],
                data_dir: data_dir()?,
                created_ms: now_ms(),
            };
            projects.push(project.clone());
            Ok(project)
        })
    }

    /// Register a new project called `name`, with the directory `path`
    /// (canonical) or none. `data_dir` picks its data dir from its id.
    pub fn create(
        &self,
        name: &str,
        path: Option<&Path>,
        data_dir: impl FnOnce(&str) -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<Project> {
        let name = name.trim();
        ensure!(!name.is_empty() && !name.contains(['\n', '\0']), "a project needs a name of one line");
        self.update(|projects| {
            ensure!(!projects.iter().any(|p| p.name == name), "there is a project called {name} already");
            if let Some(path) = path {
                taken(projects, path, None)?;
            }
            let id = new_id(projects);
            let project = Project {
                data_dir: data_dir(&id)?,
                id,
                name: name.to_owned(),
                paths: path.map(Path::to_path_buf).into_iter().collect(),
                created_ms: now_ms(),
            };
            projects.push(project.clone());
            Ok(project)
        })
    }

    /// Add the directory `path` (canonical) to the project, such as another
    /// clone of it.
    pub fn add_path(&self, which: &str, path: &Path) -> anyhow::Result<Project> {
        self.update(|projects| {
            let id = find(projects, which)?.id.clone();
            taken(projects, path, Some(&id))?;
            let project = projects.iter_mut().find(|p| p.id == id).expect("found above");
            if !project.paths.iter().any(|p| p == path) {
                project.paths.push(path.to_path_buf());
            }
            Ok(project.clone())
        })
    }

    /// The project's directory is now `to` (canonical). `from` says which
    /// directory moved; without it, the project's only directory, or the one
    /// of its directories that no longer exists.
    pub fn move_path(&self, which: &str, to: &Path, from: Option<&Path>) -> anyhow::Result<Project> {
        self.update(|projects| {
            let id = find(projects, which)?.id.clone();
            taken(projects, to, Some(&id))?;
            let project = projects.iter_mut().find(|p| p.id == id).expect("found above");
            let at = match from {
                Some(from) => {
                    let from = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
                    project.paths.iter().position(|p| *p == from).with_context(|| {
                        format!("{} is not a directory of the project {}", from.display(), project.name)
                    })?
                }
                None if project.paths.len() <= 1 => 0,
                None => {
                    let gone: Vec<usize> = (0..project.paths.len()).filter(|&i| !project.paths[i].exists()).collect();
                    match gone.as_slice() {
                        [one] => *one,
                        _ => bail!("the project {} has several directories; say which moved with --from", project.name),
                    }
                }
            };
            match project.paths.get_mut(at) {
                Some(path) => *path = to.to_path_buf(),
                None => project.paths.push(to.to_path_buf()),
            }
            let mut seen = std::collections::HashSet::new();
            project.paths.retain(|p| seen.insert(p.clone()));
            Ok(project.clone())
        })
    }

    /// Where the project works: its first directory that exists, or, for a
    /// project without one, a private directory of its own in the registry's.
    pub fn workspace(&self, project: &Project) -> anyhow::Result<PathBuf> {
        if let Some(path) = project.paths.iter().find(|p| p.is_dir()) {
            return Ok(path.clone());
        }
        if let Some(path) = project.paths.first() {
            bail!(
                "the project {}'s directory {} is gone; if it moved, tell molt with `molt project move {} NEW_DIR`",
                project.name,
                path.display(),
                project.id
            );
        }
        let files = self.dir.join("projects").join(&project.id).join(FILES);
        // Private, and with nothing in it but the project's work.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&files)
            .with_context(|| format!("creating {}", files.display()))?;
        files.canonicalize().with_context(|| format!("{}", files.display()))
    }

    /// Run `f` on the projects under the registry's lock, and save them
    /// after it changes any.
    fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> anyhow::Result<R>) -> anyhow::Result<R> {
        crate::lock::create_private_dir(&self.dir)?;
        let lock = self.dir.join(LOCK);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock)
            .with_context(|| format!("opening {}", lock.display()))?;
        lock.lock().context("locking the project registry")?;
        let path = self.dir.join(REGISTRY);
        let saved: Saved = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut projects = saved.projects.clone();
        let out = f(&mut projects)?;
        if projects != saved.projects {
            let tmp = self.dir.join(format!("{REGISTRY}.tmp"));
            let mut file: File = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            file.write_all(&serde_json::to_vec_pretty(&Saved { projects })?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))?;
        }
        Ok(out)
    }
}

fn find<'a>(projects: &'a [Project], which: &str) -> anyhow::Result<&'a Project> {
    if let Some(p) = projects.iter().find(|p| p.id == which) {
        return Ok(p);
    }
    let named: Vec<&Project> = projects.iter().filter(|p| p.name == which).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => bail!("there is no project {which}; `molt project list` lists them"),
        _ => bail!("several projects are called {which}; name one by its id"),
    }
}

/// Refuse a directory another project has.
fn taken(projects: &[Project], path: &Path, except: Option<&str>) -> anyhow::Result<()> {
    if let Some(other) = projects.iter().find(|p| Some(p.id.as_str()) != except && p.paths.iter().any(|q| q == path)) {
        bail!("{} is a directory of the project {} ({}) already", path.display(), other.name, other.id);
    }
    Ok(())
}

fn new_id(projects: &[Project]) -> String {
    loop {
        let id = format!("proj_{:016x}", rand::random::<u64>());
        if !projects.iter().any(|p| p.id == id) {
            return id;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, Registry, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::at(tmp.path().join("data/molt"));
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        (tmp, registry, a.canonicalize().unwrap(), b.canonicalize().unwrap())
    }

    fn cache(_: &str) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from("/cache/molt/x"))
    }

    #[test]
    fn a_directory_is_registered_once_with_the_data_dir_it_had() {
        let (_tmp, registry, a, _) = dirs();
        let first = registry.for_path(&a, || Ok("/cache/molt/a-1234abcd".into())).unwrap();
        assert!(first.id.starts_with("proj_"));
        assert_eq!((first.name.as_str(), first.paths.as_slice()), ("a", &[a.clone()][..]));
        assert_eq!(first.data_dir, Path::new("/cache/molt/a-1234abcd"));
        let again = registry.for_path(&a, || panic!("registered already")).unwrap();
        assert_eq!(again, first);
        assert_eq!(registry.list().unwrap(), [first]);
    }

    #[test]
    fn a_moved_directory_keeps_its_project() {
        let (tmp, registry, a, _) = dirs();
        let project = registry.for_path(&a, || Ok("/cache/molt/a".into())).unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&a, &moved).unwrap();
        let moved = moved.canonicalize().unwrap();
        let after = registry.move_path("a", &moved, None).unwrap();
        assert_eq!((after.id.as_str(), after.paths.as_slice()), (project.id.as_str(), &[moved.clone()][..]));
        assert_eq!(after.data_dir, project.data_dir, "its data, audit log and notes stay where they were");
        assert_eq!(registry.for_path(&moved, || panic!("known")).unwrap().id, project.id);
        assert_eq!(registry.workspace(&after).unwrap(), moved);
    }

    #[test]
    fn clones_share_a_project_and_a_move_finds_the_one_that_went() {
        let (tmp, registry, a, b) = dirs();
        let project = registry.create("site", Some(&a), cache).unwrap();
        let both = registry.add_path(&project.id, &b).unwrap();
        assert_eq!(both.paths, [a.clone(), b.clone()]);
        assert_eq!(registry.for_path(&b, || panic!("known")).unwrap().id, project.id);

        let c = tmp.path().join("c");
        std::fs::rename(&b, &c).unwrap();
        let c = c.canonicalize().unwrap();
        assert_eq!(registry.move_path("site", &c, None).unwrap().paths, [a.clone(), c.clone()]);
        // With both still there, it has to be told which.
        let e = registry.move_path("site", &b, None);
        assert!(e.is_err());
        std::fs::create_dir(&b).unwrap();
        let b = b.canonicalize().unwrap();
        assert!(registry.move_path("site", &b, None).unwrap_err().to_string().contains("--from"));
        assert_eq!(registry.move_path("site", &b, Some(&c)).unwrap().paths, [a.clone(), b.clone()]);
    }

    #[test]
    fn moving_to_an_existing_clone_removes_only_the_selected_old_path() {
        let (tmp, registry, a, b) = dirs();
        let c = tmp.path().join("c");
        std::fs::create_dir(&c).unwrap();
        let project = registry.create("site", Some(&a), cache).unwrap();
        registry.add_path(&project.id, &b).unwrap();
        registry.add_path(&project.id, &c).unwrap();
        assert_eq!(registry.move_path("site", &a, Some(&c)).unwrap().paths, [a.clone(), b.clone()]);
        assert_eq!(registry.move_path("site", &a, Some(&a)).unwrap().paths, [a, b]);
    }

    #[test]
    fn a_directory_belongs_to_one_project_and_names_are_unique() {
        let (_tmp, registry, a, b) = dirs();
        registry.create("one", Some(&a), cache).unwrap();
        let two = registry.create("two", Some(&b), cache).unwrap();
        assert!(registry.add_path("two", &a).unwrap_err().to_string().contains("of the project one"));
        assert!(registry.create("one", None, cache).unwrap_err().to_string().contains("called one"));
        assert!(registry.find("nothing").is_err());
        assert_eq!(registry.find(&two.id).unwrap(), two);
    }

    #[test]
    fn a_project_without_a_folder_works_in_a_private_one() {
        let (_tmp, registry, _, _) = dirs();
        let trip = registry.create("Japan trip", None, cache).unwrap();
        assert!(trip.paths.is_empty());
        let files = registry.workspace(&trip).unwrap();
        assert!(files.is_dir() && files.ends_with(format!("projects/{}/files", trip.id)), "{}", files.display());
        assert_eq!(registry.workspace(&trip).unwrap(), files);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&files).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&files).unwrap().count(), 0, "it starts empty");
    }

    #[test]
    fn a_gone_directory_says_how_to_move_it() {
        let (tmp, registry, a, _) = dirs();
        let project = registry.for_path(&a, || Ok("/cache/molt/a".into())).unwrap();
        std::fs::remove_dir(&a).unwrap();
        let e = registry.workspace(&project).unwrap_err().to_string();
        assert!(e.contains("molt project move"), "{e}");
        drop(tmp);
    }
}

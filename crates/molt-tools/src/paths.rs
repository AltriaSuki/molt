//! Confinement: which workspaces a request may name, and which paths inside
//! a workspace it may touch.
//!
//! Checks are made on canonical paths, so symlinks are resolved before the
//! "is it inside" question is asked.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use molt_proto::RemoteError;

use crate::error::{failed, invalid};
use crate::Roots;

impl Roots {
    /// Both roots canonicalized; `scratch` is created first if missing.
    pub(crate) fn canonical(&self) -> anyhow::Result<Roots> {
        fs::create_dir_all(&self.scratch).with_context(|| format!("creating {}", self.scratch.display()))?;
        let root = self.root.canonicalize().with_context(|| format!("root {}", self.root.display()))?;
        let scratch = self.scratch.canonicalize().with_context(|| format!("scratch {}", self.scratch.display()))?;
        Ok(Roots { root, scratch })
    }

    /// True when the canonical path `path` is a root or inside one.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root) || path.starts_with(&self.scratch)
    }

    /// The canonical directory a request's `workspace` names: an absolute
    /// path or one relative to `root`, which must be a directory inside a root.
    pub(crate) fn workspace(&self, workspace: &str) -> Result<PathBuf, RemoteError> {
        if workspace.contains('\0') {
            return Err(invalid("workspace contains a NUL byte"));
        }
        let path = match self.root.join(workspace).canonicalize() {
            Ok(p) => p,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
                return Err(invalid(format!("workspace {workspace} does not exist")))
            }
            Err(e) => return Err(failed(format!("workspace {workspace}: {e}"))),
        };
        if !self.contains(&path) {
            return Err(invalid(format!("workspace {workspace} is outside the directories this service may use")));
        }
        if !path.is_dir() {
            return Err(invalid(format!("workspace {workspace} is not a directory")));
        }
        Ok(path)
    }
}

/// Check a request's path without touching the disk: relative, no `..`, no
/// NUL. `""` and `.` name the workspace itself.
pub(crate) fn relative(path: &str) -> Result<PathBuf, RemoteError> {
    if path.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    let mut out = PathBuf::new();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid(format!("{path}: `..` is not allowed; name a path inside the workspace")))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(format!(
                    "{path}: absolute paths are not allowed; use a path relative to the workspace"
                )))
            }
        }
    }
    Ok(out)
}

/// How a path is named in messages: as the request spelled it.
pub(crate) fn shown(path: &str) -> &str {
    if path.is_empty() || path == "." {
        "the workspace"
    } else {
        path
    }
}

/// The canonical path of the existing `rel` inside the canonical workspace
/// `ws`. Symlinks are followed, but may not lead out of the workspace.
pub(crate) fn existing(ws: &Path, rel: &Path, shown: &str) -> Result<PathBuf, RemoteError> {
    let path = ws.join(rel).canonicalize().map_err(|e| crate::error::io(shown, e))?;
    if !path.starts_with(ws) {
        return Err(invalid(format!("{shown} leads outside the workspace")));
    }
    Ok(path)
}

/// Where to write `rel` inside the canonical workspace `ws`. The file may not
/// exist yet, but it may not be a symlink or a directory, and its nearest
/// existing ancestor must resolve inside the workspace. The result is that
/// ancestor's canonical path joined with the missing components.
pub(crate) fn writable(ws: &Path, rel: &Path, shown: &str) -> Result<PathBuf, RemoteError> {
    if rel.as_os_str().is_empty() {
        return Err(invalid("path is empty: name a file inside the workspace"));
    }
    let full = ws.join(rel);
    let mut missing: Vec<OsString> = Vec::new();
    let mut at = full.as_path();
    loop {
        match fs::symlink_metadata(at) {
            Ok(meta) => {
                if missing.is_empty() {
                    if meta.file_type().is_symlink() {
                        return Err(invalid(format!("{shown} is a symlink; refusing to write through it")));
                    }
                    if meta.is_dir() {
                        return Err(invalid(format!("{shown} is a directory")));
                    }
                }
                let real = at.canonicalize().map_err(|e| crate::error::io(shown, e))?;
                if !real.starts_with(ws) {
                    return Err(invalid(format!("{shown} leads outside the workspace")));
                }
                return Ok(missing.into_iter().rev().fold(real, |path, name| path.join(name)));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (at.file_name(), at.parent()) else {
                    return Err(invalid(format!("{shown} leads outside the workspace")));
                };
                missing.push(name.to_owned());
                at = parent;
            }
            Err(e) => return Err(crate::error::io(shown, e)),
        }
    }
}

/// `path` relative to `base`, with `/` separators.
pub(crate) fn slash(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_checked_lexically() {
        assert_eq!(relative("").unwrap(), PathBuf::new());
        assert_eq!(relative(".").unwrap(), PathBuf::new());
        assert_eq!(relative("./a/./b").unwrap(), PathBuf::from("a/b"));
        assert!(relative("a/../b").is_err());
        assert!(relative("..").is_err());
        assert!(relative("/etc/passwd").is_err());
        assert!(relative("a\0b").is_err());
    }
}

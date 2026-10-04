//! Which entries of a workspace the services see.
//!
//! One set of rules serves list, search, fork and diff, so a fork contains
//! exactly what a listing shows: dotfiles are included; `.gitignore` and
//! `.ignore` files inside the workspace apply even without a git repository
//! (forks have no `.git`); `.git` and `.molt` are always skipped. Ignore
//! files above the workspace, the global git excludes and `.git/info/exclude`
//! do not apply, since a fork could not reproduce them.

use std::path::Path;

use ignore::{DirEntry, WalkBuilder};

/// Names skipped wherever they appear.
pub(crate) const SKIPPED: [&str; 2] = [".git", ".molt"];

/// Entries strictly inside `sub` (a canonical directory inside the canonical
/// workspace `ws`), at most `depth` levels below it, in a stable order.
/// Errors are yielded as they occur. `scratch` is never entered, in case a
/// workspace contains it.
pub(crate) fn walk(
    ws: &Path,
    sub: &Path,
    depth: Option<usize>,
    scratch: &Path,
) -> impl Iterator<Item = Result<DirEntry, ignore::Error>> {
    let offset = sub.strip_prefix(ws).map(|rel| rel.components().count()).unwrap_or(0);
    // Walk from the workspace so its ignore files apply to a subdirectory. If
    // the rules exclude the subdirectory itself, the caller named it on
    // purpose: walk it from there instead, with only its own ignore files.
    let from_ws = offset == 0 || reachable(ws, sub, offset, scratch);
    let walk = if from_ws {
        let target = sub.to_path_buf();
        let skip = scratch.to_path_buf();
        builder(ws)
            .max_depth(depth.map(|d| d + offset))
            .filter_entry(move |e| keep(e, &skip) && (target.starts_with(e.path()) || e.path().starts_with(&target)))
            .build()
    } else {
        let skip = scratch.to_path_buf();
        builder(sub).max_depth(depth).filter_entry(move |e| keep(e, &skip)).build()
    };
    let sub = sub.to_path_buf();
    walk.filter(move |item| match item {
        Ok(e) => e.path() != sub && e.path().starts_with(&sub),
        Err(_) => true,
    })
}

fn builder(start: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(start);
    b.hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b));
    b
}

fn keep(entry: &DirEntry, scratch: &Path) -> bool {
    let name = entry.file_name();
    !SKIPPED.iter().any(|s| name == *s) && entry.path() != scratch
}

/// The regular files a walk of the canonical workspace `ws` returns, not
/// entering the directories `left_out` names.
pub(crate) fn files<F>(ws: &Path, left_out: F) -> impl Iterator<Item = DirEntry>
where
    F: Fn(&Path) -> bool + Send + Sync + 'static,
{
    builder(ws)
        .filter_entry(move |e| keep(e, Path::new("")) && !(is_dir(e) && left_out(e.path())))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
}

/// True when `path` (absolute, inside the canonical workspace `ws`) is one
/// of [`files`]`(ws, left_out)`. Only the directories on the way to it are read.
pub(crate) fn reaches_file<F>(ws: &Path, path: &Path, left_out: F) -> bool
where
    F: Fn(&Path) -> bool + Send + Sync + 'static,
{
    let Ok(rel) = path.strip_prefix(ws) else { return false };
    let depth = rel.components().count();
    if depth == 0 {
        return false;
    }
    let target = path.to_path_buf();
    builder(ws)
        .max_depth(Some(depth))
        .filter_entry(move |e| {
            keep(e, Path::new("")) && target.starts_with(e.path()) && !(is_dir(e) && left_out(e.path()))
        })
        .build()
        .filter_map(Result::ok)
        .any(|e| e.path() == path && e.file_type().is_some_and(|t| t.is_file()))
}

/// True for a directory other than the walk's start.
fn is_dir(entry: &DirEntry) -> bool {
    entry.depth() > 0 && entry.file_type().is_some_and(|t| t.is_dir())
}

/// True when the walk rules let a walk from `ws` reach `sub`.
fn reachable(ws: &Path, sub: &Path, offset: usize, scratch: &Path) -> bool {
    let target = sub.to_path_buf();
    let skip = scratch.to_path_buf();
    builder(ws)
        .max_depth(Some(offset))
        .filter_entry(move |e| keep(e, &skip) && target.starts_with(e.path()))
        .build()
        .filter_map(Result::ok)
        .any(|e| e.path() == sub)
}

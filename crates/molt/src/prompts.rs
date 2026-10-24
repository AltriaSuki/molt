//! The user's prompt pack (RFC 0003): where it is, what it holds, and the
//! promotion a person gives a protected prompt (`molt prompts promote`).
//!
//! The pack is `~/.config/molt/prompts/` (under `$XDG_CONFIG_HOME` when it
//! is set), or the directory `MOLT_PROMPTS_DIR` names. It is the user's:
//! the planner is pointed at it only from the user layer of the
//! configuration, never from a project, and never when it lies inside the
//! workspace, where an agent's changes are applied.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use molt_api::role::{valid_name, Source};
use molt_planner::pack::{self, Pack, Promotion, PROMOTED_FILE, PROMPTS_ENV};

use crate::layers::user_path;

/// The pack's directory: `MOLT_PROMPTS_DIR`, else `prompts/` next to the
/// user config. `None` when `MOLT_PROMPTS_DIR` is `none`, or there is no home.
pub fn dir(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match var(PROMPTS_ENV).filter(|d| !d.trim().is_empty()) {
        Some(d) if d == "none" => None,
        Some(d) => Some(PathBuf::from(d)),
        None => Some(user_path(var)?.parent()?.join("prompts")),
    }
}

/// The pack the planner of a run in `workspace` is to read, checked: `None`
/// when there is none. A pack inside the workspace is refused, since the
/// agent's changes land there.
pub fn for_workspace(workspace: &Path, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<PathBuf>> {
    let Some(dir) = dir(var) else { return Ok(None) };
    if !dir.is_dir() {
        ensure!(var(PROMPTS_ENV).is_none(), "{PROMPTS_ENV}={} is not a directory", dir.display());
        return Ok(None);
    }
    let dir = dir.canonicalize().with_context(|| format!("the prompt pack {}", dir.display()))?;
    let inside = workspace.canonicalize().is_ok_and(|ws| dir.starts_with(ws));
    ensure!(
        !inside,
        "the prompt pack {} is inside the workspace, where the agent's changes are applied: move it out, or set \
         {PROMPTS_ENV}=none to run with the built-in prompts",
        dir.display()
    );
    Pack::load(&dir)?;
    Ok(Some(dir))
}

/// What `molt prompts list` prints: each prompt, built-in or the pack's,
/// with its version and, for a protected one, whether it is in effect;
/// then the pack's roles and workflows.
pub fn list(dir: Option<&Path>) -> anyhow::Result<String> {
    let pack = load(dir)?;
    let mut out = match dir {
        Some(dir) => format!("prompt pack: {}\n", dir.display()),
        None => "prompt pack: none (the built-in prompts only)\n".to_owned(),
    };
    let mut names: Vec<&str> = pack::BUILTIN.iter().map(|(n, _)| *n).collect();
    names.extend(pack.prompts.keys().map(String::as_str).filter(|n| pack::builtin(n).is_none()));
    for name in names {
        let (text, source) = pack.text(name).expect("listed prompts exist");
        let hash = pack::sha256(text);
        let mut line =
            format!("{name:<12} {}  {}", &hash[..12], if source == Source::User { "yours" } else { "built-in" });
        if pack::protected(name) && source == Source::User {
            line.push_str(match pack.promoted.get(name) {
                Some(p) if p.sha256 == hash => ", promoted",
                Some(_) => ", changed since its promotion: the built-in is used until you promote it again",
                None => ", not promoted: the built-in is used until you run `molt prompts promote`",
            });
        } else if pack::protected(name) {
            line.push_str(", protected");
        }
        out.push_str(&format!("{}\n", line.trim_end()));
    }
    for role in &pack.roles {
        let prompt = role.prompt.as_deref().unwrap_or(role.base.name());
        out.push_str(&format!("role {}: based on the {}, speaks with {prompt}\n", role.name, role.base.name()));
    }
    for w in &pack.workflows {
        let seats: Vec<String> = w.roles.iter().map(|(slot, role)| format!("{role} as {slot}")).collect();
        out.push_str(&format!("workflow {}: {}\n", w.name, seats.join(", ")));
    }
    Ok(out)
}

/// The text of prompt `name`: the pack's file, else the built-in; with
/// `builtin`, the built-in. A protected prompt's file is shown whether or
/// not it is promoted (`list` says which).
pub fn show(dir: Option<&Path>, name: &str, builtin: bool) -> anyhow::Result<String> {
    let pack = if builtin { Pack::default() } else { load(dir)? };
    let (text, _) = pack.text(name).with_context(|| {
        let names: Vec<&str> = pack::BUILTIN.iter().map(|(n, _)| *n).collect();
        format!("there is no prompt {name:?}; the built-in ones are {}", names.join(", "))
    })?;
    Ok(format!("{text}\n"))
}

/// Promote the pack's prompt `name` as it is now: record its hash in
/// `promoted.toml`, once `confirm` (a person at a terminal) has typed its
/// name back. `out` shows them what they promote.
pub fn promote(
    dir: &Path,
    name: &str,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut(&str) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    ensure!(valid_name(name), "{name:?} is not a prompt name");
    let path = dir.join(format!("{name}.md"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}: write the prompt there first", path.display()))?;
    let pack = Pack::load(dir)?;
    let text = pack::normalize(&raw);
    let hash = pack::sha256(text);
    if pack.promoted.get(name).is_some_and(|p| p.sha256 == hash) {
        writeln!(out, "{name} ({}) is already promoted", &hash[..12])?;
        return Ok(());
    }
    let what = match pack::builtin(name) {
        Some(builtin) if builtin == text => "the same text as the built-in prompt".to_owned(),
        Some(_) => format!("in place of the built-in {name} prompt"),
        None => "a prompt of your own".to_owned(),
    };
    let guard = if pack::protected(name) { " It guards what results are accepted." } else { "" };
    writeln!(out, "{}: {} bytes, sha256 {hash}, {what}.{guard}\n", path.display(), text.len())?;
    writeln!(out, "{text}\n")?;
    let typed = confirm(&format!("Type {name} to promote this text: "))?;
    if typed.trim() != name {
        bail!("not promoted: you typed {:?}, not {name:?}", typed.trim());
    }
    let promoted_path = dir.join(PROMOTED_FILE);
    let mut promoted = pack.promoted.clone();
    let promoted_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    promoted.insert(name.to_owned(), Promotion { sha256: hash.clone(), promoted_ms });
    let tmp = dir.join(format!(".{PROMOTED_FILE}.tmp"));
    std::fs::write(&tmp, toml::to_string(&promoted)?).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &promoted_path).with_context(|| format!("writing {}", promoted_path.display()))?;
    writeln!(out, "promoted {name} ({}); runs from now on use it", &hash[..12])?;
    Ok(())
}

fn load(dir: Option<&Path>) -> anyhow::Result<Pack> {
    match dir.filter(|d| d.is_dir()) {
        Some(dir) => Pack::load(dir),
        None => Ok(Pack::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(config_home: &Path) -> impl Fn(&str) -> Option<String> {
        let home = config_home.to_str().unwrap().to_owned();
        move |name| (name == "XDG_CONFIG_HOME").then(|| home.clone())
    }

    #[test]
    fn the_pack_is_under_the_user_config_and_never_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let var = vars(root.path());
        assert_eq!(dir(&var), Some(root.path().join("molt/prompts")));
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        assert_eq!(for_workspace(&workspace, &var).unwrap(), None, "no pack yet");
        std::fs::create_dir_all(root.path().join("molt/prompts")).unwrap();
        std::fs::write(root.path().join("molt/prompts/attempt.md"), "Be brief.\n").unwrap();
        let found = for_workspace(&workspace, &var).unwrap().unwrap();
        assert_eq!(found, root.path().join("molt/prompts").canonicalize().unwrap());
        // The user's home as the workspace: the pack would be the agent's to change.
        let err = for_workspace(root.path(), &var).unwrap_err().to_string();
        assert!(err.contains("inside the workspace"), "{err}");
        let none = |name: &str| (name == PROMPTS_ENV).then(|| "none".to_owned());
        assert_eq!(dir(&none), None);
    }

    #[test]
    fn a_promotion_records_the_hash_a_person_typed_the_name_for() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("reviewer.md"), "Review strictly.\n").unwrap();
        let listed = list(Some(dir)).unwrap();
        assert!(listed.contains("reviewer") && listed.contains("not promoted"), "{listed}");

        let mut out = Vec::new();
        let err = promote(dir, "reviewer", &mut out, &mut |_| Ok("yes".into())).unwrap_err();
        assert!(err.to_string().contains("not promoted"), "{err}");
        assert!(!dir.join(PROMOTED_FILE).exists());

        let mut asked = String::new();
        promote(dir, "reviewer", &mut out, &mut |q| {
            asked = q.to_owned();
            Ok("reviewer\n".into())
        })
        .unwrap();
        assert!(asked.contains("Type reviewer"));
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("Review strictly.") && shown.contains("guards what results are accepted"), "{shown}");
        let pack = Pack::load(dir).unwrap();
        assert!(pack.is_promoted("reviewer", "Review strictly."));
        assert!(list(Some(dir)).unwrap().contains("reviewer") && list(Some(dir)).unwrap().contains("promoted"));

        // An edit undoes it until the next promotion.
        std::fs::write(dir.join("reviewer.md"), "Review leniently.\n").unwrap();
        assert!(list(Some(dir)).unwrap().contains("changed since its promotion"));
        assert!(promote(dir, "missing", &mut Vec::new(), &mut |_| Ok("missing".into())).is_err());
    }

    #[test]
    fn show_prints_the_text_in_effect_or_the_built_in() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("judge.md"), "Judge.\n").unwrap();
        assert_eq!(show(Some(root.path()), "judge", false).unwrap(), "Judge.\n");
        assert!(show(Some(root.path()), "judge", true).unwrap().starts_with("You judge the result of a task"));
        assert!(show(None, "nothing", false).is_err());
        assert_eq!(show(None, "attempt", false).unwrap(), format!("{}\n", pack::builtin("attempt").unwrap()));
    }
}

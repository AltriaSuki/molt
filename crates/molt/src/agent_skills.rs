//! The user's Agent Skills directory, and `molt skill export`, which writes
//! a learned skill out as a `SKILL.md` (issue #166).
//!
//! The directory is `~/.config/molt/skills/` (under `$XDG_CONFIG_HOME` when
//! it is set), or the one `MOLT_SKILLS_DIR` names. Like the prompt pack it
//! is the user's: the planner is pointed at it from the user layer of the
//! configuration only, and never when it lies inside the workspace, where
//! an agent's changes are applied. A project's `.molt/skills` is read by
//! the planner itself, as data.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use molt_api::agent_skills::{self as format, SkillMd};
use molt_api::skills::Skill;
use molt_planner::skills::SKILLS_ENV;

use crate::layers::user_path;

/// The skills directory: `MOLT_SKILLS_DIR`, else `skills/` next to the
/// user config. `None` when `MOLT_SKILLS_DIR` is `none`, or there is no home.
pub fn dir(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match var(SKILLS_ENV).filter(|d| !d.trim().is_empty()) {
        Some(d) if d == "none" => None,
        Some(d) => Some(PathBuf::from(d)),
        None => Some(user_path(var)?.parent()?.join("skills")),
    }
}

/// The skills directory the planner of a run in `workspace` is to read:
/// `None` when there is none. One inside the workspace is refused.
pub fn for_workspace(workspace: &Path, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<PathBuf>> {
    let Some(dir) = dir(var) else { return Ok(None) };
    if !dir.is_dir() {
        ensure!(var(SKILLS_ENV).is_none(), "{SKILLS_ENV}={} is not a directory", dir.display());
        return Ok(None);
    }
    let dir = dir.canonicalize().with_context(|| format!("the skills directory {}", dir.display()))?;
    let inside = workspace.canonicalize().is_ok_and(|ws| dir.starts_with(ws));
    ensure!(
        !inside,
        "the skills directory {} is inside the workspace, where the agent's changes are applied: move it out, or \
         set {SKILLS_ENV}=none to run without your skills (a project's own skills go in .molt/skills)",
        dir.display()
    );
    Ok(Some(dir))
}

/// A skill name from a learned skill's free-text name: lowercase letters,
/// digits and single hyphens, at most 64; `fallback` when nothing is left.
fn slug(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut out: String = out.chars().take(format::MAX_NAME).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    }
}

/// A learned skill (a promoted revision, as `memory.skills` exports it) as
/// a `SKILL.md`. Its steps become the instructions; its trigger words, the
/// acceptance check it was evaluated with and the files it expects say when
/// it applies; where it came from goes in `metadata`.
pub fn export(skill: &Skill) -> SkillMd {
    let spec = &skill.spec;
    let id_tail: String = skill.id.trim_start_matches("skill_").chars().take(12).collect();
    let name = slug(&spec.name, &format!("skill-{id_tail}"));
    let mut description = format!(
        "{}. A workflow Molt learned from a checked run and a person promoted after two independent evaluations. \
         Use it when the task mentions {}.",
        spec.name.trim().trim_end_matches('.'),
        spec.trigger_terms.join(", ")
    );
    if description.chars().count() > format::MAX_DESCRIPTION {
        description = description.chars().take(format::MAX_DESCRIPTION - 1).collect::<String>() + "…";
    }
    let mut body = format!("# {}\n\n## Steps\n\n", spec.name.trim());
    for (i, step) in spec.steps.iter().enumerate() {
        body.push_str(&format!("{}. {}\n", i + 1, step.trim()));
    }
    body.push_str(&format!(
        "\n## Done when\n\nThis command passes, run from the project's root:\n\n```sh\n{}\n```\n",
        spec.acceptance.trim()
    ));
    if !spec.prerequisites.is_empty() {
        body.push_str("\n## Applies while\n\nThese files are as they were when it was evaluated (SHA-256):\n\n");
        for (path, hash) in &spec.prerequisites {
            body.push_str(&format!("- `{path}`: `{hash}`\n"));
        }
    }
    let independent: Vec<&str> =
        skill.evaluations.iter().filter(|e| e.passed && e.run != skill.source_run).map(|e| e.run.as_str()).collect();
    let metadata = [
        ("molt-skill-id", skill.id.clone()),
        ("molt-revision", skill.revision.to_string()),
        ("molt-source-run", skill.source_run.clone()),
        ("molt-evaluated-runs", independent.join(" ")),
        ("molt-writer", format!("{} {}", skill.writer, skill.writer_version)),
    ];
    SkillMd {
        name,
        description,
        license: None,
        compatibility: None,
        metadata: metadata.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        allowed_tools: None,
        body,
    }
}

/// Write `skill` as `DIR/NAME/SKILL.md`, refusing to replace a file there.
pub fn write(skill: &SkillMd, dir: &Path) -> anyhow::Result<PathBuf> {
    skill.validate().map_err(anyhow::Error::msg).context("the exported skill does not conform")?;
    let folder = dir.join(&skill.name);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let path = folder.join(format::FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("writing {}: remove it first to export again", path.display()))?;
    std::io::Write::write_all(&mut file, skill.render().as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_api::skills::{Evaluation, Spec};

    fn learned() -> Skill {
        let evaluation = |run: &str| Evaluation {
            run: run.into(),
            check: Some("cargo test -p parser".into()),
            passed: true,
            cost_usd: 0.1,
            uncounted_calls: 0,
            duration_ms: 5,
            events: vec![],
        };
        Skill {
            id: "skill_0123456789abcdef".into(),
            revision: 2,
            spec: Spec {
                name: "Fix the Parser: quoting (v2)".into(),
                trigger_terms: vec!["parser".into(), "quote".into()],
                prerequisites: [("src/parse.rs".to_owned(), "a".repeat(64))].into(),
                steps: vec!["Find the tokenizer.".into(), "Add a failing test, then fix it: # not a comment".into()],
                acceptance: "cargo test -p parser".into(),
            },
            source_run: "run_src".into(),
            source_events: vec![],
            writer: "memory".into(),
            writer_version: "v1".into(),
            evaluations: vec![evaluation("run_a"), evaluation("run_b")],
        }
    }

    #[test]
    fn an_exported_skill_parses_back_and_conforms_to_the_format() {
        let skill = export(&learned());
        assert_eq!(skill.name, "fix-the-parser-quoting-v2");
        let dir = tempfile::tempdir().unwrap();
        let path = write(&skill, dir.path()).unwrap();
        assert_eq!(path, dir.path().join("fix-the-parser-quoting-v2/SKILL.md"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: fix-the-parser-quoting-v2\ndescription: "), "{text}");
        let back = format::parse(&text).unwrap();
        assert_eq!(back.validate(), Ok(()));
        assert_eq!(back, skill);
        // The name is its directory's, as the format asks.
        assert_eq!(path.parent().unwrap().file_name().unwrap(), back.name.as_str());
        assert!(back.description.contains("parser, quote") && back.description.chars().count() <= 1024);
        assert!(back.body.contains("1. Find the tokenizer.\n") && back.body.contains("cargo test -p parser"));
        assert_eq!(back.metadata["molt-skill-id"], "skill_0123456789abcdef");
        assert_eq!(back.metadata["molt-evaluated-runs"], "run_a run_b");
        // The planner finds it like any skill the user installed.
        let (found, notes) = molt_planner::skills::offered(Some(dir.path()), tempfile::tempdir().unwrap().path());
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!((names, notes), (vec!["fix-the-parser-quoting-v2"], vec![]));
        assert!(write(&skill, dir.path()).is_err(), "an export does not replace a file");
    }

    #[test]
    fn a_name_with_nothing_to_keep_falls_back_to_the_id_and_a_long_one_is_cut() {
        let mut skill = learned();
        skill.spec.name = "修复".into();
        assert_eq!(export(&skill).name, "skill-0123456789ab");
        skill.spec.name = format!("{} tail", "word ".repeat(30));
        let name = export(&skill).name;
        assert!(name.len() <= 64 && !name.ends_with('-'), "{name}");
        assert_eq!(format::valid_name(&name), Ok(()));
        skill.spec.name = "x".repeat(2000);
        assert!(export(&skill).validate().is_ok());
    }

    #[test]
    fn the_skills_directory_is_the_users_and_never_inside_the_workspace() {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        std::fs::create_dir_all(ws.join(".molt/skills")).unwrap();
        let config = home.path().join("config");
        let var = |inner: Option<String>| {
            let config = config.to_str().unwrap().to_owned();
            move |name: &str| match name {
                "XDG_CONFIG_HOME" => Some(config.clone()),
                n if n == SKILLS_ENV => inner.clone(),
                _ => None,
            }
        };
        assert_eq!(for_workspace(&ws, &var(None)).unwrap(), None, "none yet");
        std::fs::create_dir_all(config.join("molt/skills")).unwrap();
        let found = for_workspace(&ws, &var(None)).unwrap().unwrap();
        assert_eq!(found, config.join("molt/skills").canonicalize().unwrap());
        assert_eq!(for_workspace(&ws, &var(Some("none".into()))).unwrap(), None);
        let inside = ws.join(".molt/skills").to_str().unwrap().to_owned();
        assert!(format!("{:#}", for_workspace(&ws, &var(Some(inside))).unwrap_err()).contains("inside the workspace"));
        assert!(for_workspace(&ws, &var(Some("/no/such/dir".into()))).is_err());
    }
}

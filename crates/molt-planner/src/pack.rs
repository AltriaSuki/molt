//! The prompt pack (RFC 0003, `docs/rfc/0003-prompts-and-roles.md`): the
//! system prompts the planner speaks with, the roles that use them, and the
//! workflows that seat those roles.
//!
//! The built-in prompts are compiled in. A pack directory, the user's
//! `~/.config/molt/prompts/`, can replace any of them with a file
//! `NAME.md`, add prompts of its own, declare roles and workflows in
//! `pack.toml`, and holds `promoted.toml`, where `molt prompts promote`
//! records the prompts a person promoted. Whoever starts the planner names
//! the directory ([`PROMPTS_ENV`]); a request or a project never does.
//!
//! The prompts of the reviewer, the judge and the vetter guard the gate a
//! result has to pass, so a pack's version of one takes effect only while
//! `promoted.toml` holds its hash: an edit after the promotion, or a file
//! never promoted, leaves the built-in prompt in place, and the run says so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context};
use molt_api::model::Effort;
use molt_api::planner::tools as t;
use molt_api::role::{valid_name, Base, PromptUse, PromptVersion, RoleDecl, Source};
use molt_api::workflow::{self, Workflow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::prompts;

/// The variable that names the pack directory of a planner started as a process.
pub const PROMPTS_ENV: &str = "MOLT_PROMPTS_DIR";
/// The variable that hands a planner started as a process the prompt
/// versions a gate promoted (`molt improve`): a JSON list of [`PromptModule`].
pub const MODULES_ENV: &str = "MOLT_PROMPT_MODULES_JSON";
/// A prompt's module in the registry is named this and the prompt's name.
pub const MODULE_PREFIX: &str = "prompt.";
/// The variable that hands the planner the roles and prompts of the
/// extension packs the user installed and enabled: a JSON list of
/// [`Extension`].
pub const EXTENSIONS_ENV: &str = "MOLT_EXTENSION_ROLES_JSON";
/// The pack's roles and workflows, in its directory.
pub const PACK_FILE: &str = "pack.toml";
/// The prompts a person promoted, in the pack's directory.
pub const PROMOTED_FILE: &str = "promoted.toml";
/// The largest prompt file read.
const MAX_PROMPT: usize = 64 * 1024;
/// The most turns a role may be given.
const MAX_ROLE_TURNS: u32 = 1_000;

/// The router: picks a workflow for a run that names `auto`.
pub const ROUTER: &str = "router";
/// The vetter: looks over a designed check before the attempts start.
pub const VETTER: &str = "vetter";
/// The step designer: splits a task into the ordered steps of a plan (#47).
pub const STEPS: &str = "steps";
/// The compactor: writes an attempt's notes on the turns its conversation
/// leaves out to fit, with `MOLT_COMPACT=model` (#108).
pub const COMPACTOR: &str = crate::compact::COMPACTOR;

/// Every built-in prompt, by name, in the order `molt prompts list` shows
/// them. `attempt` is the part of an attempt's prompt that holds whatever
/// verifies the result; the planner adds the paragraph about the
/// done-check, or about the criteria, after it.
pub const BUILTIN: &[(&str, &str)] = &[
    ("attempt", prompts::ATTEMPT_BASE),
    ("explorer", prompts::ASK_SYSTEM),
    ("designer", prompts::DESIGNER_SYSTEM),
    (VETTER, prompts::VETTER_SYSTEM),
    ("reviewer", prompts::REVIEWER_SYSTEM),
    ("judge", prompts::JUDGE_SYSTEM),
    (ROUTER, prompts::ROUTE_SYSTEM),
    (STEPS, prompts::STEPS_SYSTEM),
    (COMPACTOR, crate::compact::COMPACTOR_SYSTEM),
];

/// The built-in prompt `name`.
pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, text)| *text)
}

/// The built-in prompt named `name` guards a gate: a pack's version of it
/// needs a person's promotion.
pub fn protected(name: &str) -> bool {
    name == VETTER || Base::from_name(name).is_some_and(Base::protected)
}

/// Names a declared role cannot take: the planner's own roles.
const RESERVED_ROLES: &[&str] =
    &["designer", "attempt", "explorer", "reviewer", "judge", VETTER, ROUTER, STEPS, COMPACTOR, "memory", "planner"];

/// The lowercase hex SHA-256 of `text`: a prompt's version.
pub fn sha256(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// A prompt file's text as the planner uses it: without the line breaks
/// that end it, so a file saved by an editor matches the text it holds.
pub fn normalize(text: &str) -> &str {
    text.trim_end_matches(['\n', '\r'])
}

/// One promotion of `promoted.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Promotion {
    /// The promoted text's hash ([`sha256`]).
    pub sha256: String,
    /// When, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub promoted_ms: u64,
}

/// A live version of one of the built-in prompts from the registry of
/// prompt modules, which only a gate's or a person's promotion puts there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptModule {
    /// The prompt it replaces, such as `attempt`.
    pub name: String,
    /// Its registry version id.
    pub version: String,
    pub text: String,
}

/// The roles and prompts an extension pack brings (RFC 0005), as `molt`
/// hands them to the planner once the person approved them. Each comes in
/// under the pack's name ([`extension_name`]), so it adds to the prompt
/// pack and never replaces a prompt or a role of the user's or a built-in
/// one; a role that guards the gate still speaks with a pack's prompt only
/// once a person has promoted it, as any prompt of the user's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extension {
    /// The extension pack's name.
    pub pack: String,
    /// Its directory of prompts, `NAME.md` each.
    pub dir: PathBuf,
    /// Each prompt by its name in the pack, with the SHA-256 the person
    /// approved: a file that no longer hashes to it stops the start.
    #[serde(default)]
    pub prompts: BTreeMap<String, String>,
    /// Its roles, named as in the pack; their `prompt` is one of `prompts`.
    #[serde(default)]
    pub roles: Vec<RoleDecl>,
}

/// The name an extension pack's prompt or role `name` has in the prompt
/// pack: always `PACK-NAME`. No built-in prompt or role has a `-`, so a
/// pack can never take one's name.
pub fn extension_name(pack: &str, name: &str) -> String {
    format!("{pack}-{name}")
}

/// The text of the prompt file at `path`, as the planner uses it
/// ([`normalize`]), at most 64 KiB.
pub fn read_prompt(path: &Path) -> anyhow::Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(meta.len() <= MAX_PROMPT as u64, "{} is larger than 64 KiB", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(normalize(&text).to_owned())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackFile {
    #[serde(default, rename = "role")]
    roles: Vec<RoleDecl>,
    #[serde(default, rename = "workflow")]
    workflows: Vec<Workflow>,
}

/// A prompt pack: the built-ins with what a pack directory adds.
#[derive(Clone, Debug, Default)]
pub struct Pack {
    /// Where it was read from; `None` for the built-ins alone.
    pub dir: Option<PathBuf>,
    /// The pack's prompt files by name, as the planner uses them ([`normalize`]).
    pub prompts: BTreeMap<String, String>,
    /// What `promoted.toml` holds, by prompt name.
    pub promoted: BTreeMap<String, Promotion>,
    pub roles: Vec<RoleDecl>,
    pub workflows: Vec<Workflow>,
    /// Promoted versions of built-in prompts, by prompt name, as the
    /// planner uses them ([`normalize`]): under the pack's files, over the
    /// built-ins.
    pub modules: BTreeMap<String, PromptModule>,
    /// The prompts of `prompts` that an extension pack brought, by their
    /// name here, with the pack's name.
    pub extension_prompts: BTreeMap<String, String>,
    /// The roles of `roles` that an extension pack brought, likewise.
    pub extension_roles: BTreeMap<String, String>,
}

impl Pack {
    /// The pack with the promoted prompt `modules` under its own files.
    /// Each replaces a built-in prompt that is not protected: the prompts
    /// that guard a gate change only by a person's promotion in the pack.
    pub fn with_modules(mut self, modules: Vec<PromptModule>) -> anyhow::Result<Self> {
        for mut m in modules {
            let name = m.name.clone();
            ensure!(builtin(&name).is_some(), "a prompt module replaces a built-in prompt, and {name:?} is not one");
            ensure!(!protected(&name), "the protected prompt {name} cannot come from a prompt module");
            m.text = normalize(&m.text).to_owned();
            ensure!(!m.text.trim().is_empty(), "the prompt module {name} is empty");
            ensure!(m.text.len() <= MAX_PROMPT, "the prompt module {name} is larger than 64 KiB");
            ensure!(self.modules.insert(name.clone(), m).is_none(), "two prompt modules replace {name}");
        }
        Ok(self)
    }

    /// The pack in `dir`: every `NAME.md` there, `pack.toml` and
    /// `promoted.toml`, each optional, checked as [`Pack::validate`] does.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Self::load_with(Some(dir), Vec::new())
    }

    /// The pack in `dir`, if any, with the roles and prompts of the
    /// extension packs `extensions` added ([`Pack::with_extensions`]),
    /// checked together, so that a workflow of the user's may seat a
    /// pack's role.
    pub fn load_with(dir: Option<&Path>, extensions: Vec<Extension>) -> anyhow::Result<Self> {
        let pack = match dir {
            Some(dir) => Self::read(dir)?,
            None => Self::default(),
        };
        let pack = pack.with_extensions(extensions)?;
        match dir {
            Some(dir) => pack.validate().with_context(|| format!("in the prompt pack {}", dir.display()))?,
            None => pack.validate().context("in the extension packs' roles")?,
        }
        Ok(pack)
    }

    /// The files of the pack in `dir`, not yet checked.
    fn read(dir: &Path) -> anyhow::Result<Self> {
        let entries = std::fs::read_dir(dir).with_context(|| format!("reading the prompt pack {}", dir.display()))?;
        let mut pack = Pack { dir: Some(dir.to_path_buf()), ..Pack::default() };
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".md")) else {
                continue;
            };
            ensure!(
                valid_name(name),
                "{}: a prompt's name is 1 to 64 lowercase letters, digits, - or _",
                path.display()
            );
            pack.prompts.insert(name.to_owned(), read_prompt(&path)?);
        }
        if let Some(text) = read_optional(&dir.join(PACK_FILE))? {
            let file: PackFile =
                toml::from_str(&text).with_context(|| format!("parsing {}", dir.join(PACK_FILE).display()))?;
            pack.roles = file.roles;
            pack.workflows = file.workflows;
        }
        if let Some(text) = read_optional(&dir.join(PROMOTED_FILE))? {
            pack.promoted =
                toml::from_str(&text).with_context(|| format!("parsing {}", dir.join(PROMOTED_FILE).display()))?;
        }
        Ok(pack)
    }

    /// The pack with the roles and prompts of `extensions` added, each
    /// under its pack's name. A prompt whose file is not the text that was
    /// approved, or a name the pack has already, is refused. What is added
    /// is the user's as much as their own files are, with one exception
    /// that matters: promotion. A role based on the reviewer or the judge
    /// speaks with a pack's prompt only while `promoted.toml`, which only
    /// `molt prompts promote` writes, holds that prompt's hash.
    pub fn with_extensions(mut self, extensions: Vec<Extension>) -> anyhow::Result<Self> {
        for ext in extensions {
            let pack = &ext.pack;
            for (name, approved) in &ext.prompts {
                let full = extension_name(pack, name);
                ensure!(
                    valid_name(name) && valid_name(&full),
                    "extension {pack}'s prompt {name:?}: name prompts with lowercase letters, digits, - or _, at \
                     most 64 with the pack's name"
                );
                let text = read_prompt(&ext.dir.join(format!("{name}.md")))
                    .with_context(|| format!("extension {pack}'s prompt {name}"))?;
                ensure!(
                    sha256(&text) == *approved,
                    "extension {pack}'s prompt {name} is not the text you approved: install the pack again with \
                     molt ext add, or molt ext disable {pack}"
                );
                ensure!(
                    builtin(&full).is_none() && !self.prompts.contains_key(&full),
                    "extension {pack}'s prompt {name} would be {full}, which your prompt pack or another extension \
                     has: rename one"
                );
                self.prompts.insert(full.clone(), text);
                self.extension_prompts.insert(full, pack.clone());
            }
            for mut role in ext.roles {
                let full = extension_name(pack, &role.name);
                ensure!(
                    valid_name(&role.name) && valid_name(&full),
                    "extension {pack}'s role {:?}: name roles with lowercase letters, digits, - or _, at most 64 \
                     with the pack's name",
                    role.name
                );
                if let Some(prompt) = &role.prompt {
                    ensure!(
                        ext.prompts.contains_key(prompt),
                        "extension {pack}'s role {} speaks with {prompt:?}, which is not one of the pack's prompts: \
                         add prompts/{prompt}.md, or leave prompt out for its base's",
                        role.name
                    );
                    role.prompt = Some(extension_name(pack, prompt));
                }
                ensure!(
                    !self.roles.iter().any(|r| r.name == full),
                    "extension {pack}'s role {} would be {full}, which your prompt pack or another extension has: \
                     rename one",
                    role.name
                );
                role.name = full.clone();
                self.roles.push(role);
                self.extension_roles.insert(full, pack.clone());
            }
        }
        Ok(self)
    }

    /// What is wrong with the pack, if anything: every prompt is named and
    /// not empty, every role is based on a built-in and speaks with a
    /// prompt there is, and every workflow is one the planner can run, with
    /// a role of the right kind in each slot it names.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, text) in &self.prompts {
            ensure!(valid_name(name), "{name:?} is not a prompt name: use lowercase letters, digits, - or _");
            ensure!(!text.trim().is_empty(), "the prompt {name} is empty: delete {name}.md to use the built-in one");
            ensure!(text.len() <= MAX_PROMPT, "the prompt {name} is larger than 64 KiB");
        }
        let mut names = BTreeMap::new();
        for role in &self.roles {
            let name = &role.name;
            ensure!(valid_name(name), "role {name:?}: a role's name is 1 to 64 lowercase letters, digits, - or _");
            ensure!(!RESERVED_ROLES.contains(&name.as_str()), "role {name:?} has the name of a built-in role");
            if names.insert(name.as_str(), ()).is_some() {
                bail!("two roles are named {name:?}");
            }
            if let Some(prompt) = &role.prompt {
                ensure!(self.text(prompt).is_some(), "role {name}: there is no prompt {prompt:?}: add {prompt}.md");
            }
            if let Some(turns) = role.max_turns {
                ensure!((1..=MAX_ROLE_TURNS).contains(&turns), "role {name}: max_turns must be 1 to {MAX_ROLE_TURNS}");
            }
            if let Some(share) = role.budget_share {
                ensure!(
                    matches!(role.base, Base::Designer | Base::Explorer),
                    "role {name}: budget_share is read for designer and explorer roles only; leave it out"
                );
                ensure!(share > 0.0 && share <= 1.0, "role {name}: budget_share must be more than 0 and at most 1");
            }
            if let Some(tools) = &role.tools {
                ensure!(tools.iter().all(|t| !t.trim().is_empty()), "role {name}: tools has an empty name");
            }
            if role.model.as_ref().is_some_and(|m| m.trim().is_empty()) {
                bail!("role {name}: model is empty; leave it out for the run's model");
            }
        }
        let mut workflows = BTreeMap::new();
        for w in &self.workflows {
            let name = &w.name;
            ensure!(
                workflow::builtin(Some(name)).is_none() && name != workflow::AUTO,
                "workflow {name:?} has the name of a built-in one"
            );
            if workflows.insert(name.as_str(), ()).is_some() {
                bail!("two workflows are named {name:?}");
            }
            if let Some(problem) = w.problem() {
                bail!(problem);
            }
            self.seats(w).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    /// The text of prompt `name` and where it comes from: the pack's file,
    /// else a promoted module, else the built-in.
    pub fn text(&self, name: &str) -> Option<(&str, Source)> {
        if let Some(text) = self.prompts.get(name) {
            return Some((text, Source::User));
        }
        if let Some(m) = self.modules.get(name) {
            return Some((&m.text, Source::Registry));
        }
        builtin(name).map(|text| (text, Source::Builtin))
    }

    /// A person promoted `text` as prompt `name`.
    pub fn is_promoted(&self, name: &str, text: &str) -> bool {
        self.promoted.get(name).is_some_and(|p| p.sha256 == sha256(text))
    }

    /// The role declared as `name`, or the built-in role of that name.
    fn role(&self, name: &str) -> Option<RoleDecl> {
        if let Some(base) = Base::from_name(name) {
            return Some(plain(name, base));
        }
        self.roles.iter().find(|r| r.name == name).cloned()
    }

    /// Who speaks in each part of a run through `workflow`, and the lines
    /// the run publishes about the prompts that are not the built-ins.
    pub(crate) fn seats(&self, workflow: &Workflow) -> Result<Seats, String> {
        let seat = |base: Base| -> Result<Seat, String> {
            let decl = match workflow.roles.get(base.name()) {
                None => plain(base.name(), base),
                Some(name) => {
                    let decl = self.role(name).ok_or_else(|| {
                        format!(
                            "workflow {:?} seats role {name:?}, which the prompt pack does not declare",
                            workflow.name
                        )
                    })?;
                    if decl.base != base {
                        return Err(format!(
                            "workflow {:?} seats role {name:?} as its {}, but that role is based on the {}",
                            workflow.name,
                            base.name(),
                            decl.base.name()
                        ));
                    }
                    decl
                }
            };
            Ok(self.seat(decl, base.name(), base.protected()))
        };
        Ok(Seats {
            designer: seat(Base::Designer)?,
            attempt: seat(Base::Attempt)?,
            explorer: seat(Base::Explorer)?,
            reviewer: seat(Base::Reviewer)?,
            judge: seat(Base::Judge)?,
            vetter: self.seat(internal(VETTER), VETTER, true),
            router: self.seat(internal(ROUTER), ROUTER, false),
            steps: self.seat(internal(STEPS), STEPS, false),
            compactor: self.seat(internal(COMPACTOR), COMPACTOR, false),
        })
    }

    /// `decl` in the slot whose built-in prompt is `own`: with the pack's
    /// prompt, or the built-in one when it is `protected` and not promoted.
    fn seat(&self, decl: RoleDecl, own: &str, protected: bool) -> Seat {
        let builtin = builtin(own).expect("every slot has a built-in prompt");
        let name = decl.prompt.clone().unwrap_or_else(|| own.to_owned());
        let (text, source) = self.text(&name).unwrap_or((builtin, Source::Builtin));
        let own_builtin = source == Source::Builtin && name == own;
        let (name, text, source, note) = if protected && !own_builtin && !self.is_promoted(&name, text) {
            let note = format!(
                "{}: the prompt {name} ({}) is not promoted, so the built-in {own} prompt is used; run `molt prompts \
                 promote {name}` to promote it",
                decl.name,
                &sha256(text)[..12]
            );
            (own.to_owned(), builtin, Source::Builtin, Some(note))
        } else {
            let note = match source {
                Source::User => match self.extension_prompts.get(&name) {
                    Some(pack) => Some(format!(
                        "{}: speaks with the prompt {name} ({}) of extension {pack}",
                        decl.name,
                        &sha256(text)[..12]
                    )),
                    None => Some(format!("{}: speaks with your prompt {name} ({})", decl.name, &sha256(text)[..12])),
                },
                Source::Registry => Some(format!(
                    "{}: speaks with the prompt {name} ({}) that `molt improve` promoted",
                    decl.name,
                    &sha256(text)[..12]
                )),
                Source::Builtin => None,
            };
            (name, text, source, note)
        };
        Seat {
            prompt: PromptUse { role: decl.name.clone(), prompt: PromptVersion { name, sha256: sha256(text), source } },
            text: Arc::from(text),
            note,
            tools: decl.tools,
            model: decl.model,
            effort: decl.effort,
            max_turns: decl.max_turns,
            budget_share: decl.budget_share,
        }
    }
}

fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The built-in role `name`: its base with nothing replaced.
fn plain(name: &str, base: Base) -> RoleDecl {
    RoleDecl {
        name: name.to_owned(),
        base,
        prompt: None,
        tools: None,
        model: None,
        effort: None,
        max_turns: None,
        budget_share: None,
    }
}

/// A role of the planner's own that no workflow seats (the vetter, the router).
fn internal(name: &str) -> RoleDecl {
    plain(name, Base::Designer)
}

/// One role in its slot of a run.
#[derive(Clone, Debug)]
pub(crate) struct Seat {
    /// The role and the version of the prompt it speaks with.
    pub prompt: PromptUse,
    text: Arc<str>,
    /// What the run says about its prompt, when it is not the built-in one.
    pub note: Option<String>,
    tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub max_turns: Option<u32>,
    pub budget_share: Option<f64>,
}

impl Seat {
    /// Its system prompt, with `part` after the pack's text (the attempt's
    /// paragraph about what verifies it; empty for the others).
    pub fn system(&self, part: &str) -> Arc<str> {
        if part.is_empty() {
            self.text.clone()
        } else {
            Arc::from(format!("{}{part}", self.text))
        }
    }

    /// It may be offered the tool `name`. Whatever its declaration says, a
    /// role keeps the tools that end its work.
    pub fn allows(&self, name: &str) -> bool {
        name.starts_with("submit_")
            || name == t::DISPUTE_CHECK
            || self.tools.as_ref().is_none_or(|tools| tools.iter().any(|t| t == name))
    }

    /// `tools` without those it may not be offered, in their order.
    pub fn offer(&self, tools: Vec<Value>) -> Vec<Value> {
        if self.tools.is_none() {
            return tools;
        }
        tools.into_iter().filter(|tool| tool["name"].as_str().is_some_and(|name| self.allows(name))).collect()
    }
}

/// The roles of one run, one in each slot.
#[derive(Clone, Debug)]
pub(crate) struct Seats {
    pub designer: Seat,
    pub attempt: Seat,
    pub explorer: Seat,
    pub reviewer: Seat,
    pub judge: Seat,
    pub vetter: Seat,
    pub router: Seat,
    pub steps: Seat,
    pub compactor: Seat,
}

impl Seats {
    /// The lines a run through `workflow` publishes at its start: one for
    /// each role of its stages that speaks with a prompt of the pack, or
    /// whose pack prompt is not promoted.
    pub fn notes(&self, workflow: &Workflow) -> Vec<String> {
        let s = &workflow.stages;
        let mut seats = Vec::new();
        if s.design.is_some() {
            seats.extend([&self.designer, &self.vetter]);
        }
        seats.push(if workflow.read_only() { &self.explorer } else { &self.attempt });
        if workflow.explores() {
            seats.push(&self.explorer);
        }
        if s.review.is_some_and(|r| !r.rules_only) {
            seats.push(&self.reviewer);
        }
        if !workflow.read_only() {
            seats.push(&self.judge);
        }
        seats.into_iter().filter_map(|seat| seat.note.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(files: &[(&str, &str)]) -> (tempfile::TempDir, anyhow::Result<Pack>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let loaded = Pack::load(dir.path());
        (dir, loaded)
    }

    #[test]
    fn without_a_pack_every_seat_speaks_with_its_built_in_prompt() {
        let seats = Pack::default().seats(&Workflow::verify()).unwrap();
        assert_eq!(&*seats.reviewer.system(""), prompts::REVIEWER_SYSTEM);
        assert_eq!(seats.reviewer.prompt.prompt.source, Source::Builtin);
        assert_eq!(seats.reviewer.prompt.prompt.sha256, sha256(prompts::REVIEWER_SYSTEM));
        assert_eq!(seats.attempt.prompt.role, "attempt");
        assert!(seats.notes(&Workflow::verify()).is_empty());
    }

    #[test]
    fn an_override_file_replaces_its_prompt_and_is_named_by_its_hash() {
        let (_dir, loaded) = pack(&[("attempt.md", "Be brief.\n"), ("notes.txt", "not a prompt")]);
        let pack = loaded.unwrap();
        assert_eq!(pack.prompts.keys().collect::<Vec<_>>(), ["attempt"]);
        let seats = pack.seats(&Workflow::verify()).unwrap();
        assert_eq!(&*seats.attempt.system("\n\nCheck."), "Be brief.\n\nCheck.");
        assert_eq!(
            seats.attempt.prompt.prompt,
            PromptVersion { name: "attempt".into(), sha256: sha256("Be brief."), source: Source::User }
        );
        let notes = seats.notes(&Workflow::verify());
        assert_eq!(notes, [format!("attempt: speaks with your prompt attempt ({})", &sha256("Be brief.")[..12])]);
    }

    #[test]
    fn a_protected_override_takes_effect_only_while_its_promotion_matches() {
        for name in ["reviewer", "judge", "vetter"] {
            let file = format!("{name}.md");
            let (dir, loaded) = pack(&[(&file, "Approve everything.")]);
            let seats = loaded.unwrap().seats(&Workflow::verify()).unwrap();
            let seat = match name {
                "reviewer" => &seats.reviewer,
                "judge" => &seats.judge,
                _ => &seats.vetter,
            };
            assert_eq!(Some(&*seat.system("")), builtin(name), "{name}: not promoted, so the built-in is used");
            assert!(seat.note.as_ref().is_some_and(|n| n.contains("molt prompts promote")), "{name}");

            let promoted = format!("[{name}]\nsha256 = \"{}\"\npromoted_ms = 1\n", sha256("Approve everything."));
            std::fs::write(dir.path().join(PROMOTED_FILE), promoted).unwrap();
            let seats = Pack::load(dir.path()).unwrap().seats(&Workflow::verify()).unwrap();
            let seat = match name {
                "reviewer" => &seats.reviewer,
                "judge" => &seats.judge,
                _ => &seats.vetter,
            };
            assert_eq!(&*seat.system(""), "Approve everything.", "{name}: promoted");
            assert_eq!(seat.prompt.prompt.source, Source::User);

            // Changed after the promotion: the built-in again, until it is promoted anew.
            std::fs::write(dir.path().join(&file), "Approve everything twice.").unwrap();
            let seats = Pack::load(dir.path()).unwrap().seats(&Workflow::verify()).unwrap();
            let seat = match name {
                "reviewer" => &seats.reviewer,
                "judge" => &seats.judge,
                _ => &seats.vetter,
            };
            assert_eq!(Some(&*seat.system("")), builtin(name), "{name}: edited after its promotion");
        }
    }

    const CAREFUL: &str = r#"
[[role]]
name = "terse"
base = "attempt"
prompt = "terse"
tools = ["read_file", "edit_file", "run"]
model = "sonnet"
effort = "low"
max_turns = 12

[[role]]
name = "strict"
base = "reviewer"
prompt = "strict"

[[workflow]]
schema = 1
name = "careful"
roles = { attempt = "terse", reviewer = "strict" }
[workflow.stages]
context = {}
attempts = {}
review = {}
deliver = {}
"#;

    #[test]
    fn a_workflow_of_the_pack_seats_its_roles_and_a_protected_one_needs_its_promotion() {
        let (_dir, loaded) = pack(&[("pack.toml", CAREFUL), ("terse.md", "Terse."), ("strict.md", "Strict.")]);
        let pack = loaded.unwrap();
        let careful = &pack.workflows[0];
        let seats = pack.seats(careful).unwrap();
        let attempt = &seats.attempt;
        assert_eq!((attempt.prompt.role.as_str(), &*attempt.system("")), ("terse", "Terse."));
        assert_eq!(
            (attempt.model.as_deref(), attempt.effort, attempt.max_turns),
            (Some("sonnet"), Some(Effort::Low), Some(12))
        );
        assert!(attempt.allows("edit_file") && !attempt.allows("write_file") && attempt.allows(t::DISPUTE_CHECK));
        assert_eq!(seats.reviewer.prompt.role, "strict");
        assert_eq!(&*seats.reviewer.system(""), prompts::REVIEWER_SYSTEM, "strict.md is not promoted");
        assert_eq!(seats.notes(careful).len(), 2);
        // The other workflows keep the built-in roles.
        assert_eq!(pack.seats(&Workflow::verify()).unwrap().attempt.prompt.role, "attempt");
    }

    #[test]
    fn a_pack_that_names_what_is_not_there_or_misplaces_a_role_is_refused_with_why() {
        let cases: &[(&str, &str)] = &[
            ("[[role]]\nname = \"x\"\nbase = \"attempt\"\nprompt = \"missing\"\n", "no prompt \"missing\""),
            ("[[role]]\nname = \"reviewer\"\nbase = \"reviewer\"\n", "built-in role"),
            ("[[role]]\nname = \"x\"\nbase = \"attempt\"\nbudget_share = 0.5\n", "designer and explorer roles only"),
            ("[[role]]\nname = \"x\"\nbase = \"attempt\"\nmax_turns = 0\n", "max_turns"),
            ("[[role]]\nname = \"x\"\nbase = \"attempt\"\nturns = 3\n", "unknown field"),
            (
                "[[role]]\nname = \"x\"\nbase = \"judge\"\n[[workflow]]\nschema = 1\nname = \"w\"\nroles = { attempt = \"x\" }\n\
                 stages = { attempts = {}, deliver = {} }\n",
                "based on the judge",
            ),
            (
                "[[workflow]]\nschema = 1\nname = \"w\"\nroles = { attempt = \"nobody\" }\nstages = { attempts = {}, deliver = {} }\n",
                "does not declare",
            ),
            ("[[workflow]]\nschema = 1\nname = \"verify\"\nstages = { attempts = {}, deliver = {} }\n", "built-in one"),
        ];
        for (text, why) in cases {
            let (_dir, loaded) = pack(&[("pack.toml", text)]);
            let err = format!("{:#}", loaded.unwrap_err());
            assert!(err.contains(why), "{text}: {err}");
        }
        let (_dir, loaded) = pack(&[("Attempt.md", "x")]);
        assert!(format!("{:#}", loaded.unwrap_err()).contains("lowercase"));
        let (_dir, loaded) = pack(&[("attempt.md", "\n\n")]);
        assert!(format!("{:#}", loaded.unwrap_err()).contains("empty"));
    }

    #[test]
    fn a_promoted_module_replaces_a_built_in_under_the_packs_own_file() {
        let module =
            |name: &str, text: &str| PromptModule { name: name.into(), version: "sha256:1".into(), text: text.into() };
        let improved = Pack::default().with_modules(vec![module("attempt", "Improved.\n")]).unwrap();
        let seats = improved.seats(&Workflow::verify()).unwrap();
        assert_eq!(&*seats.attempt.system(" Check."), "Improved. Check.");
        assert_eq!(seats.attempt.prompt.prompt.source, Source::Registry);
        assert!(seats.notes(&Workflow::verify())[0].contains("molt improve"));
        // The user's own file wins.
        let (_dir, loaded) = pack(&[("attempt.md", "Mine.")]);
        let mine = loaded.unwrap().with_modules(vec![module("attempt", "Improved.")]).unwrap();
        assert_eq!(&*mine.seats(&Workflow::verify()).unwrap().attempt.system(""), "Mine.");
        // Never a prompt that guards the gate, an unknown one, or an empty one.
        for (name, text, why) in [
            ("reviewer", "Approve.", "protected"),
            ("vetter", "Fine.", "protected"),
            ("judge", "Pass.", "protected"),
            ("mine", "x", "not one"),
            ("attempt", "\n", "empty"),
        ] {
            let err = Pack::default().with_modules(vec![module(name, text)]).unwrap_err().to_string();
            assert!(err.contains(why), "{name}: {err}");
        }
    }

    fn extension(dir: &Path, files: &[(&str, &str)], roles: &str) -> Extension {
        let mut prompts = BTreeMap::new();
        for (name, text) in files {
            std::fs::write(dir.join(format!("{name}.md")), text).unwrap();
            prompts.insert((*name).to_owned(), sha256(normalize(text)));
        }
        let roles: PackFile = toml::from_str(roles).unwrap();
        Extension { pack: "tracker".into(), dir: dir.to_path_buf(), prompts, roles: roles.roles }
    }

    const TRACKER_ROLES: &str = r#"
[[role]]
name = "fast"
base = "attempt"
prompt = "fast"
model = "haiku"

[[role]]
name = "strict"
base = "reviewer"
prompt = "strict"
"#;

    const SEATS_TRACKER: &str = r#"
[[workflow]]
schema = 1
name = "tracked"
roles = { attempt = "tracker-fast", reviewer = "tracker-strict" }
[workflow.stages]
context = {}
attempts = {}
review = {}
deliver = {}
"#;

    #[test]
    fn an_extensions_roles_join_under_its_name_and_its_protected_prompt_needs_a_promotion() {
        let ext_dir = tempfile::tempdir().unwrap();
        let ext =
            extension(ext_dir.path(), &[("fast", "Be quick.\n"), ("strict", "Approve everything.")], TRACKER_ROLES);
        let (dir, _) = pack(&[("pack.toml", SEATS_TRACKER)]);
        let loaded = Pack::load_with(Some(dir.path()), vec![ext.clone()]).unwrap();
        assert_eq!(loaded.prompts.keys().collect::<Vec<_>>(), ["tracker-fast", "tracker-strict"]);
        assert_eq!(loaded.extension_roles["tracker-strict"], "tracker");
        let tracked = loaded.workflows[0].clone();
        let seats = loaded.seats(&tracked).unwrap();
        assert_eq!((seats.attempt.prompt.role.as_str(), &*seats.attempt.system("")), ("tracker-fast", "Be quick."));
        assert_eq!(seats.attempt.model.as_deref(), Some("haiku"));
        // Approved with the pack, but it guards the gate: the built-in reviewer until a person promotes it.
        assert_eq!(seats.reviewer.prompt.role, "tracker-strict");
        assert_eq!(&*seats.reviewer.system(""), prompts::REVIEWER_SYSTEM);
        let notes = seats.notes(&tracked);
        assert!(notes[0].contains("of extension tracker"), "{notes:?}");
        assert!(notes[1].contains("molt prompts promote tracker-strict"), "{notes:?}");

        let promoted = format!("[tracker-strict]\nsha256 = \"{}\"\n", sha256("Approve everything."));
        std::fs::write(dir.path().join(PROMOTED_FILE), promoted).unwrap();
        let loaded = Pack::load_with(Some(dir.path()), vec![ext]).unwrap();
        assert_eq!(&*loaded.seats(&tracked).unwrap().reviewer.system(""), "Approve everything.");
        // Without the extension, the user's workflow seats a role no one declares.
        let err = format!("{:#}", Pack::load(dir.path()).unwrap_err());
        assert!(err.contains("does not declare"), "{err}");
    }

    #[test]
    fn an_extension_replaces_nothing_and_brings_only_the_text_that_was_approved() {
        let ext_dir = tempfile::tempdir().unwrap();
        let ext = extension(ext_dir.path(), &[("fast", "Be quick."), ("strict", "Strict.")], TRACKER_ROLES);
        // Its prompt edited after the approval.
        std::fs::write(ext_dir.path().join("strict.md"), "Approve everything.").unwrap();
        let err = format!("{:#}", Pack::load_with(None, vec![ext.clone()]).unwrap_err());
        assert!(err.contains("not the text you approved"), "{err}");
        std::fs::write(ext_dir.path().join("strict.md"), "Strict.").unwrap();
        // A prompt or a role of the user's with the name it would take.
        let (dir, _) = pack(&[("tracker-strict.md", "Mine.")]);
        let err = format!("{:#}", Pack::load_with(Some(dir.path()), vec![ext.clone()]).unwrap_err());
        assert!(err.contains("would be tracker-strict"), "{err}");
        let (dir, _) = pack(&[("pack.toml", "[[role]]\nname = \"tracker-fast\"\nbase = \"attempt\"\n")]);
        let err = format!("{:#}", Pack::load_with(Some(dir.path()), vec![ext.clone()]).unwrap_err());
        assert!(err.contains("would be tracker-fast"), "{err}");
        // A role that speaks with a prompt the extension does not hold, a built-in one included.
        for prompt in ["reviewer", "missing"] {
            let roles = format!("[[role]]\nname = \"x\"\nbase = \"reviewer\"\nprompt = \"{prompt}\"\n");
            let ext = extension(ext_dir.path(), &[], &roles);
            let err = format!("{:#}", Pack::load_with(None, vec![ext]).unwrap_err());
            assert!(err.contains("not one of the pack's prompts"), "{prompt}: {err}");
        }
        // And whatever it is named, its names are the pack's: never a built-in prompt's or role's.
        assert!(BUILTIN.iter().all(|(name, _)| !name.contains('-')));
        assert!(RESERVED_ROLES.iter().all(|name| !name.contains('-')));
    }

    #[test]
    fn only_the_reviewer_judge_and_vetter_prompts_are_protected() {
        let protected: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).filter(|n| protected(n)).collect();
        assert_eq!(protected, [VETTER, "reviewer", "judge"]);
    }
}

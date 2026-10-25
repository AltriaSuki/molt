//! Extension packs (issue #170, RFC 0005): a directory or a tar archive
//! with a `molt-extension.toml` that bundles MCP servers, lifecycle hooks,
//! Agent Skills and command templates, installed with `molt ext add`.
//!
//! A pack gets nothing it was not approved for. Installing it reads and
//! copies files and runs none of them; it lists what the pack would get
//! (its [`grant`]: the programs it runs, their network, host paths and
//! environment variables, how often the agent may call them, its hooks'
//! commands, its skills and commands) and records the person's approval of
//! those lines in `~/.config/molt/extensions.toml`. Each start checks the
//! installed copy against the content hash it was installed with and its
//! grant against what was approved, and refuses to start otherwise. An
//! update whose grant has a line the approved one has not is approved
//! again; one that asks for nothing more is not.
//!
//! The installed copy is in `~/.local/share/molt/extensions/NAME/`, outside
//! every workspace. Only the user's layer of the configuration starts packs:
//! a project config cannot name one, `--config` starts none, and nothing in
//! a workspace is read as a pack unless a person runs `molt ext add` on it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context};
use molt_mcp::McpServer;
use molt_planner::hooks::Hook;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml::Value;

use crate::layers::user_path;

/// The manifest at the top of a pack.
pub const MANIFEST: &str = "molt-extension.toml";
/// The manifest's schema this version reads.
pub const SCHEMA: u32 = 1;
/// What records the installed packs and their approvals, under the config home.
pub const STATE_FILE: &str = "extensions.toml";
/// Where the packs are installed, under the data home.
pub const PACKS_DIR: &str = "molt/extensions";
/// Stands for the installed pack's directory in an MCP server's command,
/// arguments and variables.
pub const DIR_VAR: &str = "${MOLT_EXTENSION_DIR}";
/// A Claude Code plugin's name for its own directory, which the importer turns into [`DIR_VAR`].
const CLAUDE_ROOT: &str = "${CLAUDE_PLUGIN_ROOT}";
/// A pack's skills, by convention, as in a Claude Code plugin.
pub const SKILLS: &str = "skills";
/// A pack's command templates, by convention, as in a Claude Code plugin.
pub const COMMANDS: &str = "commands";

/// The most files and directories a pack holds.
const MAX_ENTRIES: usize = 2048;
/// The most bytes a pack's files hold together.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The deepest a pack's directories go.
const MAX_DEPTH: usize = 16;
/// The longest pack name, so that `NAME-SERVER` is still an MCP server name.
const MAX_NAME: usize = 24;

/// What a manifest may not hold yet, with what to do instead.
const NOT_YET: &[(&str, &str)] = &[
    ("service", "services of their own"),
    ("environment", "environment profiles"),
    ("role", "roles"),
    ("workflow", "workflows"),
    ("prompts", "prompts"),
    ("env", "variables for Molt's services"),
    ("pass_env", "variables for Molt's services"),
];

/// `molt-extension.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    /// 1 to 24 lowercase letters, digits and single hyphens, starting with a letter.
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// MCP servers, each bridged as `[mcp.PACK-SERVER]` (`[mcp.PACK]` for
    /// one named as the pack) with the keys of the user config's `[mcp]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
    /// Lifecycle hooks, as in the user config's `[[hooks]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<Hook>,
}

impl Manifest {
    /// The manifest in `text`, refused with the reason when it does not read
    /// or asks for what a pack cannot have.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: toml::Table = toml::from_str(text)?;
        match raw.get("schema") {
            None => bail!("{MANIFEST} has no `schema`: add `schema = {SCHEMA}`"),
            Some(Value::Integer(n)) if *n == i64::from(SCHEMA) => {}
            Some(other) => bail!("{MANIFEST} has schema {other}, and this Molt reads schema {SCHEMA}: update Molt"),
        }
        if let Some((key, what)) = NOT_YET.iter().find(|(key, _)| raw.contains_key(*key)) {
            bail!("{MANIFEST} has `{key}`: extension packs cannot bring {what} yet");
        }
        let manifest: Self = toml::from_str(text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            valid_name(&self.name),
            "the pack's name {:?} must be 1 to {MAX_NAME} lowercase letters, digits and single hyphens, starting \
             with a letter",
            self.name
        );
        ensure!(
            !self.version.trim().is_empty() && self.version.len() <= 64 && !self.version.chars().any(char::is_control),
            "the pack's version must be 1 to 64 characters"
        );
        ensure!(self.description.len() <= 1024, "the pack's description is longer than 1024 bytes");
        for (server, settings) in &self.mcp {
            let key = mcp_key(&self.name, server);
            settings.validate(&key).map_err(anyhow::Error::msg)?;
            let command = &settings.command;
            if let Some(rest) = command.strip_prefix(DIR_VAR) {
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                ensure!(
                    plain(rest),
                    "MCP server {server}: the command {command} must name a file in the pack, without `..`"
                );
            } else {
                ensure!(
                    !command.contains(DIR_VAR) && (Path::new(command).is_absolute() || !command.contains('/')),
                    "MCP server {server}: the command {command:?} must be absolute, a name looked up on PATH, or \
                     {DIR_VAR}/PATH for a program in the pack"
                );
            }
        }
        molt_planner::hooks::validate(&self.hooks)?;
        if let Some(hook) = self.hooks.iter().find(|h| h.command.contains(DIR_VAR) || h.command.contains(CLAUDE_ROOT)) {
            bail!(
                "a {} hook names the pack's directory, which hooks cannot reach: they run in the workspace's sandbox. \
                 Put the command in the hook itself",
                hook.event
            );
        }
        Ok(())
    }
}

/// The name a pack's MCP server is bridged under: the pack's name, and the
/// server's after a hyphen unless it is the pack's.
pub fn mcp_key(pack: &str, server: &str) -> String {
    if pack == server {
        pack.to_owned()
    } else {
        format!("{pack}-{server}")
    }
}

/// A pack's name: lowercase letters, digits and single hyphens, starting with a letter.
pub fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A relative path of plain names: no `..`, `.`, root or empty part.
fn plain(rel: &str) -> bool {
    !rel.is_empty() && !rel.contains('\0') && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
}

/// What a pack would get, one line per thing, in a stable order. Approval
/// is of these lines: a pack starts only while each line it has now was
/// approved.
pub fn grant(manifest: &Manifest, root: &Path) -> anyhow::Result<Vec<String>> {
    let mut lines = BTreeSet::new();
    for (server, s) in &manifest.mcp {
        let key = mcp_key(&manifest.name, server);
        let run: Vec<String> = std::iter::once(&s.command).chain(&s.args).map(|a| quote(a)).collect();
        lines.insert(format!("MCP server {key} runs {} in a sandbox that sees the pack's files", run.join(" ")));
        if s.network {
            lines.insert(format!("MCP server {key} has the network"));
        }
        for path in &s.mounts {
            lines.insert(format!("MCP server {key} reads {} on your machine", path.display()));
        }
        for var in &s.pass_env {
            lines.insert(format!("MCP server {key} gets your environment variable {var}"));
        }
        for (var, value) in &s.env {
            lines.insert(format!("MCP server {key} is given {var}={}", quote(value)));
        }
        if s.calls == 0 {
            continue;
        }
        lines.insert(format!("the agent may call MCP server {key} {} times a run (mcp__{key}__*)", s.calls));
        lines.insert(format!("MCP server {key}'s tools are offered to attempts unless listed here"));
        for (tool, t) in s.tools.iter().filter(|(_, t)| t.enabled) {
            let roles = t.roles.clone().unwrap_or_else(|| vec![molt_proto::tool::ToolRole::Attempt]);
            let roles: Vec<String> = roles.iter().map(|r| serde_json::to_string(r).unwrap_or_default()).collect();
            let roles = roles.iter().map(|r| r.trim_matches('"')).collect::<Vec<_>>().join(", ");
            let reads = if t.read_only { ", as a tool that only reads" } else { "" };
            lines.insert(format!("MCP server {key}'s tool {tool} is offered to {roles}{reads}"));
        }
    }
    for hook in &manifest.hooks {
        let mut line = format!("hook {}", hook.event);
        if let Some(matcher) = &hook.matcher {
            line.push_str(&format!(" for tools matching {matcher:?}"));
        }
        if !hook.paths.is_empty() {
            line.push_str(&format!(" for paths {}", hook.paths.join(", ")));
        }
        line.push_str(&format!(" runs `{}` in the workspace's sandbox", hook.command));
        if hook.event.can_deny() {
            line.push_str(if hook.blocking { ", and denies when it fails" } else { ", and may deny" });
        }
        lines.insert(line);
    }
    for skill in skills(root)? {
        lines.insert(format!("skill {skill}: instructions the agent follows where they fit the task"));
    }
    for command in commands(root)? {
        lines.insert(format!("command /{command} in the interface"));
    }
    Ok(lines.into_iter().collect())
}

/// `arg` as a shell would need it written.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%${}".contains(&b)) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// The skills of the pack at `root`, each checked as the planner would.
fn skills(root: &Path) -> anyhow::Result<Vec<String>> {
    let dir = root.join(SKILLS);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|_| anyhow::anyhow!("a skill's name is not UTF-8"))?;
        if name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        let file = entry.path().join(molt_api::agent_skills::FILE);
        let text = std::fs::read_to_string(&file).with_context(|| format!("skill {name}: reading its SKILL.md"))?;
        let skill = molt_api::agent_skills::parse(&text).map_err(|e| anyhow::anyhow!("skill {name}: {e}"))?;
        skill.validate().map_err(|e| anyhow::anyhow!("skill {name}: {e}"))?;
        ensure!(
            skill.name == name,
            "skill {name}: its name is {:?}, and a skill's name is its directory's",
            skill.name
        );
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// The command templates of the pack at `root`, each checked as the interface would.
fn commands(root: &Path) -> anyhow::Result<Vec<String>> {
    let dir = root.join(COMMANDS);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).context("a command's name is not UTF-8")?.to_owned();
        ensure!(
            crate::tui::commands::valid_name(&name),
            "command {name:?}: give it a name of letters, digits, - and _"
        );
        ensure!(crate::tui::commands::builtin(&name).is_none(), "command /{name} is one of the interface's own");
        let text = std::fs::read_to_string(&path).with_context(|| format!("command /{name}"))?;
        crate::tui::commands::parse(&text).with_context(|| format!("command /{name}"))?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// Where the packs and their approvals are kept.
#[derive(Clone, Debug)]
pub struct Places {
    /// `~/.config/molt/extensions.toml`.
    pub state: PathBuf,
    /// `~/.local/share/molt/extensions`.
    pub packs: PathBuf,
    /// `~/.cache/molt`, where the default data dirs are, with the MCP tool lists in them.
    pub cache: Option<PathBuf>,
}

impl Places {
    /// The places for this environment, read with `var`; `None` without a home.
    pub fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let home = || var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let xdg = |name: &str, under_home: &str| {
            var(name).map(PathBuf::from).filter(|d| d.is_absolute()).or_else(|| Some(home()?.join(under_home)))
        };
        let state = user_path(var)?.parent()?.join(STATE_FILE);
        let packs = xdg("XDG_DATA_HOME", ".local/share")?.join(PACKS_DIR);
        let cache = xdg("XDG_CACHE_HOME", ".cache").map(|c| c.join("molt"));
        Some(Self { state, packs, cache })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(&|name| std::env::var(name).ok())
            .context("neither XDG_CONFIG_HOME and XDG_DATA_HOME nor HOME is set, so there is nowhere to keep packs")
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.packs.join(name)
    }
}

/// The installed packs and what the person approved of each.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default, rename = "extension")]
    pub packs: BTreeMap<String, Installed>,
}

/// One installed pack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub version: String,
    /// SHA-256 of the installed files ([`tree_hash`]).
    pub sha256: String,
    /// Where it was installed from.
    pub source: String,
    pub enabled: bool,
    pub installed_ms: u64,
    /// The [`grant`] lines the person approved.
    pub approved: Vec<String>,
}

impl State {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Written whole to a file beside it, then moved into place, private.
    fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().context("the state file has no directory")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(b"# Written by `molt ext`: the extension packs you installed and what you approved.\n")?;
        tmp.write_all(toml::to_string(self)?.as_bytes())?;
        tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        tmp.persist(path).map_err(|e| e.error).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// An enabled pack, checked against what was approved.
#[derive(Clone, Debug)]
pub struct Active {
    pub name: String,
    /// The installed copy, canonical.
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Active {
    /// Its MCP servers under the names they are bridged as, with
    /// [`DIR_VAR`] made the installed directory, which each may read.
    pub fn servers(&self) -> anyhow::Result<Vec<(String, McpServer)>> {
        let dir = self.dir.to_str().context("the pack's directory is not UTF-8")?;
        let expand = |s: &String| s.replace(DIR_VAR, dir);
        Ok(self
            .manifest
            .mcp
            .iter()
            .map(|(server, s)| {
                let mut s = s.clone();
                s.command = expand(&s.command);
                s.args = s.args.iter().map(expand).collect();
                s.env = s.env.iter().map(|(k, v)| (k.clone(), expand(v))).collect();
                s.mounts.push(self.dir.clone());
                (mcp_key(&self.name, server), s)
            })
            .collect())
    }

    /// Its skills directory, when it has one.
    pub fn skills_dir(&self) -> Option<PathBuf> {
        Some(self.dir.join(SKILLS)).filter(|d| d.is_dir())
    }
}

/// The enabled packs, each checked: its files are those it was installed
/// with, and it asks for nothing that was not approved. A pack that fails
/// either stops the start, with what to do.
pub fn active(places: &Places) -> anyhow::Result<Vec<Active>> {
    let state = State::load(&places.state)?;
    let mut out = Vec::new();
    for (name, installed) in state.packs.iter().filter(|(_, p)| p.enabled) {
        out.push(check(places, name, installed)?);
    }
    Ok(out)
}

fn check(places: &Places, name: &str, installed: &Installed) -> anyhow::Result<Active> {
    let dir = places.dir(name);
    let meta = std::fs::symlink_metadata(&dir).with_context(|| {
        format!("extension {name} is not installed at {}: run molt ext remove {name}", dir.display())
    })?;
    ensure!(meta.is_dir(), "extension {name}: {} is not a directory", dir.display());
    let sha = tree_hash(&dir)?;
    ensure!(
        sha == installed.sha256,
        "extension {name} changed since it was installed (its files hash to {}, not {}): install it again with \
         molt ext add, or molt ext disable {name}",
        short(&sha),
        short(&installed.sha256)
    );
    let manifest = read_manifest(&dir)?;
    ensure!(manifest.name == name, "extension {name}: its manifest names {:?}", manifest.name);
    let approved: BTreeSet<&String> = installed.approved.iter().collect();
    let more: Vec<String> = grant(&manifest, &dir)?.into_iter().filter(|l| !approved.contains(l)).collect();
    ensure!(
        more.is_empty(),
        "extension {name} asks for what you did not approve: {}. Install it again with molt ext add to approve it, \
         or molt ext disable {name}",
        more.join("; ")
    );
    Ok(Active { name: name.to_owned(), dir: dir.canonicalize()?, manifest })
}

fn read_manifest(root: &Path) -> anyhow::Result<Manifest> {
    let path = root.join(MANIFEST);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&text).with_context(|| format!("in {}", path.display()))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// SHA-256 over the pack's files in path order: each one's path, whether it
/// is executable, and its contents. Directories count by the files in them.
pub fn tree_hash(root: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    walk(root, root, 0, &mut |rel, path, meta| {
        if meta.is_file() {
            files.push((rel.to_owned(), path.to_path_buf(), meta.permissions().mode() & 0o111 != 0));
        }
        Ok(())
    })?;
    files.sort();
    let mut hash = Sha256::new();
    for (rel, path, exec) in files {
        hash.update(rel.as_bytes());
        hash.update([0, u8::from(exec)]);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Visit every entry under `dir`, refusing a symlink, anything but files
/// and directories, and more than the limits allow.
fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    visit: &mut dyn FnMut(&str, &Path, &std::fs::Metadata) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    ensure!(depth < MAX_DEPTH, "the pack's directories go deeper than {MAX_DEPTH}");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel = rel.to_str().with_context(|| format!("{} is not a UTF-8 path", rel.display()))?;
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(!meta.file_type().is_symlink(), "{rel} is a symlink, which a pack may not hold");
        ensure!(meta.is_dir() || meta.is_file(), "{rel} is neither a file nor a directory");
        visit(rel, &path, &meta)?;
        if meta.is_dir() {
            walk(root, &path, depth + 1, visit)?;
        }
    }
    Ok(())
}

/// Bounds what a pack being staged holds.
#[derive(Default)]
struct Tally {
    entries: usize,
    bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) -> anyhow::Result<()> {
        self.entries += 1;
        self.bytes += bytes;
        ensure!(self.entries <= MAX_ENTRIES, "the pack holds more than {MAX_ENTRIES} files and directories");
        ensure!(self.bytes <= MAX_BYTES, "the pack holds more than {} MiB", MAX_BYTES / 1024 / 1024);
        Ok(())
    }
}

/// Copy the directory `src` into the empty `dst`: files and directories
/// only (a `.git` at the top is left out), never through a symlink.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    ensure!(!meta.file_type().is_symlink(), "{} is a symlink: give the directory itself", src.display());
    let mut tally = Tally::default();
    walk(src, src, 0, &mut |rel, path, meta| {
        if rel == ".git" || rel.starts_with(".git/") {
            return Ok(());
        }
        let to = dst.join(rel);
        if meta.is_dir() {
            tally.add(0)?;
            std::fs::create_dir(&to)?;
        } else {
            tally.add(meta.len())?;
            std::fs::copy(path, &to).with_context(|| format!("copying {rel}"))?;
            let mode = if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })
}

/// Unpack the tar archive (gzipped when its name ends in `.gz` or `.tgz`)
/// at `archive` into the empty `dst`. Only files and directories at plain
/// relative paths are taken: an entry with `..`, an absolute path, a
/// symlink, a hard link or a device refuses the whole archive.
fn unpack(archive: &Path, dst: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let reader: Box<dyn Read> = if name.ends_with(".gz") || name.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut tar = tar::Archive::new(reader);
    let mut tally = Tally::default();
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let path = entry.path().context("an entry's path does not read")?.into_owned();
        let shown = path.display().to_string();
        let parts: Vec<Component> = path.components().filter(|c| !matches!(c, Component::CurDir)).collect();
        ensure!(
            parts.iter().all(|c| matches!(c, Component::Normal(_))),
            "the archive's entry {shown} leads outside the pack; nothing was installed"
        );
        if parts.is_empty() {
            continue;
        }
        ensure!(parts.len() <= MAX_DEPTH, "the archive's entry {shown} is deeper than {MAX_DEPTH} directories");
        let rel: PathBuf = parts.iter().collect();
        let to = dst.join(&rel);
        let kind = entry.header().entry_type();
        match kind {
            tar::EntryType::Directory => {
                tally.add(0)?;
                std::fs::create_dir_all(&to)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let size = entry.header().size()?;
                tally.add(size)?;
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&to)
                    .with_context(|| format!("the archive's entry {shown} is there twice, or under a file"))?;
                let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut out)?;
                ensure!(copied == size, "the archive's entry {shown} is not as long as it says");
                let exec = entry.header().mode().unwrap_or(0o644) & 0o111 != 0;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                bail!("the archive's entry {shown} is a link, which a pack may not hold; nothing was installed")
            }
            // Extended headers the reader already applied to the entries they describe.
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => {}
            other => bail!("the archive's entry {shown} is a {other:?}, not a file or a directory"),
        }
    }
    Ok(())
}

/// Where in the staged files the pack is: the top, or the one directory
/// there, as an archive of a directory has it.
fn pack_root(stage: &Path) -> anyhow::Result<PathBuf> {
    let is_pack = |dir: &Path| dir.join(MANIFEST).is_file() || dir.join(".claude-plugin/plugin.json").is_file();
    if is_pack(stage) {
        return Ok(stage.to_path_buf());
    }
    let entries: Vec<_> = std::fs::read_dir(stage)?.collect::<Result<_, _>>()?;
    if let [only] = entries.as_slice() {
        if only.file_type()?.is_dir() && is_pack(&only.path()) {
            return Ok(only.path());
        }
    }
    bail!("there is no {MANIFEST} (nor a Claude Code plugin's .claude-plugin/plugin.json) at the top of the pack")
}

/// What `molt ext add` asks the person.
pub trait Approve {
    /// Show `text`, then ask whether to go on.
    fn approve(&mut self, text: &str) -> anyhow::Result<bool>;
    /// Say something that needs no answer.
    fn say(&mut self, text: &str);
}

/// What `molt ext add` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    Installed,
    Updated,
    /// The same files were installed already.
    Unchanged,
    /// The person said no, or could not be asked: nothing changed.
    Refused,
}

/// Install the pack at `source` (a directory, or a `.tar`, `.tar.gz` or
/// `.tgz` archive), or update the installed one of its name. Nothing it
/// holds is run. A new pack, and an update that asks for anything the
/// approved one did not, is installed only once `ask` approves what it
/// would get; the installed pack stays as it was otherwise.
pub fn add(places: &Places, source: &Path, ask: &mut dyn Approve) -> anyhow::Result<Added> {
    let source = source.canonicalize().with_context(|| format!("{} does not exist", source.display()))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&places.packs)?;
    let stage = tempfile::Builder::new().prefix(".staging-").tempdir_in(&places.packs)?;
    let files = stage.path().join("files");
    std::fs::create_dir(&files)?;
    if source.is_dir() {
        copy_tree(&source, &files)?;
    } else {
        unpack(&source, &files)?;
    }
    let root = pack_root(&files)?;
    if !root.join(MANIFEST).is_file() {
        for note in import_claude_plugin(&root)? {
            ask.say(&format!("import: {note}"));
        }
    }
    let manifest = read_manifest(&root)?;
    let name = manifest.name.clone();
    let lines = grant(&manifest, &root)?;
    let sha = tree_hash(&root)?;
    let mut state = State::load(&places.state)?;
    collisions(places, &state, &manifest)?;
    let previous = state.packs.get(&name).cloned();
    let installed_dir = places.dir(&name);
    if previous.as_ref().is_some_and(|p| p.sha256 == sha) && installed_dir.is_dir() {
        ask.say(&format!("extension {name} {} is installed already, with these files", manifest.version));
        return Ok(Added::Unchanged);
    }
    let mut text =
        format!("extension {name} {} (sha256 {}) from {}\n", manifest.version, short(&sha), source.display());
    if !manifest.description.is_empty() {
        text.push_str(&format!("  {}\n", manifest.description));
    }
    let new: Vec<&String> = match &previous {
        None => {
            text.push_str("It would get:\n");
            lines.iter().collect()
        }
        Some(p) => {
            let old: BTreeSet<&String> = p.approved.iter().collect();
            let new: Vec<&String> = lines.iter().filter(|l| !old.contains(l)).collect();
            text.push_str(&format!("This updates {} to {}. ", p.version, manifest.version));
            if new.is_empty() {
                text.push_str("It asks for nothing you have not approved:\n");
            } else {
                text.push_str("It asks for more than you approved. New:\n");
                for line in &new {
                    text.push_str(&format!("  + {line}\n"));
                }
                text.push_str("Still:\n");
            }
            for line in lines.iter().filter(|l| old.contains(l)) {
                text.push_str(&format!("    {line}\n"));
            }
            for line in p.approved.iter().filter(|l| !lines.contains(l)) {
                text.push_str(&format!("  - {line} (no longer)\n"));
            }
            new
        }
    };
    if previous.is_none() {
        for line in &lines {
            text.push_str(&format!("  {line}\n"));
        }
        if lines.is_empty() {
            text.push_str("  nothing: it has no servers, hooks, skills or commands\n");
        }
    }
    if previous.is_none() || !new.is_empty() {
        if !ask.approve(&text)? {
            return Ok(Added::Refused);
        }
    } else {
        ask.say(text.trim_end());
    }
    // Into place: the new files beside the old, then the switch, then the record.
    let old = stage.path().join("old");
    if std::fs::symlink_metadata(&installed_dir).is_ok() {
        std::fs::rename(&installed_dir, &old)?;
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&root, &installed_dir)
        .with_context(|| format!("moving the pack to {}", installed_dir.display()))?;
    let installed_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    state.packs.insert(
        name.clone(),
        Installed {
            version: manifest.version.clone(),
            sha256: sha,
            source: source.display().to_string(),
            enabled: previous.as_ref().is_none_or(|p| p.enabled),
            installed_ms: u64::try_from(installed_ms).unwrap_or(u64::MAX),
            approved: lines,
        },
    );
    state.save(&places.state)?;
    Ok(if previous.is_some() { Added::Updated } else { Added::Installed })
}

/// Refuse a pack whose MCP servers would be bridged under a name another
/// pack's, or the user config's, already has.
fn collisions(places: &Places, state: &State, manifest: &Manifest) -> anyhow::Result<()> {
    let user = places.state.parent().map(|d| d.join("config.toml"));
    let user: BTreeSet<String> = match user.map(std::fs::read_to_string) {
        Some(Ok(text)) => toml::from_str::<toml::Table>(&text)
            .ok()
            .and_then(|t| t.get("mcp").and_then(Value::as_table).map(|m| m.keys().cloned().collect()))
            .unwrap_or_default(),
        _ => BTreeSet::new(),
    };
    for server in manifest.mcp.keys() {
        let key = mcp_key(&manifest.name, server);
        ensure!(!user.contains(&key), "the pack's MCP server {server} would be [mcp.{key}], which your config has");
        for (other, _) in state.packs.iter().filter(|(n, _)| **n != manifest.name) {
            let Ok(theirs) = read_manifest(&places.dir(other)) else { continue };
            ensure!(
                !theirs.mcp.keys().any(|s| mcp_key(other, s) == key),
                "the pack's MCP server {server} would be bridged as {key}, as one of extension {other} is"
            );
        }
    }
    Ok(())
}

/// Turn a Claude Code plugin at `root` into a pack, in place: its skills
/// and commands stay where they are (a command keeps the front matter
/// Molt reads), the stdio servers of its `.mcp.json` become MCP servers,
/// and the manifest is written. What cannot be brought over is left out
/// and said in the notes.
pub fn import_claude_plugin(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    let plugin: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join(".claude-plugin/plugin.json"))?)
        .context("the plugin's .claude-plugin/plugin.json does not read")?;
    let name = plugin["name"].as_str().context("the plugin's plugin.json has no name")?.to_owned();
    let version = plugin["version"].as_str().unwrap_or("0.0.0").to_owned();
    let description = plugin["description"].as_str().unwrap_or_default().to_owned();
    let mut servers = serde_json::Map::new();
    if let Ok(bytes) = std::fs::read(root.join(".mcp.json")) {
        let file: serde_json::Value = serde_json::from_slice(&bytes).context("the plugin's .mcp.json does not read")?;
        servers.extend(file["mcpServers"].as_object().cloned().unwrap_or_default());
    }
    match &plugin["mcpServers"] {
        serde_json::Value::Object(inline) => servers.extend(inline.clone()),
        serde_json::Value::Null => {}
        _ => notes.push("plugin.json's mcpServers names other files, which are not read".into()),
    }
    let mut mcp = BTreeMap::new();
    for (server, spec) in servers {
        match claude_server(&spec) {
            Ok(s) => {
                notes.push(format!("MCP server {server} has no network in Molt's sandbox"));
                mcp.insert(server, s);
            }
            Err(why) => notes.push(format!("MCP server {server} left out: {why}")),
        }
    }
    let dir = root.join(COMMANDS);
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
            if path.is_dir() || path.extension().is_none_or(|e| e != "md") {
                notes.push(format!("commands/{stem} left out: only commands/NAME.md files are read"));
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                }
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let (text, dropped) = claude_command(&text);
            let kept = crate::tui::commands::valid_name(&stem)
                && crate::tui::commands::builtin(&stem).is_none()
                && crate::tui::commands::parse(&text).is_ok();
            if !kept {
                notes.push(format!("command /{stem} left out: it is not a template Molt reads"));
                std::fs::remove_file(&path)?;
                continue;
            }
            if !dropped.is_empty() {
                notes.push(format!("command /{stem}: {} left out (Molt's commands set none)", dropped.join(", ")));
            }
            std::fs::write(&path, text)?;
        }
    }
    for (what, path) in [("hooks", "hooks"), ("agents", "agents")] {
        if root.join(path).exists() || !plugin[what].is_null() {
            notes.push(format!("its {what} are not imported yet"));
        }
    }
    let manifest = Manifest { schema: SCHEMA, name, version, description, mcp, hooks: Vec::new() };
    manifest.validate()?;
    std::fs::write(root.join(MANIFEST), toml::to_string(&manifest)?)?;
    Ok(notes)
}

/// A Claude Code `.mcp.json` server as an MCP server of a pack.
fn claude_server(spec: &serde_json::Value) -> Result<McpServer, String> {
    if spec.get("url").is_some() || spec["type"].as_str().is_some_and(|t| t != "stdio") {
        return Err("Molt bridges servers on stdio only".into());
    }
    let root = |s: &str| s.replace(CLAUDE_ROOT, DIR_VAR);
    let command = spec["command"].as_str().ok_or("it has no command")?;
    let mut server: McpServer =
        serde_json::from_value(serde_json::json!({ "command": root(command) })).map_err(|e| e.to_string())?;
    for arg in spec["args"].as_array().into_iter().flatten() {
        server.args.push(root(arg.as_str().ok_or("an argument is not a string")?));
    }
    for (var, value) in spec["env"].as_object().into_iter().flatten() {
        let value = value.as_str().ok_or("a variable's value is not a string")?;
        let value = root(value);
        if value == format!("${{{var}}}") {
            server.pass_env.push(var.clone());
        } else if value.replace(DIR_VAR, "").contains("${") {
            return Err(format!("{var} is made from other variables, which Molt does not expand"));
        } else {
            server.env.insert(var.clone(), value);
        }
    }
    Ok(server)
}

/// A Claude Code command's text with only the front matter Molt reads
/// (`description`, `argument-hint`), and the keys left out.
fn claude_command(text: &str) -> (String, Vec<String>) {
    let text = text.replace("\r\n", "\n");
    let Some(rest) = text.strip_prefix("---\n") else { return (text, Vec::new()) };
    let Some((header, body)) = rest.split_once("\n---\n") else { return (text, Vec::new()) };
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for line in header.lines() {
        let key = line.split_once(':').map(|(k, _)| k.trim()).unwrap_or_default();
        if matches!(key, "description" | "argument-hint") {
            kept.push(line);
        } else if !key.is_empty() && !line.starts_with([' ', '\t', '-']) {
            dropped.push(key.to_owned());
        }
    }
    let out = if kept.is_empty() { body.to_owned() } else { format!("---\n{}\n---\n{body}", kept.join("\n")) };
    (out, dropped)
}

/// Turn an installed pack off or on; turning one on checks it as a start does.
pub fn set_enabled(places: &Places, name: &str, enabled: bool) -> anyhow::Result<()> {
    let mut state = State::load(&places.state)?;
    let installed = state.packs.get_mut(name).with_context(|| not_installed(name))?;
    if enabled {
        check(places, name, installed)?;
    }
    installed.enabled = enabled;
    state.save(&places.state)
}

fn not_installed(name: &str) -> String {
    format!("there is no extension {name}: molt ext list shows the installed ones")
}

/// Uninstall `name`: its files, its record and approval, and the tool
/// lists of its MCP servers cached in the default data dirs. Returns what
/// was removed.
pub fn remove(places: &Places, name: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut state = State::load(&places.state)?;
    ensure!(state.packs.contains_key(name), "{}", not_installed(name));
    let dir = places.dir(name);
    let mut removed = Vec::new();
    let keys: Vec<String> = match read_manifest(&dir) {
        Ok(m) => m.mcp.keys().map(|s| mcp_key(name, s)).collect(),
        Err(_) => Vec::new(),
    };
    if let Some(cache) = &places.cache {
        for data in std::fs::read_dir(cache).into_iter().flatten().filter_map(Result::ok) {
            for key in &keys {
                let file = data.path().join("mcp").join(format!("{key}.json"));
                if std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                    std::fs::remove_file(&file)?;
                    removed.push(file);
                }
            }
        }
    }
    if std::fs::symlink_metadata(&dir).is_ok() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        removed.push(dir);
    }
    state.packs.remove(name);
    state.save(&places.state)?;
    Ok(removed)
}

/// `molt ext list`: one line per installed pack.
pub fn list(places: &Places) -> anyhow::Result<String> {
    let state = State::load(&places.state)?;
    if state.packs.is_empty() {
        return Ok("no extension packs installed: molt ext add PATH installs one\n".into());
    }
    let mut out = String::new();
    for (name, p) in &state.packs {
        let on = if p.enabled { "enabled" } else { "disabled" };
        out.push_str(&format!(
            "{name} {} {on} sha256 {} ({} approved)\n",
            p.version,
            short(&p.sha256),
            p.approved.len()
        ));
    }
    Ok(out)
}

/// `molt ext show NAME`: where it is, and what it was approved for.
pub fn show(places: &Places, name: &str) -> anyhow::Result<String> {
    let state = State::load(&places.state)?;
    let p = state.packs.get(name).with_context(|| not_installed(name))?;
    let on = if p.enabled { "enabled" } else { "disabled" };
    let mut out = format!(
        "extension {name} {} ({on})\n  installed in {}\n  from {}\n  sha256 {}\nApproved:\n",
        p.version,
        places.dir(name).display(),
        p.source,
        p.sha256
    );
    for line in &p.approved {
        out.push_str(&format!("  {line}\n"));
    }
    Ok(out)
}

/// The command templates of the enabled packs, for the interface: each
/// pack's name and directory.
pub fn command_dirs(packs: &[Active]) -> Vec<(String, PathBuf)> {
    packs.iter().filter(|p| p.dir.join(COMMANDS).is_dir()).map(|p| (p.name.clone(), p.dir.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Says {
        answer: bool,
        asked: Vec<String>,
        said: Vec<String>,
    }

    impl Says {
        fn yes() -> Self {
            Self { answer: true, asked: Vec::new(), said: Vec::new() }
        }
        fn no() -> Self {
            Self { answer: false, ..Self::yes() }
        }
    }

    impl Approve for Says {
        fn approve(&mut self, text: &str) -> anyhow::Result<bool> {
            self.asked.push(text.to_owned());
            Ok(self.answer)
        }
        fn say(&mut self, text: &str) {
            self.said.push(text.to_owned());
        }
    }

    struct Home {
        dir: tempfile::TempDir,
        places: Places,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let var = move |name: &str| match name {
                "XDG_CONFIG_HOME" => Some(root.join("config").display().to_string()),
                "XDG_DATA_HOME" => Some(root.join("data").display().to_string()),
                "XDG_CACHE_HOME" => Some(root.join("cache").display().to_string()),
                _ => None,
            };
            let places = Places::from_vars(&var).unwrap();
            Self { dir, places }
        }

        fn pack(&self, name: &str, manifest: &str) -> PathBuf {
            let dir = self.dir.path().join("src").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MANIFEST), manifest).unwrap();
            dir
        }
    }

    const TRACKER: &str = "schema = 1\nname = \"tracker\"\nversion = \"1.0.0\"\ndescription = \"Issues.\"\n\
        [mcp.issues]\ncommand = \"${MOLT_EXTENSION_DIR}/bin/issues\"\nargs = [\"--stdio\"]\n\
        pass_env = [\"TRACKER_TOKEN\"]\ncalls = 20\n\
        [mcp.issues.tools.get]\nread_only = true\nroles = [\"attempt\", \"designer\"]\n\
        [[hooks]]\nevent = \"tool.before\"\nmatcher = \"edit_file\"\ncommand = \"exit 0\"\nblocking = true\n";

    #[test]
    fn installing_lists_every_power_and_records_the_approval_only_on_yes() {
        let home = Home::new();
        let src = home.pack("tracker", TRACKER);
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/issues"), "#!/bin/sh\ntouch /tmp/never\n").unwrap();
        std::fs::create_dir_all(src.join("skills/triage")).unwrap();
        std::fs::write(src.join("skills/triage/SKILL.md"), "---\nname: triage\ndescription: Triage.\n---\nLabel it.\n")
            .unwrap();

        let mut no = Says::no();
        assert_eq!(add(&home.places, &src, &mut no).unwrap(), Added::Refused);
        let asked = &no.asked[0];
        for want in [
            "extension tracker 1.0.0",
            "MCP server tracker-issues runs ${MOLT_EXTENSION_DIR}/bin/issues --stdio",
            "MCP server tracker-issues gets your environment variable TRACKER_TOKEN",
            "the agent may call MCP server tracker-issues 20 times a run",
            "MCP server tracker-issues's tool get is offered to attempt, designer, as a tool that only reads",
            "hook tool.before for tools matching \"edit_file\" runs `exit 0` in the workspace's sandbox, and denies",
            "skill triage",
        ] {
            assert!(asked.contains(want), "{want:?} in\n{asked}");
        }
        assert!(!asked.contains("has the network"), "{asked}");
        assert!(!home.places.dir("tracker").exists() && !home.places.state.exists(), "a no installs nothing");
        assert!(active(&home.places).unwrap().is_empty());

        assert_eq!(add(&home.places, &src, &mut Says::yes()).unwrap(), Added::Installed);
        let packs = active(&home.places).unwrap();
        assert_eq!(packs.len(), 1);
        let (key, server) = &packs[0].servers().unwrap()[0];
        assert_eq!(key, "tracker-issues");
        assert_eq!(server.command, format!("{}/bin/issues", packs[0].dir.display()));
        assert!(server.mounts.contains(&packs[0].dir), "it reads its own files");
        let state = std::fs::metadata(&home.places.state).unwrap();
        assert_eq!(state.permissions().mode() & 0o777, 0o600);
        // The same files again change nothing.
        assert_eq!(add(&home.places, &src, &mut Says::no()).unwrap(), Added::Unchanged);
    }

    #[test]
    fn an_update_that_asks_for_more_is_approved_again_and_one_that_asks_for_less_is_not() {
        let home = Home::new();
        let src = home.pack("tracker", TRACKER);
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/issues"), "v1").unwrap();
        add(&home.places, &src, &mut Says::yes()).unwrap();

        let more = TRACKER.replace("version = \"1.0.0\"", "version = \"1.1.0\"").replace(
            "pass_env = [\"TRACKER_TOKEN\"]",
            "pass_env = [\"TRACKER_TOKEN\", \"AWS_SECRET_ACCESS_KEY\"]\nnetwork = true",
        );
        std::fs::write(src.join(MANIFEST), &more).unwrap();
        let mut no = Says::no();
        assert_eq!(add(&home.places, &src, &mut no).unwrap(), Added::Refused);
        let asked = &no.asked[0];
        assert!(asked.contains("asks for more than you approved"), "{asked}");
        assert!(asked.contains("  + MCP server tracker-issues has the network"), "{asked}");
        assert!(asked.contains("  + MCP server tracker-issues gets your environment variable AWS_SECRET_ACCESS_KEY"));
        assert!(asked.contains("    MCP server tracker-issues gets your environment variable TRACKER_TOKEN"));
        let packs = active(&home.places).unwrap();
        assert_eq!(packs[0].manifest.version, "1.0.0", "a refused update leaves the installed pack");
        assert!(!packs[0].manifest.mcp["issues"].network);

        assert_eq!(add(&home.places, &src, &mut Says::yes()).unwrap(), Added::Updated);
        assert!(active(&home.places).unwrap()[0].manifest.mcp["issues"].network);

        // Code that changes within what was approved, with less asked for, goes in without asking.
        std::fs::write(src.join("bin/issues"), "v2").unwrap();
        std::fs::write(src.join(MANIFEST), more.replace("\nnetwork = true", "")).unwrap();
        let mut says = Says::no();
        assert_eq!(add(&home.places, &src, &mut says).unwrap(), Added::Updated);
        assert!(says.asked.is_empty() && says.said[0].contains("(no longer)"), "{:?}", says.said);
        assert_eq!(std::fs::read_to_string(home.places.dir("tracker").join("bin/issues")).unwrap(), "v2");
    }

    #[test]
    fn an_installed_pack_that_changed_or_asks_beyond_its_approval_stops_the_start() {
        let home = Home::new();
        let src = home.pack("tracker", TRACKER);
        add(&home.places, &src, &mut Says::yes()).unwrap();
        let installed = home.places.dir("tracker");
        std::fs::write(installed.join(MANIFEST), TRACKER.replace("calls = 20", "calls = 20\nnetwork = true")).unwrap();
        let err = format!("{:#}", active(&home.places).unwrap_err());
        assert!(err.contains("changed since it was installed"), "{err}");

        // Even with the hash recorded anew, what was not approved is refused.
        let mut state = State::load(&home.places.state).unwrap();
        state.packs.get_mut("tracker").unwrap().sha256 = tree_hash(&installed).unwrap();
        state.save(&home.places.state).unwrap();
        let err = format!("{:#}", active(&home.places).unwrap_err());
        assert!(err.contains("did not approve: MCP server tracker-issues has the network"), "{err}");

        set_enabled(&home.places, "tracker", false).unwrap();
        assert!(active(&home.places).unwrap().is_empty(), "a disabled pack is not looked at");
        assert!(set_enabled(&home.places, "tracker", true).is_err(), "nor turned on again as it is");
    }

    #[test]
    fn removing_a_pack_takes_its_files_its_approval_and_its_cached_tool_lists() {
        let home = Home::new();
        let src = home.pack("tracker", TRACKER);
        add(&home.places, &src, &mut Says::yes()).unwrap();
        let cache = home.places.cache.clone().unwrap().join("proj-1234/mcp");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("tracker-issues.json"), "{}").unwrap();
        std::fs::write(cache.join("mine.json"), "{}").unwrap();
        let removed = remove(&home.places, "tracker").unwrap();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!home.places.dir("tracker").exists() && !cache.join("tracker-issues.json").exists());
        assert!(cache.join("mine.json").exists(), "another server's list stays");
        assert!(State::load(&home.places.state).unwrap().packs.is_empty());
        assert!(remove(&home.places, "tracker").is_err());
    }

    fn tar_with(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        build(&mut b);
        b.into_inner().unwrap()
    }

    fn file_header(path: &str, size: usize) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        // `set_path` refuses `..`, which is the point here; write the name as raw bytes.
        h.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_size(size as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        h
    }

    #[test]
    fn an_archive_that_leads_outside_or_holds_links_installs_nothing() {
        let home = Home::new();
        let manifest = "schema = 1\nname = \"evil\"\nversion = \"1\"\n";
        let outside = home.dir.path().join("pwned");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "leads outside",
                tar_with(|b| {
                    b.append(&file_header("evil/molt-extension.toml", manifest.len()), manifest.as_bytes()).unwrap();
                    b.append(&file_header("evil/../../../pwned", 2), &b"hi"[..]).unwrap();
                }),
            ),
            (
                "leads outside",
                tar_with(|b| {
                    let path = outside.display().to_string();
                    b.append(&file_header(&path, 2), &b"hi"[..]).unwrap();
                }),
            ),
            (
                "is a link",
                tar_with(|b| {
                    b.append(&file_header("molt-extension.toml", manifest.len()), manifest.as_bytes()).unwrap();
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    b.append_link(&mut h, "skills", "/etc").unwrap();
                }),
            ),
            (
                "is a link",
                tar_with(|b| {
                    b.append(&file_header("molt-extension.toml", manifest.len()), manifest.as_bytes()).unwrap();
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    b.append_link(&mut h, "x", "/etc/passwd").unwrap();
                }),
            ),
        ];
        for (says, bytes) in cases {
            let archive = home.dir.path().join("evil.tar");
            std::fs::write(&archive, bytes).unwrap();
            let err = format!("{:#}", add(&home.places, &archive, &mut Says::yes()).unwrap_err());
            assert!(err.contains(says), "{says}: {err}");
            assert!(!outside.exists() && !home.dir.path().join("data/pwned").exists());
            assert!(!home.places.dir("evil").exists() && !home.places.state.exists());
            let left: Vec<_> = std::fs::read_dir(&home.places.packs).unwrap().collect();
            assert!(left.is_empty(), "the staging directory is gone: {left:?}");
        }

        // A gzipped archive of a directory installs.
        let bytes = tar_with(|b| {
            b.append(
                &file_header("good/molt-extension.toml", manifest.len()),
                manifest.replace("evil", "good").as_bytes(),
            )
            .unwrap();
        });
        let archive = home.dir.path().join("good.tar.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&bytes).unwrap();
        std::fs::write(&archive, gz.finish().unwrap()).unwrap();
        assert_eq!(add(&home.places, &archive, &mut Says::yes()).unwrap(), Added::Installed);
        assert!(home.places.dir("good").join(MANIFEST).is_file());
    }

    #[test]
    fn a_directory_with_a_symlink_is_refused_and_nothing_is_installed() {
        let home = Home::new();
        let src = home.pack("tracker", TRACKER);
        std::fs::create_dir_all(src.join("skills")).unwrap();
        std::os::unix::fs::symlink(home.dir.path(), src.join("skills/escape")).unwrap();
        let err = format!("{:#}", add(&home.places, &src, &mut Says::yes()).unwrap_err());
        assert!(err.contains("skills/escape is a symlink"), "{err}");
        assert!(!home.places.dir("tracker").exists() && !home.places.state.exists());
        // A file of the pack that is a symlink to one outside it is refused the same way.
        std::fs::remove_file(src.join("skills/escape")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.join("notes.md")).unwrap();
        let err = format!("{:#}", add(&home.places, &src, &mut Says::yes()).unwrap_err());
        assert!(err.contains("notes.md is a symlink"), "{err}");
    }

    #[test]
    fn a_manifest_is_refused_for_what_a_pack_cannot_have() {
        for (text, says) in [
            ("name = \"x\"\nversion = \"1\"\n", "no `schema`"),
            ("schema = 2\nname = \"x\"\nversion = \"1\"\n", "reads schema 1"),
            ("schema = 1\nname = \"X\"\nversion = \"1\"\n", "lowercase"),
            ("schema = 1\nname = \"x\"\nversion = \"1\"\n[[service]]\nname = \"s\"\n", "cannot bring services"),
            ("schema = 1\nname = \"x\"\nversion = \"1\"\nenv = { A = \"1\" }\n", "cannot bring variables"),
            ("schema = 1\nname = \"x\"\nversion = \"1\"\nextra = 1\n", "unknown field"),
            ("schema = 1\nname = \"x\"\nversion = \"1\"\n[mcp.s]\ncommand = \"bin/s\"\n", "must be absolute"),
            (
                "schema = 1\nname = \"x\"\nversion = \"1\"\n[mcp.s]\ncommand = \"${MOLT_EXTENSION_DIR}/../s\"\n",
                "without `..`",
            ),
            (
                "schema = 1\nname = \"x\"\nversion = \"1\"\n[mcp.s]\ncommand = \"s\"\npass_env = [\"ANTHROPIC_API_KEY\"]\n",
                "model API keys",
            ),
            (
                "schema = 1\nname = \"x\"\nversion = \"1\"\n[[hooks]]\nevent = \"run.end\"\n\
                 command = \"${MOLT_EXTENSION_DIR}/notify\"\n",
                "cannot reach",
            ),
        ] {
            let err = format!("{:#}", Manifest::parse(text).unwrap_err());
            assert!(err.contains(says), "{text}: {err}");
        }
    }

    #[test]
    fn a_claude_code_plugin_with_commands_skills_and_a_server_is_imported() {
        let home = Home::new();
        let src = home.dir.path().join("src/review-kit");
        std::fs::create_dir_all(src.join(".claude-plugin")).unwrap();
        std::fs::write(
            src.join(".claude-plugin/plugin.json"),
            r#"{"name": "review-kit", "version": "2.0.0", "description": "Reviews."}"#,
        )
        .unwrap();
        std::fs::write(
            src.join(".mcp.json"),
            r#"{"mcpServers": {
                "lint": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/lint", "args": ["--stdio"],
                         "env": {"LINT_TOKEN": "${LINT_TOKEN}", "LEVEL": "strict"}},
                "remote": {"type": "http", "url": "https://example.com/mcp"}
            }}"#,
        )
        .unwrap();
        std::fs::create_dir_all(src.join("commands")).unwrap();
        std::fs::write(
            src.join("commands/review.md"),
            "---\ndescription: Review a change\nargument-hint: PR\nallowed-tools: Bash(git:*)\nmodel: opus\n---\n\
             Review $ARGUMENTS carefully.\n",
        )
        .unwrap();
        std::fs::create_dir_all(src.join("skills/style")).unwrap();
        std::fs::write(
            src.join("skills/style/SKILL.md"),
            "---\nname: style\ndescription: House style.\n---\nBe terse.\n",
        )
        .unwrap();
        std::fs::create_dir_all(src.join("hooks")).unwrap();
        std::fs::write(src.join("hooks/hooks.json"), "{}").unwrap();

        let mut says = Says::yes();
        assert_eq!(add(&home.places, &src, &mut says).unwrap(), Added::Installed);
        let said = says.said.join("\n");
        assert!(said.contains("MCP server remote left out: Molt bridges servers on stdio only"), "{said}");
        assert!(said.contains("command /review: allowed-tools, model left out"), "{said}");
        assert!(said.contains("its hooks are not imported yet"), "{said}");
        let asked = &says.asked[0];
        assert!(asked.contains("MCP server review-kit-lint runs ${MOLT_EXTENSION_DIR}/bin/lint --stdio"), "{asked}");
        assert!(asked.contains("gets your environment variable LINT_TOKEN"), "{asked}");
        assert!(asked.contains("is given LEVEL=strict") && asked.contains("skill style"), "{asked}");
        assert!(asked.contains("command /review in the interface"), "{asked}");

        let packs = active(&home.places).unwrap();
        let (commands, warnings) = crate::tui::commands::load_with(home.dir.path(), &command_dirs(&packs));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(commands["review"].description, "Review a change");
        assert_eq!(commands["review"].expand("#12"), "Review #12 carefully.");
        assert_eq!(packs[0].skills_dir(), Some(packs[0].dir.join("skills")));
    }
}

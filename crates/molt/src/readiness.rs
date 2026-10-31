//! Project readiness: whether a workspace's toolchains, dependencies and
//! variables are there for its commands, and whether its own build passes,
//! before any change is made. The checks run in `doctor`; this module holds
//! what they compare against, the report, the environment's fingerprint
//! and the cache keyed by it.
//!
//! Nothing here runs a command or writes into the workspace. The
//! fingerprint reads the files that decide the environment (lock files,
//! toolchain files, the profiles and the policy), the identity of the tools
//! on PATH and whether each required variable is set, never a value.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The longest build command.
const MAX_BUILD: usize = 16 * 1024;
/// At most this many required variables.
const MAX_ENV: usize = 32;

/// `[readiness]`: what the check of a workspace adds to the profiles' probes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadinessSection {
    /// The project's own build, run in the sandbox from a fork as its
    /// commands are. A failure is the project's, from before any change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Variables the project's commands need: each must be set and reach them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Check before `molt do` plans and when the interface starts. Only the
    /// user's config sets it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub before_run: bool,
}

impl ReadinessSection {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(build) = &self.build {
            ensure!(
                !build.trim().is_empty() && build.len() <= MAX_BUILD,
                "readiness.build is empty or exceeds 16 KiB: leave it out to check without a build"
            );
        }
        ensure!(self.env.len() <= MAX_ENV, "readiness.env names more than {MAX_ENV} variables");
        for name in &self.env {
            env_name(name)?;
        }
        Ok(())
    }
}

/// Whether `name` is a variable a project may require: one commands can be
/// given. Model and service credentials never are, so they are never looked at.
fn env_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.is_empty()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && !name.as_bytes()[0].is_ascii_digit(),
        "readiness.env: `{name}` is not a variable name"
    );
    ensure!(
        !["MOLT_", "ANTHROPIC_", "OPENAI_"].iter().any(|p| name.starts_with(p)) && name != "HOME",
        "readiness.env: `{name}` is a service credential or HOME, which commands are never given: leave it out"
    );
    Ok(())
}

/// One check's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    /// Not run, since something it needs is blocked.
    Skipped,
    /// It could not be told: a probe timed out or was killed.
    Unknown,
    /// The environment works, and the project's build already fails.
    BuildFails,
    /// The environment lacks something: commands cannot work until it is fixed.
    Blocked,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Self::Ready => "ok",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
            Self::BuildFails => "fails",
            Self::Blocked => "blocked",
        }
    }
}

/// What a check looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Sandbox,
    Probe,
    Toolchain,
    Dependency,
    Env,
    Build,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub kind: Kind,
    /// The profile, toolchain, variable or command.
    pub name: String,
    pub status: Status,
    /// What was found, in a sentence.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    pub ms: u64,
}

/// The readiness of one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub workspace: PathBuf,
    pub status: Status,
    /// The SHA-256 of what the environment checks depend on.
    pub fingerprint: String,
    /// The environment checks are from an earlier check of the same fingerprint.
    pub cached: bool,
    pub items: Vec<Item>,
    /// How long this check took, cached parts included at zero.
    pub ms: u64,
}

impl Report {
    pub fn new(workspace: &Path, fingerprint: String, items: Vec<Item>, cached: bool, ms: u64) -> Self {
        let status = verdict(&items);
        Self { workspace: workspace.to_path_buf(), status, fingerprint, cached, items, ms }
    }

    /// 0 ready, 1 blocked by the environment, 2 the build already fails, 3 unknown.
    pub fn exit_code(&self) -> u8 {
        match self.status {
            Status::Ready | Status::Skipped => 0,
            Status::Blocked => 1,
            Status::BuildFails => 2,
            Status::Unknown => 3,
        }
    }

    /// One line per check, the fix under each that is not ok, and a summary.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            let _ = writeln!(
                out,
                "  {:<8}{} {}: {} ({} ms)",
                item.status.word(),
                kind_word(item.kind),
                item.name,
                item.detail,
                item.ms
            );
            if let Some(fix) = item.fix.as_ref().filter(|_| item.status != Status::Ready) {
                let _ = writeln!(out, "          fix: {fix}");
            }
        }
        let _ = writeln!(out, "{}", self.summary());
        out
    }

    /// The verdict in a line, with the fingerprint and the time taken.
    pub fn summary(&self) -> String {
        let verdict = match self.status {
            Status::Ready | Status::Skipped => "ready",
            Status::Blocked => "blocked: the environment lacks what is marked blocked above",
            Status::BuildFails => "the environment is ready, and the project's build already fails before any change",
            Status::Unknown => "not known: a check could not tell",
        };
        let cached = if self.cached { ", environment checks cached" } else { "" };
        let short = self.fingerprint.get(..12).unwrap_or(&self.fingerprint);
        format!("readiness: {verdict} (fingerprint {short}, {} ms{cached})", self.ms)
    }

    /// The checks that are not ok, one line each, for a notice before a run.
    pub fn problems(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.status != Status::Ready)
            .map(|i| {
                let fix = i.fix.as_ref().map(|f| format!(" Fix: {f}")).unwrap_or_default();
                format!(
                    "{} {} {}: {}.{fix}",
                    i.status.word(),
                    kind_word(i.kind),
                    i.name,
                    i.detail.trim_end_matches('.')
                )
            })
            .collect()
    }
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Sandbox => "sandbox",
        Kind::Probe => "probe",
        Kind::Toolchain => "toolchain",
        Kind::Dependency => "dependency",
        Kind::Env => "variable",
        Kind::Build => "build",
    }
}

/// The worst status: blocked over a failing build over unknown over ready.
pub fn verdict(items: &[Item]) -> Status {
    items.iter().map(|i| i.status).filter(|s| *s != Status::Skipped).max().unwrap_or(Status::Ready)
}

/// How a version must relate to one in a declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Starts with it: `1.88` takes 1.88.0 and 1.88.1.
    Prefix,
    Ge,
    Gt,
    Le,
    Lt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    op: Op,
    version: Vec<u64>,
}

/// A toolchain version a project declares, and how to ask for the one commands get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// `rustc`, `node`, `python3`, `go`.
    pub tool: &'static str,
    /// The file and key, such as `rust-toolchain.toml` or `package.json engines.node`.
    pub source: String,
    /// What it says, as written.
    pub wants: String,
    bounds: Vec<Bound>,
    /// The command that prints the version commands get.
    pub command: &'static str,
}

impl Declared {
    fn new(tool: &'static str, command: &'static str, source: &str, wants: &str, bounds: Vec<Bound>) -> Self {
        Self { tool, source: source.to_owned(), wants: wants.trim().to_owned(), bounds, command }
    }
}

/// The toolchain versions the project in `root` declares.
pub fn declared(root: &Path) -> Vec<Declared> {
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let mut out = Vec::new();
    // Rust: the toolchain rustup picks, then the minimum the package names.
    let channel = read("rust-toolchain.toml")
        .map(|text| ("rust-toolchain.toml", text))
        .or_else(|| read("rust-toolchain").map(|text| ("rust-toolchain", text)))
        .and_then(|(file, text)| {
            let channel = match text.parse::<toml::Table>() {
                Ok(table) => table.get("toolchain")?.get("channel")?.as_str()?.to_owned(),
                Err(_) => text.lines().next()?.trim().to_owned(),
            };
            Some((file, channel))
        });
    if let Some((file, channel)) = channel.filter(|(_, c)| !c.is_empty()) {
        let bounds = numeric(&channel).map(|v| vec![Bound { op: Op::Prefix, version: v }]).unwrap_or_default();
        out.push(Declared::new("rustc", "rustc --version", file, &channel, bounds));
    }
    if let Some(cargo) = read("Cargo.toml").and_then(|t| t.parse::<toml::Table>().ok()) {
        let msrv = cargo
            .get("package")
            .and_then(|p| p.get("rust-version"))
            .and_then(|v| v.as_str())
            .or_else(|| cargo.get("workspace")?.get("package")?.get("rust-version")?.as_str());
        if let Some(v) = msrv.and_then(|m| numeric(m).map(|v| (m, v))) {
            let bounds = vec![Bound { op: Op::Ge, version: v.1 }];
            out.push(Declared::new("rustc", "rustc --version", "Cargo.toml rust-version", v.0, bounds));
        }
    }
    // Node: a version manager's file, then the package's engines.
    for file in [".nvmrc", ".node-version"] {
        let Some(line) = read(file).and_then(|t| t.lines().next().map(|l| l.trim().to_owned())) else { continue };
        if let Some(v) = numeric(line.trim_start_matches('v')) {
            out.push(Declared::new("node", "node --version", file, &line, vec![Bound { op: Op::Prefix, version: v }]));
            break;
        }
    }
    let package = read("package.json").and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    if let Some(range) = package.as_ref().and_then(|p| p["engines"]["node"].as_str()) {
        if let Some(bounds) = range_bounds(range) {
            out.push(Declared::new("node", "node --version", "package.json engines.node", range, bounds));
        }
    }
    // Python: the version pyenv and uv read, then the project's requirement.
    if let Some(line) = read(".python-version").and_then(|t| t.lines().next().map(|l| l.trim().to_owned())) {
        if let Some(v) = numeric(&line) {
            let bounds = vec![Bound { op: Op::Prefix, version: v }];
            out.push(Declared::new("python3", "python3 --version", ".python-version", &line, bounds));
        }
    }
    let pyproject = read("pyproject.toml").and_then(|t| t.parse::<toml::Table>().ok());
    if let Some(range) = pyproject.as_ref().and_then(|p| p.get("project")?.get("requires-python")?.as_str()) {
        if let Some(bounds) = range_bounds(range) {
            out.push(Declared::new("python3", "python3 --version", "pyproject.toml requires-python", range, bounds));
        }
    }
    // Go: the go line is the least version that builds the module.
    if let Some(line) =
        read("go.mod").and_then(|t| t.lines().map(str::trim).find(|l| l.starts_with("go ")).map(str::to_owned))
    {
        let wants = line.trim_start_matches("go ").trim();
        if let Some(v) = numeric(wants) {
            out.push(Declared::new("go", "go env GOVERSION", "go.mod", wants, vec![Bound { op: Op::Ge, version: v }]));
        }
    }
    out
}

/// `1.88.0` as numbers, or `None` for a name such as `stable` or `lts/*`.
fn numeric(text: &str) -> Option<Vec<u64>> {
    let text = text.trim();
    let parts: Vec<&str> = text.split('.').collect();
    let mut version = Vec::new();
    for part in &parts {
        match part.parse::<u64>() {
            Ok(n) => version.push(n),
            // `20.x` and `3.*` stop the prefix.
            Err(_) if !version.is_empty() && matches!(*part, "x" | "X" | "*") => break,
            Err(_) => return None,
        }
    }
    (!version.is_empty()).then_some(version)
}

/// The bounds of a range such as `>=18`, `^20.1`, `~3.10`, `>=3.9,<4`,
/// `20.x` or `~=3.11`, or `None` for one this does not read (`||`, tags).
fn range_bounds(range: &str) -> Option<Vec<Bound>> {
    if range.contains("||") {
        return None;
    }
    let mut bounds = Vec::new();
    for clause in range.split([',', ' ']).map(str::trim).filter(|c| !c.is_empty()) {
        let (op, rest) = [
            ("~=", Op::Ge),
            (">=", Op::Ge),
            ("<=", Op::Le),
            ("==", Op::Prefix),
            ("!=", Op::Prefix),
            (">", Op::Gt),
            ("<", Op::Lt),
            ("=", Op::Prefix),
            ("^", Op::Ge),
            ("~", Op::Ge),
        ]
        .iter()
        .find_map(|(token, op)| clause.strip_prefix(token).map(|rest| (Some((*token, *op)), rest)))
        .unwrap_or((None, clause));
        if matches!(op, Some(("!=", _))) || rest == "*" {
            continue;
        }
        let version = numeric(rest.trim_start_matches('v'))?;
        match op {
            None => bounds.push(Bound { op: Op::Prefix, version }),
            Some(("^", _)) => {
                // ^1.2 keeps the first part that is not zero.
                let keep = version.iter().position(|n| *n != 0).map_or(version.len(), |i| i + 1);
                bounds.push(Bound { op: Op::Prefix, version: version[..keep.min(version.len())].to_vec() });
                bounds.push(Bound { op: Op::Ge, version });
            }
            Some(("~", _)) => {
                bounds.push(Bound { op: Op::Prefix, version: version[..version.len().min(2)].to_vec() });
                bounds.push(Bound { op: Op::Ge, version });
            }
            Some(("~=", _)) if version.len() > 1 => {
                bounds.push(Bound { op: Op::Prefix, version: version[..version.len() - 1].to_vec() });
                bounds.push(Bound { op: Op::Ge, version });
            }
            Some((_, op)) => bounds.push(Bound { op, version }),
        }
    }
    (!bounds.is_empty()).then_some(bounds)
}

/// The first version number in a tool's output: `rustc 1.88.0 (...)`,
/// `v20.11.0`, `Python 3.12.3`, `go1.22.1`.
fn found_version(output: &str) -> Option<Vec<u64>> {
    let start = output.find(|c: char| c.is_ascii_digit())?;
    let rest = &output[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    numeric(rest[..end].trim_end_matches('.'))
}

fn cmp(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn holds(bound: &Bound, found: &[u64]) -> bool {
    use std::cmp::Ordering::*;
    match bound.op {
        Op::Prefix => found.len() >= bound.version.len() && found[..bound.version.len()] == bound.version[..],
        Op::Ge => cmp(found, &bound.version) != Less,
        Op::Gt => cmp(found, &bound.version) == Greater,
        Op::Le => cmp(found, &bound.version) != Greater,
        Op::Lt => cmp(found, &bound.version) == Less,
    }
}

/// What the output of `declared.command` says of the declaration: the
/// status, a sentence, and a fix when it is not met. `ran` is whether the
/// command exited 0.
pub fn judge(declared: &Declared, ran: bool, output: &str) -> (Status, String, Option<String>) {
    let Declared { tool, source, wants, .. } = declared;
    let said = last_line(output);
    if !ran {
        // rustup says which toolchain is missing when a toolchain file names one it lacks.
        if *tool == "rustc" && output.contains("is not installed") {
            return (
                Status::Blocked,
                format!("{source} asks for toolchain {wants}, which is not installed: {said}"),
                Some(format!("run `rustup toolchain install {wants}` on this machine, or change {source}")),
            );
        }
        return (
            Status::Blocked,
            format!("{source} asks for {wants}, and `{}` fails in the sandbox: {said}", declared.command),
            Some(format!("make {tool} visible to commands (see `molt env doctor` for its profile), or mount it with --sandbox-policy")),
        );
    }
    let Some(found) = found_version(output) else {
        return (
            Status::Unknown,
            format!("{source} asks for {wants}, and `{}` printed no version: {said}", declared.command),
            None,
        );
    };
    let shown = found.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
    if declared.bounds.iter().all(|b| holds(b, &found)) {
        return (Status::Ready, format!("{source} asks for {wants}; commands get {tool} {shown}"), None);
    }
    (
        Status::Blocked,
        format!("{source} asks for {wants}, and commands get {tool} {shown}"),
        Some(match *tool {
            "rustc" => format!(
                "install a Rust toolchain that matches {wants} (`rustup toolchain install`), or change {source}"
            ),
            "node" => format!("install Node {wants} and put it first on PATH, or change {source}"),
            "python3" => {
                format!("install Python {wants} (for a .venv, recreate it from that Python), or change {source}")
            }
            _ => format!("install Go {wants} or newer and put it first on PATH, or change {source}"),
        }),
    )
}

/// The error line of `output` when there is one, else its last line.
pub fn last_line(output: &str) -> String {
    let mut lines = output.lines().map(str::trim).filter(|l| !l.is_empty());
    let error = output.lines().map(str::trim).find(|l| l.to_ascii_lowercase().starts_with("error"));
    error.or_else(|| lines.next_back()).unwrap_or("no output").chars().take(300).collect()
}

/// Whether a required variable reaches commands: set by a profile or the
/// policy, or set here and passed. Only presence is looked at.
pub fn env_item(name: &str, set_env: &BTreeMap<String, String>, passed: &[String], present: bool) -> Item {
    let (status, detail, fix) = if set_env.contains_key(name) {
        (Status::Ready, "set for commands by a profile or the policy".to_owned(), None)
    } else if !present {
        (
            Status::Blocked,
            "is not set".to_owned(),
            Some(format!("export {name} before running molt, and pass it with --pass-env {name}")),
        )
    } else if passed.iter().any(|p| p == name) {
        (Status::Ready, "is set and passed to commands".to_owned(), None)
    } else {
        (
            Status::Blocked,
            "is set here, but commands are not given it".to_owned(),
            Some(format!("pass it with --pass-env {name} (or list it in the policy's environment)")),
        )
    };
    Item { kind: Kind::Env, name: name.to_owned(), status, detail, fix, ms: 0 }
}

/// Files whose content decides the environment, relative to the project.
const FINGERPRINT_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config.toml",
    ".cargo/config",
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "node_modules/.package-lock.json",
    ".nvmrc",
    ".node-version",
    "go.mod",
    "go.sum",
    "go.work",
    "vendor/modules.txt",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    ".python-version",
    ".venv/pyvenv.cfg",
    "venv/pyvenv.cfg",
];
/// Directories whose presence and change time decide it.
const FINGERPRINT_DIRS: &[&str] = &["node_modules", "vendor", ".venv", ".venv/bin", ".venv/lib", "venv"];

/// What the environment checks of `root` depend on.
pub struct Inputs<'a> {
    pub root: &'a Path,
    /// The profiles that apply, by name, as applied.
    pub profiles: Vec<(&'a str, &'a molt_tools::EnvProfile)>,
    /// The sandbox policy after the profiles.
    pub policy: &'a molt_tools::SandboxPolicy,
    /// The variables required, each with whether it is set here.
    pub present: Vec<(String, bool)>,
    /// PATH here.
    pub path: Option<std::ffi::OsString>,
}

/// The SHA-256, in hex, of what the environment checks of a workspace
/// depend on: Molt's version, the profiles, the policy, the lock and
/// toolchain files, the tools commands get and the Cargo cache. The build
/// is not in it: it depends on the project's code, and runs every time.
pub fn fingerprint(inputs: &Inputs) -> String {
    let mut h = Sha256::new();
    let mut put = |label: &str, bytes: &[u8]| {
        h.update((label.len() as u64).to_le_bytes());
        h.update(label.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    put("molt", env!("CARGO_PKG_VERSION").as_bytes());
    put("profiles", serde_json::to_string(&inputs.profiles).unwrap_or_default().as_bytes());
    put("policy", serde_json::to_string(inputs.policy).unwrap_or_default().as_bytes());
    put("present", serde_json::to_string(&inputs.present).unwrap_or_default().as_bytes());
    let path = inputs.path.as_deref().unwrap_or_default();
    put("PATH", path.as_encoded_bytes());
    let detect = inputs.profiles.iter().flat_map(|(_, p)| p.detect.iter().map(String::as_str));
    for rel in FINGERPRINT_FILES.iter().copied().chain(detect) {
        let digest = std::fs::read(inputs.root.join(rel)).map(|b| hex::encode(Sha256::digest(b)));
        put(rel, digest.unwrap_or_default().as_bytes());
    }
    for rel in FINGERPRINT_DIRS {
        put(rel, stamp(&inputs.root.join(rel)).as_bytes());
    }
    // The tools commands get: which file each is, and when it last changed.
    for (_, profile) in &inputs.profiles {
        for tool in &profile.tools {
            let found = std::env::split_paths(path).map(|d| d.join(tool)).find(|p| p.is_file());
            let real = found.as_ref().and_then(|p| p.canonicalize().ok());
            put(tool, format!("{found:?} {real:?} {}", real.as_deref().map(stamp).unwrap_or_default()).as_bytes());
        }
    }
    // rustup's toolchains and default, which change when one is installed.
    if let Some(rustup) = inputs.policy.set_env.get("RUSTUP_HOME").map(PathBuf::from) {
        put("rustup", listing(&rustup.join("toolchains")).as_bytes());
        put("rustup settings", stamp(&rustup.join("settings.toml")).as_bytes());
    }
    // The Cargo cache, which `cargo fetch` adds to.
    if let Some(home) = &inputs.policy.cargo_cache {
        for sub in ["registry/index", "registry/cache", "git/db"] {
            let dir = home.join(sub);
            let inner: Vec<String> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| format!("{} {}", e.file_name().to_string_lossy(), stamp(&e.path())))
                .collect();
            put(sub, format!("{} {inner:?}", stamp(&dir)).as_bytes());
        }
    }
    hex::encode(h.finalize())
}

/// Whether `path` is there, its size and when it last changed, to the nanosecond.
fn stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let at = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos());
            format!("{} {} {at:?}", if m.is_dir() { "dir" } else { "file" }, m.len())
        }
        Err(_) => "absent".to_owned(),
    }
}

/// The sorted names in a directory.
fn listing(dir: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.join("\n")
}

/// Where the last ready check of `workspace` is kept, under the data dir.
pub fn cache_path(data_dir: &Path, workspace: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
    data_dir.join("readiness").join(format!("{}.json", &key[..16]))
}

/// The environment checks of the last check of `workspace`, when it was
/// ready and had this fingerprint.
pub fn cached(path: &Path, workspace: &Path, fingerprint: &str) -> Option<Vec<Item>> {
    let report: Report = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (report.workspace == workspace && report.fingerprint == fingerprint && report.status == Status::Ready)
        .then(|| report.items.into_iter().filter(|i| i.kind != Kind::Build).collect())
}

/// Keep `report` when its environment checks all passed: a blocked or
/// unknown environment is checked afresh each time, so a fix shows at once.
pub fn store(path: &Path, report: &Report) -> anyhow::Result<()> {
    let env: Vec<Item> = report.items.iter().filter(|i| i.kind != Kind::Build).cloned().collect();
    if verdict(&env) != Status::Ready {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let kept = Report::new(&report.workspace, report.fingerprint.clone(), env, false, report.ms);
    let dir = path.parent().context("the readiness cache has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::fs::write(tmp.path(), serde_json::to_vec_pretty(&kept)?)?;
    tmp.persist(path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(root: &Path, tool: &str) -> Vec<Declared> {
        declared(root).into_iter().filter(|d| d.tool == tool).collect()
    }

    #[test]
    fn a_wrong_or_missing_rust_toolchain_is_named_with_what_to_install() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::write(ws.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.88.0\"\n").unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"a\"\nrust-version = \"1.80\"\n").unwrap();
        let rust = decl(ws, "rustc");
        assert_eq!(rust.len(), 2);
        assert_eq!((rust[0].source.as_str(), rust[0].wants.as_str()), ("rust-toolchain.toml", "1.88.0"));
        let (status, detail, fix) = judge(&rust[0], true, "rustc 1.88.0 (6b00bc388 2025-06-23)\n");
        assert_eq!(status, Status::Ready, "{detail}");
        let (status, detail, fix2) = judge(&rust[0], true, "rustc 1.79.0 (129f3b996 2024-06-10)\n");
        assert_eq!(status, Status::Blocked);
        assert!(detail.contains("asks for 1.88.0, and commands get rustc 1.79.0"), "{detail}");
        assert!(fix2.unwrap().contains("rustup toolchain install"));
        assert!(fix.is_none());
        let missing =
            "error: toolchain '1.88.0-x86_64-unknown-linux-gnu' is not installed\nhelp: run `rustup toolchain install`";
        let (status, detail, fix) = judge(&rust[0], false, missing);
        assert_eq!(status, Status::Blocked);
        assert!(detail.contains("which is not installed") && detail.contains("1.88.0-x86_64"), "{detail}");
        assert_eq!(
            fix.unwrap(),
            "run `rustup toolchain install 1.88.0` on this machine, or change rust-toolchain.toml"
        );
        assert_eq!(judge(&rust[1], true, "rustc 1.79.0").0, Status::Blocked, "below rust-version");
        assert_eq!(judge(&rust[1], true, "rustc 1.80.1").0, Status::Ready);
        // A named channel is only checked by whether it runs.
        std::fs::write(ws.join("rust-toolchain"), "stable\n").unwrap();
        std::fs::remove_file(ws.join("rust-toolchain.toml")).unwrap();
        let rust = decl(ws, "rustc");
        assert_eq!(rust[0].wants, "stable");
        assert_eq!(judge(&rust[0], true, "rustc 1.91.0").0, Status::Ready);
    }

    #[test]
    fn node_python_and_go_declarations_are_compared_with_the_versions_commands_get() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::write(ws.join(".nvmrc"), "v20\n").unwrap();
        std::fs::write(ws.join("package.json"), r#"{"engines": {"node": ">=18.17 <23"}}"#).unwrap();
        std::fs::write(ws.join(".python-version"), "3.12\n").unwrap();
        std::fs::write(ws.join("pyproject.toml"), "[project]\nrequires-python = \">=3.10,<4\"\n").unwrap();
        std::fs::write(ws.join("go.mod"), "module x\n\ngo 1.22.1\n").unwrap();
        let node = decl(ws, "node");
        assert_eq!(judge(&node[0], true, "v20.11.0").0, Status::Ready);
        assert_eq!(judge(&node[0], true, "v18.19.0").0, Status::Blocked, ".nvmrc wants 20");
        assert_eq!(judge(&node[1], true, "v22.1.0").0, Status::Ready);
        assert_eq!(judge(&node[1], true, "v23.0.0").0, Status::Blocked);
        assert_eq!(judge(&node[1], true, "v18.16.0").0, Status::Blocked);
        let python = decl(ws, "python3");
        assert_eq!(judge(&python[0], true, "Python 3.12.3").0, Status::Ready);
        let (status, detail, _) = judge(&python[0], true, "Python 3.11.9");
        assert!(
            status == Status::Blocked && detail.contains("asks for 3.12, and commands get python3 3.11.9"),
            "{detail}"
        );
        assert_eq!(judge(&python[1], true, "Python 3.9.2").0, Status::Blocked);
        let (status, detail, fix) = judge(&python[1], false, "sh: 1: python3: not found");
        assert!(status == Status::Blocked && detail.contains("python3: not found") && fix.is_some(), "{detail}");
        let go = decl(ws, "go");
        assert_eq!(judge(&go[0], true, "go1.23.0").0, Status::Ready);
        assert_eq!(judge(&go[0], true, "go1.21.5").0, Status::Blocked);
        assert_eq!(judge(&go[0], true, "weird").0, Status::Unknown);
        // Ranges this does not read are left out, not guessed.
        assert_eq!(range_bounds("^16 || ^18"), None);
        assert_eq!(range_bounds("lts/*"), None);
        assert!(range_bounds("~=3.11").is_some() && range_bounds("20.x").is_some());
        let caret = range_bounds("^20.1").unwrap();
        assert!(caret.iter().all(|b| holds(b, &[20, 5])) && !caret.iter().all(|b| holds(b, &[21, 0])));
    }

    #[test]
    fn a_required_variable_must_be_set_and_reach_commands_and_credentials_cannot_be_required() {
        let none = BTreeMap::new();
        assert_eq!(env_item("DATABASE_URL", &none, &[], false).status, Status::Blocked);
        let unpassed = env_item("DATABASE_URL", &none, &[], true);
        assert!(unpassed.status == Status::Blocked && unpassed.fix.unwrap().contains("--pass-env DATABASE_URL"));
        assert_eq!(env_item("DATABASE_URL", &none, &["DATABASE_URL".into()], true).status, Status::Ready);
        let set: BTreeMap<String, String> = [("GOPROXY".to_owned(), "off".to_owned())].into();
        assert_eq!(env_item("GOPROXY", &set, &[], false).status, Status::Ready);
        for bad in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "MOLT_SERVICE_SECRET", "HOME", "1X", "A-B"] {
            assert!(ReadinessSection { env: vec![bad.into()], ..Default::default() }.validate().is_err(), "{bad}");
        }
        assert!(ReadinessSection { build: Some(" ".into()), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn the_verdict_puts_a_blocked_environment_before_a_failing_build_and_only_ready_checks_are_cached() {
        let item = |kind, status| Item { kind, name: "x".into(), status, detail: "d".into(), fix: None, ms: 1 };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let path = cache_path(tmp.path(), &ws);
        let ready = Report::new(
            &ws,
            "f1".into(),
            vec![item(Kind::Probe, Status::Ready), item(Kind::Build, Status::BuildFails)],
            false,
            5,
        );
        assert_eq!((ready.status, ready.exit_code()), (Status::BuildFails, 2));
        store(&path, &ready).unwrap();
        assert_eq!(cached(&path, &ws, "f1").unwrap(), [item(Kind::Probe, Status::Ready)], "the build is never kept");
        assert!(cached(&path, &ws, "f2").is_none(), "another fingerprint");
        assert!(cached(&path, &tmp.path().join("other"), "f1").is_none());
        let blocked = Report::new(
            &ws,
            "f1".into(),
            vec![item(Kind::Dependency, Status::Blocked), item(Kind::Build, Status::Skipped)],
            false,
            5,
        );
        assert_eq!((blocked.status, blocked.exit_code()), (Status::Blocked, 1));
        store(&path, &blocked).unwrap();
        assert!(cached(&path, &ws, "f1").is_none(), "a blocked environment is not kept, and drops the last");
        let unknown = Report::new(&ws, "f".into(), vec![item(Kind::Probe, Status::Unknown)], false, 5);
        assert_eq!(unknown.exit_code(), 3);
    }

    #[test]
    fn the_fingerprint_changes_with_a_lock_file_a_toolchain_file_a_fixed_dependency_and_a_variable() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::write(ws.join("package.json"), r#"{"dependencies": {"left-pad": "1"}}"#).unwrap();
        let policy = molt_tools::SandboxPolicy::default();
        let profile = molt_tools::EnvProfile { detect: vec!["package.json".into()], ..Default::default() };
        let print = |present: bool| {
            fingerprint(&Inputs {
                root: ws,
                profiles: vec![("node", &profile)],
                policy: &policy,
                present: vec![("TOKEN".into(), present)],
                path: Some("/usr/bin".into()),
            })
        };
        let first = print(false);
        assert_eq!(first, print(false), "the same environment, the same fingerprint");
        std::fs::create_dir(ws.join("node_modules")).unwrap();
        let installed = print(false);
        assert_ne!(first, installed, "npm install");
        std::fs::write(ws.join("package-lock.json"), "{}").unwrap();
        let locked = print(false);
        assert_ne!(installed, locked, "a lock file");
        std::fs::write(ws.join(".nvmrc"), "22\n").unwrap();
        let pinned = print(false);
        assert_ne!(locked, pinned, "a toolchain file");
        assert_ne!(pinned, print(true), "a variable set");
        std::fs::write(ws.join("src.js"), "changed").unwrap();
        assert_eq!(pinned, print(false), "the project's code is not in it");
    }
}

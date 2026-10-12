//! Project templates are data: they set one request, never capabilities or modes.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, ensure, Context};

pub const BUILTINS: &[&str] = &[
    "help",
    "remember",
    "theme",
    "memory",
    "memory-used",
    "check",
    "protect",
    "pause",
    "resume",
    "new",
    "attempts",
    "stream",
    "model",
    "memory-model",
    "effort",
    "budget",
    "last",
    "lanes",
    "clear",
    "quit",
    "exit",
];
const MAX_BYTES: u64 = 64 * 1024;
const MAX_COMMANDS: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct Command {
    pub description: String,
    pub hint: String,
    pub check: Option<String>,
    pub attempts: Option<u32>,
    pub budget: Option<f64>,
    pub template: String,
}

pub fn load(workspace: &Path) -> (BTreeMap<String, Command>, Vec<String>) {
    let mut commands = BTreeMap::new();
    let mut warnings = Vec::new();
    let root = match workspace.canonicalize() {
        Ok(root) => root,
        Err(_) => return (commands, warnings),
    };
    let dir = root.join(".molt/commands");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (commands, warnings),
        Err(e) => return (commands, vec![format!("project commands: {e}")]),
    };
    // Bound discovery too, including directories and invalid entries.
    for (count, entry) in entries.enumerate() {
        if count >= MAX_COMMANDS {
            warnings.push(format!("project commands: only the first {MAX_COMMANDS} entries are loaded"));
            break;
        }
        let result = (|| -> anyhow::Result<Option<(String, Command)>> {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "md") {
                return Ok(None);
            }
            let name = path.file_stem().and_then(|s| s.to_str()).context("command name is not UTF-8")?.to_owned();
            ensure!(
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid command name {name:?}"
            );
            ensure!(
                !BUILTINS.contains(&name.as_str()),
                "/{name} conflicts with a built-in command; keeping the built-in"
            );
            let path = path.canonicalize()?;
            ensure!(path.starts_with(&root) && path.is_file(), "/{name} must be a file inside the workspace");
            let mut contents = String::new();
            std::fs::File::open(&path)?.take(MAX_BYTES + 1).read_to_string(&mut contents)?;
            ensure!(contents.len() as u64 <= MAX_BYTES, "/{name} exceeds {MAX_BYTES} bytes");
            Ok(Some((name, parse(&contents)?)))
        })();
        match result {
            Ok(Some((name, command))) => {
                commands.insert(name, command);
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!("project command skipped: {e:#}")),
        }
    }
    (commands, warnings)
}

/// A deliberately small frontmatter format: flat, single-line YAML scalars.
/// Reject unknown fields instead of silently accepting permissions or modes.
fn parse(text: &str) -> anyhow::Result<Command> {
    let text = text.replace("\r\n", "\n");
    let mut command = Command::default();
    let body = if let Some(rest) = text.strip_prefix("---\n") {
        let (header, body) = rest.split_once("\n---\n").context("frontmatter needs a closing --- line")?;
        let mut seen = std::collections::HashSet::new();
        for line in header.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')) {
            let (key, value) = line.split_once(':').context("frontmatter expects key: value")?;
            let key = key.trim();
            ensure!(seen.insert(key), "duplicate field {key}");
            let value = scalar(value.trim())?;
            match key {
                "description" => command.description = value,
                "argument-hint" => command.hint = value,
                "check" => {
                    ensure!(!value.trim().is_empty(), "check is empty");
                    command.check = Some(value);
                }
                "attempts" => {
                    let n = value.parse()?;
                    ensure!((1..=8).contains(&n), "attempts must be 1 to 8");
                    command.attempts = Some(n);
                }
                "budget" => {
                    let n: f64 = value.parse()?;
                    ensure!(n.is_finite() && n > 0.0, "budget must be finite and positive");
                    command.budget = Some(n);
                }
                _ => bail!("unknown command field {key}"),
            }
        }
        body
    } else {
        &text
    };
    ensure!(!body.trim().is_empty(), "task template is empty");
    command.template = body.trim().to_owned();
    Ok(command)
}

fn scalar(value: &str) -> anyhow::Result<String> {
    if value.starts_with('"') {
        return Ok(serde_json::from_str(value)?);
    }
    if let Some(value) = value.strip_prefix('\'') {
        return Ok(value.strip_suffix('\'').context("unclosed quoted scalar")?.replace("''", "'"));
    }
    ensure!(!value.starts_with(['[', '{', '|', '>']) && !value.is_empty(), "expected a single-line scalar");
    Ok(value.to_owned())
}

impl Command {
    pub fn expand(&self, arguments: &str) -> String {
        let positional: Vec<&str> = arguments.split_whitespace().collect();
        let mut out = String::new();
        let mut rest = self.template.as_str();
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            rest = &rest[at + 1..];
            if let Some(tail) = rest.strip_prefix("ARGUMENTS") {
                out.push_str(arguments);
                rest = tail;
            } else {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    out.push('$');
                    continue;
                }
                let n = rest[..digits].parse::<usize>().unwrap_or(0);
                out.push_str(n.checked_sub(1).and_then(|n| positional.get(n)).copied().unwrap_or(""));
                rest = &rest[digits..];
            }
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expansion_is_one_pass_and_positional_numbers_are_whole() {
        let c = parse("---\ndescription: Fix it\nargument-hint: ISSUE\ncheck: 'cargo test'\nattempts: 3\nbudget: 2.5\n---\nFix issue $ARGUMENTS ($1 / $10 / $2)").unwrap();
        assert_eq!(c.expand("12 $1"), "Fix issue 12 $1 (12 /  / $1)");
        assert_eq!(c.check.as_deref(), Some("cargo test"));
        for text in [
            "---\nbudget: NaN\n---\ntask",
            "---\nattempts: 9\n---\ntask",
            "---\nmode: auto\n---\ntask",
            "---\ncheck: a\ncheck: b\n---\ntask",
            "---\ndescription: broken",
            "",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
    #[test]
    fn bad_files_conflicts_and_escaping_symlinks_do_not_block_other_commands() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".molt/commands");
        std::fs::create_dir_all(&dir).unwrap();
        for builtin in ["help", "remember", "theme"] {
            std::fs::write(dir.join(format!("{builtin}.md")), "wrong").unwrap();
        }
        std::fs::write(dir.join("bad.md"), "---\nbudget: -1\n---\ntask").unwrap();
        std::fs::write(dir.join("fix.md"), "Fix $ARGUMENTS").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("escape.md")).unwrap();
        let (commands, warnings) = load(root.path());
        assert_eq!(commands.keys().collect::<Vec<_>>(), ["fix"]);
        assert_eq!(warnings.len(), 5);
    }
}

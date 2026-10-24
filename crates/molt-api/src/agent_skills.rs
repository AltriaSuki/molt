//! Agent Skills: the `SKILL.md` format many agents share (see
//! <https://agentskills.io/specification>), and the record of which skills a
//! run was offered.
//!
//! A skill is a directory holding a `SKILL.md`: YAML front matter with a
//! `name` and a `description`, then Markdown instructions. Other files of
//! the directory (`scripts/`, `references/`, `assets/`) are what the
//! instructions refer to. A run's first message names each skill and says
//! what it is for; the body and the files are read only when the model asks
//! (progressive disclosure).
//!
//! The front matter is read as the subset of YAML the format needs: `key:
//! value` lines whose value is plain, single- or double-quoted, a flow list
//! (`[a, b]`) or a block scalar (`|`, `>`, with `-` or `+`), a plain value
//! continued on indented lines, and one level of `key: value` or `- item`
//! lines under a key with no value (`metadata`, a list of tools). Anything
//! else is refused, so a project's file cannot lean on a YAML parser's
//! corners. Keys the format does not define are ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The file that makes a directory a skill.
pub const FILE: &str = "SKILL.md";
/// The longest name: lowercase letters, digits and hyphens.
pub const MAX_NAME: usize = 64;
/// The longest description, in characters.
pub const MAX_DESCRIPTION: usize = 1024;
/// The longest `compatibility`, in characters.
pub const MAX_COMPATIBILITY: usize = 500;
/// The largest `SKILL.md` read at all.
pub const MAX_FILE: usize = 128 * 1024;

/// A parsed `SKILL.md`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillMd {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    /// String keys to string values, in key order.
    pub metadata: BTreeMap<String, String>,
    /// Space-separated tool names the skill expects (experimental in the format).
    pub allowed_tools: Option<String>,
    /// The Markdown after the front matter.
    pub body: String,
}

/// Whether `name` is a skill name the format allows: 1 to 64 lowercase
/// letters, digits and hyphens, not starting or ending with a hyphen and
/// with no two hyphens in a row.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(format!("a skill's name has 1 to {MAX_NAME} characters, not {}", name.chars().count()));
    }
    if let Some(c) = name.chars().find(|c| !(c.is_alphanumeric() && !c.is_uppercase()) && *c != '-') {
        return Err(format!("a skill's name has only lowercase letters, digits and hyphens: {name:?} has {c:?}"));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err(format!(
            "a skill's name neither starts nor ends with a hyphen nor has two in a row: {name:?} does"
        ));
    }
    Ok(())
}

impl SkillMd {
    /// What is wrong with it as the format defines a skill, if anything.
    pub fn validate(&self) -> Result<(), String> {
        valid_name(&self.name)?;
        let chars = self.description.chars().count();
        if self.description.trim().is_empty() || chars > MAX_DESCRIPTION {
            return Err(format!(
                "skill {}: the description says what the skill does and when to use it, in 1 to \
                 {MAX_DESCRIPTION} characters, not {chars}",
                self.name
            ));
        }
        if let Some(c) = &self.compatibility {
            let chars = c.chars().count();
            if c.trim().is_empty() || chars > MAX_COMPATIBILITY {
                return Err(format!(
                    "skill {}: compatibility has 1 to {MAX_COMPATIBILITY} characters, not {chars}",
                    self.name
                ));
            }
        }
        if let Some(key) = self.metadata.keys().find(|k| !plain_key(k)) {
            return Err(format!(
                "skill {}: metadata key {key:?} is not a plain key (letters, digits, '-', '_', '.')",
                self.name
            ));
        }
        Ok(())
    }

    /// The file's text: the front matter, then the body.
    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", self.name));
        out.push_str(&format!("description: {}\n", quoted(&self.description)));
        if let Some(license) = &self.license {
            out.push_str(&format!("license: {}\n", quoted(license)));
        }
        if let Some(compatibility) = &self.compatibility {
            out.push_str(&format!("compatibility: {}\n", quoted(compatibility)));
        }
        if !self.metadata.is_empty() {
            out.push_str("metadata:\n");
            for (key, value) in &self.metadata {
                out.push_str(&format!("  {key}: {}\n", quoted(value)));
            }
        }
        if let Some(tools) = &self.allowed_tools {
            out.push_str(&format!("allowed-tools: {}\n", quoted(tools)));
        }
        out.push_str("---\n");
        if !self.body.is_empty() {
            out.push('\n');
            out.push_str(self.body.trim_end());
            out.push('\n');
        }
        out
    }
}

/// A YAML double-quoted scalar: a JSON string is one.
fn quoted(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialize")
}

fn plain_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A front matter value.
#[derive(Debug, PartialEq)]
enum Value {
    Text(String),
    List(Vec<String>),
    Map(Vec<(String, String)>),
}

/// Read a `SKILL.md`: its front matter's fields and its body. It is not
/// [`SkillMd::validate`]d.
pub fn parse(text: &str) -> Result<SkillMd, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Err("SKILL.md starts with a front matter line `---`".into());
    }
    let mut front = Vec::new();
    let mut closed = false;
    let mut used = first.len();
    for line in lines.by_ref() {
        used += line.len();
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare == "---" || bare == "..." {
            closed = true;
            break;
        }
        front.push(bare);
    }
    if !closed {
        return Err("SKILL.md's front matter has no closing `---` line".into());
    }
    let body = text[used..].trim_start_matches(['\n', '\r']).to_owned();
    let fields = front_matter(&front)?;
    let mut skill = SkillMd { body, ..SkillMd::default() };
    let text = |key: &str, value: Value| match value {
        Value::Text(s) => Ok(s),
        _ => Err(format!("{key} is a single value in SKILL.md's front matter")),
    };
    for (key, value) in fields {
        if !matches!(key.as_str(), "name" | "description" | "license" | "compatibility" | "metadata" | "allowed-tools")
        {
            // Fields the format does not define: other agents add their own.
            continue;
        }
        let value = value?;
        match key.as_str() {
            "name" => skill.name = text(&key, value)?,
            "description" => skill.description = text(&key, value)?,
            "license" => skill.license = Some(text(&key, value)?),
            "compatibility" => skill.compatibility = Some(text(&key, value)?),
            "metadata" => match value {
                Value::Map(pairs) => skill.metadata = pairs.into_iter().collect(),
                Value::Text(s) if s.is_empty() => {}
                _ => return Err("metadata is a map of keys to values in SKILL.md's front matter".into()),
            },
            "allowed-tools" => {
                skill.allowed_tools = Some(match value {
                    Value::Text(s) => s,
                    Value::List(items) => items.join(" "),
                    Value::Map(_) => return Err("allowed-tools is a list of tool names".into()),
                })
            }
            _ => unreachable!("only the format's keys get here"),
        }
    }
    if skill.name.is_empty() {
        return Err("SKILL.md's front matter has no name".into());
    }
    if skill.description.is_empty() {
        return Err("SKILL.md's front matter has no description".into());
    }
    Ok(skill)
}

fn indented(line: &str) -> bool {
    line.starts_with([' ', '\t'])
}

fn blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// A top-level key and its value, or why the value does not read.
type Field = (String, Result<Value, String>);

/// The front matter's top-level keys and values, in order.
/// A value that does not read is an error only for a key the format defines.
fn front_matter(lines: &[&str]) -> Result<Vec<Field>, String> {
    let mut fields: Vec<Field> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let at = i + 2; // 1-based, after the opening `---`
        i += 1;
        if blank_or_comment(line) {
            continue;
        }
        if indented(line) {
            return Err(format!("SKILL.md line {at}: indented where a key was expected"));
        }
        let (key, rest) = split_key(line).ok_or_else(|| format!("SKILL.md line {at}: expected `key: value`"))?;
        if fields.iter().any(|(k, _)| *k == key) {
            return Err(format!("SKILL.md line {at}: {key} is given twice"));
        }
        // The indented lines after it, for a block, a nested map or list, or a continued value.
        let start = i;
        while i < lines.len() && (indented(lines[i]) || lines[i].trim().is_empty()) {
            i += 1;
        }
        let more = &lines[start..i];
        let value = (|| {
            Ok(if rest.is_empty() {
                nested(more, start + 2)?
            } else if let Some(indicator) = rest.strip_prefix(['|', '>']) {
                let folded = rest.starts_with('>');
                Value::Text(block(more, folded, indicator.trim(), at)?)
            } else {
                let first = scalar(rest, at)?;
                let tail: Vec<&str> = more.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
                match first {
                    Value::Text(s) if !tail.is_empty() => {
                        if rest.starts_with(['"', '\'', '[']) {
                            return Err(format!("SKILL.md line {at}: a quoted value or list ends on its own line"));
                        }
                        Value::Text(std::iter::once(s.as_str()).chain(tail).collect::<Vec<_>>().join(" "))
                    }
                    _ if !tail.is_empty() => return Err(format!("SKILL.md line {at}: unexpected indented lines")),
                    value => value,
                }
            })
        })();
        fields.push((key, value));
    }
    Ok(fields)
}

/// `key: rest` of a line, with the key checked and `rest` trimmed.
fn split_key(line: &str) -> Option<(String, &str)> {
    let (key, rest) = match line.split_once(": ") {
        Some((key, rest)) => (key, rest),
        None => (line.trim_end().strip_suffix(':')?, ""),
    };
    let key = key.trim_end();
    let ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then(|| (key.to_owned(), rest.trim()))
}

/// The lines under a key with no value: a map, a list, or nothing.
fn nested(lines: &[&str], first: usize) -> Result<Value, String> {
    let items: Vec<(usize, &str)> =
        lines.iter().enumerate().filter(|(_, l)| !blank_or_comment(l)).map(|(n, l)| (n + first, l.trim())).collect();
    if items.is_empty() {
        return Ok(Value::Text(String::new()));
    }
    if items.iter().all(|(_, l)| l.starts_with("- ") || *l == "-") {
        let list = items
            .iter()
            .map(|(at, l)| match scalar(l.trim_start_matches('-').trim(), *at)? {
                Value::Text(s) => Ok(s),
                _ => Err(format!("SKILL.md line {at}: a list item is a single value")),
            })
            .collect::<Result<_, String>>()?;
        return Ok(Value::List(list));
    }
    let mut map: Vec<(String, String)> = Vec::new();
    for (at, line) in items {
        let (key, rest) = split_key(line).ok_or_else(|| format!("SKILL.md line {at}: expected `key: value`"))?;
        if map.iter().any(|(k, _)| *k == key) {
            return Err(format!("SKILL.md line {at}: {key} is given twice"));
        }
        if rest.is_empty() || rest.starts_with(['|', '>']) {
            return Err(format!("SKILL.md line {at}: values are nested one level deep at most"));
        }
        match scalar(rest, at)? {
            Value::Text(s) => map.push((key, s)),
            _ => return Err(format!("SKILL.md line {at}: values are nested one level deep at most")),
        }
    }
    Ok(Value::Map(map))
}

/// A `|` (literal) or `>` (folded) block's text.
fn block(lines: &[&str], folded: bool, indicator: &str, at: usize) -> Result<String, String> {
    let chomp = match indicator {
        "" => None,
        "-" => Some(false),
        "+" => Some(true),
        _ => return Err(format!("SKILL.md line {at}: a block is `|` or `>`, with `-` or `+` at most")),
    };
    let indent = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .unwrap_or(0);
    let lines: Vec<&str> = lines.iter().map(|l| if l.len() >= indent { &l[indent..] } else { "" }).collect();
    let mut text = String::new();
    if folded {
        let mut previous_blank = true;
        for line in &lines {
            if line.trim().is_empty() {
                text.push('\n');
                previous_blank = true;
            } else {
                if !previous_blank {
                    text.push(' ');
                }
                text.push_str(line);
                previous_blank = false;
            }
        }
    } else {
        text = lines.join("\n");
    }
    let content = text.trim_end_matches('\n');
    Ok(match chomp {
        Some(false) => content.to_owned(),
        Some(true) => format!("{text}\n"),
        None if content.is_empty() => String::new(),
        None => format!("{content}\n"),
    })
}

/// One value on its line: quoted, a flow list, or plain.
fn scalar(rest: &str, at: usize) -> Result<Value, String> {
    let bad = |what: &str| format!("SKILL.md line {at}: {what}");
    if let Some(inner) = rest.strip_prefix('"') {
        let (value, after) = double_quoted(inner).ok_or_else(|| bad("a double-quoted value has no closing quote"))?;
        trailing(after).map_err(|_| bad("text after a quoted value"))?;
        return Ok(Value::Text(value));
    }
    if let Some(inner) = rest.strip_prefix('\'') {
        let (value, after) = single_quoted(inner).ok_or_else(|| bad("a single-quoted value has no closing quote"))?;
        trailing(after).map_err(|_| bad("text after a quoted value"))?;
        return Ok(Value::Text(value));
    }
    if let Some(inner) = rest.strip_prefix('[') {
        let inner = strip_comment(inner).trim_end();
        let inner = inner.strip_suffix(']').ok_or_else(|| bad("a list `[...]` ends on its own line"))?;
        let items = inner
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| match scalar(s, at)? {
                Value::Text(s) => Ok(s),
                _ => Err(bad("lists hold single values")),
            })
            .collect::<Result<_, String>>()?;
        return Ok(Value::List(items));
    }
    if rest.starts_with(['{', '&', '*', '!', '%', '@', '`']) {
        return Err(bad("this form of YAML value is not read in a skill's front matter: quote the value"));
    }
    Ok(Value::Text(strip_comment(rest).trim().to_owned()))
}

/// A plain value without its ` # comment`.
fn strip_comment(s: &str) -> &str {
    match s.find(" #").or_else(|| s.find("\t#")) {
        Some(at) => &s[..at],
        None => s,
    }
}

fn trailing(after: &str) -> Result<(), ()> {
    let after = after.trim();
    if after.is_empty() || after.starts_with('#') {
        Ok(())
    } else {
        Err(())
    }
}

/// The text of a double-quoted value after its opening quote, and what follows the closing one.
fn double_quoted(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[i + 1..])),
            '\\' => {
                let (_, e) = chars.next()?;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    '"' | '\\' | '/' | ' ' => out.push(e),
                    'x' | 'u' | 'U' => {
                        let len = match e {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let hex: String = (0..len).filter_map(|_| chars.next().map(|(_, c)| c)).collect();
                        let code = u32::from_str_radix(&hex, 16).ok().filter(|_| hex.len() == len)?;
                        out.push(char::from_u32(code)?);
                    }
                    _ => return None,
                }
            }
            c => out.push(c),
        }
    }
    None
}

/// The text of a single-quoted value after its opening quote, and what follows the closing one.
fn single_quoted(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let at = rest.find('\'')?;
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        match rest.strip_prefix('\'') {
            Some(more) => {
                out.push('\'');
                rest = more;
            }
            None => return Some((out, rest)),
        }
    }
}

/// Where an offered skill comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// The user's own skills directory (`~/.config/molt/skills`): trusted.
    User,
    /// The project's `.molt/skills`: data, written by whoever wrote the project.
    Project,
    /// A skill memory learned and a person promoted after independent evaluation.
    Learned,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Learned => "learned",
        }
    }
}

/// One skill a run's first message offered, as the recall snapshot keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedSkill {
    pub name: String,
    pub source: SkillSource,
    /// Its `SKILL.md`: absolute for a user skill, relative to the workspace
    /// for a project one; a learned skill's id.
    pub location: String,
    /// SHA-256 of the `SKILL.md` as read, in hex; empty for a learned skill.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// A learned skill's revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// The most skills one recall snapshot records.
pub const MAX_OFFERED: usize = 64;

impl OfferedSkill {
    /// What is wrong with it as a record, if anything.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() || self.name.len() > 256 || self.location.len() > 4096 || self.sha256.len() > 64 {
            return Err(
                "an offered skill has a name of 1 to 256 bytes, a location of at most 4 KiB and a SHA-256".into()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_format() {
        for ok in ["pdf", "pdf-processing", "a1-b2", "données"] {
            assert_eq!(valid_name(ok), Ok(()), "{ok}");
        }
        for bad in ["", "PDF", "-pdf", "pdf-", "pdf--x", "pdf_x", "pdf x", "a/b", &"a".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_front_matter_reads_the_forms_skills_use() {
        let text = "---\nname: pdf-tools\ndescription: >-\n  Extract text from PDF files.\n  Use when the task \
                    names a PDF.\nlicense: 'Apache-2.0'\nmetadata:\n  author: example-org\n  version: \"1.0\"\n\
                    allowed-tools: [Bash, Read]\nx-other: { ignored: nope }\n---\n\n# PDF tools\n\nRun scripts/x.py.\n";
        // A key the format does not define is passed over, whatever its value.
        let skill = parse(text).unwrap();
        assert_eq!(parse(&text.replace("x-other: { ignored: nope }\n", "# a comment\n")).unwrap(), skill);
        assert_eq!(skill.name, "pdf-tools");
        assert_eq!(skill.description, "Extract text from PDF files. Use when the task names a PDF.");
        assert_eq!(skill.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(skill.metadata["author"], "example-org");
        assert_eq!(skill.metadata["version"], "1.0");
        assert_eq!(skill.allowed_tools.as_deref(), Some("Bash Read"));
        assert_eq!(skill.body, "# PDF tools\n\nRun scripts/x.py.\n");
        assert_eq!(skill.validate(), Ok(()));

        let plain = parse("---\nname: a\ndescription: Does a thing: well # really\n  and more\n---\nbody").unwrap();
        assert_eq!(plain.description, "Does a thing: well and more");
        let literal = parse("---\nname: a\ndescription: |\n  one\n  two\n---\n").unwrap();
        assert_eq!(literal.description, "one\ntwo\n");
        let escaped = parse("---\r\nname: a\r\ndescription: \"say \\\"hi\\\"\\u00e9\"\r\n---\r\nb").unwrap();
        assert_eq!(escaped.description, "say \"hi\"é");
    }

    #[test]
    fn a_malformed_front_matter_is_refused_with_where() {
        for (text, says) in [
            ("name: a\n", "starts with"),
            ("---\nname: a\ndescription: b\n", "closing"),
            ("---\nname: a\n---\n", "no description"),
            ("---\ndescription: b\n---\n", "no name"),
            ("---\nname: a\nname: b\ndescription: c\n---\n", "twice"),
            ("---\nname: a\ndescription: \"open\n---\n", "closing quote"),
            ("---\nname: a\ndescription: b\nmetadata:\n  deep:\n    x: y\n---\n", "line 5"),
            ("---\n  name: a\n---\n", "indented"),
            ("---\nname: *alias\ndescription: b\n---\n", "quote the value"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(says), "{text:?}: {e}");
        }
    }

    #[test]
    fn a_rendered_skill_reads_back_the_same() {
        let skill = SkillMd {
            name: "fix-parser".into(),
            description: "Fix the parser: \"quotes\", # hashes, and\nnew lines.".into(),
            license: None,
            compatibility: Some("Molt".into()),
            metadata: [("molt-skill-id".to_owned(), "skill_1".to_owned()), ("molt.rev".into(), "2".into())].into(),
            allowed_tools: None,
            body: "# Fix parser\n\n1. Read it.\n".into(),
        };
        assert_eq!(skill.validate(), Ok(()));
        assert_eq!(parse(&skill.render()).unwrap(), skill);
        let long = SkillMd { description: "x".repeat(MAX_DESCRIPTION + 1), ..skill.clone() };
        assert!(long.validate().is_err());
        let bad_key = SkillMd { metadata: [("a b".to_owned(), "c".to_owned())].into(), ..skill };
        assert!(bad_key.validate().is_err());
    }
}

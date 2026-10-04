//! Source languages and the symbols parsed out of them.
//!
//! Symbols come from each grammar's tags query, run by `tree-sitter-tags`:
//! a `definition.*` capture is a definition and a `reference.*` capture a
//! reference, its kind being the part after the dot. Some grammars' queries
//! leave out what the ranking needs, so a few supplements are appended (see
//! [`Lang::query`]); a supplement only ever adds patterns after the grammar's
//! own, and `tree-sitter-tags` keeps the earliest pattern's tag for a node,
//! so a definition is never demoted to a reference.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::Language;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

/// Most definitions kept from one file, so a generated or pathological file
/// cannot swamp the model.
const MAX_FILE_DEFS: usize = 5_000;
/// Most references kept from one file.
const MAX_FILE_REFS: usize = 20_000;
/// Longest name kept; anything longer is not an identifier.
const MAX_NAME_BYTES: usize = 200;
/// Longest signature kept, in characters.
const MAX_SIGNATURE_CHARS: usize = 160;

/// A language Molt can parse symbols from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
}

/// Trait method signatures and constants; calls through a path
/// (`Db::open(..)`), with a turbofish, and macros by path; the type named by a
/// path (`Lang::of`); and every other use of a type name. Without them most
/// of a Rust crate's internal dependencies (types in signatures, associated
/// functions) would be invisible.
const RUST_EXTRA: &str = r#"
(function_signature_item name: (identifier) @name) @definition.method
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(call_expression
  function: (scoped_identifier name: (identifier) @name)) @reference.call
(call_expression
  function: (generic_function function: (identifier) @name)) @reference.call
(call_expression
  function: (generic_function function: (scoped_identifier name: (identifier) @name))) @reference.call
(call_expression
  function: (generic_function function: (field_expression field: (field_identifier) @name))) @reference.call
(macro_invocation
  macro: (scoped_identifier name: (identifier) @name)) @reference.call
(scoped_identifier path: (identifier) @name) @reference.type
(type_identifier) @name @reference.type
"#;

/// The C grammar's query has no references at all: add calls and type uses.
const C_EXTRA: &str = r#"
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(type_identifier) @name @reference.type
"#;

/// As for C, plus calls through a namespace or class (`ns::f(..)`).
const CPP_EXTRA: &str = r#"
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (identifier) @name)) @reference.call
(type_identifier) @name @reference.type
"#;

/// Type aliases and enums, which neither the JavaScript nor the TypeScript
/// query defines, and every use of a type name.
const TS_EXTRA: &str = r#"
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(type_identifier) @name @reference.type
"#;

/// Uses of a type name (fields, parameters, locals), beyond the grammar's
/// `new`, `extends` and `implements`.
const JAVA_EXTRA: &str = r#"
(type_identifier) @name @reference.type
"#;

impl Lang {
    pub(crate) const ALL: [Lang; 9] = [
        Lang::Rust,
        Lang::Python,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Go,
        Lang::Java,
        Lang::C,
        Lang::Cpp,
    ];

    /// The language of a file, by its extension.
    pub(crate) fn of(path: &str) -> Option<Lang> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let (_, ext) = name.rsplit_once('.')?;
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "cpp",
        }
    }

    fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }

    /// The tags query and the locals query it needs. TypeScript's own tags
    /// query only covers what JavaScript lacks (interfaces, signatures,
    /// modules), so it is appended to JavaScript's: the TypeScript grammars
    /// extend the JavaScript one, so its patterns apply as they are.
    fn query(self) -> (String, &'static str) {
        match self {
            Lang::Rust => (format!("{}\n{RUST_EXTRA}", tree_sitter_rust::TAGS_QUERY), ""),
            Lang::Python => (tree_sitter_python::TAGS_QUERY.to_owned(), ""),
            Lang::JavaScript => (tree_sitter_javascript::TAGS_QUERY.to_owned(), tree_sitter_javascript::LOCALS_QUERY),
            Lang::TypeScript | Lang::Tsx => {
                let (js, ts) = (tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY);
                (format!("{js}\n{ts}\n{TS_EXTRA}"), "")
            }
            Lang::Go => (tree_sitter_go::TAGS_QUERY.to_owned(), ""),
            Lang::Java => (format!("{}\n{JAVA_EXTRA}", tree_sitter_java::TAGS_QUERY), ""),
            Lang::C => (format!("{}\n{C_EXTRA}", tree_sitter_c::TAGS_QUERY), ""),
            Lang::Cpp => (format!("{}\n{CPP_EXTRA}", tree_sitter_cpp::TAGS_QUERY), ""),
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    /// The language's tags configuration, built on first use and shared by
    /// every thread (it is `Sync`; only the parsing context is per thread).
    /// `None` if it does not build, which the tests rule out.
    pub(crate) fn config(self) -> Option<&'static TagsConfiguration> {
        static CONFIGS: [OnceLock<Option<TagsConfiguration>>; Lang::ALL.len()] =
            [const { OnceLock::new() }; Lang::ALL.len()];
        CONFIGS[self.index()]
            .get_or_init(|| {
                let (tags, locals) = self.query();
                TagsConfiguration::new(self.language(), &tags, locals)
                    .inspect_err(|e| tracing::warn!("cannot parse {} symbols: {e}", self.name()))
                    .ok()
            })
            .as_ref()
    }
}

/// A definition found in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Def {
    pub name: String,
    /// What the grammar calls it: `function`, `class`, `method`, ...
    pub kind: &'static str,
    /// 1-based.
    pub line: u32,
    /// The line the name is on, indentation kept (see [`signature`]).
    pub signature: String,
}

/// A use of a name found in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ref {
    pub name: String,
    /// 1-based.
    pub line: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Symbols {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

thread_local! {
    /// A parser and query cursor per thread, reused across files.
    static CONTEXT: RefCell<TagsContext> = RefCell::new(TagsContext::new());
}

/// The symbols of `source`, a file in `lang`. A file that does not parse
/// cleanly still yields what tree-sitter recovered; one that cannot be
/// parsed at all yields nothing.
pub(crate) fn extract(lang: Lang, source: &[u8]) -> Symbols {
    let mut out = Symbols::default();
    let Some(config) = lang.config() else { return out };
    CONTEXT.with_borrow_mut(|ctx| {
        let tags = match ctx.generate_tags(config, source, None) {
            Ok((tags, _)) => tags,
            Err(e) => {
                tracing::debug!("cannot parse a {} file: {e}", lang.name());
                return;
            }
        };
        for tag in tags {
            let tag = match tag {
                Ok(tag) => tag,
                Err(e) => {
                    tracing::debug!("stopped parsing a {} file: {e}", lang.name());
                    break;
                }
            };
            let Some(name) = name(&source[tag.name_range.clone()]) else { continue };
            let line = u32::try_from(tag.span.start.row + 1).unwrap_or(u32::MAX);
            if tag.is_definition {
                if out.defs.len() < MAX_FILE_DEFS {
                    let start = tag.name_range.start - tag.span.start.column;
                    out.defs.push(Def {
                        name: name.to_owned(),
                        kind: config.syntax_type_name(tag.syntax_type_id),
                        line,
                        signature: signature(&source[start..]),
                    });
                }
            } else if out.refs.len() < MAX_FILE_REFS {
                out.refs.push(Ref { name: name.to_owned(), line });
            }
            if out.defs.len() >= MAX_FILE_DEFS && out.refs.len() >= MAX_FILE_REFS {
                break;
            }
        }
    });
    out
}

/// A tag's name, if it looks like one: text without spaces, of sane length.
fn name(bytes: &[u8]) -> Option<&str> {
    let name = std::str::from_utf8(bytes).ok()?;
    let plausible = !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_whitespace);
    plausible.then_some(name)
}

/// The first line of `rest` as a signature: indentation kept so nesting
/// shows in the map, tabs as four spaces, trailing whitespace dropped, cut
/// to [`MAX_SIGNATURE_CHARS`]. Only a bounded prefix is looked at, so a
/// minified file with one huge line costs no more than any other.
pub(crate) fn signature(rest: &[u8]) -> String {
    let window = &rest[..rest.len().min(MAX_SIGNATURE_CHARS * 4)];
    let line = window.split(|&b| b == b'\n').next().unwrap_or_default();
    let mut out = String::new();
    let mut chars = 0;
    for c in String::from_utf8_lossy(line).chars() {
        if chars >= MAX_SIGNATURE_CHARS {
            break;
        }
        if c == '\t' {
            out.push_str("    ");
            chars += 4;
        } else {
            out.push(c);
            chars += 1;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_a_working_configuration() {
        for lang in Lang::ALL {
            assert!(lang.config().is_some(), "{lang:?}");
        }
    }

    #[test]
    fn languages_are_chosen_by_extension() {
        assert_eq!(Lang::of("src/main.rs"), Some(Lang::Rust));
        assert_eq!(Lang::of("a/b.pyi"), Some(Lang::Python));
        assert_eq!(Lang::of("web/app.mjs"), Some(Lang::JavaScript));
        assert_eq!(Lang::of("web/app.cts"), Some(Lang::TypeScript));
        assert_eq!(Lang::of("web/App.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::of("inc/x.h"), Some(Lang::C));
        assert_eq!(Lang::of("inc/x.hpp"), Some(Lang::Cpp));
        assert_eq!(Lang::of("README.md"), None);
        assert_eq!(Lang::of("Makefile"), None);
        assert_eq!(Lang::of("dir.rs/Makefile"), None);
    }

    #[test]
    fn signatures_keep_indentation_and_stay_short() {
        assert_eq!(signature(b"\tfn f(x: u8) {  \r\nnext"), "    fn f(x: u8) {");
        assert_eq!(signature(b"    def g():\n"), "    def g():");
        let long = format!("fn {}()", "é".repeat(400));
        let sig = signature(long.as_bytes());
        assert_eq!(sig.chars().count(), MAX_SIGNATURE_CHARS);
        assert!(sig.starts_with("fn éé"));
    }

    #[test]
    fn names_must_look_like_names() {
        assert_eq!(name(b"parse"), Some("parse"));
        assert_eq!(name(b""), None);
        assert_eq!(name(b"a b"), None);
        assert_eq!(name(&[0xff, 0xfe]), None);
    }

    #[test]
    fn a_huge_file_is_capped() {
        let source: String = (0..MAX_FILE_DEFS + 10).map(|i| format!("fn f{i}() {{ g(); }}\n")).collect();
        let symbols = extract(Lang::Rust, source.as_bytes());
        assert_eq!(symbols.defs.len(), MAX_FILE_DEFS);
        assert_eq!(symbols.refs.len(), MAX_FILE_DEFS + 10);
    }
}

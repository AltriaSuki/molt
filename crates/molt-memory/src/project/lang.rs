//! Source languages and the symbols parsed out of them.
//!
//! Symbols come from each grammar's tags query: a `definition.*` capture is
//! a definition and a `reference.*` capture a reference, its kind being the
//! part after the dot. Some grammars' queries leave out what the ranking
//! needs, so a few supplements are appended (see [`Lang::query`]); a
//! supplement only ever adds patterns after the grammar's own, and a node
//! keeps the tag of the earliest pattern that names it, so a definition is
//! never demoted to a reference.
//!
//! The queries are run here rather than through `tree-sitter-tags`, so that
//! each file has a deadline: some inputs (deep nesting, long runs of
//! comments) make query matching quadratic, and one such file could
//! otherwise hold an index for many minutes.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tree_sitter::{Language, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator};

/// Most definitions kept from one file, so a generated or pathological file
/// cannot swamp the model.
const MAX_FILE_DEFS: usize = 5_000;
/// Most references kept from one file.
const MAX_FILE_REFS: usize = 20_000;
/// Longest name kept; anything longer is not an identifier.
const MAX_NAME_BYTES: usize = 200;
/// Longest signature kept, in characters.
const MAX_SIGNATURE_CHARS: usize = 160;
/// Longest one file may take to parse and query. A file that runs out
/// keeps the symbols found so far.
const MAX_FILE_TIME: Duration = Duration::from_secs(2);
/// Most matches a query may have in progress at once. Without a bound, deep
/// nesting keeps one alive per level at every step. Editors use the same.
const MATCH_LIMIT: u32 = 256;
/// Bump when extraction changes in a way the queries and grammars do not
/// show (see [`Lang::fingerprint`]), so that every file is parsed again.
const REVISION: u32 = 1;

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

    /// The tags query. TypeScript's own only covers what JavaScript lacks
    /// (interfaces, signatures, modules), so it is appended to JavaScript's:
    /// the TypeScript grammars extend the JavaScript one, so its patterns
    /// apply as they are. JavaScript also ships a locals query, which would
    /// drop references to local variables; none of its tags patterns asks for
    /// that, and tracking scopes costs time quadratic in their number.
    fn query(self) -> String {
        match self {
            Lang::Rust => format!("{}\n{RUST_EXTRA}", tree_sitter_rust::TAGS_QUERY),
            Lang::Python => tree_sitter_python::TAGS_QUERY.to_owned(),
            Lang::JavaScript => tree_sitter_javascript::TAGS_QUERY.to_owned(),
            Lang::TypeScript | Lang::Tsx => {
                let (js, ts) = (tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY);
                format!("{js}\n{ts}\n{TS_EXTRA}")
            }
            Lang::Go => tree_sitter_go::TAGS_QUERY.to_owned(),
            Lang::Java => format!("{}\n{JAVA_EXTRA}", tree_sitter_java::TAGS_QUERY),
            Lang::C => format!("{}\n{C_EXTRA}", tree_sitter_c::TAGS_QUERY),
            Lang::Cpp => format!("{}\n{CPP_EXTRA}", tree_sitter_cpp::TAGS_QUERY),
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    /// The language's compiled query, built on first use and shared by every
    /// thread (only the parser and cursor are per thread). `None` if it does
    /// not build, which the tests rule out.
    pub(crate) fn tagger(self) -> Option<&'static Tagger> {
        static TAGGERS: [OnceLock<Option<Tagger>>; Lang::ALL.len()] = [const { OnceLock::new() }; Lang::ALL.len()];
        TAGGERS[self.index()]
            .get_or_init(|| {
                Tagger::new(self).inspect_err(|e| tracing::warn!("cannot parse {} symbols: {e}", self.name())).ok()
            })
            .as_ref()
    }

    /// What files in this language are parsed with: the grammar, the query
    /// and the limits. A file stored under another fingerprint is parsed
    /// again, so a new grammar or query applies to every file. `None` when
    /// the language cannot be parsed: its files are then tried each time.
    pub(crate) fn fingerprint(self) -> Option<&'static str> {
        self.tagger().map(|t| t.fingerprint.as_str())
    }
}

/// A language's compiled tags query, and what its captures mean.
pub(crate) struct Tagger {
    language: Language,
    query: Query,
    /// The index of the `@name` capture.
    name: u32,
    /// Per capture: for `@definition.<kind>` and `@reference.<kind>`, the
    /// kind and whether it defines. Other captures (`@doc`, `@local.*`) mean
    /// nothing here.
    tags: Vec<Option<(Box<str>, bool)>>,
    fingerprint: String,
}

impl Tagger {
    fn new(lang: Lang) -> Result<Self, String> {
        let language = lang.language();
        let text = lang.query();
        let query = Query::new(&language, &text).map_err(|e| e.to_string())?;
        let names = query.capture_names();
        let name = names.iter().position(|n| *n == "name").ok_or("the query has no @name")? as u32;
        let tags = names
            .iter()
            .map(|n| match (n.strip_prefix("definition."), n.strip_prefix("reference.")) {
                (Some(kind), _) => Some((kind.into(), true)),
                (_, Some(kind)) => Some((kind.into(), false)),
                _ => None,
            })
            .collect();
        let mut hash = Sha256::new();
        for part in [
            REVISION.to_string(),
            lang.name().to_owned(),
            language.abi_version().to_string(),
            language.node_kind_count().to_string(),
            language.field_count().to_string(),
            format!("{MAX_FILE_DEFS} {MAX_FILE_REFS} {MAX_NAME_BYTES} {MAX_SIGNATURE_CHARS} {MATCH_LIMIT}"),
            text,
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        let fingerprint = hex::encode(&hash.finalize()[..8]);
        Ok(Self { language, query, name, tags, fingerprint })
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
    static CONTEXT: RefCell<(Parser, QueryCursor)> = RefCell::new((Parser::new(), QueryCursor::new()));
}

/// The symbols of `source`, a file in `lang`. A file that does not parse
/// cleanly still yields what tree-sitter recovered; one that cannot be
/// parsed at all yields nothing, and one that takes too long yields what
/// was found in time.
pub(crate) fn extract(lang: Lang, source: &[u8]) -> Symbols {
    extract_by(lang, source, Instant::now() + MAX_FILE_TIME)
}

/// A tag found by the query, before its name is checked.
struct Tag {
    /// Where the name is.
    start: usize,
    end: usize,
    row: usize,
    column: usize,
    kind: &'static str,
    definition: bool,
}

/// [`extract`], giving up at `deadline`.
fn extract_by(lang: Lang, source: &[u8], deadline: Instant) -> Symbols {
    let mut out = Symbols::default();
    let Some(tagger) = lang.tagger() else { return out };
    let in_time = || if Instant::now() < deadline { ControlFlow::Continue(()) } else { ControlFlow::Break(()) };
    CONTEXT.with_borrow_mut(|(parser, cursor)| {
        if let Err(e) = parser.set_language(&tagger.language) {
            tracing::debug!("cannot parse {}: {e}", lang.name());
            return;
        }
        // A parse that was stopped would otherwise go on where it stopped.
        parser.reset();
        let mut parse_check = |_: &_| in_time();
        let tree = parser.parse_with_options(
            &mut |i, _| source.get(i..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut parse_check)),
        );
        let Some(tree) = tree else {
            tracing::debug!("gave up parsing a {} file after {MAX_FILE_TIME:?}", lang.name());
            return;
        };
        cursor.set_match_limit(MATCH_LIMIT);
        let mut query_check = |_: &_| in_time();
        let mut matches = cursor.matches_with_options(
            &tagger.query,
            tree.root_node(),
            source,
            QueryCursorOptions::new().progress_callback(&mut query_check),
        );
        // Tags wait here, ordered by where their name ends, until no later
        // match can tag the same name; a name keeps the earliest pattern's tag.
        let mut queue: VecDeque<(Tag, usize)> = VecDeque::new();
        while let Some(m) = matches.next() {
            let (mut name, mut tag) = (None, None);
            for capture in m.captures() {
                if capture.index == tagger.name {
                    name = Some(capture.node);
                } else if let Some(Some((kind, definition))) = tagger.tags.get(capture.index as usize) {
                    tag = Some((&**kind, *definition));
                }
            }
            let (Some(name), Some((kind, definition))) = (name, tag) else { continue };
            if name.has_error() {
                continue;
            }
            let at = name.start_position();
            let tag = Tag {
                start: name.start_byte(),
                end: name.end_byte(),
                row: at.row,
                column: at.column,
                kind,
                definition,
            };
            match queue.binary_search_by_key(&(tag.end, tag.start), |(t, _)| (t.end, t.start)) {
                Ok(i) if queue[i].1 > m.pattern_index => queue[i] = (tag, m.pattern_index),
                Ok(_) => {}
                Err(i) => queue.insert(i, (tag, m.pattern_index)),
            }
            while queue.len() > 1 && queue[0].0.end < queue[queue.len() - 1].0.start {
                let (tag, _) = queue.pop_front().expect("the queue has two tags");
                if !keep(&mut out, source, tag) {
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            tracing::debug!("stopped reading a {} file's symbols after {MAX_FILE_TIME:?}", lang.name());
        }
        for (tag, _) in queue {
            if !keep(&mut out, source, tag) {
                return;
            }
        }
    });
    out
}

/// Add `tag` to `out` if it names something and there is room. False once
/// there is no room for anything more.
fn keep(out: &mut Symbols, source: &[u8], tag: Tag) -> bool {
    if let Some(name) = name(&source[tag.start..tag.end]) {
        let line = u32::try_from(tag.row + 1).unwrap_or(u32::MAX);
        if tag.definition {
            if out.defs.len() < MAX_FILE_DEFS {
                out.defs.push(Def {
                    name: name.to_owned(),
                    kind: tag.kind,
                    line,
                    signature: signature(&source[tag.start - tag.column..]),
                });
            }
        } else if out.refs.len() < MAX_FILE_REFS {
            out.refs.push(Ref { name: name.to_owned(), line });
        }
    }
    out.defs.len() < MAX_FILE_DEFS || out.refs.len() < MAX_FILE_REFS
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
    fn every_language_has_a_working_query_and_its_own_fingerprint() {
        let mut seen = std::collections::HashSet::new();
        for lang in Lang::ALL {
            let fingerprint = lang.fingerprint().unwrap_or_else(|| panic!("{lang:?} has no query"));
            assert!(seen.insert(fingerprint), "{lang:?}");
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

    #[test]
    fn a_file_that_takes_too_long_keeps_what_was_found_in_time() {
        let source: String = (0..2_000).map(|i| format!("fn f{i}() {{ g(); }}\n")).collect();
        assert_eq!(extract(Lang::Rust, source.as_bytes()).defs.len(), 2_000);
        let late = extract_by(Lang::Rust, source.as_bytes(), Instant::now());
        assert!(late.defs.len() < 2_000, "{}", late.defs.len());
        // Matching is quadratic in nesting depth here, and stops at the deadline.
        let nested = "(".repeat(256 * 1024);
        let clock = Instant::now();
        extract_by(Lang::JavaScript, nested.as_bytes(), Instant::now() + Duration::from_millis(200));
        assert!(clock.elapsed() < Duration::from_secs(3), "{:?}", clock.elapsed());
        // The parser starts afresh after a parse it gave up on.
        assert_eq!(extract(Lang::Rust, b"fn after() {}\n").defs[0].name, "after");
    }
}

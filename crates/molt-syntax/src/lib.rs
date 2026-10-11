//! Source languages Molt can parse, and the syntax errors a change to a
//! file introduces.
//!
//! The grammars are tree-sitter's, built in: nothing outside the process is
//! run and nothing is fetched, so a check works inside the sandbox and costs
//! a few milliseconds. tree-sitter recovers from errors, so a file that does
//! not parse still has a tree, with `ERROR` nodes where text could not be
//! placed and `MISSING` nodes where a token had to be assumed; those are the
//! errors reported here. Only errors a change adds are reported (see
//! [`introduced`]), so a file that was already broken, or a construct a
//! grammar does not know, does not get in the model's way.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, Parser};

/// Longest one file may take to parse, as for the symbol index. A file that
/// runs out is not checked.
pub const MAX_PARSE_TIME: Duration = Duration::from_secs(2);
/// Most errors looked at in one file; a file far past this is not code the
/// grammar understands, and walking all of it would only cost time.
const MAX_SCANNED: usize = 1_000;
/// Most characters of a line shown as context, and of the text an error names.
const MAX_LINE_CHARS: usize = 200;
const MAX_TOKEN_CHARS: usize = 40;

/// A language Molt can parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
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

impl Lang {
    pub const ALL: [Lang; 9] = [
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
    pub fn of(path: &str) -> Option<Lang> {
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

    pub fn name(self) -> &'static str {
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

    /// The tree-sitter grammar.
    pub fn language(self) -> Language {
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
}

/// A syntax error in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// 1-based.
    pub line: u32,
    /// What is wrong, e.g. ``missing `)` `` or ``unexpected `}` ``.
    pub message: String,
    /// The line before, the line itself and the line after, each as
    /// `NUMBER | text`, so the model can see the error without reading the
    /// file again.
    pub context: String,
}

/// The syntax errors in `after` that `before` (the file as it was; `None`
/// for a new file) does not have, in file order, if the file is in a
/// language Molt can parse. An error counts as the same when its message
/// and the text of its line are, wherever the line moved to. Empty when the
/// language is unknown, or either version takes longer than
/// [`MAX_PARSE_TIME`] to parse: a check that cannot finish says nothing.
pub fn introduced(path: &str, before: Option<&str>, after: &str) -> Vec<Problem> {
    let Some(lang) = Lang::of(path) else { return Vec::new() };
    let Some(found) = errors(lang, after, Instant::now() + MAX_PARSE_TIME) else { return Vec::new() };
    if found.is_empty() {
        return Vec::new();
    }
    let mut known: HashMap<(String, String), usize> = HashMap::new();
    if let Some(before) = before {
        let Some(old) = errors(lang, before, Instant::now() + MAX_PARSE_TIME) else { return Vec::new() };
        for e in old {
            *known.entry(e.key).or_default() += 1;
        }
    }
    found
        .into_iter()
        .filter(|e| match known.get_mut(&e.key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .map(|e| e.problem)
        .collect()
}

/// A syntax error, and what identifies it across versions of the file.
struct Found {
    problem: Problem,
    key: (String, String),
}

/// Every syntax error in `source`, or `None` if it cannot be parsed by `deadline`.
fn errors(lang: Lang, source: &str, deadline: Instant) -> Option<Vec<Found>> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    let bytes = source.as_bytes();
    let mut in_time =
        |_: &_| if Instant::now() < deadline { ControlFlow::Continue(()) } else { ControlFlow::Break(()) };
    let tree = parser.parse_with_options(
        &mut |i, _| bytes.get(i..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut in_time)),
    )?;
    let root = tree.root_node();
    let mut nodes = Vec::new();
    if root.has_error() {
        collect(root, &mut nodes);
    }
    let lines: Vec<&str> = source.lines().collect();
    Some(nodes.into_iter().map(|node| found(node, bytes, &lines)).collect())
}

/// The outermost `ERROR` and every `MISSING` node under `node`, in order.
fn collect<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if out.len() >= MAX_SCANNED {
        return;
    }
    if node.is_missing() || node.is_error() {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() {
            collect(child, out);
        }
    }
}

fn found(node: Node, source: &[u8], lines: &[&str]) -> Found {
    let row = node.start_position().row;
    let message = if node.is_missing() {
        let kind = node.kind();
        if node.is_named() {
            format!("missing {}", kind.replace('_', " "))
        } else {
            format!("missing `{kind}`")
        }
    } else {
        let text = String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]);
        match text.split_whitespace().next() {
            Some(token) => format!("unexpected `{}`", clip(token, MAX_TOKEN_CHARS)),
            None => "syntax error".to_owned(),
        }
    };
    let line_text = lines.get(row).copied().unwrap_or_default();
    let context = (row.saturating_sub(1)..=row + 1)
        .filter_map(|r| lines.get(r).map(|text| format!("{} | {}", r + 1, clip(text, MAX_LINE_CHARS))))
        .collect::<Vec<_>>()
        .join("\n");
    let line = u32::try_from(row + 1).unwrap_or(u32::MAX);
    Found { key: (message.clone(), line_text.trim().to_owned()), problem: Problem { line, message, context } }
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn every_language_parses_valid_code_without_errors() {
        let samples = [
            ("a.rs", "fn main() { let x = (1 + 2); }\n"),
            ("a.py", "def f(x):\n    return (x + 1)\n"),
            ("a.js", "function f(x) { return (x + 1); }\n"),
            ("a.ts", "function f(x: number): number { return (x + 1); }\n"),
            ("a.tsx", "const A = () => <div>{1}</div>;\n"),
            ("a.go", "package a\n\nfunc F(x int) int { return (x + 1) }\n"),
            ("A.java", "class A { int f(int x) { return (x + 1); } }\n"),
            ("a.c", "int f(int x) { return (x + 1); }\n"),
            ("a.cpp", "int f(int x) { return (x + 1); }\n"),
        ];
        for (path, source) in samples {
            assert_eq!(introduced(path, None, source), Vec::new(), "{path}");
        }
    }

    #[test]
    fn a_missing_bracket_is_reported_where_it_is() {
        let before = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let after = "fn main() {\n    let x = (1;\n    println!(\"{x}\");\n}\n";
        let found = introduced("src/main.rs", Some(before), after);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 2);
        assert_eq!(found[0].message, "missing `)`");
        assert_eq!(found[0].context, "1 | fn main() {\n2 |     let x = (1;\n3 |     println!(\"{x}\");");

        let py = introduced("app.py", Some("def f(x):\n    return x\n"), "def f(x:\n    return x\n");
        assert_eq!(py.first().map(|p| p.line), Some(1), "{py:?}");

        let ts = "export function f(x: number): number {\n  return g(x;\n}\n";
        let found = introduced("src/f.ts", Some("export function f(x: number): number {\n  return g(x);\n}\n"), ts);
        assert!(found.iter().any(|p| p.line == 2), "{found:?}");
    }

    #[test]
    fn errors_the_file_already_had_are_not_reported_again() {
        let broken = "fn a() {\n    let x = (1;\n}\n\nfn b() {}\n";
        // An unrelated edit, which moves the old error down a line.
        let edited = "// a comment\nfn a() {\n    let x = (1;\n}\n\nfn b() { let y = 2; }\n";
        assert_eq!(introduced("a.rs", Some(broken), edited), Vec::new());
        // A second, new error is.
        let worse = "fn a() {\n    let x = (1;\n}\n\nfn b() { let y = [2; }\n";
        let found = introduced("a.rs", Some(broken), worse);
        assert_eq!(found.iter().map(|p| p.line).collect::<Vec<_>>(), vec![5], "{found:?}");
    }

    #[test]
    fn a_new_file_reports_all_its_errors_and_unknown_languages_none() {
        assert!(!introduced("new.rs", None, "fn f( {}\n").is_empty());
        assert_eq!(introduced("notes.md", None, "fn f( {"), Vec::new());
        assert_eq!(introduced("Makefile", Some("a:"), "a: ((("), Vec::new());
    }

    #[test]
    fn a_file_that_takes_too_long_is_not_checked() {
        let source = "fn f( {\n".repeat(1000);
        assert!(errors(Lang::Rust, &source, Instant::now()).is_none());
        assert!(errors(Lang::Rust, &source, Instant::now() + MAX_PARSE_TIME).is_some());
    }

    #[test]
    fn long_lines_and_tokens_are_clipped() {
        let long = format!("let x = ({};\n", "a".repeat(1000));
        let found = introduced("a.rs", None, &format!("fn f() {{\n{long}}}\n"));
        assert!(found.iter().all(|p| p.context.lines().all(|l| l.chars().count() < MAX_LINE_CHARS + 10)), "{found:?}");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("ab", 3), "ab");
    }
}

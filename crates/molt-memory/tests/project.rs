//! The project model, through its public functions: what is parsed from
//! each language, how an index follows the files on disk, how the map ranks,
//! and what a symbol lookup returns.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use molt_api::memory::{Definition, MapRequest, MapResponse, SymbolsRequest, SymbolsResponse};
use molt_memory::{project, Db};
use molt_proto::ErrorCode;
use tempfile::TempDir;

/// A canonical workspace holding `files`.
fn workspace(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for (path, contents) in files {
        write(&root, path, contents);
    }
    (dir, root)
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn set_mtime(root: &Path, rel: &str, at: SystemTime) {
    File::options().write(true).open(root.join(rel)).unwrap().set_modified(at).unwrap();
}

fn mtime(root: &Path, rel: &str) -> SystemTime {
    fs::metadata(root.join(rel)).unwrap().modified().unwrap()
}

fn lookup(db: &Db, root: &Path, name: &str) -> SymbolsResponse {
    let req = SymbolsRequest { workspace: String::new(), name: name.into(), references: true, limit: None };
    project::symbols(db, root, &req).unwrap()
}

fn defs(db: &Db, root: &Path, name: &str) -> Vec<Definition> {
    lookup(db, root, name).definitions
}

/// The (path, kind) of each definition of `name`.
fn kinds(db: &Db, root: &Path, name: &str) -> Vec<(String, String)> {
    defs(db, root, name).into_iter().map(|d| (d.path, d.kind)).collect()
}

/// The (path, line) of each reference to `name`.
fn uses(db: &Db, root: &Path, name: &str) -> Vec<(String, u64)> {
    lookup(db, root, name).references.into_iter().map(|r| (r.path, r.line)).collect()
}

fn map(db: &Db, root: &Path, query: &str, max_tokens: Option<u32>) -> MapResponse {
    let req = MapRequest { workspace: String::new(), query: query.into(), max_tokens };
    project::map(db, root, &req).unwrap()
}

/// The files of a map, in order.
fn files_of(map: &MapResponse) -> Vec<&str> {
    map.map.lines().filter(|l| !l.starts_with(' ') && l.ends_with(':')).map(|l| l.trim_end_matches(':')).collect()
}

/// The (file, definition line) entries of a map.
fn entries_of(map: &MapResponse) -> Vec<(String, String)> {
    let mut file = "";
    let mut out = Vec::new();
    for line in map.map.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') {
            out.push((file.to_owned(), line.to_owned()));
        } else {
            file = line.trim_end_matches(':');
        }
    }
    out
}

fn has(found: &[(String, String)], path: &str, kind: &str) -> bool {
    found.iter().any(|(p, k)| p == path && k == kind)
}

// Languages.

/// Index one file and check that each `(name, kind)` is defined in it and
/// each `(name, line)` referenced from it.
fn check_language(path: &str, source: &str, defined: &[(&str, &str)], referenced: &[(&str, u64)]) {
    let (_dir, root) = workspace(&[(path, source)]);
    let db = Db::in_memory().unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed), (1, 1), "{path}");
    for (name, kind) in defined {
        let found = kinds(&db, &root, name);
        assert!(has(&found, path, kind), "{path}: no {kind} {name} in {found:?}");
    }
    for (name, line) in referenced {
        let found = uses(&db, &root, name);
        assert!(found.contains(&(path.to_owned(), *line)), "{path}: no reference to {name} at {line} in {found:?}");
    }
}

#[test]
fn rust_definitions_and_references() {
    let source = r#"use crate::db::Db;

pub const LIMIT: usize = 3;

pub struct Kernel {
    log: AuditLog,
}

pub enum Mode { Fast }

pub trait Service {
    fn handle(&self);
}

impl Kernel {
    pub fn start(db: &Db) -> Self {
        let log = AuditLog::open(db);
        helper();
        log.flush();
        Kernel { log }
    }
}

macro_rules! shout { () => {} }

fn helper() {}
"#;
    check_language(
        "src/kernel.rs",
        source,
        &[
            ("LIMIT", "constant"),
            ("Kernel", "class"),
            ("Mode", "class"),
            ("Service", "interface"),
            ("handle", "method"),
            ("start", "method"),
            ("shout", "macro"),
            ("helper", "function"),
        ],
        // A type in a field and a signature, a path call, a plain call and a method call.
        &[("AuditLog", 6), ("Db", 16), ("open", 17), ("helper", 18), ("flush", 19)],
    );
}

#[test]
fn python_definitions_and_references() {
    let source = "RETRIES = 3\n\nclass Store(Base):\n    def save(self, item):\n        validate(item)\n        self.flush()\n\ndef validate(item):\n    pass\n";
    check_language(
        "app/store.py",
        source,
        &[("RETRIES", "constant"), ("Store", "class"), ("save", "function"), ("validate", "function")],
        &[("validate", 5), ("flush", 6)],
    );
}

#[test]
fn javascript_definitions_and_references() {
    let source = "class Cart {\n  total() {\n    return sum(this.items);\n  }\n}\n\nfunction sum(xs) {\n  return new Total(xs);\n}\n\nconst render = () => sum([]);\n";
    check_language(
        "web/cart.js",
        source,
        &[("Cart", "class"), ("total", "method"), ("sum", "function"), ("render", "function")],
        &[("sum", 3), ("Total", 8)],
    );
}

#[test]
fn typescript_has_classes_functions_methods_and_interfaces() {
    let source = "export interface Shape {\n  area(): number;\n}\n\nexport class Circle implements Shape {\n  area(): number {\n    return square(this.r);\n  }\n}\n\nexport function square(x: number): Size {\n  return x * x;\n}\n\nexport type Size = number;\nexport enum Color { Red }\n";
    check_language(
        "src/shape.ts",
        source,
        &[
            ("Shape", "interface"),
            ("Circle", "class"),
            ("area", "method"),
            ("square", "function"),
            ("Size", "type"),
            ("Color", "enum"),
        ],
        &[("square", 7), ("Size", 11), ("Shape", 5)],
    );
}

#[test]
fn tsx_has_classes_functions_methods_and_interfaces() {
    let source = "interface Props {\n  name: string;\n}\n\nclass Greeting extends React.Component<Props> {\n  render() {\n    return <b>{shout(this.props.name)}</b>;\n  }\n}\n\nfunction shout(s: string) {\n  return s.toUpperCase();\n}\n";
    check_language(
        "ui/Greeting.tsx",
        source,
        &[("Props", "interface"), ("Greeting", "class"), ("render", "method"), ("shout", "function")],
        &[("shout", 7)],
    );
}

#[test]
fn go_definitions_and_references() {
    let source = "package store\n\ntype Store struct{}\n\nfunc (s *Store) Save() error {\n\treturn validate()\n}\n\nfunc validate() error {\n\treturn nil\n}\n";
    check_language(
        "store/store.go",
        source,
        &[("Store", "type"), ("Save", "method"), ("validate", "function")],
        &[("validate", 6), ("Store", 5)],
    );
}

#[test]
fn java_definitions_and_references() {
    let source = "class Store implements Saver {\n    void save(Item item) {\n        validate(item);\n    }\n}\n\ninterface Saver {}\n";
    check_language(
        "src/Store.java",
        source,
        &[("Store", "class"), ("save", "method"), ("Saver", "interface")],
        &[("validate", 3), ("Saver", 1), ("Item", 2)],
    );
}

#[test]
fn c_definitions_and_references() {
    let source =
        "struct point { int x; };\ntypedef int length;\n\nlength norm(struct point p) {\n    return square(p.x);\n}\n";
    check_language(
        "src/geom.c",
        source,
        &[("point", "class"), ("length", "type"), ("norm", "function")],
        &[("square", 5), ("length", 4)],
    );
}

#[test]
fn cpp_definitions_and_references() {
    let source = "class Shape {\n public:\n  double area();\n};\n\ndouble Shape::area() {\n  return geom::square(2.0);\n}\n\nstruct Point {};\n";
    check_language(
        "src/shape.cpp",
        source,
        &[("Shape", "class"), ("area", "method"), ("Point", "class")],
        &[("square", 7)],
    );
}

#[test]
fn files_without_a_grammar_are_not_in_the_model() {
    let (_dir, root) =
        workspace(&[("README.md", "# fn main() {}\n"), ("data.json", "{}"), ("src/main.rs", "fn main() {}\n")]);
    let db = Db::in_memory().unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed, r.skipped, r.symbols), (1, 1, 0, 1));
}

#[test]
fn a_file_that_does_not_parse_is_indexed_without_failing() {
    let (_dir, root) = workspace(&[("src/broken.rs", "fn ((( {{{ struct"), ("src/ok.rs", "fn fine() {}\n")]);
    let db = Db::in_memory().unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed), (2, 2));
    assert_eq!(kinds(&db, &root, "fine"), [("src/ok.rs".to_owned(), "function".to_owned())]);
}

// Incremental indexing.

#[test]
fn an_unchanged_tree_is_not_parsed_again() {
    let (_dir, root) = workspace(&[("a.rs", "fn a() {}\n"), ("b/c.py", "def c():\n    pass\n")]);
    let db = Db::in_memory().unwrap();
    let first = project::index(&db, &root, None).unwrap();
    assert_eq!((first.files, first.parsed, first.removed, first.symbols), (2, 2, 0, 2));
    let second = project::index(&db, &root, None).unwrap();
    assert_eq!((second.files, second.parsed, second.removed, second.symbols), (2, 0, 0, 2));
}

#[test]
fn only_a_changed_file_is_parsed_again() {
    let (_dir, root) = workspace(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    write(&root, "a.rs", "fn renamed() {}\nfn another() {}\n");
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed, r.symbols), (2, 1, 3));
    assert!(defs(&db, &root, "a").is_empty());
    assert_eq!(kinds(&db, &root, "renamed"), [("a.rs".to_owned(), "function".to_owned())]);
}

#[test]
fn a_deleted_file_leaves_with_its_symbols() {
    let (_dir, root) = workspace(&[("a.rs", "fn a() { b(); }\n"), ("b.rs", "fn b() { a(); }\n")]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    fs::remove_file(root.join("b.rs")).unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.removed, r.symbols), (1, 1, 1));
    assert!(defs(&db, &root, "b").is_empty());
    assert!(uses(&db, &root, "a").is_empty(), "b.rs's reference to a is gone too");
}

#[test]
fn a_rewrite_within_the_same_mtime_tick_is_seen() {
    let (_dir, root) = workspace(&[("a.rs", "fn one() {}\n")]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    // Same size, same mtime: only the contents changed, as when a file is
    // rewritten within one timestamp tick of being indexed.
    let before = mtime(&root, "a.rs");
    write(&root, "a.rs", "fn two() {}\n");
    set_mtime(&root, "a.rs", before);
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!(r.parsed, 1);
    assert_eq!(defs(&db, &root, "two").len(), 1);
}

#[test]
fn a_file_read_well_after_its_last_change_is_trusted_by_size_and_mtime() {
    let (_dir, root) = workspace(&[("a.rs", "fn one() {}\n")]);
    let old = SystemTime::now() - Duration::from_secs(3600);
    set_mtime(&root, "a.rs", old);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    // Now the row is trusted: a same-size change that keeps the mtime is
    // not even read. That is the bargain (size, mtime) checks make.
    write(&root, "a.rs", "fn two() {}\n");
    set_mtime(&root, "a.rs", old);
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!(r.parsed, 0);
    assert_eq!(defs(&db, &root, "one").len(), 1);
    // A new mtime gets it read again.
    set_mtime(&root, "a.rs", old + Duration::from_secs(1));
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!(r.parsed, 1);
    assert_eq!(defs(&db, &root, "two").len(), 1);
    // Touching a file without changing it costs a hash, not a parse.
    set_mtime(&root, "a.rs", old + Duration::from_secs(2));
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!(r.parsed, 0);
}

#[test]
fn named_paths_are_indexed_or_dropped() {
    let (_dir, root) = workspace(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n"), ("c.rs", "fn c() {}\n")]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    write(&root, "a.rs", "fn a2() {}\n");
    fs::remove_file(root.join("b.rs")).unwrap();
    write(&root, "new/d.rs", "fn d() {}\n");
    write(&root, "c.rs", "fn c2() {}\n"); // changed, but not named: left alone

    let named: Vec<String> = ["a.rs", "./b.rs", "new/d.rs", "notes.txt", "new"].map(String::from).into();
    let r = project::index(&db, &root, Some(&named)).unwrap();
    assert_eq!((r.files, r.parsed, r.removed, r.symbols), (3, 2, 1, 3));
    assert_eq!(defs(&db, &root, "a2").len(), 1);
    assert!(defs(&db, &root, "b").is_empty());
    assert_eq!(defs(&db, &root, "d").len(), 1);
    assert_eq!(defs(&db, &root, "c").len(), 1, "c.rs was not named");
}

#[test]
fn named_paths_must_stay_in_the_workspace() {
    let (_dir, root) = workspace(&[("a.rs", "fn a() {}\n")]);
    let db = Db::in_memory().unwrap();
    for bad in ["/etc/passwd", "../a.rs", "src/../../a.rs"] {
        let err = project::index(&db, &root, Some(&[bad.to_owned()])).unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{bad}");
    }
}

#[test]
fn named_paths_inside_git_or_molt_or_through_a_symlink_are_dropped() {
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "secret.rs", "fn secret() {}\n");
    let (_dir, root) = workspace(&[(".git/hook.rs", "fn hook() {}\n"), (".molt/x.rs", "fn x() {}\n")]);
    std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), root.join("direct.rs")).unwrap();
    let db = Db::in_memory().unwrap();
    let named: Vec<String> = [".git/hook.rs", ".molt/x.rs", "linked/secret.rs", "direct.rs"].map(String::from).into();
    let r = project::index(&db, &root, Some(&named)).unwrap();
    assert_eq!((r.files, r.parsed), (0, 0));
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed), (0, 0));
}

#[test]
fn ignored_files_are_left_out_and_dropped_when_they_become_ignored() {
    let (_dir, root) = workspace(&[
        (".gitignore", "target/\ngenerated.rs\n"),
        ("src/lib.rs", "fn lib() {}\n"),
        ("src/generated.rs", "fn generated() {}\n"),
        ("target/debug/build.rs", "fn built() {}\n"),
        ("src/later.rs", "fn later() {}\n"),
    ]);
    let db = Db::in_memory().unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!(r.files, 2);
    assert!(defs(&db, &root, "generated").is_empty());
    assert!(defs(&db, &root, "built").is_empty());

    write(&root, ".gitignore", "target/\ngenerated.rs\nlater.rs\n");
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.removed), (1, 1));
    assert!(defs(&db, &root, "later").is_empty());
}

#[test]
fn binary_and_oversized_files_are_skipped() {
    let big = format!("fn big() {{}}\n{}", "// padding\n".repeat(100_000));
    let (_dir, root) = workspace(&[("a.rs", "fn a() {}\n"), ("big.rs", &big), ("bin.rs", "fn bin() {}\0\n")]);
    let db = Db::in_memory().unwrap();
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.parsed, r.skipped, r.truncated), (1, 1, 2, false));
    assert!(defs(&db, &root, "big").is_empty());
    assert!(defs(&db, &root, "bin").is_empty());

    // A file in the model that turns binary leaves it.
    write(&root, "a.rs", "fn a() {}\0\n");
    let r = project::index(&db, &root, None).unwrap();
    assert_eq!((r.files, r.removed, r.skipped), (0, 1, 3));
}

#[test]
fn workspaces_are_kept_apart() {
    let (_d1, one) = workspace(&[("a.rs", "fn shared() {}\n")]);
    let (_d2, two) = workspace(&[("b.rs", "fn shared() {}\nfn other() {}\n")]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &one, None).unwrap();
    let r = project::index(&db, &two, None).unwrap();
    assert_eq!((r.files, r.symbols), (1, 2));
    assert_eq!(kinds(&db, &one, "shared"), [("a.rs".to_owned(), "function".to_owned())]);
    assert_eq!(kinds(&db, &two, "shared"), [("b.rs".to_owned(), "function".to_owned())]);
    fs::remove_file(two.join("b.rs")).unwrap();
    project::index(&db, &two, None).unwrap();
    assert_eq!(defs(&db, &one, "shared").len(), 1);
}

// The map.

/// A small crate: `core.rs` is used by every other file; `billing.rs` and
/// `leaf.rs` by none.
fn crate_fixture() -> (TempDir, PathBuf) {
    let user = |name: &str| {
        format!(
            "use crate::core::{{Engine, Config}};\n\npub fn {name}(config: &Config) -> Engine {{\n    let engine = Engine::new(config);\n    engine.run();\n    helper();\n    engine\n}}\n"
        )
    };
    workspace(&[
        (
            "src/core.rs",
            "pub struct Config {\n    pub verbose: bool,\n}\n\npub struct Engine {\n    config: Config,\n}\n\nimpl Engine {\n    pub fn new(config: &Config) -> Engine {\n        Engine { config: Config { verbose: config.verbose } }\n    }\n\n    pub fn run(&self) {}\n}\n\npub fn helper() {}\n",
        ),
        ("src/a.rs", &user("start_a")),
        ("src/b.rs", &user("start_b")),
        ("src/c.rs", &user("start_c")),
        ("src/billing.rs", "pub fn compute_invoice(total: u64) -> u64 {\n    total * 2\n}\n"),
        ("src/leaf.rs", "pub struct Lonely;\n\nimpl Lonely {\n    pub fn wander(&self) {}\n}\n"),
    ])
}

#[test]
fn the_most_used_module_ranks_first_when_nothing_is_mentioned() {
    let (_dir, root) = crate_fixture();
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let m = map(&db, &root, "", None);
    let files = files_of(&m);
    assert_eq!(files[0], "src/core.rs", "{}", m.map);
    let at = |f: &str| files.iter().position(|x| *x == f).unwrap();
    assert!(at("src/core.rs") < at("src/leaf.rs"));
    assert!(at("src/core.rs") < at("src/billing.rs"));
    assert_eq!(m.files as usize, files.len());
    assert_eq!(m.symbols as usize, entries_of(&m).len());
    // Everything fits in the default budget; nesting shows.
    assert_eq!(m.files, 6);
    assert!(m.map.contains("src/core.rs:\n    1: pub struct Config {\n"), "{}", m.map);
    assert!(m.map.contains("   10:     pub fn new(config: &Config) -> Engine {\n"), "{}", m.map);
}

#[test]
fn a_mentioned_identifier_ranks_its_file_first() {
    let (_dir, root) = crate_fixture();
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let m = map(&db, &root, "Make compute_invoice round to cents", None);
    assert_eq!(files_of(&m)[0], "src/billing.rs", "{}", m.map);
    // In another case too, for names of four characters or more.
    let m = map(&db, &root, "why is lonely never used?", None);
    assert_eq!(files_of(&m)[0], "src/leaf.rs", "{}", m.map);
}

#[test]
fn a_mentioned_path_ranks_its_file_first() {
    let (_dir, root) = crate_fixture();
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let absolute = format!("{}/src/leaf.rs", root.display());
    for query in ["clean up src/leaf.rs please", "see `leaf.rs:4`", absolute.as_str()] {
        let m = map(&db, &root, query, None);
        assert_eq!(files_of(&m)[0], "src/leaf.rs", "{query}: {}", m.map);
    }
    // A word naming a file (without its extension) mentions it too. What
    // the mentioned file does not reach is still ranked by the code's
    // structure, not left in path order.
    let m = map(&db, &root, "the billing totals are wrong", None);
    assert_eq!(files_of(&m)[..2], ["src/billing.rs", "src/core.rs"], "{}", m.map);
}

#[test]
fn the_map_keeps_to_its_budget() {
    let (_dir, root) = crate_fixture();
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let full = map(&db, &root, "", None);
    let full_entries = entries_of(&full);
    let mut last = 0;
    for budget in [1, 10, 25, 50, 80, 120, 1000] {
        let m = map(&db, &root, "", Some(budget));
        assert!(m.tokens <= budget, "{budget}: {} tokens", m.tokens);
        assert_eq!(m.tokens as usize, m.map.len().div_ceil(4));
        // A smaller budget picks a prefix of the same ranking.
        let entries = entries_of(&m);
        assert!(entries.iter().all(|e| full_entries.contains(e)), "{budget}");
        assert!(entries.len() >= last, "{budget}");
        last = entries.len();
    }
    assert_eq!(map(&db, &root, "", Some(1)).map, "");
    let small = map(&db, &root, "", Some(25));
    assert!(small.symbols > 0 && small.symbols < full.symbols);
    let bigger = entries_of(&map(&db, &root, "", Some(50)));
    assert!(entries_of(&small).iter().all(|e| bigger.contains(e)));
    // Budgets are clamped, not refused.
    assert_eq!(map(&db, &root, "", Some(0)).map, "");
    assert_eq!(map(&db, &root, "", Some(u32::MAX)), full);
}

#[test]
fn the_map_is_deterministic() {
    let (_dir, root) = crate_fixture();
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let query = "Engine helper start_b";
    let first = map(&db, &root, query, Some(200));
    assert_eq!(map(&db, &root, query, Some(200)), first);
    // A model built in another order gives the same map.
    let other = Db::in_memory().unwrap();
    for path in ["src/leaf.rs", "src/c.rs", "src/core.rs", "src/b.rs", "src/billing.rs", "src/a.rs"] {
        project::index(&other, &root, Some(&[path.to_owned()])).unwrap();
    }
    assert_eq!(map(&other, &root, query, Some(200)), first);
}

#[test]
fn an_empty_or_unknown_project_has_an_empty_map() {
    let (_dir, root) = workspace(&[("README.md", "nothing to see\n")]);
    let db = Db::in_memory().unwrap();
    let empty = MapResponse { map: String::new(), files: 0, symbols: 0, tokens: 0 };
    assert_eq!(map(&db, &root, "anything", None), empty);
    project::index(&db, &root, None).unwrap();
    assert_eq!(map(&db, &root, "anything", None), empty);
}

// Symbols.

#[test]
fn symbols_match_exactly_then_ignoring_case() {
    let (_dir, root) = workspace(&[
        ("a.rs", "pub struct Parser;\npub fn parse() {}\n"),
        ("b.rs", "fn use_it() {\n    parse();\n    let p: Parser = Parser;\n}\n"),
    ]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();

    let found = lookup(&db, &root, "parse");
    assert_eq!(found.definitions.len(), 1);
    let def = &found.definitions[0];
    assert_eq!((def.path.as_str(), def.line, def.kind.as_str(), def.name.as_str()), ("a.rs", 2, "function", "parse"));
    assert_eq!(def.signature, "pub fn parse() {}");
    assert_eq!(found.references.len(), 1);
    assert!(!found.truncated);

    // `parser` matches nothing exactly, so `Parser` is found ignoring case.
    let found = lookup(&db, &root, " parser ");
    assert_eq!(found.definitions.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Parser"]);
    assert_eq!(found.references.iter().map(|r| r.line).collect::<Vec<_>>(), [3], "one entry per line");

    assert_eq!(
        lookup(&db, &root, "nothing"),
        SymbolsResponse { definitions: vec![], references: vec![], truncated: false }
    );
}

#[test]
fn references_show_the_line_as_it_is_now() {
    let (_dir, root) = workspace(&[
        ("a.rs", "pub fn target() {}\n"),
        ("b.rs", "fn one() {\n\ttarget();\n}\n"),
        ("c.rs", "fn two() {\n    target();\n}\nfn three() { target() }\n"),
    ]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let texts = |db: &Db| -> Vec<(String, u64, String)> {
        lookup(db, &root, "target").references.into_iter().map(|r| (r.path, r.line, r.text)).collect()
    };
    assert_eq!(
        texts(&db),
        [
            ("b.rs".to_owned(), 2, "target();".to_owned()),
            ("c.rs".to_owned(), 2, "target();".to_owned()),
            ("c.rs".to_owned(), 4, "fn three() { target() }".to_owned()),
        ]
    );
    // Not indexed again: the lines are read from the files as they are now.
    write(&root, "b.rs", "fn one() {\n    // edited\n}\n");
    write(&root, "c.rs", "fn two() {}\n");
    assert_eq!(
        texts(&db),
        [
            ("b.rs".to_owned(), 2, "// edited".to_owned()),
            ("c.rs".to_owned(), 2, String::new()),
            ("c.rs".to_owned(), 4, String::new()),
        ]
    );
}

#[test]
fn symbols_are_limited() {
    let defs_src: String = (0..8).map(|i| format!("fn twin() {{}} // {i}\n")).collect();
    let uses_src: String = (0..8).map(|_| "fn user() { twin(); }\n").collect();
    let (_dir, root) = workspace(&[("a.rs", &defs_src), ("b.rs", &uses_src)]);
    let db = Db::in_memory().unwrap();
    project::index(&db, &root, None).unwrap();
    let ask = |limit, references| {
        let req = SymbolsRequest { workspace: String::new(), name: "twin".into(), references, limit };
        project::symbols(&db, &root, &req).unwrap()
    };
    let all = ask(None, true);
    assert_eq!((all.definitions.len(), all.references.len(), all.truncated), (8, 8, false));
    let some = ask(Some(3), true);
    assert_eq!((some.definitions.len(), some.references.len(), some.truncated), (3, 3, true));
    assert_eq!(some.definitions.iter().map(|d| d.line).collect::<Vec<_>>(), [1, 2, 3]);
    let exact = ask(Some(8), true);
    assert!(!exact.truncated);
    let no_refs = ask(Some(0), false);
    assert_eq!((no_refs.definitions.len(), no_refs.references.len(), no_refs.truncated), (1, 0, true));
}

#[test]
fn an_empty_name_is_refused() {
    let (_dir, root) = workspace(&[]);
    let db = Db::in_memory().unwrap();
    for name in ["", "  \t"] {
        let req = SymbolsRequest { workspace: String::new(), name: name.into(), references: true, limit: None };
        assert_eq!(project::symbols(&db, &root, &req).unwrap_err().code, ErrorCode::Invalid);
    }
}

/// Index and map this repository's own crates, and print timings and a
/// sample map. Run with
/// `cargo test -p molt-memory --release --test project -- --ignored --nocapture`.
#[test]
#[ignore]
fn this_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").canonicalize().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("memory.sqlite")).unwrap();
    let t = Instant::now();
    let cold = project::index(&db, &root, None).unwrap();
    println!("cold index: {cold:?} ({:?})", t.elapsed());
    let t = Instant::now();
    let warm = project::index(&db, &root, None).unwrap();
    println!("warm index: {warm:?} ({:?})", t.elapsed());
    let query = "make the kernel's audit log readable by trace id";
    let t = Instant::now();
    let m = map(&db, &root, query, None);
    println!("map: {} files, {} symbols, {} tokens ({:?})", m.files, m.symbols, m.tokens, t.elapsed());
    println!("{}", m.map.lines().take(40).collect::<Vec<_>>().join("\n"));
    let t = Instant::now();
    let plain = map(&db, &root, "", None);
    println!("map without a query: {} files, {} symbols ({:?})", plain.files, plain.symbols, t.elapsed());
    let t = Instant::now();
    let found = lookup(&db, &root, "Envelope");
    println!(
        "symbols Envelope: {} definitions, {} references ({:?})",
        found.definitions.len(),
        found.references.len(),
        t.elapsed()
    );
}

/// Index and map a generated workspace of a few thousand files that use one
/// another, and print timings. Run like [`this_repository`].
#[test]
#[ignore]
fn a_large_generated_workspace() {
    const FILES: usize = 3000;
    let (_dir, root) = workspace(&[]);
    // A fixed linear congruential sequence, so every run builds the same tree.
    let mut seed: u64 = 7;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize % n
    };
    for i in 0..FILES {
        let mut src = format!("pub struct Type{i} {{\n    inner: Vec<u8>,\n}}\n\nimpl Type{i} {{\n");
        src.push_str("    pub fn new() -> Self {\n        Self { inner: Vec::new() }\n    }\n\n");
        for f in 0..5 {
            src.push_str(&format!("    pub fn method_{i}_{f}(&self, other: &Type{}) -> usize {{\n", next(FILES)));
            for _ in 0..4 {
                let j = next(FILES);
                src.push_str(&format!("        let x = Type{j}::new();\n        x.method_{j}_{}(&x);\n", next(5)));
            }
            src.push_str("        self.inner.len()\n    }\n\n");
        }
        src.push_str("}\n");
        write(&root, &format!("src/m{}/file{i}.rs", i % 30), &src);
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("memory.sqlite")).unwrap();
    let t = Instant::now();
    let cold = project::index(&db, &root, None).unwrap();
    println!("cold index: {cold:?} ({:?})", t.elapsed());
    let t = Instant::now();
    let warm = project::index(&db, &root, None).unwrap();
    println!("warm index: {warm:?} ({:?})", t.elapsed());
    write(&root, "src/m0/file0.rs", "pub struct Type0;\n");
    let t = Instant::now();
    let one = project::index(&db, &root, Some(&["src/m0/file0.rs".to_owned()])).unwrap();
    println!("one file by path: {one:?} ({:?})", t.elapsed());
    for query in ["", "why does method_42_3 call Type7 twice"] {
        let t = Instant::now();
        let m = map(&db, &root, query, None);
        println!("map {query:?}: {} files, {} symbols, {} tokens ({:?})", m.files, m.symbols, m.tokens, t.elapsed());
        println!("{}", m.map.lines().take(12).collect::<Vec<_>>().join("\n"));
    }
    let t = Instant::now();
    let found = lookup(&db, &root, "new");
    println!(
        "symbols new: {} definitions, {} references ({:?})",
        found.definitions.len(),
        found.references.len(),
        t.elapsed()
    );
}

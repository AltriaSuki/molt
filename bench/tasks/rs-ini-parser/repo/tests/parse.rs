use iniconf::{parse, Ini};

fn ini(src: &str) -> Ini {
    match parse(src) {
        Ok(ini) => ini,
        Err(err) => panic!("failed to parse {src:?}: {err}"),
    }
}

#[test]
fn reads_sections_and_entries() {
    let ini = ini("[server]\nhost=localhost\nport=8080\n\n[log]\nlevel=info\n");
    assert_eq!(ini.get("server", "host"), Some("localhost"));
    assert_eq!(ini.get("server", "port"), Some("8080"));
    assert_eq!(ini.get("log", "level"), Some("info"));
}

#[test]
fn entries_before_any_header_are_in_the_unnamed_section() {
    let ini = ini("name=billing\n[server]\nport=1\n");
    assert_eq!(ini.get("", "name"), Some("billing"));
    assert_eq!(ini.get("server", "name"), None);
    assert_eq!(ini.get("", "port"), None);
}

#[test]
fn whitespace_around_keys_and_values_is_trimmed() {
    let ini = ini("  [ db ]  \n   user   =   app owner   \n\tpass\t=\tsecret\t\n");
    assert_eq!(ini.get("db", "user"), Some("app owner"));
    assert_eq!(ini.get("db", "pass"), Some("secret"));
}

#[test]
fn value_may_contain_equals_signs() {
    let ini = ini("[http]\nurl = https://example.com/?a=1&b=2\n");
    assert_eq!(ini.get("http", "url"), Some("https://example.com/?a=1&b=2"));
}

#[test]
fn missing_lookups_are_none() {
    let ini = ini("[a]\nx = 1\n");
    assert_eq!(ini.get("a", "y"), None);
    assert_eq!(ini.get("b", "x"), None);
    assert_eq!(ini.get("", "x"), None);
}

#[test]
fn has_section_reports_headers() {
    let ini = ini("[a]\nx = 1\n[empty]\n");
    assert!(ini.has_section("a"));
    assert!(ini.has_section("empty"));
    assert!(!ini.has_section("b"));
}

#[test]
fn empty_values_are_allowed() {
    let ini = ini("[a]\nx =\ny = \n");
    assert_eq!(ini.get("a", "x"), Some(""));
    assert_eq!(ini.get("a", "y"), Some(""));
}

#[test]
fn empty_input_has_nothing() {
    let ini = ini("");
    assert_eq!(ini.get("", "x"), None);
    assert!(!ini.has_section("x"));
}

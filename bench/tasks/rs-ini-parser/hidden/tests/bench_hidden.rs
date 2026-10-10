//! Comments, quoting, case, ordering, errors and `Ini`'s accessors.

use iniconf::config::LogLevel;
use iniconf::{parse, Config, ConfigError, ErrorKind, Ini, ParseError};

fn ini(src: &str) -> Ini {
    match parse(src) {
        Ok(ini) => ini,
        Err(err) => panic!("failed to parse {src:?}: {err:?}"),
    }
}

fn error(src: &str) -> ParseError {
    match parse(src) {
        Ok(_) => panic!("expected {src:?} to be rejected"),
        Err(err) => err,
    }
}

/// (source, section, key, expected value)
fn check_values(cases: &[(&str, &str, &str, &str)]) {
    for &(src, section, key, expected) in cases {
        assert_eq!(
            ini(src).get(section, key),
            Some(expected),
            "value of [{section}] {key} in {src:?}"
        );
    }
}

/// (source, line, kind)
fn check_errors(cases: &[(&str, usize, ErrorKind)]) {
    for (src, line, kind) in cases {
        assert_eq!(
            parse(src).map(|_| ()),
            Err(ParseError {
                line: *line,
                kind: kind.clone()
            }),
            "error for {src:?}"
        );
    }
}

// ---- lines and comments ----

#[test]
fn comment_lines_are_skipped_even_when_they_contain_equals() {
    let ini = ini("; top comment\n# x = 1\n   ; y = 2\n\t# z=3\n\n[s]\n# a = 1\n; b = 2\nc = 3\n");
    assert_eq!(ini.sections(), vec!["s"]);
    assert_eq!(ini.keys("s"), vec!["c"]);
    assert_eq!(ini.keys(""), Vec::<&str>::new());
    assert_eq!(ini.get("", "# x"), None);
    assert_eq!(ini.get("s", "c"), Some("3"));
}

#[test]
fn crlf_line_endings() {
    let ini = ini("top = 1\r\n\r\n[S]\r\na = x ; c\r\nb = \"q\"\r\n");
    assert_eq!(ini.get("", "top"), Some("1"));
    assert_eq!(ini.get("s", "a"), Some("x"));
    assert_eq!(ini.get("s", "b"), Some("q"));
    assert_eq!(
        parse("a = 1\r\n\r\n[x\r\n").map(|_| ()),
        Err(ParseError {
            line: 3,
            kind: ErrorKind::BadSection
        })
    );
}

#[test]
fn input_with_only_comments_has_no_sections() {
    assert_eq!(ini("").sections(), Vec::<&str>::new());
    assert_eq!(
        ini("\n\n   \n; nothing\n# here\n").sections(),
        Vec::<&str>::new()
    );
}

// ---- unquoted values ----

#[test]
fn unquoted_values_and_inline_comments() {
    check_values(&[
        ("a = 1", "", "a", "1"),
        ("  a   =   hello   world  ", "", "a", "hello   world"),
        ("port = 80 ; http", "", "port", "80"),
        ("port = 80 # http", "", "port", "80"),
        ("port = 80\t; http", "", "port", "80"),
        ("port = 80\t# http", "", "port", "80"),
        ("port = 80     ;; double", "", "port", "80"),
        ("port=80 ;c", "", "port", "80"),
        ("a = one two ; three ; four", "", "a", "one two"),
        ("a = x # y ; z", "", "a", "x"),
        ("a = ; nothing here", "", "a", ""),
        ("a = # nothing here", "", "a", ""),
        ("a =", "", "a", ""),
    ]);
}

#[test]
fn comment_markers_without_whitespace_before_them_are_kept() {
    check_values(&[
        ("color=#fff", "", "color", "#fff"),
        ("a = x;y", "", "a", "x;y"),
        ("a = x#y", "", "a", "x#y"),
        ("a =;x", "", "a", ";x"),
        ("a =#x", "", "a", "#x"),
        ("tag = v1.2#rc;3", "", "tag", "v1.2#rc;3"),
        ("a = x;y #z", "", "a", "x;y"),
    ]);
}

#[test]
fn unquoted_values_are_literal() {
    check_values(&[
        ("a = b = c", "", "a", "b = c"),
        ("url = http://h/?q=1&r=2", "", "url", "http://h/?q=1&r=2"),
        (r"path = C:\temp\new", "", "path", r"C:\temp\new"),
        (r"re = \d+\n", "", "re", r"\d+\n"),
        (r#"say = he said "hi""#, "", "say", r#"he said "hi""#),
        (r#"a = x "b ; c""#, "", "a", r#"x "b"#),
        (r#"a = x""#, "", "a", r#"x""#),
        ("name = Zoë Ångström", "", "name", "Zoë Ångström"),
    ]);
}

// ---- quoted values ----

#[test]
fn quoted_values() {
    check_values(&[
        (r#"q = "hello""#, "", "q", "hello"),
        (r#"q="tight""#, "", "q", "tight"),
        (r#"q =    "lead""#, "", "q", "lead"),
        (r#"q = """#, "", "q", ""),
        (r#"q = "  padded  ""#, "", "q", "  padded  "),
        (r#"q = "a ; b # c""#, "", "q", "a ; b # c"),
        (r##"q = "#not a comment""##, "", "q", "#not a comment"),
        (r#"q = "MiXeD Case""#, "", "q", "MiXeD Case"),
        (r#"q = "x = y""#, "", "q", "x = y"),
        (r#"q = "unicode é ✓""#, "", "q", "unicode é ✓"),
    ]);
}

#[test]
fn quoted_values_with_escapes() {
    check_values(&[
        (r#"q = "say \"hi\"""#, "", "q", "say \"hi\""),
        (r#"q = "back\\slash""#, "", "q", "back\\slash"),
        (r#"q = "line1\nline2""#, "", "q", "line1\nline2"),
        (r#"q = "col1\tcol2""#, "", "q", "col1\tcol2"),
        (r#"q = "ends with \\""#, "", "q", "ends with \\"),
        (r#"q = "\\\\server\\share""#, "", "q", "\\\\server\\share"),
        (r#"q = "\"""#, "", "q", "\""),
        (
            r#"q = "\\n is not a newline""#,
            "",
            "q",
            "\\n is not a newline",
        ),
        (r#"q = "a\\" ; c"#, "", "q", "a\\"),
    ]);
}

#[test]
fn comments_and_whitespace_after_the_closing_quote() {
    check_values(&[
        (r#"q = "a" ; comment"#, "", "q", "a"),
        (r#"q = "a";comment"#, "", "q", "a"),
        (r#"q = "a"   # comment "with" quotes"#, "", "q", "a"),
        (r##"q = "a"#comment"##, "", "q", "a"),
        ("q = \"a\"   \t", "", "q", "a"),
    ]);
}

// ---- sections ----

#[test]
fn section_headers() {
    check_values(&[
        ("[s]\nk = v", "s", "k", "v"),
        ("[ My Section ]\nk = v", "my section", "k", "v"),
        ("[a] ; comment\nk = 1", "a", "k", "1"),
        ("[a] # comment\nk = 1", "a", "k", "1"),
        ("[a];comment\nk = 1", "a", "k", "1"),
        ("[a]#comment\nk = 1", "a", "k", "1"),
        ("[a]   \t\nk = 1", "a", "k", "1"),
        ("a[0] = 1", "", "a[0]", "1"),
        ("my key = v", "", "my key", "v"),
    ]);
}

#[test]
fn section_name_ends_at_the_first_bracket() {
    let ini = ini("[a] ; [b]\nk = 1\n");
    assert_eq!(ini.sections(), vec!["a"]);
    assert_eq!(ini.get("a", "k"), Some("1"));
}

#[test]
fn repeated_header_continues_the_section() {
    let ini = ini("[a]\nx = 1\n[b]\ny = 2\n[a]\nz = 3\n");
    assert_eq!(ini.sections(), vec!["a", "b"]);
    assert_eq!(ini.keys("a"), vec!["x", "z"]);
    assert_eq!(ini.keys("b"), vec!["y"]);
    assert_eq!(ini.get("a", "x"), Some("1"));
    assert_eq!(ini.get("a", "z"), Some("3"));
}

#[test]
fn repeated_header_in_another_case_is_the_same_section() {
    let ini = ini("[Net]\nx = 1\n[other]\n[NET]\ny = 2\n[net]\n");
    assert_eq!(ini.sections(), vec!["net", "other"]);
    assert_eq!(ini.keys("net"), vec!["x", "y"]);
}

#[test]
fn sections_keep_their_order_of_first_appearance() {
    let src =
        "[zeta]\n[alpha]\nk=1\n[Mid]\n[beta]\n[omega]\n[gamma]\n[delta]\n[alpha]\nj=2\n[epsilon]\n";
    let ini = ini(src);
    assert_eq!(
        ini.sections(),
        vec!["zeta", "alpha", "mid", "beta", "omega", "gamma", "delta", "epsilon"]
    );
}

#[test]
fn unnamed_section_is_listed_first_only_when_it_has_keys() {
    let with = ini("top = 1\nOther = 2\n[b]\nk = v\n[a]\n");
    assert_eq!(with.sections(), vec!["", "b", "a"]);
    assert_eq!(with.keys(""), vec!["top", "other"]);
    assert!(with.has_section(""));

    let without = ini("; comment\n[b]\nk = v\n");
    assert_eq!(without.sections(), vec!["b"]);
    assert!(!without.has_section(""));
}

#[test]
fn empty_sections_are_listed() {
    let ini = ini("[empty]\n[full]\nk = v\n[also empty]\n");
    assert_eq!(ini.sections(), vec!["empty", "full", "also empty"]);
    assert_eq!(ini.keys("empty"), Vec::<&str>::new());
    assert!(ini.has_section("empty"));
    assert!(ini.has_section("Also Empty"));
}

// ---- keys and case ----

#[test]
fn keys_keep_their_order_and_the_last_value_wins() {
    let ini = ini("[s]\nzeta = 1\nalpha = 2\nMid = 3\nalpha = 4\nbeta = 5\nZETA = 6\n");
    assert_eq!(ini.keys("s"), vec!["zeta", "alpha", "mid", "beta"]);
    assert_eq!(ini.get("s", "alpha"), Some("4"));
    assert_eq!(ini.get("s", "zeta"), Some("6"));
    assert_eq!(ini.get("s", "mid"), Some("3"));
}

#[test]
fn duplicate_keys_across_repeated_headers() {
    let ini = ini("[s]\na = 1\nb = 2\n[t]\na = x\n[S]\na = 3\nc = 4\n");
    assert_eq!(ini.keys("s"), vec!["a", "b", "c"]);
    assert_eq!(ini.get("s", "a"), Some("3"));
    assert_eq!(ini.get("t", "a"), Some("x"));
}

#[test]
fn names_are_case_insensitive_and_values_keep_their_case() {
    let ini = ini("Name = MyApp\n[Server]\nHost = LocalHost\nPORT = 8080\n");
    assert_eq!(ini.sections(), vec!["", "server"]);
    assert_eq!(ini.keys("server"), vec!["host", "port"]);
    assert_eq!(ini.keys("SERVER"), vec!["host", "port"]);
    assert_eq!(ini.keys(""), vec!["name"]);
    for (section, key) in [("server", "host"), ("SERVER", "HOST"), ("Server", "hOsT")] {
        assert_eq!(
            ini.get(section, key),
            Some("LocalHost"),
            "[{section}] {key}"
        );
    }
    assert_eq!(ini.get("", "NAME"), Some("MyApp"));
    assert_eq!(ini.get("server", "Port"), Some("8080"));
    assert!(ini.has_section("SERVER"));
    assert!(!ini.has_section("client"));
}

#[test]
fn missing_lookups() {
    let ini = ini("[a]\nx = 1\n");
    assert_eq!(ini.get("a", "y"), None);
    assert_eq!(ini.get("b", "x"), None);
    assert_eq!(ini.get("", "x"), None);
    assert_eq!(ini.keys("b"), Vec::<&str>::new());
    assert_eq!(ini.keys(""), Vec::<&str>::new());
}

// ---- get_as ----

#[test]
fn get_as_parses_values() {
    let ini = ini(
        "[Server]\nPort = 8080\noffset = -5\nratio = 0.25\nenabled = true\nname = \"edge 1\"\nbig = 300\nword = abc\n",
    );
    assert_eq!(ini.get_as::<u16>("server", "port"), Some(Ok(8080)));
    assert_eq!(ini.get_as::<u16>("SERVER", "PORT"), Some(Ok(8080)));
    assert_eq!(ini.get_as::<i64>("server", "offset"), Some(Ok(-5)));
    assert_eq!(ini.get_as::<f64>("server", "ratio"), Some(Ok(0.25)));
    assert_eq!(ini.get_as::<bool>("server", "enabled"), Some(Ok(true)));
    assert_eq!(
        ini.get_as::<String>("server", "name"),
        Some(Ok("edge 1".to_string()))
    );
    assert!(matches!(ini.get_as::<u8>("server", "big"), Some(Err(_))));
    assert!(matches!(ini.get_as::<u16>("server", "word"), Some(Err(_))));
    assert!(matches!(ini.get_as::<bool>("server", "word"), Some(Err(_))));
    assert!(ini.get_as::<u16>("server", "missing").is_none());
    assert!(ini.get_as::<u16>("client", "port").is_none());
}

#[test]
fn get_as_error_is_the_from_str_error() {
    let ini = ini("n = x\n");
    let err: std::num::ParseIntError = ini.get_as::<i32>("", "n").unwrap().unwrap_err();
    assert_eq!(err, "x".parse::<i32>().unwrap_err());
}

// ---- errors ----

#[test]
fn bad_section_headers() {
    check_errors(&[
        ("[a", 1, ErrorKind::BadSection),
        ("\n\n[a b", 3, ErrorKind::BadSection),
        ("[]", 1, ErrorKind::BadSection),
        ("[   ]", 1, ErrorKind::BadSection),
        ("[] ; empty", 1, ErrorKind::BadSection),
        ("[a] x", 1, ErrorKind::BadSection),
        ("[a]x", 1, ErrorKind::BadSection),
        ("[a]]", 1, ErrorKind::BadSection),
        ("[a] = 1", 1, ErrorKind::BadSection),
        ("[a] [b]", 1, ErrorKind::BadSection),
        ("ok = 1\n[s]\n  [t] u  \n", 3, ErrorKind::BadSection),
    ]);
}

#[test]
fn missing_equals() {
    check_errors(&[
        ("key", 1, ErrorKind::MissingEquals),
        ("[s]\njust some words", 2, ErrorKind::MissingEquals),
        ("[s]\nkey ; comment", 2, ErrorKind::MissingEquals),
        ("a]", 1, ErrorKind::MissingEquals),
        ("a = 1\nb = 2\n\n  c  \n", 4, ErrorKind::MissingEquals),
    ]);
}

#[test]
fn empty_keys() {
    check_errors(&[
        ("= 1", 1, ErrorKind::EmptyKey),
        ("[s]\n   =   ", 2, ErrorKind::EmptyKey),
        ("[s]\na = 1\n=", 3, ErrorKind::EmptyKey),
        ("\t= \"quoted\"", 1, ErrorKind::EmptyKey),
    ]);
}

#[test]
fn bad_escapes() {
    check_errors(&[
        (r#"x = "a\qb""#, 1, ErrorKind::BadEscape('q')),
        (r#"x = "\0""#, 1, ErrorKind::BadEscape('0')),
        (r#"x = "it\'s""#, 1, ErrorKind::BadEscape('\'')),
        (r#"x = "\ ""#, 1, ErrorKind::BadEscape(' ')),
        ("[s]\n\nx = \"tab\\T\"", 3, ErrorKind::BadEscape('T')),
        (r#"x = "caf\é""#, 1, ErrorKind::BadEscape('é')),
    ]);
}

#[test]
fn unterminated_quotes() {
    check_errors(&[
        (r#"x = "abc"#, 1, ErrorKind::UnterminatedQuote),
        (r#"x = ""#, 1, ErrorKind::UnterminatedQuote),
        (r#"x = "abc\""#, 1, ErrorKind::UnterminatedQuote),
        (r#"x = "abc\"#, 1, ErrorKind::UnterminatedQuote),
        (r#"x = "a ; b"#, 1, ErrorKind::UnterminatedQuote),
        ("[s]\nx = \"spans\nlines\"", 2, ErrorKind::UnterminatedQuote),
    ]);
}

#[test]
fn text_after_the_closing_quote() {
    check_errors(&[
        (r#"x = "a" b"#, 1, ErrorKind::TrailingAfterQuote),
        (r#"x = "a"b"#, 1, ErrorKind::TrailingAfterQuote),
        (r#"x = "a""b""#, 1, ErrorKind::TrailingAfterQuote),
        (r#"x = "a" "b""#, 1, ErrorKind::TrailingAfterQuote),
        (r#"x = "a\\"b""#, 1, ErrorKind::TrailingAfterQuote),
        (
            "[s]\nok = 1\nx = \"a\" = b",
            3,
            ErrorKind::TrailingAfterQuote,
        ),
    ]);
}

#[test]
fn line_numbers_count_blank_and_comment_lines() {
    let err = error("; header\n\n[s]\n# note\nok = 1\n\nbroken\n");
    assert_eq!(err.line, 7);
    assert_eq!(err.kind, ErrorKind::MissingEquals);
}

#[test]
fn parsing_stops_at_the_first_bad_line() {
    check_errors(&[
        ("a = 1\n[bad\nnoequals\n= 1", 2, ErrorKind::BadSection),
        ("a = 1\nnoequals\n[bad", 2, ErrorKind::MissingEquals),
        (
            "ok = \"fine\"\nx = \"open\n[bad",
            2,
            ErrorKind::UnterminatedQuote,
        ),
    ]);
}

// ---- Display and std::error::Error ----

#[test]
fn errors_display_their_line_and_description() {
    let cases = [
        ("[a", "line 1: malformed section header"),
        ("[s]\nkey", "line 2: missing '=' in entry"),
        ("\n\n= 1", "line 3: empty key"),
        ("x = \"a\\qb\"", "line 1: unknown escape '\\q'"),
        ("[s]\nx = \"open", "line 2: unterminated quoted value"),
        ("x = \"a\" b", "line 1: unexpected text after closing quote"),
    ];
    for (src, expected) in cases {
        assert_eq!(error(src).to_string(), expected, "message for {src:?}");
    }
}

#[test]
fn display_of_hand_built_errors() {
    let err = ParseError {
        line: 12,
        kind: ErrorKind::BadEscape('x'),
    };
    assert_eq!(err.to_string(), "line 12: unknown escape '\\x'");
    let err = ParseError {
        line: 100,
        kind: ErrorKind::EmptyKey,
    };
    assert_eq!(format!("{err}"), "line 100: empty key");
}

fn load_port(src: &str) -> Result<u16, Box<dyn std::error::Error>> {
    let ini = parse(src)?;
    Ok(ini.get_as::<u16>("server", "port").unwrap_or(Ok(80))?)
}

#[test]
fn parse_error_is_a_std_error() {
    let err = error("[s]\nnope");
    let dyn_err: &dyn std::error::Error = &err;
    assert_eq!(dyn_err.to_string(), "line 2: missing '=' in entry");

    let boxed = load_port("[server]\nport 8080").unwrap_err();
    assert_eq!(boxed.to_string(), "line 2: missing '=' in entry");
    assert_eq!(load_port("[Server]\nPort = 9090 ; tls").unwrap(), 9090);
}

#[test]
fn errors_are_comparable_and_cloneable() {
    let err = error("[s]\nx = \"\\z\"");
    let copy = err.clone();
    assert_eq!(err, copy);
    assert_eq!(
        copy,
        ParseError {
            line: 2,
            kind: ErrorKind::BadEscape('z')
        }
    );
    assert_ne!(ErrorKind::BadEscape('a'), ErrorKind::BadEscape('b'));
}

// ---- the config module on top ----

#[test]
fn config_reads_a_commented_and_quoted_file() {
    let src = "\
# billing service
Name = \"billing\" ; service name

[Server]
Host = 0.0.0.0   ; all interfaces
PORT = 9090
keep_alive = off # behind a proxy

[log]
level = DEBUG
file = \"/var/log/billing #1.log\"
";
    let config: Config = src.parse().unwrap();
    assert_eq!(config.name, "billing");
    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 9090);
    assert_eq!(config.server.workers, 4);
    assert!(!config.server.keep_alive);
    assert_eq!(config.log.level, LogLevel::Debug);
    assert_eq!(config.log.file.as_deref(), Some("/var/log/billing #1.log"));
}

#[test]
fn config_reports_parse_errors() {
    assert_eq!(
        "name = x\n[server]\nport\n".parse::<Config>(),
        Err(ConfigError::Parse(ParseError {
            line: 3,
            kind: ErrorKind::MissingEquals
        }))
    );
    let err = "name = x\n[server]\nport = 70000 ; too big\n"
        .parse::<Config>()
        .unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { ref key, ref value, .. } if key == "port" && value == "70000"),
        "{err:?}"
    );
}

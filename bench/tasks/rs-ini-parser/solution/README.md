# iniconf

Reads INI-style configuration files. `iniconf::parse` turns the text of a
file into an `Ini` (named sections of `key = value` entries), and
`iniconf::config` reads the typed settings our services start with out of
one.

```rust
let ini = iniconf::parse("name = billing\n[server]\nport = 9090\n")?;
assert_eq!(ini.get("", "name"), Some("billing"));
assert_eq!(ini.get("server", "port"), Some("9090"));

let config: iniconf::Config = std::fs::read_to_string("billing.ini")?.parse()?;
println!("{} listens on {}:{}", config.name, config.server.host, config.server.port);
```

## File format

```ini
; billing service
name = billing

[server]
host = 0.0.0.0     ; all interfaces
port = 9090
banner = "Welcome!\nPlease log in."
```

- Lines are trimmed. Empty lines and lines starting with `;` or `#` are
  comments.
- `[name]` starts a section; only whitespace and a comment may follow the
  `]`. Entries before the first header belong to the unnamed section `""`.
  Repeating a header continues that section.
- `key = value` sets a key in the current section, split at the first `=`;
  the whitespace around the key and the value is dropped. Setting a key
  again replaces its value.
- In an unquoted value, a `;` or `#` with whitespace right before it starts
  a comment (`color=#fff` keeps its `#`). Everything else is literal.
- A value starting with `"` runs to the closing quote and may contain `;`,
  `#` and the escapes `\"`, `\\`, `\n`, `\t`. Only whitespace and a comment
  may follow the closing quote.
- Section names and keys are case-insensitive (stored lowercased); values
  keep their case.

`parse` stops at the first bad line with a `ParseError { line, kind }`, whose
`kind` is one of `BadSection`, `MissingEquals`, `EmptyKey`,
`BadEscape(char)`, `UnterminatedQuote` and `TrailingAfterQuote`, and which
displays as e.g. `line 3: missing '=' in entry`.

`Ini` lookups: `get(section, key)`, `get_as::<T>(section, key)` (parses with
`FromStr`), `sections()` and `keys(section)` (in order of first appearance)
and `has_section(name)`.

## Service settings

`Config::from_ini(&ini)`, or `src.parse::<Config>()` straight from the text,
reads:

| setting             | type                                        | default     |
|---------------------|---------------------------------------------|-------------|
| `name`              | string, required                            |             |
| `server.host`       | string                                      | `127.0.0.1` |
| `server.port`       | `u16`                                       | `8080`      |
| `server.workers`    | `usize`, at least 1                         | `4`         |
| `server.keep_alive` | `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0` | `true`  |
| `log.level`         | `error`, `warn`, `info`, `debug`, `trace`   | `info`      |
| `log.file`          | path; empty or absent means stderr          | none        |

Failures are `ConfigError`s: `Parse`, `Missing { section, key }` or
`Invalid { section, key, value, expected }`.

## Layout

- `src/parser.rs`: `parse`, text to `Ini`.
- `src/ini.rs`: `Ini` and its lookups.
- `src/error.rs`: `ParseError`.
- `src/config.rs`: `Config` and friends.

## Tests

Rust 1.80 or newer, no dependencies:

```
cargo test --offline --lib --test parse --test config
```

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
name = billing

[server]
host = 0.0.0.0
port = 9090
```

- `[name]` starts a section. Entries before the first header belong to the
  unnamed section `""`.
- `key = value` sets a key in the current section; the whitespace around the
  key and the value is dropped.

Anything else on a line is currently ignored.

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

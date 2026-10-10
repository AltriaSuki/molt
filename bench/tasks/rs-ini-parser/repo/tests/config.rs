use iniconf::config::{LogConfig, LogLevel, ServerConfig};
use iniconf::{parse, Config, ConfigError};

const FULL: &str = "\
name = billing

[server]
host = 0.0.0.0
port = 9090
workers = 8
keep_alive = no

[log]
level = Debug
file = /var/log/billing.log
";

fn load(src: &str) -> Result<Config, ConfigError> {
    src.parse()
}

#[test]
fn reads_every_setting() {
    let config = load(FULL).unwrap();
    assert_eq!(
        config,
        Config {
            name: "billing".to_string(),
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 9090,
                workers: 8,
                keep_alive: false,
            },
            log: LogConfig {
                level: LogLevel::Debug,
                file: Some("/var/log/billing.log".to_string()),
            },
        }
    );
}

#[test]
fn from_ini_matches_from_str() {
    let ini = parse(FULL).unwrap();
    assert_eq!(Config::from_ini(&ini), load(FULL));
}

#[test]
fn everything_but_the_name_has_a_default() {
    let config = load("name = tiny\n").unwrap();
    assert_eq!(config.name, "tiny");
    assert_eq!(config.server, ServerConfig::default());
    assert_eq!(config.server.port, 8080);
    assert_eq!(config.server.workers, 4);
    assert!(config.server.keep_alive);
    assert_eq!(config.log.level, LogLevel::Info);
    assert_eq!(config.log.file, None);
}

#[test]
fn name_is_required() {
    let missing = ConfigError::Missing {
        section: String::new(),
        key: "name".to_string(),
    };
    assert_eq!(load("[server]\nport = 1\n"), Err(missing.clone()));
    assert_eq!(load("name =\n"), Err(missing.clone()));
    assert_eq!(missing.to_string(), "missing required setting `name`");
}

#[test]
fn bad_numbers_are_reported_with_their_setting() {
    let err = load("name = x\n[server]\nport = 70000\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::Invalid {
            section: "server".to_string(),
            key: "port".to_string(),
            value: "70000".to_string(),
            expected: "a port number from 0 to 65535",
        }
    );
    assert_eq!(
        err.to_string(),
        "`server.port` is \"70000\", expected a port number from 0 to 65535"
    );
}

#[test]
fn workers_must_be_positive() {
    let err = load("name = x\n[server]\nworkers = 0\n").unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { ref key, .. } if key == "workers"),
        "{err:?}"
    );
}

#[test]
fn keep_alive_takes_the_usual_booleans() {
    for (raw, expected) in [("on", true), ("off", false), ("TRUE", true), ("0", false)] {
        let config = load(&format!("name = x\n[server]\nkeep_alive = {raw}\n")).unwrap();
        assert_eq!(config.server.keep_alive, expected, "keep_alive = {raw}");
    }
    let err = load("name = x\n[server]\nkeep_alive = maybe\n").unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { ref value, .. } if value == "maybe"),
        "{err:?}"
    );
}

#[test]
fn unknown_log_levels_are_rejected() {
    let err = load("name = x\n[log]\nlevel = loud\n").unwrap_err();
    assert!(
        matches!(err, ConfigError::Invalid { ref section, ref key, .. } if section == "log" && key == "level"),
        "{err:?}"
    );
}

#[test]
fn empty_log_file_means_stderr() {
    let config = load("name = x\n[log]\nfile =\n").unwrap();
    assert_eq!(config.log.file, None);
}

#[test]
fn log_levels_are_ordered_quietest_first() {
    assert!(LogLevel::Error < LogLevel::Warn);
    assert!(LogLevel::Debug < LogLevel::Trace);
    assert_eq!(LogLevel::from_name("WARN"), Some(LogLevel::Warn));
    assert_eq!(LogLevel::from_name("verbose"), None);
}

//! The typed settings a service reads from its INI file.
//!
//! ```ini
//! name = billing
//!
//! [server]
//! host = 0.0.0.0
//! port = 9090
//! workers = 8
//! keep_alive = yes
//!
//! [log]
//! level = debug
//! file = /var/log/billing.log
//! ```
//!
//! Only `name` is required; everything else has a default (see
//! [`ServerConfig::default`] and [`LogConfig::default`]).

use std::fmt;
use std::str::FromStr;

use crate::{parse, Ini, ParseError};

/// Everything a service needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The service name, from the top-level `name` key.
    pub name: String,
    pub server: ServerConfig,
    pub log: LogConfig,
}

/// The `[server]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Number of worker threads, at least 1.
    pub workers: usize,
    pub keep_alive: bool,
}

impl Default for ServerConfig {
    fn default() -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            workers: 4,
            keep_alive: true,
        }
    }
}

/// The `[log]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogConfig {
    pub level: LogLevel,
    /// Where to write the log; `None` (no `file` key, or an empty one) means stderr.
    pub file: Option<String>,
}

/// How much to log, quietest first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    const ALL: [LogLevel; 5] = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Trace,
    ];

    /// The level's name as written in config files.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }

    /// Looks a level up by name, ignoring ASCII case.
    pub fn from_name(name: &str) -> Option<LogLevel> {
        LogLevel::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(name))
    }
}

/// Why a [`Config`] could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file is not valid INI.
    Parse(ParseError),
    /// A required key is absent.
    Missing { section: String, key: String },
    /// A key is present but its value is not usable.
    Invalid {
        section: String,
        key: String,
        value: String,
        expected: &'static str,
    },
}

impl ConfigError {
    fn missing(section: &str, key: &str) -> ConfigError {
        ConfigError::Missing {
            section: section.to_string(),
            key: key.to_string(),
        }
    }

    fn invalid(section: &str, key: &str, value: &str, expected: &'static str) -> ConfigError {
        ConfigError::Invalid {
            section: section.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            expected,
        }
    }
}

/// `server.port`, or just `name` for a top-level key.
fn setting_name(section: &str, key: &str) -> String {
    if section.is_empty() {
        key.to_string()
    } else {
        format!("{section}.{key}")
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Parse(err) => write!(f, "malformed config file: {err}"),
            ConfigError::Missing { section, key } => {
                write!(
                    f,
                    "missing required setting `{}`",
                    setting_name(section, key)
                )
            }
            ConfigError::Invalid {
                section,
                key,
                value,
                expected,
            } => write!(
                f,
                "`{}` is {value:?}, expected {expected}",
                setting_name(section, key)
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ParseError> for ConfigError {
    fn from(err: ParseError) -> ConfigError {
        ConfigError::Parse(err)
    }
}

const PORT: &str = "a port number from 0 to 65535";
const WORKERS: &str = "a whole number of at least 1";
const BOOL: &str = "one of true, false, yes, no, on, off, 1, 0";
const LEVEL: &str = "one of error, warn, info, debug, trace";

impl Config {
    /// Reads the settings out of a parsed file.
    pub fn from_ini(ini: &Ini) -> Result<Config, ConfigError> {
        let name = ini
            .get("", "name")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ConfigError::missing("", "name"))?
            .to_string();

        let defaults = ServerConfig::default();
        let server = ServerConfig {
            host: ini
                .get("server", "host")
                .unwrap_or(&defaults.host)
                .to_string(),
            port: read(ini, "server", "port", PORT)?.unwrap_or(defaults.port),
            workers: read(ini, "server", "workers", WORKERS)?.unwrap_or(defaults.workers),
            keep_alive: read_bool(ini, "server", "keep_alive")?.unwrap_or(defaults.keep_alive),
        };
        if server.workers == 0 {
            return Err(ConfigError::invalid("server", "workers", "0", WORKERS));
        }

        let level = match ini.get("log", "level") {
            None => LogLevel::default(),
            Some(raw) => LogLevel::from_name(raw)
                .ok_or_else(|| ConfigError::invalid("log", "level", raw, LEVEL))?,
        };
        let file = ini
            .get("log", "file")
            .filter(|path| !path.is_empty())
            .map(str::to_string);

        Ok(Config {
            name,
            server,
            log: LogConfig { level, file },
        })
    }
}

impl FromStr for Config {
    type Err = ConfigError;

    /// Parses INI source text and reads the settings from it.
    fn from_str(src: &str) -> Result<Config, ConfigError> {
        Config::from_ini(&parse(src)?)
    }
}

/// An optional setting parsed with `FromStr`.
fn read<T: FromStr>(
    ini: &Ini,
    section: &str,
    key: &str,
    expected: &'static str,
) -> Result<Option<T>, ConfigError> {
    match ini.get(section, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| ConfigError::invalid(section, key, raw, expected)),
    }
}

/// An optional on/off setting.
fn read_bool(ini: &Ini, section: &str, key: &str) -> Result<Option<bool>, ConfigError> {
    match ini.get(section, key) {
        None => Ok(None),
        Some(raw) => parse_bool(raw)
            .map(Some)
            .ok_or_else(|| ConfigError::invalid(section, key, raw, BOOL)),
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans_accept_the_usual_spellings() {
        for raw in ["true", "Yes", "ON", "1"] {
            assert_eq!(parse_bool(raw), Some(true), "{raw}");
        }
        for raw in ["false", "No", "off", "0"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw}");
        }
        for raw in ["", "y", "enabled", "2"] {
            assert_eq!(parse_bool(raw), None, "{raw}");
        }
    }

    #[test]
    fn setting_names_omit_the_unnamed_section() {
        assert_eq!(setting_name("", "name"), "name");
        assert_eq!(setting_name("server", "port"), "server.port");
    }
}

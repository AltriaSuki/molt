//! `iniconf` reads INI-style configuration files.
//!
//! [`parse`] turns source text into an [`Ini`], a set of named sections
//! holding `key = value` entries, and the [`config`] module reads the typed
//! settings our services need out of one.
//!
//! ```
//! let ini = iniconf::parse("[server]\nport = 8080\n").unwrap();
//! assert_eq!(ini.get("server", "port"), Some("8080"));
//! ```

pub mod config;
mod error;
mod ini;
mod parser;

pub use config::{Config, ConfigError};
pub use error::{ErrorKind, ParseError};
pub use ini::Ini;
pub use parser::parse;

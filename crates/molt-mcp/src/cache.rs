//! The cached tool list of one MCP server, in Molt's data dir.
//!
//! `listed` is what the server last said its tools are, under `key`, the
//! hash of the server's configuration: `molt-mcp list` uses it instead of
//! starting the server when the key still matches. `offered` is what the
//! current run's planner was given; the serving bridge holds calls to it
//! and never changes it, so that a bridge the supervisor restarts still
//! knows what the model was told.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The version of the cache file this crate reads and writes.
const CACHE_SCHEMA: u32 = 1;

/// One tool as an MCP server lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "object")]
    pub input_schema: Value,
}

fn object() -> Value {
    serde_json::json!({ "type": "object" })
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CacheFile {
    pub schema: u32,
    pub key: String,
    #[serde(default)]
    pub listed: Vec<ListedTool>,
    #[serde(default)]
    pub offered: Vec<ListedTool>,
}

impl CacheFile {
    pub fn new(key: String, listed: Vec<ListedTool>) -> Self {
        Self { schema: CACHE_SCHEMA, key, offered: listed.clone(), listed }
    }

    /// The cache at `path`, if there is a readable one of this version.
    pub fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let cache: Self = serde_json::from_slice(&bytes).ok()?;
        (cache.schema == CACHE_SCHEMA).then_some(cache)
    }

    /// Write it to `path` whole, private to this user.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

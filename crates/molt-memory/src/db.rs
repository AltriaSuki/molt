//! The memory database: one SQLite file holding the notes and the project
//! models, written through one connection.

use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use rusqlite::Connection;

/// Bumped when the schema changes in a way old databases must be migrated for.
const SCHEMA_VERSION: i64 = 1;

/// The database. Every read and write goes through its one connection, so
/// writes never race; keep the work done under [`Db::with`] short.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open or create the database at `path`. A new file is created private
    /// to its owner (0600), since notes and the project model describe the
    /// user's code; SQLite gives its journal files the same mode. A missing
    /// parent directory is created private too (0700).
    ///
    /// An existing file that others may read or write is refused: memory
    /// never made it. One that came with a cloned repository (git checks
    /// files out 0644) could otherwise plant notes in every run.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mode = file.metadata()?.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "{} is open to others (mode {mode:o}), so memory did not make it: a database that came with a project \
             could plant notes. Delete it, or if it is yours, make it private with `chmod 600`",
            path.display()
        );
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        Self::init(conn).with_context(|| format!("preparing {}", path.display()))
    }

    /// A database that lives in memory, for tests.
    pub fn in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "the database was written by a newer Molt (schema {version}; this one knows up to {SCHEMA_VERSION})"
        );
        crate::notes::register(&conn)?;
        conn.execute_batch(crate::notes::SCHEMA)?;
        conn.execute_batch(crate::project::SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Run `f` with the connection. A panic in an earlier `f` does not lock
    /// the database for good: each write is one transaction, so a panic
    /// mid-way leaves nothing half done.
    pub fn with<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        f(&mut self.conn.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_new_database_is_private_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite");
        Db::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let db = Db::open(&path).unwrap();
        let version: i64 = db.with(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0))).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_database_from_a_newer_molt_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite");
        Connection::open(&path).unwrap().pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = Db::open(&path).err().unwrap();
        assert!(format!("{err:#}").contains("newer Molt"), "{err:#}");
    }

    #[test]
    fn a_database_others_could_have_written_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite");
        Db::open(&path).unwrap();
        // As a clone of a repository that ships one would have it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = Db::open(&path).err().unwrap();
        assert!(format!("{err:#}").contains("open to others (mode 644)"), "{err:#}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Db::open(&path).unwrap();
    }
}

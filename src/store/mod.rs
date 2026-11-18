//! SQLite store: triage state (watch levels, snoozes, categories), voice
//! notes, opportunity ids and the contribution log. Never message text.

mod activity;
mod kv;
mod schema;
mod watch;

pub use activity::ActivityRow;
pub use watch::{TargetKind, WatchLevel};

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use rusqlite::Connection;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        // Holds names and triage state: owner-only, like the directory.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        schema::migrate(&conn)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    /// The connection, for the store's own modules. Each table's methods live
    /// in their own file under `src/store/`.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite itself consistent, so
        // keep serving instead of poisoning every later call.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

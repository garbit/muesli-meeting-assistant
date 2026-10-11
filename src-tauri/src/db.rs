//! SQLite storage (rusqlite, bundled, WAL) with versioned migrations.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub struct Db {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[&str] = &[include_str!("../migrations/001_init.sql")];

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let migrations = Migrations::new(MIGRATIONS.iter().map(|sql| M::up(sql)).collect());
        migrations.to_latest(&mut conn).context("running migrations")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let migrations = Migrations::new(MIGRATIONS.iter().map(|sql| M::up(sql)).collect());
        migrations.to_latest(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Run a closure with the connection. Keep closures short; this is a mutex.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        f(&conn)
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        f(&mut conn)
    }

    pub fn get_setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT value_json FROM settings WHERE key = ?1")?;
            let mut rows = stmt.query([key])?;
            match rows.next()? {
                Some(row) => {
                    let s: String = row.get(0)?;
                    Ok(Some(serde_json::from_str(&s)?))
                }
                None => Ok(None),
            }
        })
    }

    pub fn set_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO settings(key, value_json) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                rusqlite::params![key, json],
            )?;
            Ok(())
        })
    }
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_settings_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.set_setting("preset", &"balanced").unwrap();
        let v: Option<String> = db.get_setting("preset").unwrap();
        assert_eq!(v.as_deref(), Some("balanced"));
        let n: i64 = db.with(|c| Ok(c.query_row("SELECT count(*) FROM templates", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0);
    }
}

//! Single-node SQLite backend for [`Store`].
//!
//! rusqlite is synchronous, so all access hops onto the blocking pool via
//! `spawn_blocking` around one mutex-guarded connection. That is plenty for
//! Phase 0–2 write rates (see ARCHITECTURE §2.2); read-heavy paths can grow a
//! read pool later without touching the trait.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::{keys, Store};
use crate::error::StoreError;

/// Ordered, append-only migration list. `PRAGMA user_version` tracks the last
/// applied index + 1. Never edit an entry that has shipped — append instead.
const MIGRATIONS: &[&str] = &[
    // v1: settings KV — the seed of all replicated durable state.
    "CREATE TABLE settings (
        key        TEXT PRIMARY KEY,
        value      TEXT NOT NULL,
        updated_at INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;",
];

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (creating if necessary) the database at `path` and migrate it.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?)
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        // WAL for concurrent-reader friendliness on real files; in-memory
        // databases report their own journal mode, which is fine.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        Ok(SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let target = MIGRATIONS.len() as i64;
        if current > target {
            return Err(StoreError::Migration(format!(
                "database schema is v{current}, but this binary only knows v{target} — \
                 refusing to open a database from a newer plurx"
            )));
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = index as i64 + 1;
            conn.execute_batch(&format!("BEGIN;\n{sql}\nCOMMIT;"))
                .map_err(|e| StoreError::Migration(format!("migrating to v{version}: {e}")))?;
            conn.pragma_update(None, "user_version", version)?;
            tracing::info!(version, "applied schema migration");
        }

        // First startup: mint the permanent instance id.
        let existing: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![keys::INSTANCE_ID],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_none() {
            let id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                params![keys::INSTANCE_ID, id],
            )?;
            tracing::info!(instance_id = %id, "generated new instance id");
        }
        Ok(())
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| StoreError::Task("sqlite connection mutex poisoned".to_owned()))?;
            f(&guard)
        })
        .await
        .map_err(|e| StoreError::Task(e.to_string()))?
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn ping(&self) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))?;
            Ok(())
        })
        .await
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let key = key.to_owned();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
    }

    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO settings (key, value, updated_at)
                 VALUES (?1, ?2, unixepoch())
                 ON CONFLICT(key) DO UPDATE
                    SET value = excluded.value, updated_at = unixepoch()",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    async fn instance_id(&self) -> Result<String, StoreError> {
        self.get_setting(keys::INSTANCE_ID).await?.ok_or_else(|| {
            StoreError::Database("instance.id missing — migration invariant broken".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_roundtrip_and_upsert() {
        let store = SqliteStore::open_in_memory().expect("open");
        assert_eq!(store.get_setting("server.name").await.expect("get"), None);

        store.put_setting("server.name", "den").await.expect("put");
        assert_eq!(
            store.get_setting("server.name").await.expect("get"),
            Some("den".to_owned())
        );

        store
            .put_setting("server.name", "attic")
            .await
            .expect("upsert");
        assert_eq!(
            store.get_setting("server.name").await.expect("get"),
            Some("attic".to_owned())
        );
    }

    #[tokio::test]
    async fn instance_id_is_a_uuid_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("plurx.db");

        let first = {
            let store = SqliteStore::open(&db).expect("open");
            store.instance_id().await.expect("instance id")
        };
        uuid::Uuid::parse_str(&first).expect("instance id is a uuid");

        let store = SqliteStore::open(&db).expect("reopen");
        let second = store.instance_id().await.expect("instance id");
        assert_eq!(
            first, second,
            "instance id must be immutable across restarts"
        );
    }

    #[tokio::test]
    async fn refuses_databases_from_the_future() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("plurx.db");
        SqliteStore::open(&db).expect("create");

        {
            let conn = Connection::open(&db).expect("raw open");
            conn.pragma_update(None, "user_version", 9999)
                .expect("bump");
        }

        match SqliteStore::open(&db).map(|_| ()) {
            Err(StoreError::Migration(msg)) => assert!(msg.contains("newer")),
            other => panic!("expected migration error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_succeeds_on_healthy_store() {
        let store = SqliteStore::open_in_memory().expect("open");
        store.ping().await.expect("ping");
    }
}

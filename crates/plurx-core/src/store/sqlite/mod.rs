//! Single-node SQLite backend for the [`Store`](super::Store) trait family.
//!
//! rusqlite is synchronous, so all access hops onto the blocking pool via
//! `spawn_blocking` around one mutex-guarded connection. That is plenty for
//! Phase 0–2 write rates (see ARCHITECTURE §2.2); read-heavy paths can grow a
//! read pool later without touching the traits.
//!
//! Implementation is split by domain area: `users`, `library`, `media`,
//! `watch` — this file owns open/migrate, shared row mappers, and settings.

mod library;
mod media;
mod trakt;
mod users;
mod watch;

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{keys, SettingsStore};
use crate::domain::{Item, ItemKind, MediaFile, User};
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
    // v2: Phase 1 — users/auth, libraries, media items & files, watch state,
    // and full-text search over items.
    "CREATE TABLE users (
        id            INTEGER PRIMARY KEY,
        username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
        password_hash TEXT NOT NULL,
        is_admin      INTEGER NOT NULL DEFAULT 0,
        created_at    INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;

    CREATE TABLE tokens (
        token_hash   TEXT PRIMARY KEY,
        user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        device       TEXT,
        created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
        last_seen_at INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;

    CREATE TABLE libraries (
        id         INTEGER PRIMARY KEY,
        name       TEXT NOT NULL UNIQUE,
        kind       TEXT NOT NULL CHECK (kind IN ('movies','shows')),
        paths      TEXT NOT NULL,
        created_at INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;

    CREATE TABLE items (
        id             INTEGER PRIMARY KEY,
        library_id     INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
        kind           TEXT NOT NULL CHECK (kind IN ('movie','show','season','episode')),
        parent_id      INTEGER REFERENCES items(id) ON DELETE CASCADE,
        title          TEXT NOT NULL,
        sort_title     TEXT NOT NULL,
        year           INTEGER,
        overview       TEXT,
        tmdb_id        INTEGER,
        imdb_id        TEXT,
        season_number  INTEGER,
        episode_number INTEGER,
        air_date       TEXT,
        runtime_ms     INTEGER,
        poster_path    TEXT,
        backdrop_path  TEXT,
        added_at       INTEGER NOT NULL DEFAULT (unixepoch()),
        updated_at     INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;
    CREATE INDEX idx_items_library_kind ON items(library_id, kind);
    CREATE INDEX idx_items_parent ON items(parent_id);
    CREATE INDEX idx_items_added ON items(added_at DESC);

    CREATE TABLE files (
        id               INTEGER PRIMARY KEY,
        item_id          INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        path             TEXT NOT NULL UNIQUE,
        size             INTEGER NOT NULL,
        mtime            INTEGER NOT NULL,
        duration_ms      INTEGER,
        container        TEXT,
        video_codec      TEXT,
        video_profile    TEXT,
        width            INTEGER,
        height           INTEGER,
        bit_depth        INTEGER,
        hdr              TEXT,
        bitrate          INTEGER,
        audio_streams    TEXT NOT NULL DEFAULT '[]',
        subtitle_streams TEXT NOT NULL DEFAULT '[]',
        probe_json       TEXT,
        scanned_at       INTEGER NOT NULL DEFAULT (unixepoch())
    ) STRICT;
    CREATE INDEX idx_files_item ON files(item_id);

    CREATE TABLE watch_state (
        user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        position_ms INTEGER NOT NULL DEFAULT 0,
        duration_ms INTEGER,
        watched     INTEGER NOT NULL DEFAULT 0,
        updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
        PRIMARY KEY (user_id, item_id)
    ) STRICT;
    CREATE INDEX idx_watch_updated ON watch_state(user_id, updated_at DESC);

    CREATE VIRTUAL TABLE items_fts USING fts5(
        title, overview, content='items', content_rowid='id'
    );
    CREATE TRIGGER items_fts_ai AFTER INSERT ON items BEGIN
        INSERT INTO items_fts(rowid, title, overview)
        VALUES (new.id, new.title, new.overview);
    END;
    CREATE TRIGGER items_fts_ad AFTER DELETE ON items BEGIN
        INSERT INTO items_fts(items_fts, rowid, title, overview)
        VALUES ('delete', old.id, old.title, old.overview);
    END;
    CREATE TRIGGER items_fts_au AFTER UPDATE OF title, overview ON items BEGIN
        INSERT INTO items_fts(items_fts, rowid, title, overview)
        VALUES ('delete', old.id, old.title, old.overview);
        INSERT INTO items_fts(rowid, title, overview)
        VALUES (new.id, new.title, new.overview);
    END;",
    // v3: Phase 2 — mark a (shows) library as anime, so the scanner uses
    // absolute episode numbering and enriches from AniList.
    "ALTER TABLE libraries ADD COLUMN anime INTEGER NOT NULL DEFAULT 0;",
    // v4: a human HDR label incl. the Dolby Vision profile ("Dolby Vision ·
    // Profile 7 (HDR10-compatible)", "HDR10+"). `hdr` stays the coarse type the
    // decision engine keys on; this is display detail. Backfilled on next scan.
    "ALTER TABLE files ADD COLUMN hdr_format TEXT;",
    // v5: Trakt account links (per user — one row each) and the per-file
    // manual A/V sync correction. Rescans never touch audio_offset_ms.
    "ALTER TABLE files ADD COLUMN audio_offset_ms INTEGER NOT NULL DEFAULT 0;

    CREATE TABLE trakt_auth (
        user_id         INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
        access_token    TEXT NOT NULL,
        refresh_token   TEXT NOT NULL,
        expires_at      INTEGER NOT NULL,
        trakt_username  TEXT,
        connected_at    INTEGER NOT NULL DEFAULT (unixepoch()),
        last_sync_at    INTEGER NOT NULL DEFAULT 0,
        last_activities TEXT
    ) STRICT;",
];

/// Column list matching [`item_from_row`]. Prefix with a table alias via
/// [`item_cols`].
const ITEM_COLS: &str = "id, library_id, kind, parent_id, title, sort_title, year, overview, \
     tmdb_id, imdb_id, season_number, episode_number, air_date, runtime_ms, \
     poster_path, backdrop_path, added_at, updated_at";

/// `ITEM_COLS` qualified with a table alias (e.g. `i.id, i.library_id, ...`).
fn item_cols(alias: &str) -> String {
    ITEM_COLS
        .split(", ")
        .map(|c| format!("{alias}.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn conversion_err(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(message)),
    )
}

/// Map a row selected with [`ITEM_COLS`] (starting at column `base`).
fn item_from_row(row: &Row<'_>, base: usize) -> rusqlite::Result<Item> {
    let kind_raw: String = row.get(base + 2)?;
    let kind = ItemKind::parse(&kind_raw)
        .ok_or_else(|| conversion_err(base + 2, format!("unknown item kind `{kind_raw}`")))?;
    Ok(Item {
        id: row.get(base)?,
        library_id: row.get(base + 1)?,
        kind,
        parent_id: row.get(base + 3)?,
        title: row.get(base + 4)?,
        sort_title: row.get(base + 5)?,
        year: row.get(base + 6)?,
        overview: row.get(base + 7)?,
        tmdb_id: row.get(base + 8)?,
        imdb_id: row.get(base + 9)?,
        season_number: row.get(base + 10)?,
        episode_number: row.get(base + 11)?,
        air_date: row.get(base + 12)?,
        runtime_ms: row.get(base + 13)?,
        poster_path: row.get(base + 14)?,
        backdrop_path: row.get(base + 15)?,
        added_at: row.get(base + 16)?,
        updated_at: row.get(base + 17)?,
    })
}

const FILE_COLS: &str = "id, item_id, path, size, mtime, duration_ms, container, video_codec, \
     video_profile, width, height, bit_depth, hdr, bitrate, audio_streams, \
     subtitle_streams, scanned_at, hdr_format, audio_offset_ms";

fn file_from_row(row: &Row<'_>) -> rusqlite::Result<MediaFile> {
    let path: String = row.get(2)?;
    let audio_json: String = row.get(14)?;
    let subs_json: String = row.get(15)?;
    Ok(MediaFile {
        id: row.get(0)?,
        item_id: row.get(1)?,
        path: path.into(),
        size: row.get(3)?,
        mtime: row.get(4)?,
        duration_ms: row.get(5)?,
        container: row.get(6)?,
        video_codec: row.get(7)?,
        video_profile: row.get(8)?,
        width: row.get(9)?,
        height: row.get(10)?,
        bit_depth: row.get(11)?,
        hdr: row.get(12)?,
        bitrate: row.get(13)?,
        audio_streams: serde_json::from_str(&audio_json)
            .map_err(|e| conversion_err(14, format!("audio_streams: {e}")))?,
        subtitle_streams: serde_json::from_str(&subs_json)
            .map_err(|e| conversion_err(15, format!("subtitle_streams: {e}")))?,
        scanned_at: row.get(16)?,
        hdr_format: row.get(17)?,
        audio_offset_ms: row.get(18)?,
    })
}

const USER_COLS: &str = "id, username, password_hash, is_admin, created_at";

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        is_admin: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
    })
}

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
        Self::backfill_hdr_format(&conn)?;
        Ok(SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// One-time backfill of `hdr_format` from the probe JSON already stored for
    /// each file. The incremental scanner skips unchanged files, so without this
    /// an existing library would never show the new HDR/Dolby-Vision detail
    /// short of a destructive re-add. Gated by a settings flag so it runs once.
    fn backfill_hdr_format(conn: &Connection) -> Result<(), StoreError> {
        const FLAG: &str = "hdr_format_backfilled_v1";
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![FLAG],
                |r| r.get(0),
            )
            .optional()?;
        if done.is_some() {
            return Ok(());
        }
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, probe_json FROM files \
                 WHERE hdr_format IS NULL AND probe_json IS NOT NULL",
            )?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut updated = 0usize;
        for (id, json) in rows {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(fmt) = crate::scan::probe::parse_probe_json(&value).hdr_format {
                    conn.execute(
                        "UPDATE files SET hdr_format = ?1 WHERE id = ?2",
                        params![fmt, id],
                    )?;
                    updated += 1;
                }
            }
        }
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, '1')",
            params![FLAG],
        )?;
        if updated > 0 {
            tracing::info!(updated, "backfilled HDR detail from stored probe data");
        }
        Ok(())
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
impl SettingsStore for SqliteStore {
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
    use crate::store::SettingsStore;

    #[tokio::test]
    async fn settings_roundtrip_and_upsert() {
        let store = SqliteStore::open_in_memory().expect("open");
        assert_eq!(store.get_setting("k").await.expect("get"), None);
        store.put_setting("k", "v1").await.expect("put");
        store.put_setting("k", "v2").await.expect("upsert");
        assert_eq!(
            store.get_setting("k").await.expect("get"),
            Some("v2".to_owned())
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
}

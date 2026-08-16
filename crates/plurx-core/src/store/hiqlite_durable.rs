//! Replicated integration, outbox, cache-location, and offline-package facts.
//!
//! Cache and offline rows carry `node_id` because the database fact replicates
//! while the bytes do not. A row owned by another node is routing and cleanup
//! information, never proof that this process can open the relative path.

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::hiqlite::{database_error, timeout_store, validate_sql, HiqliteAuthStore, TimedClient};
use super::{
    OfflinePackageStore, OutboxEntry, TraktStore, TranscodeCacheStore, WatchedOutboxStore,
};
use crate::domain::{
    CachedTranscode, NewOfflinePackage, OfflineActivityPackage, OfflineCreateOutcome, OfflineLease,
    OfflineLeaseOutcome, OfflinePackage, OfflinePackageStats, OfflineRemovalPlanEntry,
    OfflineRemovalReport, TraktAuth, OFFLINE_NODE_REMOVED_CODE,
};
use crate::error::StoreError;
use crate::secrets::SealedSecret;
use crate::store::persistable_credential;
use crate::trakt::{Ident, LocalWatch, SyncCandidate};

const DURABLE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS trakt_auth (
    user_id         INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    access_token    TEXT NOT NULL,
    refresh_token   TEXT NOT NULL,
    expires_at      INTEGER NOT NULL,
    trakt_username  TEXT,
    connected_at    INTEGER NOT NULL,
    last_sync_at    INTEGER NOT NULL,
    last_activities TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS watched_outbox (
    id         INTEGER PRIMARY KEY,
    payload    TEXT NOT NULL,
    attempts   INTEGER NOT NULL,
    last_error TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('pending', 'ok', 'failed')),
    next_at    INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    claim_until INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS watched_outbox_due
    ON watched_outbox(status, next_at, claim_until);

CREATE TABLE IF NOT EXISTS transcode_cache_recipes (
    recipe_hash    TEXT PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    recipe_version INTEGER NOT NULL,
    created_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS transcode_cache_recipes_file
    ON transcode_cache_recipes(file_id);

CREATE TABLE IF NOT EXISTS transcode_cache_locations (
    recipe_hash   TEXT NOT NULL REFERENCES transcode_cache_recipes(recipe_hash)
                               ON DELETE CASCADE,
    node_id       TEXT NOT NULL,
    storage_class TEXT NOT NULL CHECK (storage_class IN ('local', 'shared')),
    relative_dir  TEXT NOT NULL,
    bytes         INTEGER NOT NULL,
    complete      INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    PRIMARY KEY (recipe_hash, node_id, storage_class)
) STRICT;
CREATE INDEX IF NOT EXISTS transcode_cache_lru
    ON transcode_cache_locations(node_id, complete, last_used_at);

CREATE TABLE IF NOT EXISTS offline_packages (
    id                TEXT PRIMARY KEY,
    request_id        TEXT NOT NULL,
    user_id           INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    file_id           INTEGER NOT NULL,
    node_id           TEXT NOT NULL,
    source_path       TEXT NOT NULL,
    source_size       INTEGER NOT NULL,
    source_mtime      INTEGER NOT NULL,
    recipe_hash       TEXT,
    effective_rate_control TEXT NOT NULL DEFAULT 'vbr'
        CHECK (effective_rate_control = 'vbr'
               OR (effective_rate_control GLOB 'qvbr:[0-9]*'
                   AND substr(effective_rate_control, 6) NOT GLOB '*[^0-9]*'
                   AND length(substr(effective_rate_control, 6)) BETWEEN 1 AND 3
                   AND CAST(substr(effective_rate_control, 6) AS INTEGER) BETWEEN 0 AND 255
                   AND printf('%d', CAST(substr(effective_rate_control, 6) AS INTEGER)) =
                       substr(effective_rate_control, 6))),
    target_height     INTEGER NOT NULL,
    output_width      INTEGER,
    output_height     INTEGER,
    audio_index       INTEGER,
    audio_offset_ms   INTEGER NOT NULL,
    subtitle_index    INTEGER,
    subtitle_language TEXT,
    subtitle_mode     TEXT NOT NULL CHECK (subtitle_mode IN ('none', 'native', 'burned')),
    state             TEXT NOT NULL CHECK (state IN ('queued', 'preparing', 'ready', 'failed')),
    phase             TEXT NOT NULL,
    progress_millis   INTEGER NOT NULL,
    estimated_bytes   INTEGER NOT NULL,
    reserved_bytes    INTEGER NOT NULL,
    actual_bytes      INTEGER,
    duration_ms       INTEGER,
    error_code        TEXT,
    error_message     TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    last_access_at    INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    UNIQUE (user_id, request_id)
) STRICT;
CREATE INDEX IF NOT EXISTS offline_packages_queue
    ON offline_packages(node_id, state, created_at);
CREATE INDEX IF NOT EXISTS offline_packages_recipe
    ON offline_packages(node_id, recipe_hash, state);
CREATE INDEX IF NOT EXISTS offline_packages_user_state
    ON offline_packages(user_id, state, updated_at);

CREATE TABLE IF NOT EXISTS offline_package_leases (
    token_hash     TEXT PRIMARY KEY,
    package_id     TEXT NOT NULL UNIQUE REFERENCES offline_packages(id) ON DELETE CASCADE,
    created_at     INTEGER NOT NULL,
    last_access_at INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS offline_package_leases_expiry
    ON offline_package_leases(expires_at);

CREATE TABLE IF NOT EXISTS offline_lease_guards (
    package_id TEXT PRIMARY KEY REFERENCES offline_packages(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL,
    had_lease  INTEGER NOT NULL
) STRICT;

-- One node's answer to "can you actually read this package's source?", asked
-- while its owner is being removed (`CLUSTERING-PLAN.md` §6.7).
--
-- This table exists because `offline_packages.source_path` replicates and a
-- mount does not. Only a node that opened the exact snapshotted bytes may be
-- re-homed onto, so the proof has to be a durable fact somebody wrote after
-- looking, not an inference from a path that travelled here.
--
-- It stores no media path: the answer is keyed to the package, which already
-- holds the only copy of that path. `answered_at` is NULL until the node
-- replies, and an answer is only evidence while it is fresh, so removal reads
-- it with a recency bound rather than trusting whatever is on the row.
--
-- Cluster-only by construction. There is no SQLite counterpart because a
-- single-node install has no removal path to run this protocol for.
CREATE TABLE IF NOT EXISTS offline_source_probes (
    package_id   TEXT NOT NULL REFERENCES offline_packages(id) ON DELETE CASCADE,
    node_id      TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    answered_at  INTEGER,
    readable     INTEGER CHECK (readable IN (0, 1)),
    PRIMARY KEY (package_id, node_id)
) STRICT;
CREATE INDEX IF NOT EXISTS offline_source_probes_pending
    ON offline_source_probes(node_id, answered_at, requested_at);
"#;

pub(super) async fn install_schema(client: &hiqlite::Client) -> Result<(), StoreError> {
    validate_sql(DURABLE_SCHEMA)?;
    for result in timeout_store(client.batch(DURABLE_SCHEMA)).await? {
        result.map_err(database_error)?;
    }
    Ok(())
}

struct JsonValueRow {
    value: String,
}

impl From<&mut Row<'_>> for JsonValueRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

#[derive(Serialize)]
struct DurableDump {
    trakt_auth: Vec<String>,
    watched_outbox: Vec<String>,
    transcode_cache_recipes: Vec<String>,
    transcode_cache_locations: Vec<String>,
    offline_packages: Vec<String>,
    offline_package_leases: Vec<String>,
    offline_lease_guards: Vec<String>,
    offline_source_probes: Vec<String>,
}

pub(super) async fn local_durable_digest(client: &TimedClient) -> Result<String, StoreError> {
    async fn rows(client: &TimedClient, sql: &'static str) -> Result<Vec<String>, StoreError> {
        Ok(client
            .query_map::<JsonValueRow, _>(sql, params!())
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| row.value)
            .collect())
    }

    let dump = DurableDump {
        trakt_auth: rows(
            client,
            "SELECT json_array(user_id, access_token, refresh_token, expires_at, \
                    trakt_username, connected_at, last_sync_at, last_activities) AS value \
             FROM trakt_auth ORDER BY user_id",
        )
        .await?,
        watched_outbox: rows(
            client,
            "SELECT json_array(id, payload, attempts, last_error, status, next_at, \
                    created_at, updated_at, claim_until) AS value \
             FROM watched_outbox ORDER BY id",
        )
        .await?,
        transcode_cache_recipes: rows(
            client,
            "SELECT json_array(recipe_hash, file_id, recipe_version, created_at) AS value \
             FROM transcode_cache_recipes ORDER BY recipe_hash",
        )
        .await?,
        transcode_cache_locations: rows(
            client,
            "SELECT json_array(recipe_hash, node_id, storage_class, relative_dir, bytes, \
                    complete, last_used_at, last_seen_at) AS value \
             FROM transcode_cache_locations ORDER BY recipe_hash, node_id, storage_class",
        )
        .await?,
        offline_packages: rows(
            client,
            "SELECT json_array(id, request_id, user_id, file_id, node_id, source_path, \
                    source_size, source_mtime, recipe_hash, effective_rate_control, \
                    target_height, output_width, \
                    output_height, audio_index, audio_offset_ms, subtitle_index, \
                    subtitle_language, subtitle_mode, state, phase, progress_millis, \
                    estimated_bytes, reserved_bytes, actual_bytes, duration_ms, error_code, \
                    error_message, created_at, updated_at, last_access_at, expires_at) AS value \
             FROM offline_packages ORDER BY id",
        )
        .await?,
        offline_package_leases: rows(
            client,
            "SELECT json_array(token_hash, package_id, created_at, last_access_at, expires_at) \
                    AS value FROM offline_package_leases ORDER BY token_hash",
        )
        .await?,
        offline_lease_guards: rows(
            client,
            "SELECT json_array(package_id, token_hash, had_lease) AS value \
             FROM offline_lease_guards ORDER BY package_id",
        )
        .await?,
        offline_source_probes: rows(
            client,
            "SELECT json_array(package_id, node_id, requested_at, answered_at, readable) \
                    AS value FROM offline_source_probes ORDER BY package_id, node_id",
        )
        .await?,
    };
    let bytes = serde_json::to_vec(&dump).map_err(database_error)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

struct TraktAuthRow {
    user_id: i64,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    trakt_username: Option<String>,
    connected_at: i64,
    last_sync_at: i64,
    last_activities: Option<String>,
}

impl From<&mut Row<'_>> for TraktAuthRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            user_id: row.get("user_id"),
            access_token: row.get("access_token"),
            refresh_token: row.get("refresh_token"),
            expires_at: row.get("expires_at"),
            trakt_username: row.get("trakt_username"),
            connected_at: row.get("connected_at"),
            last_sync_at: row.get("last_sync_at"),
            last_activities: row.get("last_activities"),
        }
    }
}

impl From<TraktAuthRow> for TraktAuth {
    fn from(row: TraktAuthRow) -> Self {
        Self {
            user_id: row.user_id,
            // Raft only ever carried the envelope; this is the same adoption
            // the SQLite backend does, so both replicas agree byte for byte
            // and the refresh-token compare-and-set stays an equality test.
            access_token: SealedSecret::from_stored(row.access_token),
            refresh_token: SealedSecret::from_stored(row.refresh_token),
            expires_at: row.expires_at,
            trakt_username: row.trakt_username,
            connected_at: row.connected_at,
            last_sync_at: row.last_sync_at,
            last_activities: row.last_activities,
        }
    }
}

const TRAKT_AUTH_COLS: &str = "user_id, access_token, refresh_token, expires_at, \
    trakt_username, connected_at, last_sync_at, last_activities";

struct SyncCandidateRow {
    item_id: i64,
    kind: String,
    own_tmdb: Option<i64>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    show_tmdb: Option<i64>,
    position_ms: Option<i64>,
    duration_ms: Option<i64>,
    watched: Option<i64>,
    updated_at: Option<i64>,
    file_duration_ms: Option<i64>,
}

impl From<&mut Row<'_>> for SyncCandidateRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            item_id: row.get("item_id"),
            kind: row.get("kind"),
            own_tmdb: row.get("own_tmdb"),
            season_number: row.get("season_number"),
            episode_number: row.get("episode_number"),
            show_tmdb: row.get("show_tmdb"),
            position_ms: row.get("position_ms"),
            duration_ms: row.get("duration_ms"),
            watched: row.get("watched"),
            updated_at: row.get("updated_at"),
            file_duration_ms: row.get("file_duration_ms"),
        }
    }
}

impl TryFrom<SyncCandidateRow> for Option<SyncCandidate> {
    type Error = StoreError;

    fn try_from(row: SyncCandidateRow) -> Result<Self, Self::Error> {
        let ident = match row.kind.as_str() {
            "movie" => row.own_tmdb.map(|tmdb| Ident::Movie { tmdb }),
            "episode" => match (row.show_tmdb, row.season_number, row.episode_number) {
                (Some(show_tmdb), Some(season), Some(episode)) => Some(Ident::Episode {
                    show_tmdb,
                    season: i32::try_from(season).map_err(|_| {
                        StoreError::Database(format!("season number {season} does not fit i32"))
                    })?,
                    episode: i32::try_from(episode).map_err(|_| {
                        StoreError::Database(format!("episode number {episode} does not fit i32"))
                    })?,
                }),
                _ => None,
            },
            other => {
                return Err(StoreError::Database(format!(
                    "unexpected Trakt candidate item kind {other}"
                )))
            }
        };
        Ok(ident.map(|ident| SyncCandidate {
            item_id: row.item_id,
            ident,
            watch: row.position_ms.map(|position_ms| LocalWatch {
                position_ms,
                duration_ms: row.duration_ms,
                watched: row.watched.unwrap_or(0) != 0,
                updated_at: row.updated_at.unwrap_or(0),
            }),
            file_duration_ms: row.file_duration_ms,
        }))
    }
}

#[async_trait]
impl TraktStore for HiqliteAuthStore {
    async fn get_trakt_auth(&self, user_id: i64) -> Result<Option<TraktAuth>, StoreError> {
        Ok(self
            .client()
            .query_consistent_map::<TraktAuthRow, _>(
                format!("SELECT {TRAKT_AUTH_COLS} FROM trakt_auth WHERE user_id = $1"),
                params!(user_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into))
    }

    async fn list_trakt_auth(&self) -> Result<Vec<TraktAuth>, StoreError> {
        Ok(self
            .client()
            .query_consistent_map::<TraktAuthRow, _>(
                format!("SELECT {TRAKT_AUTH_COLS} FROM trakt_auth ORDER BY user_id"),
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn put_trakt_auth(&self, auth: &TraktAuth) -> Result<(), StoreError> {
        self.execute(
            "INSERT INTO trakt_auth (user_id, access_token, refresh_token, expires_at, \
                 trakt_username, connected_at, last_sync_at, last_activities) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT(user_id) DO UPDATE SET access_token = excluded.access_token, \
                 refresh_token = excluded.refresh_token, expires_at = excluded.expires_at, \
                 trakt_username = excluded.trakt_username, connected_at = excluded.connected_at, \
                 last_sync_at = excluded.last_sync_at, last_activities = excluded.last_activities",
            params!(
                auth.user_id,
                // The same refusal the SQLite backend applies, on the path where
                // it matters most: this statement becomes a raft log entry that
                // every voter keeps.
                persistable_credential(&auth.access_token)?,
                persistable_credential(&auth.refresh_token)?,
                auth.expires_at,
                auth.trakt_username.clone(),
                auth.connected_at,
                auth.last_sync_at,
                auth.last_activities.clone()
            ),
        )
        .await?;
        Ok(())
    }

    async fn delete_trakt_auth(&self, user_id: i64) -> Result<(), StoreError> {
        self.execute(
            "DELETE FROM trakt_auth WHERE user_id = $1",
            params!(user_id),
        )
        .await?;
        Ok(())
    }

    async fn delete_trakt_auth_if_current(
        &self,
        user_id: i64,
        expected_refresh_token: &SealedSecret,
    ) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "DELETE FROM trakt_auth WHERE user_id = $1 AND refresh_token = $2",
                params!(user_id, expected_refresh_token.as_stored().to_owned()),
            )
            .await?
            > 0)
    }

    async fn update_trakt_tokens(
        &self,
        user_id: i64,
        expected_refresh_token: &SealedSecret,
        access_token: &SealedSecret,
        refresh_token: &SealedSecret,
        expires_at: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "UPDATE trakt_auth SET access_token = $1, refresh_token = $2, expires_at = $3 \
             WHERE user_id = $4 AND refresh_token = $5",
                params!(
                    persistable_credential(access_token)?,
                    persistable_credential(refresh_token)?,
                    expires_at,
                    user_id,
                    // Compared, never written — see the SQLite backend.
                    expected_refresh_token.as_stored().to_owned()
                ),
            )
            .await?
            > 0)
    }

    async fn set_trakt_sync(
        &self,
        user_id: i64,
        last_sync_at: i64,
        last_activities: Option<&str>,
    ) -> Result<(), StoreError> {
        self.execute(
            "UPDATE trakt_auth SET last_sync_at = $1, last_activities = $2 WHERE user_id = $3",
            params!(last_sync_at, last_activities, user_id),
        )
        .await?;
        Ok(())
    }

    async fn trakt_sync_candidates(&self, user_id: i64) -> Result<Vec<SyncCandidate>, StoreError> {
        self.client()
            .query_consistent_map::<SyncCandidateRow, _>(
                "SELECT i.id AS item_id, i.kind AS kind, i.tmdb_id AS own_tmdb, \
                        i.season_number AS season_number, i.episode_number AS episode_number, \
                        sh.tmdb_id AS show_tmdb, w.position_ms AS position_ms, \
                        w.duration_ms AS duration_ms, w.watched AS watched, \
                        w.updated_at AS updated_at, \
                        (SELECT f.duration_ms FROM files f WHERE f.item_id = i.id \
                         AND f.duration_ms IS NOT NULL LIMIT 1) AS file_duration_ms \
                 FROM items i LEFT JOIN items se ON se.id = i.parent_id \
                 LEFT JOIN items sh ON sh.id = se.parent_id \
                 LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = $1 \
                 WHERE i.kind IN ('movie','episode')",
                params!(user_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Option::<SyncCandidate>::try_from)
            .filter_map(|candidate| candidate.transpose())
            .collect()
    }
}

#[derive(Clone)]
struct OutboxRow {
    id: i64,
    payload: String,
    attempts: i64,
    last_error: String,
    status: String,
    next_at: i64,
    claim_until: i64,
}

impl From<&mut Row<'_>> for OutboxRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            payload: row.get("payload"),
            attempts: row.get("attempts"),
            last_error: row.get("last_error"),
            status: row.get("status"),
            next_at: row.get("next_at"),
            claim_until: row.get("claim_until"),
        }
    }
}

impl From<OutboxRow> for OutboxEntry {
    fn from(row: OutboxRow) -> Self {
        Self {
            id: row.id,
            payload: row.payload,
            attempts: row.attempts,
            last_error: row.last_error,
            status: row.status,
            next_at: row.next_at,
            claim_until: row.claim_until,
        }
    }
}

#[async_trait]
impl WatchedOutboxStore for HiqliteAuthStore {
    async fn enqueue_watched(&self, payload: &str) -> Result<i64, StoreError> {
        let now = self.now()?;
        let sql = "INSERT INTO watched_outbox \
                    (payload, attempts, last_error, status, next_at, created_at, updated_at, \
                     claim_until) \
                 VALUES ($1, 0, '', 'pending', $2, $2, $2, 0) RETURNING id";
        validate_sql(sql)?;
        let mut rows = self
            .client()
            .execute_returning_map::<_, IdRow>(sql, params!(payload, now))
            .await
            .map_err(database_error)?;
        rows.pop()
            .transpose()
            .map_err(database_error)?
            .map(|row| row.id)
            .ok_or_else(|| StoreError::Database("outbox insert returned no id".to_owned()))
    }

    async fn due_watched(&self, limit: i64) -> Result<Vec<OutboxEntry>, StoreError> {
        let now = self.now()?;
        let claim_until = now.saturating_add(60);
        let sql = "UPDATE watched_outbox SET claim_until = $1 \
                 WHERE id IN (SELECT id FROM watched_outbox \
                   WHERE status = 'pending' AND next_at <= $2 AND claim_until <= $2 \
                   ORDER BY next_at, id LIMIT $3) \
                 RETURNING id, payload, attempts, last_error, status, next_at, claim_until";
        validate_sql(sql)?;
        Ok(self
            .client()
            .execute_returning_map::<_, OutboxRow>(sql, params!(claim_until, now, limit))
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn settle_watched(&self, entry: &OutboxEntry) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE watched_outbox SET attempts = $1, last_error = $2, status = $3, \
                 next_at = $4, updated_at = $5, claim_until = 0 \
             WHERE id = $6 AND claim_until = $7",
            params!(
                entry.attempts,
                entry.last_error.as_str(),
                entry.status.as_str(),
                entry.next_at,
                now,
                entry.id,
                entry.claim_until
            ),
        )
        .await?;
        Ok(())
    }

    async fn watched_outbox_counts(&self) -> Result<(i64, i64, i64), StoreError> {
        let row = self.client()
            .query_consistent_map::<OutboxCountsRow, _>(
                "SELECT COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending, \
                        COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0) AS ok, \
                        COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failed \
                 FROM watched_outbox",
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Database("outbox count returned no row".to_owned()))?;
        Ok((row.pending, row.ok, row.failed))
    }
}

struct IdRow {
    id: i64,
}

impl From<&mut Row<'_>> for IdRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self { id: row.get("id") }
    }
}

struct OutboxCountsRow {
    pending: i64,
    ok: i64,
    failed: i64,
}

impl From<&mut Row<'_>> for OutboxCountsRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            pending: row.get("pending"),
            ok: row.get("ok"),
            failed: row.get("failed"),
        }
    }
}

struct CacheRow {
    recipe_hash: String,
    file_id: i64,
    storage_class: String,
    relative_dir: String,
    bytes: i64,
    complete: i64,
    last_used_at: i64,
}

impl From<&mut Row<'_>> for CacheRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            recipe_hash: row.get("recipe_hash"),
            file_id: row.get("file_id"),
            storage_class: row.get("storage_class"),
            relative_dir: row.get("relative_dir"),
            bytes: row.get("bytes"),
            complete: row.get("complete"),
            last_used_at: row.get("last_used_at"),
        }
    }
}

impl From<CacheRow> for CachedTranscode {
    fn from(row: CacheRow) -> Self {
        Self {
            recipe_hash: row.recipe_hash,
            file_id: row.file_id,
            storage_class: row.storage_class,
            relative_dir: row.relative_dir,
            bytes: row.bytes,
            complete: row.complete != 0,
            last_used_at: row.last_used_at,
        }
    }
}

const CACHE_COLS: &str = "l.recipe_hash AS recipe_hash, r.file_id AS file_id, \
    l.storage_class AS storage_class, l.relative_dir AS relative_dir, l.bytes AS bytes, \
    l.complete AS complete, l.last_used_at AS last_used_at";

fn cached(rows: Vec<CacheRow>) -> Vec<CachedTranscode> {
    rows.into_iter().map(Into::into).collect()
}

#[async_trait]
impl TranscodeCacheStore for HiqliteAuthStore {
    async fn cache_hit(
        &self,
        recipe_hash: &str,
        node_id: &str,
    ) -> Result<Option<CachedTranscode>, StoreError> {
        Ok(self
            .client()
            .query_consistent_map::<CacheRow, _>(
                format!(
                    "SELECT {CACHE_COLS} FROM transcode_cache_locations l \
                     JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash \
                     WHERE l.recipe_hash = $1 AND l.node_id = $2 AND l.complete = 1"
                ),
                params!(recipe_hash, node_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into))
    }

    async fn claim_cache_entry(
        &self,
        recipe_hash: &str,
        file_id: i64,
        recipe_version: i64,
        node_id: &str,
        relative_dir: &str,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        let statements = vec![
            (
                "INSERT INTO transcode_cache_recipes \
                    (recipe_hash, file_id, recipe_version, created_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT(recipe_hash) DO NOTHING"
                    .to_owned(),
                params!(recipe_hash, file_id, recipe_version, now),
            ),
            (
                "INSERT INTO transcode_cache_locations \
                    (recipe_hash, node_id, storage_class, relative_dir, bytes, complete, \
                     last_used_at, last_seen_at) \
                 VALUES ($1, $2, 'local', $3, 0, 0, $4, $4) \
                 ON CONFLICT(recipe_hash, node_id, storage_class) DO NOTHING"
                    .to_owned(),
                params!(recipe_hash, node_id, relative_dir, now),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let results = self
            .client()
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        Ok(results.get(1).copied().unwrap_or(0) > 0)
    }

    async fn touch_cache_claim(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE transcode_cache_locations SET last_seen_at = $1 \
             WHERE recipe_hash = $2 AND node_id = $3 AND complete = 0",
            params!(now, recipe_hash, node_id),
        )
        .await?;
        Ok(())
    }

    async fn complete_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        bytes: i64,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE transcode_cache_locations SET complete = 1, bytes = $1, \
                 last_used_at = $2, last_seen_at = $2 \
             WHERE recipe_hash = $3 AND node_id = $4 AND storage_class = 'local'",
            params!(bytes, now, recipe_hash, node_id),
        )
        .await?;
        Ok(())
    }

    async fn touch_cache_entry(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE transcode_cache_locations SET last_used_at = $1 \
             WHERE recipe_hash = $2 AND node_id = $3",
            params!(now, recipe_hash, node_id),
        )
        .await?;
        Ok(())
    }

    async fn cache_by_age(
        &self,
        node_id: &str,
        limit: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError> {
        Ok(cached(
            self.client()
                .query_consistent_map::<CacheRow, _>(
                    format!(
                        "SELECT {CACHE_COLS} FROM transcode_cache_locations l \
                         JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash \
                         WHERE l.node_id = $1 AND l.storage_class = 'local' \
                           AND l.complete = 1 AND NOT EXISTS ( \
                             SELECT 1 FROM offline_packages p \
                             WHERE p.recipe_hash = l.recipe_hash AND p.node_id = l.node_id \
                               AND p.state IN ('queued', 'preparing', 'ready')) \
                         ORDER BY l.last_used_at ASC, l.rowid ASC LIMIT $2"
                    ),
                    params!(node_id, limit),
                )
                .await
                .map_err(database_error)?,
        ))
    }

    async fn stale_cache_claims(
        &self,
        node_id: &str,
        older_than_unix: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError> {
        Ok(cached(
            self.client()
                .query_consistent_map::<CacheRow, _>(
                    format!(
                        "SELECT {CACHE_COLS} FROM transcode_cache_locations l \
                         JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash \
                         WHERE l.node_id = $1 AND l.storage_class = 'local' \
                           AND l.complete = 0 AND l.last_seen_at < $2 AND NOT EXISTS ( \
                             SELECT 1 FROM offline_packages p WHERE p.file_id = r.file_id \
                               AND p.node_id = l.node_id \
                               AND p.state IN ('queued', 'preparing'))"
                    ),
                    params!(node_id, older_than_unix),
                )
                .await
                .map_err(database_error)?,
        ))
    }

    async fn all_cache_rows(&self, node_id: &str) -> Result<Vec<CachedTranscode>, StoreError> {
        Ok(cached(
            self.client()
                .query_consistent_map::<CacheRow, _>(
                    format!(
                        "SELECT {CACHE_COLS} FROM transcode_cache_locations l \
                         JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash \
                         WHERE l.node_id = $1 ORDER BY l.recipe_hash, l.storage_class"
                    ),
                    params!(node_id),
                )
                .await
                .map_err(database_error)?,
        ))
    }

    async fn forget_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        storage_class: &str,
    ) -> Result<(), StoreError> {
        let statements = vec![
            (
                "DELETE FROM transcode_cache_locations WHERE recipe_hash = $1 \
                 AND node_id = $2 AND storage_class = $3"
                    .to_owned(),
                params!(recipe_hash, node_id, storage_class),
            ),
            (
                "DELETE FROM transcode_cache_recipes WHERE recipe_hash = $1 \
                 AND NOT EXISTS (SELECT 1 FROM transcode_cache_locations \
                                 WHERE recipe_hash = $1)"
                    .to_owned(),
                params!(recipe_hash),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        self.client()
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        Ok(())
    }

    async fn cache_bytes(&self, node_id: &str) -> Result<i64, StoreError> {
        let row = self
            .client()
            .query_consistent_map::<ScalarRow, _>(
                "SELECT COALESCE(SUM(l.bytes), 0) AS value \
                 FROM transcode_cache_locations l \
                 WHERE l.node_id = $1 AND l.storage_class = 'local' AND l.complete = 1 \
                   AND NOT EXISTS (SELECT 1 FROM offline_packages p \
                     WHERE p.recipe_hash = l.recipe_hash AND p.node_id = l.node_id \
                       AND p.state IN ('queued', 'preparing', 'ready'))",
                params!(node_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Database("cache byte sum returned no row".to_owned()))?;
        Ok(row.value)
    }
}

struct ScalarRow {
    value: i64,
}

impl From<&mut Row<'_>> for ScalarRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

struct OfflineAdmissionRow {
    rows: i64,
    user_used: i64,
    node_used: i64,
}

impl From<&mut Row<'_>> for OfflineAdmissionRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            rows: row.get("rows"),
            user_used: row.get("user_used"),
            node_used: row.get("node_used"),
        }
    }
}

#[derive(Clone)]
struct OfflinePackageRow {
    id: String,
    request_id: String,
    user_id: i64,
    file_id: i64,
    node_id: String,
    source_path: String,
    source_size: i64,
    source_mtime: i64,
    recipe_hash: Option<String>,
    effective_rate_control: String,
    target_height: i64,
    audio_index: Option<i64>,
    audio_offset_ms: i64,
    output_width: Option<i64>,
    output_height: Option<i64>,
    subtitle_index: Option<i64>,
    subtitle_language: Option<String>,
    subtitle_mode: String,
    state: String,
    phase: String,
    progress_millis: i64,
    estimated_bytes: i64,
    reserved_bytes: i64,
    actual_bytes: Option<i64>,
    duration_ms: Option<i64>,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at: i64,
    updated_at: i64,
    last_access_at: i64,
    expires_at: i64,
}

impl From<&mut Row<'_>> for OfflinePackageRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            request_id: row.get("request_id"),
            user_id: row.get("user_id"),
            file_id: row.get("file_id"),
            node_id: row.get("node_id"),
            source_path: row.get("source_path"),
            source_size: row.get("source_size"),
            source_mtime: row.get("source_mtime"),
            recipe_hash: row.get("recipe_hash"),
            effective_rate_control: row.get("effective_rate_control"),
            target_height: row.get("target_height"),
            audio_index: row.get("audio_index"),
            audio_offset_ms: row.get("audio_offset_ms"),
            output_width: row.get("output_width"),
            output_height: row.get("output_height"),
            subtitle_index: row.get("subtitle_index"),
            subtitle_language: row.get("subtitle_language"),
            subtitle_mode: row.get("subtitle_mode"),
            state: row.get("state"),
            phase: row.get("phase"),
            progress_millis: row.get("progress_millis"),
            estimated_bytes: row.get("estimated_bytes"),
            reserved_bytes: row.get("reserved_bytes"),
            actual_bytes: row.get("actual_bytes"),
            duration_ms: row.get("duration_ms"),
            error_code: row.get("error_code"),
            error_message: row.get("error_message"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            last_access_at: row.get("last_access_at"),
            expires_at: row.get("expires_at"),
        }
    }
}

impl From<OfflinePackageRow> for OfflinePackage {
    fn from(row: OfflinePackageRow) -> Self {
        Self {
            id: row.id,
            request_id: row.request_id,
            user_id: row.user_id,
            file_id: row.file_id,
            node_id: row.node_id,
            source_path: row.source_path,
            source_size: row.source_size,
            source_mtime: row.source_mtime,
            recipe_hash: row.recipe_hash,
            effective_rate_control: row.effective_rate_control,
            target_height: row.target_height,
            audio_index: row.audio_index,
            audio_offset_ms: row.audio_offset_ms,
            output_width: row.output_width,
            output_height: row.output_height,
            subtitle_index: row.subtitle_index,
            subtitle_language: row.subtitle_language,
            subtitle_mode: row.subtitle_mode,
            state: row.state,
            phase: row.phase,
            progress_millis: row.progress_millis,
            estimated_bytes: row.estimated_bytes,
            reserved_bytes: row.reserved_bytes,
            actual_bytes: row.actual_bytes,
            duration_ms: row.duration_ms,
            error_code: row.error_code,
            error_message: row.error_message,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_access_at: row.last_access_at,
            expires_at: row.expires_at,
        }
    }
}

const PACKAGE_COLS: &str = "id, request_id, user_id, file_id, node_id, source_path, \
    source_size, source_mtime, recipe_hash, effective_rate_control, target_height, audio_index, audio_offset_ms, \
    output_width, output_height, subtitle_index, subtitle_language, subtitle_mode, state, \
    phase, progress_millis, estimated_bytes, reserved_bytes, actual_bytes, duration_ms, \
    error_code, error_message, created_at, updated_at, last_access_at, expires_at";

fn package_cols(alias: &str) -> String {
    PACKAGE_COLS
        .split(", ")
        .map(|column| format!("{alias}.{column} AS {column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn one_package(rows: Vec<OfflinePackageRow>) -> Option<OfflinePackage> {
    rows.into_iter().next().map(Into::into)
}

fn same_request(existing: &OfflinePackage, requested: &NewOfflinePackage) -> bool {
    existing.file_id == requested.file_id
        && existing.node_id == requested.node_id
        && existing.source_path == requested.source_path
        && existing.source_size == requested.source_size
        && existing.source_mtime == requested.source_mtime
        && existing.target_height == requested.target_height
        && existing.output_width == requested.output_width
        && existing.output_height == requested.output_height
        && existing.audio_index == requested.audio_index
        && existing.audio_offset_ms == requested.audio_offset_ms
        && existing.subtitle_index == requested.subtitle_index
        && existing.subtitle_language == requested.subtitle_language
        && existing.subtitle_mode == requested.subtitle_mode
        && existing.estimated_bytes == requested.estimated_bytes
        && existing.reserved_bytes == requested.reserved_bytes
}

fn exceeds_byte_limit(used: i64, reserved: i64, limit: i64) -> bool {
    limit <= 0 || reserved < 0 || used > limit || reserved > limit - used
}

struct OfflineLeaseRow {
    token_hash: String,
    package_id: String,
    created_at: i64,
    last_access_at: i64,
    expires_at: i64,
}

impl From<&mut Row<'_>> for OfflineLeaseRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            token_hash: row.get("token_hash"),
            package_id: row.get("package_id"),
            created_at: row.get("created_at"),
            last_access_at: row.get("last_access_at"),
            expires_at: row.get("expires_at"),
        }
    }
}

impl From<OfflineLeaseRow> for OfflineLease {
    fn from(row: OfflineLeaseRow) -> Self {
        Self {
            token_hash: row.token_hash,
            package_id: row.package_id,
            created_at: row.created_at,
            last_access_at: row.last_access_at,
            expires_at: row.expires_at,
        }
    }
}

struct OfflineActivityRow {
    package: OfflinePackageRow,
    lease_active: i64,
}

impl From<&mut Row<'_>> for OfflineActivityRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            package: OfflinePackageRow::from(&mut *row),
            lease_active: row.get("lease_active"),
        }
    }
}

struct OfflineStatsRow {
    queued: i64,
    preparing: i64,
    ready: i64,
    failed: i64,
    queued_bytes: i64,
    preparing_bytes: i64,
    ready_bytes: i64,
    failed_bytes: i64,
    active_leases: i64,
    pinned_bytes: i64,
}

impl From<&mut Row<'_>> for OfflineStatsRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            queued: row.get("queued"),
            preparing: row.get("preparing"),
            ready: row.get("ready"),
            failed: row.get("failed"),
            queued_bytes: row.get("queued_bytes"),
            preparing_bytes: row.get("preparing_bytes"),
            ready_bytes: row.get("ready_bytes"),
            failed_bytes: row.get("failed_bytes"),
            active_leases: row.get("active_leases"),
            pinned_bytes: row.get("pinned_bytes"),
        }
    }
}

impl From<OfflineStatsRow> for OfflinePackageStats {
    fn from(row: OfflineStatsRow) -> Self {
        Self {
            queued: row.queued,
            preparing: row.preparing,
            ready: row.ready,
            failed: row.failed,
            queued_bytes: row.queued_bytes,
            preparing_bytes: row.preparing_bytes,
            ready_bytes: row.ready_bytes,
            failed_bytes: row.failed_bytes,
            active_leases: row.active_leases,
            pinned_bytes: row.pinned_bytes,
        }
    }
}

#[async_trait]
impl OfflinePackageStore for HiqliteAuthStore {
    async fn create_offline_package(
        &self,
        package: &NewOfflinePackage,
        max_rows_per_user: i64,
        max_bytes_per_user: i64,
        max_bytes_global: i64,
    ) -> Result<OfflineCreateOutcome, StoreError> {
        if crate::transcode::EffectiveRateControl::parse_snapshot(&package.effective_rate_control)
            .is_none()
        {
            return Err(StoreError::Database(format!(
                "invalid offline effective rate control {:?}",
                package.effective_rate_control
            )));
        }
        let now = self.now()?;
        let sql = "INSERT INTO offline_packages (id, request_id, user_id, file_id, node_id, \
                    source_path, source_size, source_mtime, effective_rate_control, \
                    target_height, audio_index, \
                    audio_offset_ms, output_width, output_height, subtitle_index, \
                    subtitle_language, subtitle_mode, state, phase, progress_millis, \
                    estimated_bytes, reserved_bytes, created_at, updated_at, last_access_at, \
                    expires_at) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                    $15, $16, $17, 'queued', 'waiting_for_encoder', 0, $18, $19, $20, $20, $20, $21 \
                 WHERE NOT EXISTS (SELECT 1 FROM offline_packages \
                     WHERE user_id = $3 AND request_id = $2) \
                   AND $22 > 0 \
                   AND (SELECT COUNT(*) FROM offline_packages WHERE user_id = $3) < $22 \
                   AND $19 >= 0 AND $23 >= $19 \
                   AND (SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                     FROM offline_packages WHERE user_id = $3 \
                       AND state IN ('queued', 'preparing', 'ready')) <= $23 - $19 \
                   AND $24 >= $19 \
                   AND (SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                     FROM offline_packages WHERE node_id = $5 \
                       AND state IN ('queued', 'preparing', 'ready')) <= $24 - $19 \
                 RETURNING id, request_id, user_id, file_id, node_id, source_path, \
                    source_size, source_mtime, recipe_hash, effective_rate_control, \
                    target_height, audio_index, \
                    audio_offset_ms, output_width, output_height, subtitle_index, \
                    subtitle_language, subtitle_mode, state, phase, progress_millis, \
                    estimated_bytes, reserved_bytes, actual_bytes, duration_ms, error_code, \
                    error_message, created_at, updated_at, last_access_at, expires_at";
        validate_sql(sql)?;
        for attempt in 0..2 {
            let inserted = self
                .client()
                .execute_returning_map::<_, OfflinePackageRow>(
                    sql,
                    params!(
                        package.id.as_str(),
                        package.request_id.as_str(),
                        package.user_id,
                        package.file_id,
                        package.node_id.as_str(),
                        package.source_path.as_str(),
                        package.source_size,
                        package.source_mtime,
                        package.effective_rate_control.as_str(),
                        package.target_height,
                        package.audio_index,
                        package.audio_offset_ms,
                        package.output_width,
                        package.output_height,
                        package.subtitle_index,
                        package.subtitle_language.as_deref(),
                        package.subtitle_mode.as_str(),
                        package.estimated_bytes,
                        package.reserved_bytes,
                        now,
                        package.expires_at,
                        max_rows_per_user,
                        max_bytes_per_user,
                        max_bytes_global
                    ),
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(database_error)?;
            if let Some(created) = one_package(inserted) {
                return Ok(OfflineCreateOutcome::Created(created));
            }

            if let Some(existing) = one_package(
                self.client()
                    .query_consistent_map::<OfflinePackageRow, _>(
                        format!(
                            "SELECT {PACKAGE_COLS} FROM offline_packages \
                             WHERE user_id = $1 AND request_id = $2"
                        ),
                        params!(package.user_id, package.request_id.as_str()),
                    )
                    .await
                    .map_err(database_error)?,
            ) {
                return Ok(if same_request(&existing, package) {
                    OfflineCreateOutcome::Existing(existing)
                } else {
                    OfflineCreateOutcome::RequestConflict
                });
            }

            let admission = self
                .client()
                .query_consistent_map::<OfflineAdmissionRow, _>(
                    "SELECT \
                       (SELECT COUNT(*) FROM offline_packages WHERE user_id = $1) AS rows, \
                       (SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                          FROM offline_packages WHERE user_id = $1 \
                            AND state IN ('queued', 'preparing', 'ready')) AS user_used, \
                       (SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                          FROM offline_packages WHERE node_id = $2 \
                            AND state IN ('queued', 'preparing', 'ready')) AS node_used",
                    params!(package.user_id, package.node_id.as_str()),
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    StoreError::Database("offline admission snapshot returned no row".to_owned())
                })?;
            if max_rows_per_user <= 0 || admission.rows >= max_rows_per_user {
                return Ok(OfflineCreateOutcome::RowLimit {
                    limit: max_rows_per_user.max(0),
                });
            }
            if exceeds_byte_limit(
                admission.user_used,
                package.reserved_bytes,
                max_bytes_per_user,
            ) {
                return Ok(OfflineCreateOutcome::ByteLimit {
                    used: admission.user_used,
                    limit: max_bytes_per_user.max(0),
                });
            }
            if exceeds_byte_limit(
                admission.node_used,
                package.reserved_bytes,
                max_bytes_global,
            ) {
                return Ok(OfflineCreateOutcome::GlobalByteLimit {
                    used: admission.node_used,
                    limit: max_bytes_global.max(0),
                });
            }
            if attempt == 1 {
                return Err(StoreError::Database(
                    "offline admission changed concurrently; retry the request".to_owned(),
                ));
            }
        }
        unreachable!("bounded offline admission loop always returns")
    }

    async fn offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        Ok(one_package(
            self.client()
                .query_consistent_map::<OfflinePackageRow, _>(
                    format!(
                        "SELECT {PACKAGE_COLS} FROM offline_packages \
                         WHERE id = $1 AND user_id = $2"
                    ),
                    params!(package_id, user_id),
                )
                .await
                .map_err(database_error)?,
        ))
    }

    async fn renew_offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
        expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let now = self.now()?;
        let statements = vec![
            (
                "UPDATE offline_packages SET last_access_at = $1, expires_at = $2, \
                     updated_at = $1 WHERE id = $3 AND user_id = $4"
                    .to_owned(),
                params!(now, expires_at, package_id, user_id),
            ),
            (
                "UPDATE offline_package_leases SET last_access_at = $1, expires_at = $2 \
                 WHERE package_id = $3 AND EXISTS (SELECT 1 FROM offline_packages \
                     WHERE id = $3 AND user_id = $4)"
                    .to_owned(),
                params!(now, expires_at, package_id, user_id),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let changed = self
            .client()
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if changed.first().copied().unwrap_or(0) == 0 {
            return Ok(None);
        }
        self.offline_package_for_user(package_id, user_id).await
    }

    async fn offline_activity_packages(
        &self,
        node_id: &str,
        now: i64,
        active_since: i64,
        limit: i64,
    ) -> Result<Vec<OfflineActivityPackage>, StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<OfflineActivityRow, _>(
                format!(
                    "SELECT {}, EXISTS (SELECT 1 FROM offline_package_leases l \
                         WHERE l.package_id = p.id AND l.expires_at > $1 \
                           AND l.last_access_at >= $2) AS lease_active \
                     FROM offline_packages p WHERE p.node_id = $3 AND ( \
                         p.state IN ('queued', 'preparing') OR EXISTS ( \
                           SELECT 1 FROM offline_package_leases l \
                           WHERE l.package_id = p.id AND l.expires_at > $1 \
                             AND l.last_access_at >= $2)) \
                     ORDER BY CASE p.state WHEN 'preparing' THEN 0 \
                              WHEN 'queued' THEN 1 ELSE 2 END, \
                         (SELECT COALESCE(MAX(recent.last_access_at), 0) \
                          FROM offline_package_leases recent \
                          WHERE recent.package_id = p.id) DESC, p.created_at, p.id LIMIT $4",
                    package_cols("p")
                ),
                params!(now, active_since, node_id, limit.clamp(1, 100)),
            )
            .await
            .map_err(database_error)?;
        Ok(rows
            .into_iter()
            .map(|row| OfflineActivityPackage {
                package: row.package.into(),
                lease_active: row.lease_active != 0,
            })
            .collect())
    }

    async fn offline_package_stats(
        &self,
        node_id: &str,
        now: i64,
    ) -> Result<OfflinePackageStats, StoreError> {
        let row = self.client()
            .query_consistent_map::<OfflineStatsRow, _>(
                "SELECT COALESCE(SUM(state = 'queued'), 0) AS queued, \
                    COALESCE(SUM(state = 'preparing'), 0) AS preparing, \
                    COALESCE(SUM(state = 'ready'), 0) AS ready, \
                    COALESCE(SUM(state = 'failed'), 0) AS failed, \
                    COALESCE(SUM(CASE WHEN state = 'queued' \
                      THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0) AS queued_bytes, \
                    COALESCE(SUM(CASE WHEN state = 'preparing' \
                      THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0) AS preparing_bytes, \
                    COALESCE(SUM(CASE WHEN state = 'ready' \
                      THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0) AS ready_bytes, \
                    COALESCE(SUM(CASE WHEN state = 'failed' \
                      THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0) AS failed_bytes, \
                    (SELECT COUNT(*) FROM offline_package_leases l \
                     JOIN offline_packages active ON active.id = l.package_id \
                     WHERE active.node_id = $1 AND active.state = 'ready' \
                       AND l.expires_at > $2) AS active_leases, \
                    (SELECT COALESCE(SUM(location.bytes), 0) \
                     FROM transcode_cache_locations location \
                     WHERE location.node_id = $1 AND location.storage_class = 'local' \
                       AND location.complete = 1 AND EXISTS ( \
                         SELECT 1 FROM offline_packages pinned \
                         WHERE pinned.node_id = location.node_id \
                           AND pinned.recipe_hash = location.recipe_hash \
                           AND pinned.state IN ('queued', 'preparing', 'ready'))) AS pinned_bytes \
                 FROM offline_packages WHERE node_id = $1",
                params!(node_id, now),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Database("offline stats returned no row".to_owned()))?;
        Ok(row.into())
    }

    async fn reset_interrupted_offline_packages(&self, node_id: &str) -> Result<u64, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET state = 'queued', phase = 'waiting_for_encoder', \
                 updated_at = $1 WHERE node_id = $2 AND state = 'preparing'",
                params!(now, node_id),
            )
            .await? as u64)
    }

    async fn claim_next_offline_package(
        &self,
        node_id: &str,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let now = self.now()?;
        let sql = "UPDATE offline_packages SET state = 'preparing', \
                     phase = 'waiting_for_encoder', updated_at = $1 \
                 WHERE id = (SELECT candidate.id FROM offline_packages candidate \
                     WHERE candidate.node_id = $2 AND candidate.state = 'queued' \
                     ORDER BY (SELECT COUNT(*) FROM offline_packages served \
                               WHERE served.node_id = candidate.node_id \
                                 AND served.user_id = candidate.user_id \
                                 AND served.state != 'queued'), \
                              candidate.created_at, candidate.id LIMIT 1) \
                   AND state = 'queued' \
                 RETURNING id, request_id, user_id, file_id, node_id, source_path, \
                    source_size, source_mtime, recipe_hash, effective_rate_control, \
                    target_height, audio_index, \
                    audio_offset_ms, output_width, output_height, subtitle_index, \
                    subtitle_language, subtitle_mode, state, phase, progress_millis, \
                    estimated_bytes, reserved_bytes, actual_bytes, duration_ms, error_code, \
                    error_message, created_at, updated_at, last_access_at, expires_at";
        validate_sql(sql)?;
        let rows = self
            .client()
            .execute_returning_map::<_, OfflinePackageRow>(sql, params!(now, node_id))
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        Ok(one_package(rows))
    }

    async fn requeue_offline_package(&self, package_id: &str) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET state = 'queued', phase = 'waiting_for_encoder', \
                 updated_at = $1 WHERE id = $2 AND state = 'preparing'",
                params!(now, package_id),
            )
            .await?
            > 0)
    }

    async fn set_offline_package_recipe(
        &self,
        package_id: &str,
        recipe_hash: &str,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET recipe_hash = $1, updated_at = $2 \
                 WHERE id = $3 AND state = 'preparing' \
                   AND (recipe_hash IS NULL OR recipe_hash = $1)",
                params!(recipe_hash, now, package_id),
            )
            .await?
            > 0)
    }

    async fn update_offline_progress(
        &self,
        package_id: &str,
        phase: &str,
        progress_millis: i64,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET phase = $1, \
                 progress_millis = MAX(progress_millis, $2), updated_at = $3 \
                 WHERE id = $4 AND state = 'preparing'",
                params!(phase, progress_millis.clamp(0, 999), now, package_id),
            )
            .await?
            > 0)
    }

    async fn fail_offline_package(
        &self,
        package_id: &str,
        node_id: &str,
        phase: &str,
        code: &str,
        message: &str,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET state = 'failed', phase = $1, error_code = $2, \
                 error_message = $3, updated_at = $4 \
                 WHERE id = $5 AND node_id = $6 AND state IN ('queued', 'preparing')",
                params!(phase, code, message, now, package_id, node_id),
            )
            .await?
            > 0)
    }

    async fn put_offline_lease(
        &self,
        package_id: &str,
        user_id: i64,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<OfflineLeaseOutcome, StoreError> {
        let now = self.now()?;
        let statements = vec![
            (
                "INSERT INTO offline_lease_guards (package_id, token_hash, had_lease) \
                 SELECT p.id, $1, EXISTS (SELECT 1 FROM offline_package_leases \
                                         WHERE package_id = p.id) \
                 FROM offline_packages p WHERE p.id = $2 AND p.user_id = $3 \
                   AND p.state = 'ready' AND NOT EXISTS ( \
                     SELECT 1 FROM offline_package_leases \
                     WHERE package_id = p.id AND token_hash != $1) \
                 ON CONFLICT(package_id) DO NOTHING"
                    .to_owned(),
                params!(token_hash, package_id, user_id),
            ),
            (
                "INSERT INTO offline_package_leases \
                    (token_hash, package_id, created_at, last_access_at, expires_at) \
                 SELECT token_hash, package_id, $1, $1, $2 FROM offline_lease_guards \
                 WHERE package_id = $3 AND had_lease = 0"
                    .to_owned(),
                params!(now, expires_at, package_id),
            ),
            (
                "UPDATE offline_package_leases SET last_access_at = $1, expires_at = $2 \
                 WHERE package_id = $3 AND token_hash = (SELECT token_hash \
                     FROM offline_lease_guards WHERE package_id = $3)"
                    .to_owned(),
                params!(now, expires_at, package_id),
            ),
            (
                "UPDATE offline_packages SET last_access_at = $1, expires_at = $2, \
                     updated_at = $1 WHERE id = $3 \
                   AND EXISTS (SELECT 1 FROM offline_lease_guards WHERE package_id = $3)"
                    .to_owned(),
                params!(now, expires_at, package_id),
            ),
            (
                "DELETE FROM offline_lease_guards WHERE package_id = $1".to_owned(),
                params!(package_id),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let results = self
            .client()
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.first().copied().unwrap_or(0) == 0 {
            let state = self
                .client()
                .query_consistent_map::<StateRow, _>(
                    "SELECT state FROM offline_packages WHERE id = $1 AND user_id = $2",
                    params!(package_id, user_id),
                )
                .await
                .map_err(database_error)?
                .into_iter()
                .next()
                .map(|row| row.state);
            return Ok(if state.as_deref() == Some("ready") {
                OfflineLeaseOutcome::TokenConflict
            } else {
                OfflineLeaseOutcome::PackageNotReady
            });
        }
        let lease = self
            .client()
            .query_consistent_map::<OfflineLeaseRow, _>(
                "SELECT token_hash, package_id, created_at, last_access_at, expires_at \
                 FROM offline_package_leases WHERE package_id = $1",
                params!(package_id),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .map(Into::into)
            .ok_or_else(|| StoreError::Database("lease transaction returned no row".to_owned()))?;
        Ok(if results.get(1).copied().unwrap_or(0) > 0 {
            OfflineLeaseOutcome::Created(lease)
        } else {
            OfflineLeaseOutcome::Renewed(lease)
        })
    }

    async fn offline_package_for_lease(
        &self,
        token_hash: &str,
        now: i64,
        renewed_expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let query = format!(
            "SELECT {} FROM offline_packages p \
             JOIN offline_package_leases l ON l.package_id = p.id \
             WHERE l.token_hash = $1 AND l.expires_at > $2 AND p.state = 'ready'",
            package_cols("p")
        );
        let package = one_package(
            self.client()
                .query_consistent_map::<OfflinePackageRow, _>(
                    query.clone(),
                    params!(token_hash, now),
                )
                .await
                .map_err(database_error)?,
        );
        let Some(mut current) = package else {
            return Ok(None);
        };
        if now.saturating_sub(current.last_access_at) < 60 {
            return Ok(Some(current));
        }
        let statements = vec![
            (
                "UPDATE offline_package_leases SET last_access_at = $1, expires_at = $2 \
                 WHERE token_hash = $3 AND expires_at > $1"
                    .to_owned(),
                params!(now, renewed_expires_at, token_hash),
            ),
            (
                "UPDATE offline_packages SET last_access_at = $1, expires_at = $2, \
                     updated_at = $1 WHERE id = $3 AND state = 'ready' \
                   AND EXISTS (SELECT 1 FROM offline_package_leases \
                               WHERE package_id = $3 AND last_access_at = $1)"
                    .to_owned(),
                params!(now, renewed_expires_at, current.id.as_str()),
            ),
        ];
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        let results = self
            .client()
            .txn(statements)
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.first().copied().unwrap_or(0) == 0 {
            return Ok(None);
        }
        current.last_access_at = now;
        current.updated_at = now;
        current.expires_at = renewed_expires_at;
        Ok(Some(current))
    }

    async fn mark_offline_package_ready(
        &self,
        package_id: &str,
        node_id: &str,
        recipe_hash: &str,
        actual_bytes: i64,
        duration_ms: i64,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "UPDATE offline_packages SET state = 'ready', phase = 'ready', \
                 progress_millis = 1000, recipe_hash = $1, actual_bytes = $2, \
                 duration_ms = $3, error_code = NULL, error_message = NULL, \
                 updated_at = $4, last_access_at = $4 \
                 WHERE id = $5 AND node_id = $6 AND state IN ('queued', 'preparing')",
                params!(
                    recipe_hash,
                    actual_bytes,
                    duration_ms,
                    now,
                    package_id,
                    node_id
                ),
            )
            .await?
            > 0)
    }

    async fn delete_offline_package(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "DELETE FROM offline_packages WHERE id = $1 AND user_id = $2",
                params!(package_id, user_id),
            )
            .await?
            > 0)
    }

    async fn expire_offline_packages(&self, now: i64) -> Result<u64, StoreError> {
        Ok(self
            .execute(
                "DELETE FROM offline_packages WHERE expires_at <= $1",
                params!(now),
            )
            .await? as u64)
    }

    // --- Node removal (`CLUSTERING-PLAN.md` §6.7) -------------------------

    async fn unresolved_offline_packages(
        &self,
        node_id: &str,
    ) -> Result<Vec<OfflinePackage>, StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<OfflinePackageRow, _>(
                format!(
                    "SELECT {PACKAGE_COLS} FROM offline_packages WHERE node_id = $1 \
                     AND state IN ('queued', 'preparing', 'ready') ORDER BY created_at, id"
                ),
                params!(node_id),
            )
            .await
            .map_err(database_error)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn offline_transfers_in_flight(
        &self,
        node_id: &str,
        now: i64,
        active_since: i64,
    ) -> Result<i64, StoreError> {
        let row = self
            .client()
            .query_consistent_map::<CountRow, _>(
                "SELECT COUNT(*) AS count FROM offline_package_leases l \
                 JOIN offline_packages p ON p.id = l.package_id \
                 WHERE p.node_id = $1 AND p.state = 'ready' \
                   AND l.expires_at > $2 AND l.last_access_at >= $3",
                params!(node_id, now, active_since),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                StoreError::Database("offline transfer count returned no row".to_owned())
            })?;
        Ok(row.count)
    }

    async fn request_offline_source_probes(
        &self,
        package_ids: &[String],
        node_ids: &[String],
        now: i64,
    ) -> Result<u64, StoreError> {
        if package_ids.is_empty() || node_ids.is_empty() {
            return Ok(0);
        }
        // Re-asking clears any earlier answer in the same statement. A "yes"
        // recorded before this removal describes a mount that may since have
        // gone; treating it as evidence is exactly the silent reassignment
        // §6.7 forbids.
        let statements = package_ids
            .iter()
            .flat_map(|package_id| {
                node_ids.iter().map(move |node_id| {
                    (
                        "INSERT INTO offline_source_probes \
                           (package_id, node_id, requested_at, answered_at, readable) \
                         VALUES ($1, $2, $3, NULL, NULL) \
                         ON CONFLICT(package_id, node_id) DO UPDATE SET \
                           requested_at = excluded.requested_at, \
                           answered_at = NULL, readable = NULL",
                        params!(package_id.as_str(), node_id.as_str(), now),
                    )
                })
            })
            .collect::<Vec<_>>();
        let mut requested = 0_u64;
        for result in self.client().txn(statements).await? {
            requested += result.map_err(database_error)? as u64;
        }
        Ok(requested)
    }

    async fn pending_offline_source_probes(
        &self,
        node_id: &str,
        requested_since: i64,
    ) -> Result<Vec<OfflinePackage>, StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<OfflinePackageRow, _>(
                format!(
                    "SELECT {} FROM offline_packages p \
                     JOIN offline_source_probes probe ON probe.package_id = p.id \
                     WHERE probe.node_id = $1 AND probe.answered_at IS NULL \
                       AND probe.requested_at >= $2 \
                     ORDER BY probe.requested_at, p.id",
                    package_cols("p")
                ),
                params!(node_id, requested_since),
            )
            .await
            .map_err(database_error)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn answer_offline_source_probe(
        &self,
        package_id: &str,
        node_id: &str,
        readable: bool,
        now: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "UPDATE offline_source_probes SET answered_at = $1, readable = $2 \
                 WHERE package_id = $3 AND node_id = $4 AND answered_at IS NULL",
                params!(now, i64::from(readable), package_id, node_id),
            )
            .await?
            > 0)
    }

    async fn outstanding_offline_source_probes(
        &self,
        requested_at: i64,
    ) -> Result<i64, StoreError> {
        let row = self
            .client()
            .query_consistent_map::<CountRow, _>(
                "SELECT COUNT(*) AS count FROM offline_source_probes \
                 WHERE requested_at >= $1 AND answered_at IS NULL",
                params!(requested_at),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                StoreError::Database("outstanding probe count returned no row".to_owned())
            })?;
        Ok(row.count)
    }

    async fn verified_offline_source_nodes(
        &self,
        package_id: &str,
        requested_since: i64,
    ) -> Result<Vec<String>, StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<NodeIdRow, _>(
                // Freshness keys on `requested_at`, not `answered_at`, and the
                // difference is load-bearing. Both columns are wall-clock
                // seconds, but `requested_at` is written by the node running
                // the removal while `answered_at` is written by the node that
                // answered. Comparing across those two clocks would let a
                // survivor that runs a second behind have a perfectly good
                // answer thrown away, failing a package `node_removed` that
                // should have been re-homed.
                //
                // No freshness is lost: re-asking clears `answered_at`, so a
                // non-NULL answer on a row this removal asked for can only be
                // this removal's answer.
                "SELECT node_id FROM offline_source_probes \
                 WHERE package_id = $1 AND readable = 1 \
                   AND answered_at IS NOT NULL AND requested_at >= $2 \
                 ORDER BY node_id",
                params!(package_id, requested_since),
            )
            .await
            .map_err(database_error)?;
        Ok(rows.into_iter().map(|row| row.node_id).collect())
    }

    async fn resolve_offline_packages_for_removal(
        &self,
        node_id: &str,
        plan: &[OfflineRemovalPlanEntry],
        now: i64,
    ) -> Result<OfflineRemovalReport, StoreError> {
        if plan.is_empty() {
            return Ok(OfflineRemovalReport::default());
        }
        let mut statements = Vec::with_capacity(plan.len());
        for entry in plan {
            match entry.requeue_to.as_deref() {
                // Re-homing moves ownership of a task and nothing else. The
                // operator's file is untouched; only `node_id` moves, and only
                // onto a node that answered a probe by reading that file.
                //
                // `state = 'queued'` puts it back at the front of the ordinary
                // queue rather than inventing a re-homed state, so the new
                // owner claims it through the one code path that already
                // re-checks the source before spending an encoder on it.
                Some(target) => statements.push((
                    "UPDATE offline_packages SET node_id = $1, state = 'queued', \
                     phase = 'waiting_for_encoder', recipe_hash = NULL, progress_millis = 0, \
                     error_code = NULL, error_message = NULL, updated_at = $2 \
                     WHERE id = $3 AND node_id = $4 \
                       AND state IN ('queued', 'preparing')",
                    params!(target, now, entry.package_id.as_str(), node_id),
                )),
                // Releasing the reservation is the point, not bookkeeping: the
                // bytes it accounted for were on the node that just left, so
                // holding the user's and the node's byte budget against them
                // until the seven-day expiry charges a traveller for storage
                // nobody has.
                None => statements.push((
                    "UPDATE offline_packages SET state = 'failed', phase = 'node_removed', \
                     error_code = $1, error_message = $2, reserved_bytes = 0, \
                     actual_bytes = NULL, updated_at = $3 \
                     WHERE id = $4 AND node_id = $5 \
                       AND state IN ('queued', 'preparing', 'ready')",
                    params!(
                        OFFLINE_NODE_REMOVED_CODE,
                        NODE_REMOVED_MESSAGE,
                        now,
                        entry.package_id.as_str(),
                        node_id
                    ),
                )),
            }
        }
        let results = self.client().txn(statements).await?;
        let mut report = OfflineRemovalReport::default();
        for (entry, result) in plan.iter().zip(results) {
            let changed = result.map_err(database_error)?;
            if changed == 0 {
                // The whole transaction committed or none of it did, so a
                // no-op row means the package moved underneath the plan. Say
                // so instead of reporting a resolution that did not happen:
                // removal treats this as unresolved and refuses.
                return Err(StoreError::Database(format!(
                    "offline package {} changed while its removal plan was being applied",
                    entry.package_id
                )));
            }
            if entry.requeue_to.is_some() {
                report.requeued += 1;
            } else {
                report.failed += 1;
            }
        }
        Ok(report)
    }
}

/// The one operator- and client-visible sentence for a package that lost its
/// node. Deliberately says nothing about the media: a path is exactly the kind
/// of private fact that leaks through an error string.
const NODE_REMOVED_MESSAGE: &str =
    "The server that was preparing this download was removed from the cluster. \
     Request it again to prepare it on a remaining server.";

struct CountRow {
    count: i64,
}

impl From<&mut Row<'_>> for CountRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            count: row.get("count"),
        }
    }
}

struct NodeIdRow {
    node_id: String,
}

impl From<&mut Row<'_>> for NodeIdRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            node_id: row.get("node_id"),
        }
    }
}

struct StateRow {
    state: String,
}

impl From<&mut Row<'_>> for StateRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            state: row.get("state"),
        }
    }
}

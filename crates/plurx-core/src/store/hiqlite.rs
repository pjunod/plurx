//! The first replicated store slice: settings and authentication state.
//!
//! This module intentionally implements only [`SettingsStore`], [`UserStore`],
//! and [`ApiKeyStore`]. SQLite remains the daemon's complete [`Store`](super::Store)
//! until the later M1 slices port every remaining trait. The narrow type keeps
//! that boundary honest: it cannot be placed behind `Arc<dyn Store>` yet.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::{Client, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::replicated::ReplicatedSql;
use super::{keys, ApiKeyStore, SettingsStore, UserStore};
use crate::domain::{ApiKey, User};
use crate::error::StoreError;

pub const AUTH_SCHEMA_VERSION: i64 = 3;
pub const AUTH_PROTOCOL_VERSION: i64 = 3;

const READY_TIMEOUT: Duration = Duration::from_secs(3);

const AUTH_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cluster_meta (
    singleton        INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version   INTEGER NOT NULL,
    protocol_min     INTEGER NOT NULL,
    protocol_max     INTEGER NOT NULL,
    migrated_at      INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    is_admin      INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS tokens (
    token_hash   TEXT PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device       TEXT,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS api_keys (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    key_hash     TEXT NOT NULL UNIQUE,
    scopes       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    disabled     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS api_keys_hash ON api_keys(key_hash);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterCompatibility {
    pub schema_version: i64,
    pub protocol_version: i64,
}

impl ClusterCompatibility {
    pub const CURRENT: Self = Self {
        schema_version: AUTH_SCHEMA_VERSION,
        protocol_version: AUTH_PROTOCOL_VERSION,
    };
}

/// A hiqlite client restricted to the first replicated Store slice.
#[derive(Clone)]
pub struct HiqliteAuthStore {
    pub(super) client: Client,
    clock: Arc<dyn Clock>,
}

impl HiqliteAuthStore {
    /// Create the M1b schema on a fresh cluster and seed its logical identity.
    ///
    /// Only the bootstrap coordinator calls this. Other voters call [`open`]
    /// after the acknowledged schema write has replicated.
    pub async fn bootstrap(client: Client, instance_id: &str) -> Result<Self, StoreError> {
        validate_sql(AUTH_SCHEMA)?;
        let results = client.batch(AUTH_SCHEMA).await.map_err(database_error)?;
        for result in results {
            result.map_err(database_error)?;
        }
        super::hiqlite_catalog::install_schema(&client).await?;
        super::hiqlite_durable::install_schema(&client).await?;

        let store = Self::with_clock(client, Arc::new(SystemClock));
        let now = store.now()?;
        store
            .execute(
                "INSERT INTO cluster_meta \
                 (singleton, schema_version, protocol_min, protocol_max, migrated_at) \
                 VALUES (1, $1, $2, $2, $3) \
                 ON CONFLICT(singleton) DO NOTHING",
                params!(AUTH_SCHEMA_VERSION, AUTH_PROTOCOL_VERSION, now),
            )
            .await?;
        store
            .verify_compatibility(ClusterCompatibility::CURRENT)
            .await?;
        store.put_setting(keys::INSTANCE_ID, instance_id).await?;
        Ok(store)
    }

    /// Open an already-bootstrapped cluster, refusing incompatible state.
    pub async fn open(client: Client) -> Result<Self, StoreError> {
        let store = Self::with_clock(client, Arc::new(SystemClock));
        store
            .verify_compatibility(ClusterCompatibility::CURRENT)
            .await?;
        Ok(store)
    }

    /// Run the compatibility guard through a remote client *before* starting
    /// this process's raft voter. A rejected process therefore cannot migrate
    /// state, vote, or become leader.
    pub async fn preflight_voter(
        remote: &Client,
        supported: ClusterCompatibility,
    ) -> Result<(), StoreError> {
        let meta = timeout_consistent(remote.query_consistent_map::<CompatibilityRow, _>(
            "SELECT schema_version, protocol_min, protocol_max \
                 FROM cluster_meta WHERE singleton = 1",
            params!(),
        ))
        .await?;
        verify_compatibility_rows(meta, supported)
    }

    pub async fn verify_compatibility(
        &self,
        supported: ClusterCompatibility,
    ) -> Result<(), StoreError> {
        Self::preflight_voter(&self.client, supported).await
    }

    /// Hash ordered local state for the separate-process replica-equality gate.
    /// This is deliberately not a consistent leader read: the check needs to
    /// observe each voter's own applied SQLite state. Only the digest leaves
    /// this process; password and credential hashes never enter test logs.
    pub async fn local_dump_digest(&self) -> Result<String, StoreError> {
        let dump = AuthStoreDump {
            catalog_digest: super::hiqlite_catalog::local_catalog_digest(&self.client).await?,
            durable_digest: super::hiqlite_durable::local_durable_digest(&self.client).await?,
            cluster_meta: self
                .client
                .query_map(
                    "SELECT singleton, schema_version, protocol_min, protocol_max, migrated_at \
                     FROM cluster_meta ORDER BY singleton",
                    params!(),
                )
                .await
                .map_err(database_error)?,
            settings: self
                .client
                .query_map(
                    "SELECT key, value, updated_at FROM settings ORDER BY key",
                    params!(),
                )
                .await
                .map_err(database_error)?,
            users: self
                .client
                .query_map(
                    "SELECT id, username, password_hash, is_admin, created_at \
                     FROM users ORDER BY id",
                    params!(),
                )
                .await
                .map_err(database_error)?,
            tokens: self
                .client
                .query_map(
                    "SELECT token_hash, user_id, device, created_at, last_seen_at \
                     FROM tokens ORDER BY token_hash",
                    params!(),
                )
                .await
                .map_err(database_error)?,
            api_keys: self
                .client
                .query_map(
                    "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled \
                     FROM api_keys ORDER BY id",
                    params!(),
                )
                .await
                .map_err(database_error)?,
        };
        let bytes = serde_json::to_vec(&dump).map_err(database_error)?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    fn with_clock(client: Client, clock: Arc<dyn Clock>) -> Self {
        Self { client, clock }
    }

    pub(super) fn now(&self) -> Result<i64, StoreError> {
        self.clock.now()
    }

    pub(super) async fn execute(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<usize, StoreError> {
        validate_sql(sql)?;
        self.client
            .execute(sql, params)
            .await
            .map_err(database_error)
    }

    async fn user_optional(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<Option<User>, StoreError> {
        let mut rows = self
            .client
            .query_consistent_map::<UserRow, _>(sql, params)
            .await
            .map_err(database_error)?;
        Ok(rows.pop().map(Into::into))
    }

    async fn key_optional(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<Option<ApiKey>, StoreError> {
        let mut rows = self
            .client
            .query_consistent_map::<ApiKeyRow, _>(sql, params)
            .await
            .map_err(database_error)?;
        Ok(rows.pop().map(Into::into))
    }
}

#[async_trait]
impl SettingsStore for HiqliteAuthStore {
    async fn ping(&self) -> Result<(), StoreError> {
        self.client.is_healthy_db().await.map_err(database_error)?;
        let rows = timeout_consistent(
            self.client
                .query_consistent_map::<PingRow, _>("SELECT 1 AS healthy", params!()),
        )
        .await?;
        if rows.len() == 1 && rows[0].healthy == 1 {
            Ok(())
        } else {
            Err(StoreError::Database(
                "replicated readiness query returned an invalid result".to_owned(),
            ))
        }
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let rows = self
            .client
            .query_consistent_map::<SettingValueRow, _>(
                "SELECT value FROM settings WHERE key = $1",
                params!(key),
            )
            .await
            .map_err(database_error)?;
        Ok(rows.into_iter().next().map(|row| row.value))
    }

    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
             ON CONFLICT(key) DO UPDATE SET \
             value = excluded.value, updated_at = excluded.updated_at",
            params!(key, value, now),
        )
        .await?;
        Ok(())
    }

    async fn instance_id(&self) -> Result<String, StoreError> {
        self.get_setting(keys::INSTANCE_ID).await?.ok_or_else(|| {
            StoreError::Database("instance.id missing — migration invariant broken".to_owned())
        })
    }
}

#[async_trait]
impl UserStore for HiqliteAuthStore {
    async fn count_users(&self) -> Result<i64, StoreError> {
        let rows = self
            .client
            .query_consistent_map::<CountRow, _>("SELECT COUNT(*) AS count FROM users", params!())
            .await
            .map_err(database_error)?;
        one_count(rows)
    }

    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<User, StoreError> {
        let now = self.now()?;
        let sql = "INSERT INTO users \
                   (username, password_hash, is_admin, created_at) \
                   VALUES ($1, $2, $3, $4) \
                   RETURNING id, username, password_hash, is_admin, created_at";
        validate_sql(sql)?;
        let row = self
            .client
            .execute_returning_map_one::<_, UserRow>(
                sql,
                params!(username, password_hash, is_admin, now),
            )
            .await
            .map_err(database_error)?;
        Ok(row.into())
    }

    async fn get_user(&self, id: i64) -> Result<Option<User>, StoreError> {
        self.user_optional(
            "SELECT id, username, password_hash, is_admin, created_at \
             FROM users WHERE id = $1",
            params!(id),
        )
        .await
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, StoreError> {
        self.user_optional(
            "SELECT id, username, password_hash, is_admin, created_at \
             FROM users WHERE username = $1",
            params!(username),
        )
        .await
    }

    async fn list_users(&self) -> Result<Vec<User>, StoreError> {
        Ok(self
            .client
            .query_consistent_map::<UserRow, _>(
                "SELECT id, username, password_hash, is_admin, created_at \
                 FROM users ORDER BY username",
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn delete_user(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .execute("DELETE FROM users WHERE id = $1", params!(id))
            .await?
            > 0)
    }

    async fn count_admins(&self) -> Result<i64, StoreError> {
        let rows = self
            .client
            .query_consistent_map::<CountRow, _>(
                "SELECT COUNT(*) AS count FROM users WHERE is_admin = 1",
                params!(),
            )
            .await
            .map_err(database_error)?;
        one_count(rows)
    }

    async fn set_password(&self, id: i64, password_hash: &str) -> Result<bool, StoreError> {
        // hiqlite binds by SQLite parameter index, and `$N` is a named
        // parameter whose index follows first appearance. Keep `$1` first in
        // the SQL even when the SET clause precedes the predicate.
        Ok(self
            .execute(
                "UPDATE users SET password_hash = $1 WHERE id = $2",
                params!(password_hash, id),
            )
            .await?
            > 0)
    }

    async fn set_admin(&self, id: i64, is_admin: bool) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "UPDATE users SET is_admin = $1 WHERE id = $2",
                params!(is_admin, id),
            )
            .await?
            > 0)
    }

    async fn delete_tokens_for_user(&self, user_id: i64) -> Result<u64, StoreError> {
        Ok(self
            .execute("DELETE FROM tokens WHERE user_id = $1", params!(user_id))
            .await? as u64)
    }

    async fn create_token(
        &self,
        token_hash: &str,
        user_id: i64,
        device: Option<&str>,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "INSERT INTO tokens \
             (token_hash, user_id, device, created_at, last_seen_at) \
             VALUES ($1, $2, $3, $4, $4)",
            params!(token_hash, user_id, device, now),
        )
        .await?;
        Ok(())
    }

    async fn user_for_token(&self, token_hash: &str) -> Result<Option<User>, StoreError> {
        let user = self
            .user_optional(
                "SELECT u.id, u.username, u.password_hash, u.is_admin, u.created_at \
                 FROM users u JOIN tokens t ON t.user_id = u.id \
                 WHERE t.token_hash = $1",
                params!(token_hash),
            )
            .await?;
        if user.is_some() {
            let now = self.now()?;
            self.execute(
                "UPDATE tokens SET last_seen_at = $1 \
                 WHERE token_hash = $2 AND last_seen_at < $3",
                params!(now, token_hash, now.saturating_sub(60)),
            )
            .await?;
        }
        Ok(user)
    }

    async fn delete_token(&self, token_hash: &str) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "DELETE FROM tokens WHERE token_hash = $1",
                params!(token_hash),
            )
            .await?
            > 0)
    }
}

#[async_trait]
impl ApiKeyStore for HiqliteAuthStore {
    async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        scopes: &[String],
    ) -> Result<ApiKey, StoreError> {
        let now = self.now()?;
        let scopes = serde_json::to_string(scopes)
            .map_err(|error| StoreError::Database(error.to_string()))?;
        let sql = "INSERT INTO api_keys \
                   (name, key_hash, scopes, created_at, disabled) \
                   VALUES ($1, $2, $3, $4, 0) \
                   RETURNING id, name, key_hash, scopes, created_at, last_used_at, disabled";
        validate_sql(sql)?;
        let row = self
            .client
            .execute_returning_map_one::<_, ApiKeyRow>(sql, params!(name, key_hash, scopes, now))
            .await
            .map_err(database_error)?;
        Ok(row.into())
    }

    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        Ok(self
            .client
            .query_consistent_map::<ApiKeyRow, _>(
                "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled \
                 FROM api_keys ORDER BY created_at, id",
                params!(),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn api_key_for_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError> {
        self.key_optional(
            "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled \
             FROM api_keys WHERE key_hash = $1",
            params!(key_hash),
        )
        .await
    }

    async fn touch_api_key(&self, id: i64) -> Result<(), StoreError> {
        let now = self.now()?;
        self.execute(
            "UPDATE api_keys SET last_used_at = $1 WHERE id = $2",
            params!(now, id),
        )
        .await?;
        Ok(())
    }

    async fn delete_api_key(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .execute("DELETE FROM api_keys WHERE id = $1", params!(id))
            .await?
            > 0)
    }

    async fn set_api_key_disabled(&self, id: i64, disabled: bool) -> Result<bool, StoreError> {
        Ok(self
            .execute(
                "UPDATE api_keys SET disabled = $1 WHERE id = $2",
                params!(disabled, id),
            )
            .await?
            > 0)
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> Result<i64, StoreError>;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<i64, StoreError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| StoreError::Task(format!("system clock before unix epoch: {error}")))?
            .as_secs();
        i64::try_from(seconds)
            .map_err(|_| StoreError::Task("system clock exceeds i64 unix seconds".to_owned()))
    }
}

pub(super) fn validate_sql(sql: &str) -> Result<(), StoreError> {
    ReplicatedSql::new(sql)
        .map(|_| ())
        .map_err(|error| StoreError::Database(error.to_string()))?;
    validate_parameter_order(sql)
}

/// hiqlite binds parameters with rusqlite's numeric parameter index. SQLite
/// treats `$N` as a *named* parameter, so its index is assigned by first
/// appearance rather than by the number after `$`. Requiring first appearance
/// to be `$1`, `$2`, ... keeps `params!(...)` aligned with the statement.
fn validate_parameter_order(sql: &str) -> Result<(), StoreError> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut next = 1_u32;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'$' if bytes.get(index + 1).is_some_and(u8::is_ascii_digit) => {
                let start = index + 1;
                index = start;
                while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                let ordinal = sql[start..index]
                    .parse::<u32>()
                    .map_err(|error| StoreError::Database(error.to_string()))?;
                if ordinal == next {
                    next += 1;
                } else if ordinal >= next || ordinal == 0 {
                    return Err(StoreError::Database(format!(
                        "hiqlite placeholders must first appear in order; expected ${next}, found ${ordinal}"
                    )));
                }
            }
            _ => index += 1,
        }
    }
    Ok(())
}

pub(super) fn database_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Database(error.to_string())
}

async fn timeout_consistent<T>(
    operation: impl std::future::Future<Output = Result<T, hiqlite::Error>>,
) -> Result<T, StoreError> {
    tokio::time::timeout(READY_TIMEOUT, operation)
        .await
        .map_err(|_| StoreError::Database("cluster quorum check timed out".to_owned()))?
        .map_err(database_error)
}

fn one_count(rows: Vec<CountRow>) -> Result<i64, StoreError> {
    if rows.len() == 1 {
        Ok(rows[0].count)
    } else {
        Err(StoreError::Database(format!(
            "count query returned {} rows",
            rows.len()
        )))
    }
}

fn verify_compatibility_rows(
    rows: Vec<CompatibilityRow>,
    supported: ClusterCompatibility,
) -> Result<(), StoreError> {
    let [meta] = rows.as_slice() else {
        return Err(StoreError::Migration(format!(
            "cluster compatibility metadata returned {} rows",
            rows.len()
        )));
    };
    if meta.schema_version != supported.schema_version {
        return Err(StoreError::Migration(format!(
            "cluster schema {} is incompatible with voter schema {}",
            meta.schema_version, supported.schema_version
        )));
    }
    if !(meta.protocol_min..=meta.protocol_max).contains(&supported.protocol_version) {
        return Err(StoreError::Migration(format!(
            "cluster protocol range {}..={} excludes voter protocol {}",
            meta.protocol_min, meta.protocol_max, supported.protocol_version
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct AuthStoreDump {
    catalog_digest: String,
    durable_digest: String,
    cluster_meta: Vec<ClusterMetaDumpRow>,
    settings: Vec<SettingDumpRow>,
    users: Vec<UserDumpRow>,
    tokens: Vec<TokenDumpRow>,
    api_keys: Vec<ApiKeyDumpRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CompatibilityRow {
    schema_version: i64,
    protocol_min: i64,
    protocol_max: i64,
}

impl From<&mut Row<'_>> for CompatibilityRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            schema_version: row.get("schema_version"),
            protocol_min: row.get("protocol_min"),
            protocol_max: row.get("protocol_max"),
        }
    }
}

struct PingRow {
    healthy: i64,
}

impl From<&mut Row<'_>> for PingRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            healthy: row.get("healthy"),
        }
    }
}

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

struct SettingValueRow {
    value: String,
}

impl From<&mut Row<'_>> for SettingValueRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

struct UserRow {
    id: i64,
    username: String,
    password_hash: String,
    is_admin: bool,
    created_at: i64,
}

impl From<&mut Row<'_>> for UserRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            username: row.get("username"),
            password_hash: row.get("password_hash"),
            is_admin: row.get::<i64>("is_admin") != 0,
            created_at: row.get("created_at"),
        }
    }
}

impl From<UserRow> for User {
    fn from(row: UserRow) -> Self {
        Self {
            id: row.id,
            username: row.username,
            password_hash: row.password_hash,
            is_admin: row.is_admin,
            created_at: row.created_at,
        }
    }
}

struct ApiKeyRow {
    id: i64,
    name: String,
    key_hash: String,
    scopes: String,
    created_at: i64,
    last_used_at: Option<i64>,
    disabled: bool,
}

impl From<&mut Row<'_>> for ApiKeyRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            id: row.get("id"),
            name: row.get("name"),
            key_hash: row.get("key_hash"),
            scopes: row.get("scopes"),
            created_at: row.get("created_at"),
            last_used_at: row.get("last_used_at"),
            disabled: row.get::<i64>("disabled") != 0,
        }
    }
}

impl From<ApiKeyRow> for ApiKey {
    fn from(row: ApiKeyRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            key_hash: row.key_hash,
            // Corrupt scope data must fail closed, as in the SQLite backend.
            scopes: serde_json::from_str(&row.scopes).unwrap_or_default(),
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            disabled: row.disabled,
        }
    }
}

macro_rules! dump_row {
    ($name:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct $name { $( $field: $ty ),+ }

        impl From<&mut Row<'_>> for $name {
            fn from(row: &mut Row<'_>) -> Self {
                Self { $( $field: row.get(stringify!($field)) ),+ }
            }
        }
    };
}

dump_row!(ClusterMetaDumpRow {
    singleton: i64,
    schema_version: i64,
    protocol_min: i64,
    protocol_max: i64,
    migrated_at: i64,
});
dump_row!(SettingDumpRow {
    key: String,
    value: String,
    updated_at: i64,
});
dump_row!(UserDumpRow {
    id: i64,
    username: String,
    password_hash: String,
    is_admin: i64,
    created_at: i64,
});
dump_row!(TokenDumpRow {
    token_hash: String,
    user_id: i64,
    device: Option<String>,
    created_at: i64,
    last_seen_at: i64,
});
dump_row!(ApiKeyDumpRow {
    id: i64,
    name: String,
    key_hash: String,
    scopes: String,
    created_at: i64,
    last_used_at: Option<i64>,
    disabled: i64,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_auth_schema_and_writes_bind_every_clock_value() {
        validate_sql(AUTH_SCHEMA).expect("schema has no connection-local values");
        for sql in [
            "INSERT INTO settings VALUES ($1, $2, $3)",
            "INSERT INTO users VALUES ($1, $2, $3, $4, $5)",
            "INSERT INTO tokens VALUES ($1, $2, $3, $4, $4)",
            "INSERT INTO api_keys VALUES ($1, $2, $3, $4, $5, $6, $7)",
        ] {
            validate_sql(sql).expect("bound replicated write");
        }

        let error = validate_sql("UPDATE users SET password_hash = $2 WHERE id = $1")
            .expect_err("out-of-order named parameters");
        assert!(error.to_string().contains("expected $1, found $2"));
        validate_sql("UPDATE users SET password_hash = $1 WHERE id = $2")
            .expect("first-appearance order");
        validate_sql("SELECT '$2', id FROM users WHERE id = $1 -- $3")
            .expect("quoted and commented placeholders are data");
    }

    #[test]
    fn compatibility_rejects_schema_and_protocol_drift() {
        let current = CompatibilityRow {
            schema_version: AUTH_SCHEMA_VERSION,
            protocol_min: AUTH_PROTOCOL_VERSION,
            protocol_max: AUTH_PROTOCOL_VERSION,
        };
        verify_compatibility_rows(vec![current], ClusterCompatibility::CURRENT)
            .expect("current voter");

        let old_schema = ClusterCompatibility {
            schema_version: AUTH_SCHEMA_VERSION - 1,
            protocol_version: AUTH_PROTOCOL_VERSION,
        };
        let error = verify_compatibility_rows(
            vec![CompatibilityRow {
                schema_version: AUTH_SCHEMA_VERSION,
                protocol_min: AUTH_PROTOCOL_VERSION,
                protocol_max: AUTH_PROTOCOL_VERSION,
            }],
            old_schema,
        )
        .expect_err("old schema must refuse");
        assert!(error.to_string().contains("incompatible"));

        let old_protocol = ClusterCompatibility {
            schema_version: AUTH_SCHEMA_VERSION,
            protocol_version: AUTH_PROTOCOL_VERSION - 1,
        };
        let error = verify_compatibility_rows(
            vec![CompatibilityRow {
                schema_version: AUTH_SCHEMA_VERSION,
                protocol_min: AUTH_PROTOCOL_VERSION,
                protocol_max: AUTH_PROTOCOL_VERSION,
            }],
            old_protocol,
        )
        .expect_err("old protocol must refuse");
        assert!(error.to_string().contains("excludes"));
    }
}

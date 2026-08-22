//! The complete replicated durable-store backend.
//!
//! The trait implementations are split across this module and the sibling
//! catalogue, media, and durable modules. SQLite remains the daemon's selected
//! [`Store`](super::Store) until M2 imports existing state and activates this
//! backend; backend completeness alone is not permission to skip that gate.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::{Client, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::replicated::ReplicatedSql;
use super::telemetry::NodeLocalTelemetry;
use super::{
    keys, ApiKeyStore, ArtworkRepairFence, NetworkPriorStore, PlaybackTelemetryStore,
    SettingsStore, UserStore,
};
use crate::domain::{
    ApiKey, NetworkPrior, NetworkPriorObservation, PlaybackEvent, PlaybackEventQuery, User,
};
use crate::error::StoreError;

// v6 adds revision-bound ebook reading state; v7 adds first-class book facts;
// v8 adds monotone cluster-work leases. Every additive step is applied through
// Raft before the daemon opens the
// store. v5 remains a supported direct-upgrade source so an offline node is
// not forced to install every intermediate Cinema release; older or future
// schemas still fail closed.
pub const AUTH_SCHEMA_VERSION: i64 = 8;
/// Oldest schema this binary can advance through the complete migration chain.
pub const AUTH_SCHEMA_MIGRATION_SOURCE: i64 = 5;
const READING_SCHEMA_VERSION: i64 = 6;
const BOOK_SCHEMA_MIGRATION_SOURCE: i64 = READING_SCHEMA_VERSION;
const LEASE_SCHEMA_MIGRATION_SOURCE: i64 = 7;
pub const AUTH_PROTOCOL_VERSION: i64 = 4;

const STORE_TIMEOUT: Duration = Duration::from_secs(3);

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

CREATE TABLE IF NOT EXISTS job_leases (
    resource       TEXT PRIMARY KEY,
    owner_node_id  TEXT NOT NULL,
    fence          INTEGER NOT NULL CHECK (fence > 0),
    revision       INTEGER NOT NULL CHECK (revision > 0),
    expires_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
) STRICT;
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

/// A hiqlite client implementing the complete replicated [`Store`](super::Store).
#[derive(Clone)]
pub struct HiqliteAuthStore {
    client: TimedClient,
    clock: Arc<dyn Clock>,
    telemetry: NodeLocalTelemetry,
    activity_refreshes: Arc<ActivityRefreshGate>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ActivityCredential {
    Token(String),
    ApiKey(i64),
}

#[derive(Default)]
struct ActivityRefreshGate {
    reservations: Mutex<HashMap<ActivityCredential, i64>>,
}

impl ActivityRefreshGate {
    fn try_reserve(
        self: &Arc<Self>,
        credential: ActivityCredential,
        now: i64,
    ) -> Option<ActivityRefreshReservation> {
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reservations
            .retain(|_, reserved_at| !crate::auth::activity_refresh_due(Some(*reserved_at), now));
        if reservations.contains_key(&credential) {
            return None;
        }
        reservations.insert(credential.clone(), now);
        Some(ActivityRefreshReservation {
            gate: Arc::clone(self),
            credential,
            reserved_at: now,
            retained: false,
        })
    }

    fn release(&self, credential: &ActivityCredential, reserved_at: i64) {
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reservations.get(credential) == Some(&reserved_at) {
            reservations.remove(credential);
        }
    }
}

struct ActivityRefreshReservation {
    gate: Arc<ActivityRefreshGate>,
    credential: ActivityCredential,
    reserved_at: i64,
    retained: bool,
}

impl ActivityRefreshReservation {
    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for ActivityRefreshReservation {
    fn drop(&mut self) {
        if !self.retained {
            self.gate.release(&self.credential, self.reserved_at);
        }
    }
}

/// The only application-facing path to hiqlite. Keeping the timeout at this
/// boundary prevents new catalogue or durable-store calls from accidentally
/// waiting forever on a wedged leader.
#[derive(Clone)]
pub(super) struct TimedClient(TimedClientInner);

#[derive(Clone)]
enum TimedClientInner {
    Connected(Client),
    #[cfg(test)]
    Disconnected,
}

impl TimedClient {
    fn new(client: Client) -> Self {
        Self(TimedClientInner::Connected(client))
    }

    fn inner(&self) -> &Client {
        match &self.0 {
            TimedClientInner::Connected(client) => client,
            #[cfg(test)]
            TimedClientInner::Disconnected => {
                panic!("validation test attempted hiqlite I/O")
            }
        }
    }

    pub(super) async fn query_consistent_map<T, S>(
        &self,
        sql: S,
        params: hiqlite::Params,
    ) -> Result<Vec<T>, StoreError>
    where
        T: for<'a, 'r> From<&'a mut hiqlite::Row<'r>> + Send + 'static,
        S: Into<Cow<'static, str>>,
    {
        let sql = sql.into();
        validate_sql(&sql)?;
        timeout_store(self.inner().query_consistent_map(sql, params)).await
    }

    pub(super) async fn query_map<T, S>(
        &self,
        sql: S,
        params: hiqlite::Params,
    ) -> Result<Vec<T>, StoreError>
    where
        T: for<'a, 'r> From<&'a mut hiqlite::Row<'r>> + Send + 'static,
        S: Into<Cow<'static, str>>,
    {
        let sql = sql.into();
        validate_sql(&sql)?;
        timeout_store(self.inner().query_map(sql, params)).await
    }

    pub(super) async fn execute<S>(
        &self,
        sql: S,
        params: hiqlite::Params,
    ) -> Result<usize, StoreError>
    where
        S: Into<Cow<'static, str>>,
    {
        let sql = sql.into();
        validate_sql(&sql)?;
        timeout_store(self.inner().execute(sql, params)).await
    }

    pub(super) async fn execute_returning_map<S, T>(
        &self,
        sql: S,
        params: hiqlite::Params,
    ) -> Result<Vec<Result<T, hiqlite::Error>>, StoreError>
    where
        S: Into<Cow<'static, str>>,
        T: for<'a, 'r> From<&'a mut hiqlite::Row<'r>> + Send + 'static,
    {
        let sql = sql.into();
        validate_sql(&sql)?;
        timeout_store(self.inner().execute_returning_map(sql, params)).await
    }

    pub(super) async fn execute_returning_map_one<S, T>(
        &self,
        sql: S,
        params: hiqlite::Params,
    ) -> Result<T, StoreError>
    where
        S: Into<Cow<'static, str>>,
        T: for<'a, 'r> From<&'a mut hiqlite::Row<'r>> + Send + 'static,
    {
        let sql = sql.into();
        validate_sql(&sql)?;
        timeout_store(self.inner().execute_returning_map_one(sql, params)).await
    }

    pub(super) async fn txn<C, Q>(
        &self,
        statements: Q,
    ) -> Result<Vec<Result<usize, hiqlite::Error>>, StoreError>
    where
        Q: IntoIterator<Item = (C, hiqlite::Params)>,
        C: Into<Cow<'static, str>>,
    {
        let statements = statements
            .into_iter()
            .map(|(sql, params)| (sql.into(), params))
            .collect::<Vec<_>>();
        for (sql, _) in &statements {
            validate_sql(sql)?;
        }
        timeout_store(self.inner().txn(statements)).await
    }

    pub(super) async fn is_healthy_db(&self) -> Result<(), StoreError> {
        timeout_store(self.inner().is_healthy_db()).await
    }
}

#[cfg(test)]
pub(super) fn disconnected_test_client() -> TimedClient {
    TimedClient(TimedClientInner::Disconnected)
}

impl HiqliteAuthStore {
    pub(super) fn client(&self) -> &TimedClient {
        &self.client
    }

    /// Create the complete durable schema on a fresh cluster and seed its
    /// logical identity.
    ///
    /// Only the bootstrap coordinator calls this. Other voters call [`open`]
    /// after the acknowledged schema write has replicated.
    pub async fn bootstrap(
        client: Client,
        instance_id: &str,
        telemetry_path: &Path,
    ) -> Result<Self, StoreError> {
        Self::bootstrap_with_clock(client, instance_id, telemetry_path, Arc::new(SystemClock)).await
    }

    /// Bootstrap with a fixed clock for deterministic separate-process
    /// replicated-store validation. Production callers use [`Self::bootstrap`].
    #[doc(hidden)]
    pub async fn validation_bootstrap_at(
        client: Client,
        instance_id: &str,
        telemetry_path: &Path,
        now: i64,
    ) -> Result<Self, StoreError> {
        Self::bootstrap_with_clock(
            client,
            instance_id,
            telemetry_path,
            Arc::new(FixedClock(now)),
        )
        .await
    }

    async fn bootstrap_with_clock(
        client: Client,
        instance_id: &str,
        telemetry_path: &Path,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StoreError> {
        validate_sql(AUTH_SCHEMA)?;
        let results = timeout_store(client.batch(AUTH_SCHEMA)).await?;
        for result in results {
            result.map_err(database_error)?;
        }
        super::hiqlite_catalog::install_schema(&client).await?;
        super::hiqlite_durable::install_schema(&client).await?;

        let store = Self::with_clock(client, clock, NodeLocalTelemetry::open(telemetry_path)?);
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
        let now = store.now()?;
        store
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT(key) DO NOTHING",
                params!(keys::INSTANCE_ID, instance_id, now),
            )
            .await?;
        let persisted = store.instance_id().await?;
        if persisted != instance_id {
            return Err(StoreError::Identity(format!(
                "cluster instance.id is {persisted}, refusing bootstrap as {instance_id}"
            )));
        }
        Ok(store)
    }

    /// Open an already-bootstrapped cluster, refusing incompatible state.
    pub async fn open(client: Client, telemetry_path: &Path) -> Result<Self, StoreError> {
        let store = Self::with_clock(
            client,
            Arc::new(SystemClock),
            NodeLocalTelemetry::open(telemetry_path)?,
        );
        store
            .verify_compatibility(ClusterCompatibility::CURRENT)
            .await?;
        Ok(store)
    }

    /// Open the daemon's already-activated cluster, applying every supported
    /// additive schema step through the replicated state machine first.
    ///
    /// This path is intentionally separate from [`open`]: maintenance clients
    /// attach to a running voter and must never become a second migration
    /// coordinator. Every version step is one Raft transaction: a crash leaves
    /// either the prior version or a complete next version, and the next boot
    /// resumes the chain from the durable marker.
    pub async fn open_or_migrate(
        client: Client,
        telemetry_path: &Path,
    ) -> Result<Self, StoreError> {
        let store = Self::with_clock(
            client,
            Arc::new(SystemClock),
            NodeLocalTelemetry::open(telemetry_path)?,
        );
        store.migrate_schema().await?;
        store
            .verify_compatibility(ClusterCompatibility::CURRENT)
            .await?;
        Ok(store)
    }

    async fn migrate_schema(&self) -> Result<(), StoreError> {
        loop {
            let sql = "SELECT schema_version, protocol_min, protocol_max \
                       FROM cluster_meta WHERE singleton = 1";
            let rows = self
                .client()
                .query_consistent_map::<CompatibilityRow, _>(sql, params!())
                .await?;
            match schema_migration_action(&rows, ClusterCompatibility::CURRENT)? {
                SchemaMigrationAction::Current => return Ok(()),
                SchemaMigrationAction::MigrateFrom(AUTH_SCHEMA_MIGRATION_SOURCE) => {
                    let now = self.now()?;
                    let results = self
                        .client()
                        .txn([
                            (
                                super::hiqlite_catalog::READING_STATE_TABLE_SCHEMA,
                                params!(),
                            ),
                            (
                                super::hiqlite_catalog::READING_STATE_INDEX_SCHEMA,
                                params!(),
                            ),
                            (
                                "UPDATE cluster_meta SET schema_version = $1, migrated_at = $2 \
                                 WHERE singleton = 1 AND schema_version = $3",
                                params!(READING_SCHEMA_VERSION, now, AUTH_SCHEMA_MIGRATION_SOURCE),
                            ),
                        ])
                        .await?;
                    results
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(database_error)?;
                }
                SchemaMigrationAction::MigrateFrom(BOOK_SCHEMA_MIGRATION_SOURCE) => {
                    let now = self.now()?;
                    let results = self
                        .client()
                        .txn([
                            (super::hiqlite_catalog::BOOK_AUTHOR_SCHEMA, params!()),
                            (super::hiqlite_catalog::BOOK_WORK_SCHEMA, params!()),
                            (super::hiqlite_catalog::BOOK_EDITION_SCHEMA, params!()),
                            (super::hiqlite_catalog::BOOK_SOURCE_SCHEMA, params!()),
                            (super::hiqlite_catalog::BOOK_WORK_INDEX_SCHEMA, params!()),
                            (
                                "UPDATE cluster_meta SET schema_version = $1, migrated_at = $2 \
                                 WHERE singleton = 1 AND schema_version = $3",
                                params!(
                                    LEASE_SCHEMA_MIGRATION_SOURCE,
                                    now,
                                    BOOK_SCHEMA_MIGRATION_SOURCE
                                ),
                            ),
                        ])
                        .await?;
                    results
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(database_error)?;
                }
                SchemaMigrationAction::MigrateFrom(LEASE_SCHEMA_MIGRATION_SOURCE) => {
                    let now = self.now()?;
                    let results = self
                        .client()
                        .txn([
                            (super::hiqlite_coordination::JOB_LEASES_SCHEMA, params!()),
                            (
                                "UPDATE cluster_meta SET schema_version = $1, migrated_at = $2 \
                                 WHERE singleton = 1 AND schema_version = $3",
                                params!(AUTH_SCHEMA_VERSION, now, LEASE_SCHEMA_MIGRATION_SOURCE),
                            ),
                        ])
                        .await?;
                    results
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(database_error)?;
                }
                SchemaMigrationAction::MigrateFrom(version) => {
                    return Err(StoreError::Migration(format!(
                        "cluster schema {version} has no migration implementation"
                    )));
                }
            }
        }
    }

    /// Run the compatibility guard through a remote client *before* starting
    /// this process's raft voter. A rejected process therefore cannot migrate
    /// state, vote, or become leader.
    pub async fn preflight_voter(
        remote: &Client,
        supported: ClusterCompatibility,
    ) -> Result<(), StoreError> {
        let sql = "SELECT schema_version, protocol_min, protocol_max \
                   FROM cluster_meta WHERE singleton = 1";
        validate_sql(sql)?;
        let meta =
            timeout_store(remote.query_consistent_map::<CompatibilityRow, _>(sql, params!()))
                .await?;
        verify_compatibility_rows(meta, supported)
    }

    pub async fn verify_compatibility(
        &self,
        supported: ClusterCompatibility,
    ) -> Result<(), StoreError> {
        let sql = "SELECT schema_version, protocol_min, protocol_max \
                   FROM cluster_meta WHERE singleton = 1";
        let meta = self
            .client()
            .query_consistent_map::<CompatibilityRow, _>(sql, params!())
            .await?;
        verify_compatibility_rows(meta, supported)
    }

    /// Hash ordered local state for the separate-process replica-equality gate.
    /// This is deliberately not a consistent leader read: the check needs to
    /// observe each voter's own applied SQLite state. Only the digest leaves
    /// this process; password and credential hashes never enter test logs.
    pub async fn local_dump_digest(&self) -> Result<String, StoreError> {
        let dump = self.local_auth_dump().await?;
        let bytes = serde_json::to_vec(&dump).map_err(database_error)?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    /// Full validation payload used only by the separate-process gate. The
    /// controller independently hashes it and checks known fields so replica
    /// equality cannot certify a broken digest implementation.
    pub async fn validation_local_dump(&self) -> Result<String, StoreError> {
        serde_json::to_string(&self.local_auth_dump().await?).map_err(database_error)
    }

    /// Clear mutable application rows between backend-neutral contract cases.
    ///
    /// This is deliberately outside [`Store`](super::Store): production code
    /// must never gain a generic "erase the cluster" operation. The contract
    /// harness keeps the three voter processes alive and serializes calls to
    /// this validation-only helper so every scenario still starts empty.
    #[doc(hidden)]
    pub async fn validation_reset_contract_state(&self) -> Result<(), StoreError> {
        self.telemetry.clear().await?;
        let statements = [
            "DELETE FROM job_leases",
            "DELETE FROM offline_source_probes",
            "DELETE FROM offline_lease_guards",
            "DELETE FROM offline_package_leases",
            "DELETE FROM offline_packages",
            "DELETE FROM transcode_cache_locations",
            "DELETE FROM transcode_cache_recipes",
            "DELETE FROM watched_outbox",
            "DELETE FROM trakt_auth",
            "DELETE FROM watch_state",
            "DELETE FROM reading_state",
            "DELETE FROM scan_reconcile_items",
            "DELETE FROM scan_reconcile_guards",
            "DELETE FROM library_roots",
            "DELETE FROM files",
            "DELETE FROM items",
            "DELETE FROM libraries",
            "DELETE FROM tokens",
            "DELETE FROM api_keys",
            "DELETE FROM users",
            "DELETE FROM settings WHERE key <> $1 \
               AND key NOT GLOB 'internal.cluster_job_owner_removed.*'",
        ];
        for sql in statements {
            validate_sql(sql)?;
        }
        timeout_store(self.client().txn(vec![
            (statements[0].to_owned(), params!()),
            (statements[1].to_owned(), params!()),
            (statements[2].to_owned(), params!()),
            (statements[3].to_owned(), params!()),
            (statements[4].to_owned(), params!()),
            (statements[5].to_owned(), params!()),
            (statements[6].to_owned(), params!()),
            (statements[7].to_owned(), params!()),
            (statements[8].to_owned(), params!()),
            (statements[9].to_owned(), params!()),
            (statements[10].to_owned(), params!()),
            (statements[11].to_owned(), params!()),
            (statements[12].to_owned(), params!()),
            (statements[13].to_owned(), params!()),
            (statements[14].to_owned(), params!()),
            (statements[15].to_owned(), params!()),
            (statements[16].to_owned(), params!()),
            (statements[17].to_owned(), params!()),
            (statements[18].to_owned(), params!()),
            (statements[19].to_owned(), params!()),
            (statements[20].to_owned(), params!(keys::INSTANCE_ID)),
        ]))
        .await?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(database_error)?;
        Ok(())
    }

    /// Hash only authoritative catalogue tables from this voter. The cluster
    /// gate uses this to prove a damaged derived FTS index cannot change truth.
    pub async fn validation_local_catalog_truth_digest(&self) -> Result<String, StoreError> {
        tokio::time::timeout(
            STORE_TIMEOUT,
            super::hiqlite_catalog::local_catalog_truth_digest(self.client()),
        )
        .await
        .map_err(|_| StoreError::Database("replicated store operation timed out".to_owned()))?
    }

    async fn local_auth_dump(&self) -> Result<AuthStoreDump, StoreError> {
        for sql in [
            "SELECT singleton, schema_version, protocol_min, protocol_max, migrated_at FROM cluster_meta ORDER BY singleton",
            "SELECT key, value, updated_at FROM settings ORDER BY key",
            "SELECT id, username, password_hash, is_admin, created_at FROM users ORDER BY id",
            "SELECT token_hash, user_id, device, created_at, last_seen_at FROM tokens ORDER BY token_hash",
            "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled FROM api_keys ORDER BY id",
            "SELECT resource, owner_node_id, fence, revision, expires_at_ms, updated_at_ms FROM job_leases ORDER BY resource",
        ] {
            validate_sql(sql)?;
        }
        Ok(AuthStoreDump {
            catalog_digest: tokio::time::timeout(
                STORE_TIMEOUT,
                super::hiqlite_catalog::local_catalog_digest(self.client()),
            )
            .await
            .map_err(|_| {
                StoreError::Database("replicated store operation timed out".to_owned())
            })??,
            durable_digest: tokio::time::timeout(
                STORE_TIMEOUT,
                super::hiqlite_durable::local_durable_digest(self.client()),
            )
            .await
            .map_err(|_| {
                StoreError::Database("replicated store operation timed out".to_owned())
            })??,
            cluster_meta: timeout_store(self.client().query_map(
                "SELECT singleton, schema_version, protocol_min, protocol_max, migrated_at \
                     FROM cluster_meta ORDER BY singleton",
                params!(),
            ))
            .await?,
            settings: timeout_store(self.client().query_map(
                "SELECT key, value, updated_at FROM settings ORDER BY key",
                params!(),
            ))
            .await?,
            users: timeout_store(self.client().query_map(
                "SELECT id, username, password_hash, is_admin, created_at \
                     FROM users ORDER BY id",
                params!(),
            ))
            .await?,
            tokens: timeout_store(self.client().query_map(
                "SELECT token_hash, user_id, device, created_at, last_seen_at \
                     FROM tokens ORDER BY token_hash",
                params!(),
            ))
            .await?,
            api_keys: timeout_store(self.client().query_map(
                "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled \
                     FROM api_keys ORDER BY id",
                params!(),
            ))
            .await?,
            job_leases: timeout_store(self.client().query_map(
                "SELECT resource, owner_node_id, fence, revision, expires_at_ms, updated_at_ms \
                     FROM job_leases ORDER BY resource",
                params!(),
            ))
            .await?,
        })
    }

    fn with_clock(client: Client, clock: Arc<dyn Clock>, telemetry: NodeLocalTelemetry) -> Self {
        Self {
            client: TimedClient::new(client),
            clock,
            telemetry,
            activity_refreshes: Arc::new(ActivityRefreshGate::default()),
        }
    }

    pub(super) fn now(&self) -> Result<i64, StoreError> {
        self.clock.now()
    }

    async fn refresh_token_activity_if_due(
        &self,
        token_hash: &str,
        observed_last_seen_at: i64,
        now: i64,
    ) -> Result<(), StoreError> {
        if !crate::auth::activity_refresh_due(Some(observed_last_seen_at), now) {
            return Ok(());
        }
        let credential = ActivityCredential::Token(token_hash.to_owned());
        let Some(reservation) = self.activity_refreshes.try_reserve(credential, now) else {
            return Ok(());
        };

        let outcome = async {
            let rows = self
                .client()
                .query_consistent_map::<ActivityTimestampRow, _>(
                    "SELECT last_seen_at AS last_activity_at \
                     FROM tokens WHERE token_hash = $1",
                    params!(token_hash),
                )
                .await?;
            let Some(current) = rows.into_iter().next() else {
                return Ok(());
            };
            if crate::auth::activity_refresh_due(current.last_activity_at, now) {
                self.execute(
                    "UPDATE tokens SET last_seen_at = $1 \
                     WHERE token_hash = $2 AND last_seen_at < $3",
                    params!(
                        now,
                        token_hash,
                        now.saturating_sub(crate::auth::ACTIVITY_REFRESH_SECS)
                    ),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if outcome.is_ok() {
            reservation.retain();
        }
        outcome
    }

    async fn refresh_api_key_activity(&self, id: i64, now: i64) -> Result<(), StoreError> {
        let credential = ActivityCredential::ApiKey(id);
        let Some(reservation) = self.activity_refreshes.try_reserve(credential, now) else {
            return Ok(());
        };

        let outcome = async {
            let rows = self
                .client()
                .query_consistent_map::<ActivityTimestampRow, _>(
                    "SELECT last_used_at AS last_activity_at \
                     FROM api_keys WHERE id = $1 AND disabled = 0",
                    params!(id),
                )
                .await?;
            let Some(current) = rows.into_iter().next() else {
                return Ok(());
            };
            if crate::auth::activity_refresh_due(current.last_activity_at, now) {
                self.execute(
                    "UPDATE api_keys SET last_used_at = $1 \
                     WHERE id = $2 AND disabled = 0 \
                       AND (last_used_at IS NULL OR last_used_at < $3)",
                    params!(
                        now,
                        id,
                        now.saturating_sub(crate::auth::ACTIVITY_REFRESH_SECS)
                    ),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if outcome.is_ok() {
            reservation.retain();
        }
        outcome
    }

    pub(super) async fn execute(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<usize, StoreError> {
        validate_sql(sql)?;
        timeout_store(self.client().execute(sql, params)).await
    }

    async fn user_optional(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<Option<User>, StoreError> {
        validate_sql(sql)?;
        let mut rows = timeout_store(
            self.client()
                .query_consistent_map::<UserRow, _>(sql, params),
        )
        .await?;
        Ok(rows.pop().map(Into::into))
    }

    async fn key_optional(
        &self,
        sql: &'static str,
        params: hiqlite::Params,
    ) -> Result<Option<ApiKey>, StoreError> {
        validate_sql(sql)?;
        let mut rows = timeout_store(
            self.client()
                .query_consistent_map::<ApiKeyRow, _>(sql, params),
        )
        .await?;
        Ok(rows.pop().map(Into::into))
    }
}

#[async_trait]
impl PlaybackTelemetryStore for HiqliteAuthStore {
    async fn record_playback_event(&self, event: &PlaybackEvent) -> Result<i64, StoreError> {
        self.telemetry.record(event.clone()).await
    }

    async fn prune_playback_events(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.telemetry.prune(before_ms, limit).await
    }

    async fn playback_events(
        &self,
        query: &PlaybackEventQuery,
    ) -> Result<Vec<PlaybackEvent>, StoreError> {
        self.telemetry.events(query.clone()).await
    }
}

#[async_trait]
impl NetworkPriorStore for HiqliteAuthStore {
    async fn observe_network_prior(
        &self,
        observation: &NetworkPriorObservation,
    ) -> Result<NetworkPrior, StoreError> {
        self.telemetry.observe_prior(observation.clone()).await
    }

    async fn network_prior(
        &self,
        credential_generation: &str,
        client_class: &str,
        network_fingerprint: &str,
    ) -> Result<Option<NetworkPrior>, StoreError> {
        self.telemetry
            .prior(
                credential_generation.to_owned(),
                client_class.to_owned(),
                network_fingerprint.to_owned(),
            )
            .await
    }

    async fn prune_network_priors(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.telemetry.prune_priors(before_ms, limit).await
    }
}

#[async_trait]
impl SettingsStore for HiqliteAuthStore {
    async fn ping(&self) -> Result<(), StoreError> {
        timeout_store(self.client().is_healthy_db()).await?;
        let sql = "SELECT 1 AS healthy";
        validate_sql(sql)?;
        let rows = timeout_store(
            self.client()
                .query_consistent_map::<PingRow, _>(sql, params!()),
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
        let sql = "SELECT value FROM settings WHERE key = $1";
        validate_sql(sql)?;
        let rows = timeout_store(
            self.client()
                .query_consistent_map::<SettingValueRow, _>(sql, params!(key)),
        )
        .await?;
        Ok(rows.into_iter().next().map(|row| row.value))
    }

    async fn get_setting_pair(
        &self,
        first: &str,
        second: &str,
    ) -> Result<(Option<String>, Option<String>), StoreError> {
        let sql = "SELECT key, value FROM settings WHERE key = $1 OR key = $2 ORDER BY key";
        validate_sql(sql)?;
        let rows = timeout_store(
            self.client()
                .query_consistent_map::<SettingEntryRow, _>(sql, params!(first, second)),
        )
        .await?;
        let mut pair = (None, None);
        for row in rows {
            if row.key == first {
                pair.0 = Some(row.value.clone());
            }
            if row.key == second {
                pair.1 = Some(row.value);
            }
        }
        Ok(pair)
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

    async fn put_setting_if_absent(&self, key: &str, value: &str) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
                 ON CONFLICT(key) DO NOTHING",
                params!(key, value, now),
            )
            .await?
            == 1)
    }

    async fn put_setting_if_absent_if_artwork_repair_current(
        &self,
        key: &str,
        value: &str,
        expected_item_id: i64,
        fence: &ArtworkRepairFence,
    ) -> Result<bool, StoreError> {
        let now = self.now()?;
        Ok(self
            .execute(
                "INSERT INTO settings (key, value, updated_at) \
                 SELECT $1, $2, $3 WHERE $4 = $5 AND EXISTS (\
                   SELECT 1 FROM cluster_artwork_repairs \
                   WHERE item_id = $5 AND owner_node_id = $6 AND leader_term = $7 \
                     AND generation = $8) \
                 ON CONFLICT(key) DO NOTHING",
                params!(
                    key,
                    value,
                    now,
                    expected_item_id,
                    fence.item_id,
                    fence.owner_node_id.as_str(),
                    fence.leader_term,
                    fence.generation
                ),
            )
            .await?
            == 1)
    }

    async fn put_settings(&self, values: &[(&str, &str)]) -> Result<(), StoreError> {
        let now = self.now()?;
        let sql = "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
                   ON CONFLICT(key) DO UPDATE SET \
                   value = excluded.value, updated_at = excluded.updated_at";
        validate_sql(sql)?;
        self.client()
            .txn(
                values
                    .iter()
                    .map(|(key, value)| (sql.to_owned(), params!(*key, *value, now))),
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
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
        let sql = "SELECT COUNT(*) AS count FROM users";
        validate_sql(sql)?;
        let rows = timeout_store(
            self.client()
                .query_consistent_map::<CountRow, _>(sql, params!()),
        )
        .await?;
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
        let row = timeout_store(self.client().execute_returning_map_one::<_, UserRow>(
            sql,
            params!(username, password_hash, is_admin, now),
        ))
        .await?;
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
        let sql = "SELECT id, username, password_hash, is_admin, created_at \
                   FROM users ORDER BY username";
        validate_sql(sql)?;
        Ok(timeout_store(
            self.client()
                .query_consistent_map::<UserRow, _>(sql, params!()),
        )
        .await?
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
        let sql = "SELECT COUNT(*) AS count FROM users WHERE is_admin = 1";
        validate_sql(sql)?;
        let rows = timeout_store(
            self.client()
                .query_consistent_map::<CountRow, _>(sql, params!()),
        )
        .await?;
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
        let sql = "SELECT u.id, u.username, u.password_hash, u.is_admin, u.created_at, \
                          t.last_seen_at \
                 FROM users u JOIN tokens t ON t.user_id = u.id \
                 WHERE t.token_hash = $1";
        validate_sql(sql)?;
        let mut rows = timeout_store(
            self.client()
                .query_consistent_map::<TokenUserRow, _>(sql, params!(token_hash)),
        )
        .await?;
        let row = rows.pop();
        if let Some(row) = row.as_ref() {
            let now = self.now()?;
            self.refresh_token_activity_if_due(token_hash, row.last_seen_at, now)
                .await?;
        }
        Ok(row.map(Into::into))
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
        let row =
            timeout_store(self.client().execute_returning_map_one::<_, ApiKeyRow>(
                sql,
                params!(name, key_hash, scopes, now),
            ))
            .await?;
        Ok(row.into())
    }

    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        let sql = "SELECT id, name, key_hash, scopes, created_at, last_used_at, disabled \
                   FROM api_keys ORDER BY created_at, id";
        validate_sql(sql)?;
        Ok(timeout_store(
            self.client()
                .query_consistent_map::<ApiKeyRow, _>(sql, params!()),
        )
        .await?
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
        self.refresh_api_key_activity(id, now).await
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

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> Result<i64, StoreError> {
        Ok(self.0)
    }
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
            b'[' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b']' {
                        if bytes.get(index + 1) == Some(&b']') {
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
                let token = &sql[start..index];
                if token.len() > 1 && token.starts_with('0') {
                    return Err(StoreError::Database(format!(
                        "hiqlite placeholders must use canonical $N spelling; found ${token}"
                    )));
                }
                let ordinal = token
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
            b'$' | b'?' | b':' | b'@' => {
                return Err(StoreError::Database(format!(
                    "hiqlite statements may only use canonical $N placeholders; found {:?}",
                    bytes[index] as char
                )));
            }
            _ => index += 1,
        }
    }
    Ok(())
}

pub(super) fn database_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Database(error.to_string())
}

pub(super) async fn timeout_store<T, E>(
    operation: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, StoreError>
where
    E: std::fmt::Display,
{
    tokio::time::timeout(STORE_TIMEOUT, operation)
        .await
        .map_err(|_| StoreError::Database("replicated store operation timed out".to_owned()))?
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaMigrationAction {
    Current,
    MigrateFrom(i64),
}

fn schema_migration_action(
    rows: &[CompatibilityRow],
    supported: ClusterCompatibility,
) -> Result<SchemaMigrationAction, StoreError> {
    let [meta] = rows else {
        return Err(StoreError::Migration(format!(
            "cluster compatibility metadata returned {} rows",
            rows.len()
        )));
    };
    if !(meta.protocol_min..=meta.protocol_max).contains(&supported.protocol_version) {
        return Err(StoreError::Migration(format!(
            "cluster protocol range {}..={} excludes voter protocol {}",
            meta.protocol_min, meta.protocol_max, supported.protocol_version
        )));
    }
    match meta.schema_version {
        version if version == supported.schema_version => Ok(SchemaMigrationAction::Current),
        AUTH_SCHEMA_MIGRATION_SOURCE
        | BOOK_SCHEMA_MIGRATION_SOURCE
        | LEASE_SCHEMA_MIGRATION_SOURCE => {
            Ok(SchemaMigrationAction::MigrateFrom(meta.schema_version))
        }
        version => Err(StoreError::Migration(format!(
            "cluster schema {version} cannot migrate to voter schema {}",
            supported.schema_version
        ))),
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
    job_leases: Vec<JobLeaseDumpRow>,
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

struct SettingEntryRow {
    key: String,
    value: String,
}

impl From<&mut Row<'_>> for SettingEntryRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            key: row.get("key"),
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

struct TokenUserRow {
    user: UserRow,
    last_seen_at: i64,
}

impl From<&mut Row<'_>> for TokenUserRow {
    fn from(row: &mut Row<'_>) -> Self {
        let last_seen_at = row.get("last_seen_at");
        Self {
            user: UserRow::from(&mut *row),
            last_seen_at,
        }
    }
}

impl From<TokenUserRow> for User {
    fn from(row: TokenUserRow) -> Self {
        row.user.into()
    }
}

struct ActivityTimestampRow {
    last_activity_at: Option<i64>,
}

impl From<&mut Row<'_>> for ActivityTimestampRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            last_activity_at: row.get("last_activity_at"),
        }
    }
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
dump_row!(JobLeaseDumpRow {
    resource: String,
    owner_node_id: String,
    fence: i64,
    revision: i64,
    expires_at_ms: i64,
    updated_at_ms: i64,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_activity_reservation_is_released_for_retry() {
        let gate = Arc::new(ActivityRefreshGate::default());
        let credential = ActivityCredential::Token("token-hash".to_owned());

        {
            let _unfinished = gate
                .try_reserve(credential.clone(), 1_000)
                .expect("first reservation");
            assert!(gate.try_reserve(credential.clone(), 1_000).is_none());
        }
        let retained = gate
            .try_reserve(credential.clone(), 1_000)
            .expect("dropped reservation must permit retry");
        retained.retain();
        assert!(gate.try_reserve(credential.clone(), 1_060).is_none());
        assert!(gate.try_reserve(credential, 1_061).is_some());
    }

    #[test]
    fn replicated_store_modules_cannot_bypass_the_timed_client_accessor() {
        for (name, source) in [
            ("catalog", include_str!("hiqlite_catalog.rs")),
            ("coordination", include_str!("hiqlite_coordination.rs")),
            ("media", include_str!("hiqlite_media.rs")),
            ("durable", include_str!("hiqlite_durable.rs")),
        ] {
            let compact: String = source
                .chars()
                .filter(|char| !char.is_whitespace())
                .collect();
            assert!(
                !compact.contains("self.client."),
                "{name} store bypasses HiqliteAuthStore::client(), which enforces the 3s timeout"
            );
        }
    }

    #[tokio::test]
    async fn timed_read_accessors_reject_misordered_placeholders_before_io() {
        let client = disconnected_test_client();
        for error in [
            client
                .query_consistent_map::<CountRow, _>(
                    "SELECT $2 AS count WHERE $1 = 1",
                    params!(1, 2),
                )
                .await
                .err()
                .expect("consistent helper must validate SQL"),
            client
                .query_map::<CountRow, _>("SELECT $2 AS count WHERE $1 = 1", params!(1, 2))
                .await
                .err()
                .expect("local helper must validate SQL"),
        ] {
            assert!(error.to_string().contains("expected $1, found $2"));
        }
    }

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
        validate_sql("SELECT [$2], [a'b] FROM users WHERE id = $1")
            .expect("bracketed identifiers are not placeholders or strings");

        for unsupported in [
            "SELECT $1, $01, $2",
            "SELECT ?2, ?1",
            "SELECT ? FROM users",
            "SELECT :id FROM users",
            "SELECT @id FROM users",
            "SELECT $id FROM users",
        ] {
            validate_sql(unsupported).expect_err("unsupported placeholder spelling");
        }
        validate_sql("SELECT [a'b], $2, $1 FROM users")
            .expect_err("bracket quote cannot hide misordered placeholders");
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

        let existing_v4 = CompatibilityRow {
            schema_version: 4,
            protocol_min: AUTH_PROTOCOL_VERSION,
            protocol_max: AUTH_PROTOCOL_VERSION,
        };
        let error = verify_compatibility_rows(vec![existing_v4], ClusterCompatibility::CURRENT)
            .expect_err("the strict open path must never migrate");
        assert!(error.to_string().contains("schema 4 is incompatible"));

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

    #[test]
    fn daemon_schema_gate_accepts_the_complete_supported_chain() {
        assert_eq!(
            AUTH_SCHEMA_MIGRATION_SOURCE + 3,
            AUTH_SCHEMA_VERSION,
            "this implementation contains the v5→v6, v6→v7, and v7→v8 steps"
        );
        let row = |schema_version| CompatibilityRow {
            schema_version,
            protocol_min: AUTH_PROTOCOL_VERSION,
            protocol_max: AUTH_PROTOCOL_VERSION,
        };
        assert_eq!(
            schema_migration_action(&[row(AUTH_SCHEMA_VERSION)], ClusterCompatibility::CURRENT)
                .expect("current schema"),
            SchemaMigrationAction::Current
        );
        assert_eq!(
            schema_migration_action(
                &[row(AUTH_SCHEMA_MIGRATION_SOURCE)],
                ClusterCompatibility::CURRENT,
            )
            .expect("oldest supported predecessor"),
            SchemaMigrationAction::MigrateFrom(AUTH_SCHEMA_MIGRATION_SOURCE)
        );
        assert_eq!(
            schema_migration_action(
                &[row(BOOK_SCHEMA_MIGRATION_SOURCE)],
                ClusterCompatibility::CURRENT,
            )
            .expect("book-schema predecessor"),
            SchemaMigrationAction::MigrateFrom(BOOK_SCHEMA_MIGRATION_SOURCE)
        );
        assert_eq!(
            schema_migration_action(
                &[row(LEASE_SCHEMA_MIGRATION_SOURCE)],
                ClusterCompatibility::CURRENT,
            )
            .expect("immediate predecessor"),
            SchemaMigrationAction::MigrateFrom(LEASE_SCHEMA_MIGRATION_SOURCE)
        );

        for rows in [Vec::new(), vec![row(4)], vec![row(7), row(7)]] {
            let error = schema_migration_action(&rows, ClusterCompatibility::CURRENT)
                .expect_err("ambiguous or unsupported state must fail before writes");
            assert!(
                error.to_string().contains("cannot migrate")
                    || error.to_string().contains("metadata returned"),
                "{error}"
            );
        }

        let incompatible_protocol = CompatibilityRow {
            schema_version: AUTH_SCHEMA_MIGRATION_SOURCE,
            protocol_min: AUTH_PROTOCOL_VERSION + 1,
            protocol_max: AUTH_PROTOCOL_VERSION + 1,
        };
        let error =
            schema_migration_action(&[incompatible_protocol], ClusterCompatibility::CURRENT)
                .expect_err("protocol drift must fail before migration");
        assert!(error.to_string().contains("excludes"), "{error}");
    }
}

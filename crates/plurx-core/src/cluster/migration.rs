//! Crash-safe preparation for the one-time SQLite-to-Hiqlite import.
//!
//! The coordinator keeps SQLite authoritative until a fully imported one-voter
//! target has a durable completion marker. It then stops that voter, renames
//! the target atomically, fsyncs the data directory, and reopens the active
//! target before the daemon may start producers or bind HTTP.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "hiqlite-store")]
use std::borrow::Cow;
#[cfg(feature = "hiqlite-store")]
use std::collections::BTreeSet;
#[cfg(feature = "hiqlite-store")]
use std::net::{IpAddr, SocketAddr};
#[cfg(feature = "hiqlite-store")]
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::error::StoreError;
use crate::store::SQLITE_SCHEMA_VERSION;

#[cfg(feature = "hiqlite-store")]
use crate::config::Config;
#[cfg(feature = "hiqlite-store")]
use crate::secrets::{self, CredentialKey, SealedRowCensus};
#[cfg(feature = "hiqlite-store")]
use crate::store::{
    HiqliteAuthStore, SettingsStore, SqliteImportReport, SqliteImportTableDigest, SqliteStore,
    Store, TraktStore, AUTH_SCHEMA_VERSION,
};
#[cfg(feature = "hiqlite-store")]
use hiqlite::tls::ServerTlsConfig;
#[cfg(feature = "hiqlite-store")]
use hiqlite::{Client, Node, NodeConfig};
#[cfg(feature = "hiqlite-store")]
use serde::{Deserialize, Serialize};

pub const SQLITE_FILENAME: &str = "plurx.db";
pub const MIGRATION_DIRNAME: &str = "migration";
pub const HIQLITE_INCOMING_DIRNAME: &str = "hiqlite.incoming";
pub const HIQLITE_ACTIVE_DIRNAME: &str = "hiqlite";
pub const ACTIVATION_MARKER_FILENAME: &str = "activation.json";
/// Breadcrumb proving this data directory already handed authority to Hiqlite.
///
/// It lives beside `plurx.db` rather than inside the target, because its whole
/// job is to survive the target's disappearance.
pub const ACTIVATED_SOURCE_FILENAME: &str = "hiqlite-activated.json";
#[cfg(feature = "hiqlite-store")]
const ACTIVATION_ATTEMPT_FILENAME: &str = "hiqlite-activation.in-progress";
#[cfg(feature = "hiqlite-store")]
const RAFT_SECRET_FILENAME: &str = "secret_raft";
#[cfg(feature = "hiqlite-store")]
const API_SECRET_FILENAME: &str = "secret_api";
#[cfg(feature = "hiqlite-store")]
const HIQLITE_DATABASE_FILENAME: &str = "plurx.db";
#[cfg(feature = "hiqlite-store")]
const DAEMON_LOCK_FILENAME: &str = ".plurxd.lock";
#[cfg(feature = "hiqlite-store")]
const HIQLITE_START_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(feature = "hiqlite-store")]
const ACTIVATION_MARKER_VERSION: u32 = 1;
#[cfg(feature = "hiqlite-store")]
const ACTIVATION_FAILPOINT_ENV: &str = "PLURX_CLUSTER_ACTIVATION_FAILPOINT";
#[cfg(feature = "hiqlite-store")]
const ACTIVATION_CRASH_EXIT: i32 = 86;
/// How many source backups `migration/` keeps, newest first.
const MIGRATION_BACKUP_RETENTION: usize = 3;

/// Immutable source material for the row-import and parity phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSqliteImport {
    pub source_path: PathBuf,
    pub backup_path: PathBuf,
    pub backup_sha256: String,
    pub schema_version: i64,
    pub cluster_id: String,
}

/// The durable proof that the incoming target passed the complete import gate.
///
/// This file is fsynced before the directory becomes active. Startup never
/// treats the presence of Hiqlite files alone as activation: a missing,
/// malformed, or identity-mismatched marker is an ambiguous target and fails
/// closed instead of silently falling back to stale SQLite state.
#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationMarker {
    pub marker_version: u32,
    pub cluster_id: String,
    pub source_backup_sha256: String,
    pub source_schema_version: i64,
    pub replicated_schema_version: i64,
    pub imported_rows: u64,
    pub table_hashes: Vec<SqliteImportTableDigest>,
}

/// Record that this data directory already activated, kept outside the target.
///
/// The retained `plurx.db` is a rollback source, not a current one, and nothing
/// else can tell the two apart: a data directory that lost `hiqlite/` looks
/// exactly like one that never activated. Without this breadcrumb the next boot
/// would re-import the stale source and silently discard every write since
/// activation. It is written after the rename that made the target authoritative
/// and re-asserted on every later boot that opens one.
#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivatedSourceRecord {
    pub cluster_id: String,
    pub source_backup_sha256: String,
}

/// Which durable backend the daemon selected for this boot.
#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedBackend {
    Replicated,
    /// One recovery boot after an interrupted activation. The attempt marker
    /// is consumed before this value is returned, so a later restart may retry.
    SqliteRecovery,
}

/// Store selection returned before daemon producers and listeners are built.
#[cfg(feature = "hiqlite-store")]
pub struct SelectedStore {
    pub store: Arc<dyn Store>,
    pub identity: super::ClusterIdentity,
    /// Node-local key used only by outbound credential consumers.
    pub credential_key: Arc<CredentialKey>,
    pub backend: SelectedBackend,
    local_client: Option<Client>,
    _daemon_lock: File,
}

#[cfg(feature = "hiqlite-store")]
impl SelectedStore {
    /// Flush and stop the selected local voter before the Tokio runtime exits.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        if let Some(client) = &self.local_client {
            client.shutdown().await.map_err(|error| {
                StoreError::Database(format!("stopping one-voter Hiqlite: {error}"))
            })?;
        }
        Ok(())
    }
}

#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivationFailpoint {
    Quiescence,
    Incoming,
    Marker,
    Rename,
}

#[cfg(feature = "hiqlite-store")]
impl ActivationFailpoint {
    fn configured() -> Result<Option<Self>, StoreError> {
        let Some(value) = std::env::var(ACTIVATION_FAILPOINT_ENV)
            .ok()
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let point = match value.as_str() {
            "after-quiescence" => Self::Quiescence,
            "after-incoming" => Self::Incoming,
            "after-marker" => Self::Marker,
            "after-rename" => Self::Rename,
            _ => {
                return Err(StoreError::Migration(format!(
                    "invalid {ACTIVATION_FAILPOINT_ENV} value {value:?}; expected one of \
                     after-quiescence, after-incoming, after-marker, after-rename"
                )));
            }
        };
        Ok(Some(point))
    }

    fn crash_if(configured: Option<Self>, point: Self) {
        if configured == Some(point) {
            eprintln!(
                "injected cluster activation crash at {point:?}; SQLite and its migration \
                 backup remain unchanged; rollback command: plurxd run"
            );
            std::process::exit(ACTIVATION_CRASH_EXIT);
        }
    }
}

/// Select the daemon's one-voter replicated store before any producer or HTTP
/// listener exists.
///
/// A prior interrupted attempt consumes exactly one SQLite recovery boot: its
/// incoming target is removed, the attempt marker is fsynced away, and the
/// unchanged legacy store is returned. A completed atomic target always wins.
#[cfg(feature = "hiqlite-store")]
pub async fn select_daemon_store(config: &Config) -> Result<SelectedStore, StoreError> {
    install_default_crypto_provider();
    std::fs::create_dir_all(&config.storage.data_dir)
        .map_err(|error| migration_io("creating", &config.storage.data_dir, error))?;
    let daemon_lock = acquire_daemon_lock(&config.storage.data_dir)?;

    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    if path_exists(&active)? {
        let selected = open_active_store(config, daemon_lock).await?;
        // A crash immediately after rename may expose the target before its
        // parent-directory entry is durable. Observing it on recovery lets us
        // finish that durability boundary before clearing attempt artifacts.
        sync_directory(&config.storage.data_dir)?;
        remove_abandoned_incoming(&config.storage.data_dir)?;
        remove_activation_attempt(&config.storage.data_dir)?;
        return Ok(selected);
    }

    // No active target, but this directory has already handed authority over.
    // Re-importing the retained source here would look like a clean first boot
    // and silently discard everything written since activation, so refuse
    // before any cleanup, attempt marker, or import can start.
    if let Some(record) = read_activated_source_record(&config.storage.data_dir)? {
        return Err(StoreError::Migration(format!(
            "{} was already activated as cluster {} but {} is missing: refusing to \
             re-import {}, which is the pre-activation rollback source and would \
             discard every change since. Restore the replicated target from a copy, \
             or delete {} to deliberately accept that rollback and its data loss",
            config.storage.data_dir.display(),
            record.cluster_id,
            active.display(),
            config.storage.data_dir.join(SQLITE_FILENAME).display(),
            config
                .storage
                .data_dir
                .join(ACTIVATED_SOURCE_FILENAME)
                .display(),
        )));
    }

    ensure_sqlite_source(&config.storage.data_dir)?;
    // Future-schema refusal must precede cleanup or attempt-marker writes.
    inspect_sqlite_source(&config.storage.data_dir.join(SQLITE_FILENAME))?;
    // The injection surface belongs only to a pending activation. Reading it
    // before the active-target return let a stale or misspelled environment
    // value take an already-activated server offline even though no failpoint
    // could fire on that path.
    let failpoint = ActivationFailpoint::configured()?;

    let incoming = config.storage.data_dir.join(HIQLITE_INCOMING_DIRNAME);
    let interrupted =
        path_exists(&activation_attempt_path(&config.storage.data_dir))? || path_exists(&incoming)?;
    if interrupted {
        remove_abandoned_incoming(&config.storage.data_dir)?;
        remove_activation_attempt(&config.storage.data_dir)?;
        let legacy = super::open_store(config).await?;
        return Ok(SelectedStore {
            store: legacy.store,
            identity: legacy.identity,
            credential_key: legacy.credential_key,
            backend: SelectedBackend::SqliteRecovery,
            local_client: None,
            _daemon_lock: daemon_lock,
        });
    }

    // Reuse the ordinary SQLite startup upgrade before the immutable backup is
    // published. This is the one permitted source mutation: a pre-encryption
    // Trakt row must be sealed under the node-local key before the importer can
    // audit it, and no cleartext application row may ever be submitted to Raft.
    // Opening here is still quiescent and happens after the future-schema
    // refusal above. Drop the source connection before the online backup.
    let legacy = super::open_store(config).await?;
    let identity = legacy.identity;
    let credential_key = legacy.credential_key;
    drop(legacy.store);

    match activate_fresh_store(config, failpoint, daemon_lock, identity, credential_key).await {
        Ok(store) => Ok(store),
        Err(error) => Err(activation_failure(&config.storage.data_dir, error)),
    }
}

/// Connect a maintenance command to an already-running activated voter.
///
/// This path never imports and never starts a second local voter. It is safe
/// beside `plurxd run`; an unmigrated directory is refused before creating any
/// replicated state.
#[cfg(feature = "hiqlite-store")]
pub async fn connect_activated_store(config: &Config) -> Result<Arc<dyn Store>, StoreError> {
    install_default_crypto_provider();
    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    if !path_exists(&active)? {
        return Err(StoreError::Migration(format!(
            "{} is not activated; only `plurxd run` may import SQLite into Hiqlite",
            config.storage.data_dir.display()
        )));
    }
    let marker = read_activation_marker(&active)?;
    super::initialize_identity(&config.storage.data_dir, &marker.cluster_id)?;
    let secret_api = read_secret(&config.storage.data_dir.join(API_SECRET_FILENAME))?;
    let address = local_client_address(config.cluster.api_bind).to_string();
    let client = Client::remote(vec![address], true, true, secret_api, true, None)
        .await
        .map_err(|error| {
            StoreError::Database(format!(
                "connecting to the running one-voter Hiqlite store: {error}"
            ))
        })?;
    let store = HiqliteAuthStore::open(client, &active.join("telemetry.db")).await?;
    verify_store_identity(&store, &marker.cluster_id).await?;
    Ok(Arc::new(store))
}

#[cfg(feature = "hiqlite-store")]
async fn activate_fresh_store(
    config: &Config,
    failpoint: Option<ActivationFailpoint>,
    daemon_lock: File,
    identity: super::ClusterIdentity,
    credential_key: Arc<CredentialKey>,
) -> Result<SelectedStore, StoreError> {
    write_activation_attempt(&config.storage.data_dir)?;
    ActivationFailpoint::crash_if(failpoint, ActivationFailpoint::Quiescence);

    let prepared = prepare_sqlite_import(&config.storage.data_dir)?;
    if identity.cluster_id != prepared.cluster_id {
        return Err(StoreError::Identity(format!(
            "SQLite instance.id changed while preparing activation: opened {}, backup contains {}",
            identity.cluster_id, prepared.cluster_id
        )));
    }
    verify_legacy_ownership(&prepared.backup_path, &identity)?;
    let secrets = load_or_create_secrets(&config.storage.data_dir)?;
    let incoming = config.storage.data_dir.join(HIQLITE_INCOMING_DIRNAME);
    std::fs::create_dir(&incoming).map_err(|error| migration_io("creating", &incoming, error))?;
    if let Err(error) = sync_directory(&config.storage.data_dir) {
        return Err(cleanup_incoming_failure(&incoming, error));
    }

    // The staging voter is process-local and never accepts a peer. Running it
    // without TLS lets Hiqlite's graceful shutdown close both listeners before
    // rename; the active voter below uses TLS for its exposed API transport.
    let client = match start_local_voter(config, &incoming, &secrets, false).await {
        Ok(client) => client,
        Err(error) => {
            remove_abandoned_incoming(&config.storage.data_dir)?;
            return Err(error);
        }
    };
    let store = match HiqliteAuthStore::bootstrap(
        client.clone(),
        &prepared.cluster_id,
        &incoming.join("telemetry.db"),
    )
    .await
    {
        Ok(store) => store,
        Err(error) => return abort_incoming(client, &incoming, error).await,
    };
    ActivationFailpoint::crash_if(failpoint, ActivationFailpoint::Incoming);

    let report = match store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
    {
        Ok(report) => report,
        Err(error) => {
            drop(store);
            return abort_incoming(client, &incoming, error).await;
        }
    };
    let marker = ActivationMarker::from_report(&prepared.cluster_id, report);
    if let Err(error) = write_activation_marker(&incoming, &marker) {
        drop(store);
        return abort_incoming(client, &incoming, error).await;
    }
    ActivationFailpoint::crash_if(failpoint, ActivationFailpoint::Marker);

    drop(store);
    if let Err(error) = client.shutdown().await {
        drop(client);
        return Err(cleanup_incoming_failure(
            &incoming,
            StoreError::Database(format!("stopping incoming Hiqlite voter: {error}")),
        ));
    }
    drop(client);
    if let Err(error) = sync_directory(&incoming) {
        return Err(cleanup_incoming_failure(&incoming, error));
    }

    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    if let Err(error) = std::fs::rename(&incoming, &active) {
        return Err(cleanup_incoming_failure(
            &incoming,
            migration_io("activating", &active, error),
        ));
    }
    ActivationFailpoint::crash_if(failpoint, ActivationFailpoint::Rename);
    sync_directory(&config.storage.data_dir)?;
    remove_activation_attempt(&config.storage.data_dir)?;

    open_active_store_with_key(config, daemon_lock, Some(credential_key)).await
}

#[cfg(feature = "hiqlite-store")]
impl ActivationMarker {
    fn from_report(cluster_id: &str, report: SqliteImportReport) -> Self {
        Self {
            marker_version: ACTIVATION_MARKER_VERSION,
            cluster_id: cluster_id.to_owned(),
            source_backup_sha256: report.backup_sha256,
            source_schema_version: report.source_schema_version,
            replicated_schema_version: AUTH_SCHEMA_VERSION,
            imported_rows: report.imported_rows,
            table_hashes: report.tables,
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.marker_version != ACTIVATION_MARKER_VERSION {
            return Err(StoreError::Migration(format!(
                "unsupported Hiqlite activation marker version {}",
                self.marker_version
            )));
        }
        if self.cluster_id.trim().is_empty()
            || self.source_schema_version <= 0
            || self.replicated_schema_version != AUTH_SCHEMA_VERSION
            || !is_sha256(&self.source_backup_sha256)
            || self.table_hashes.is_empty()
        {
            return Err(StoreError::Migration(
                "Hiqlite activation marker is incomplete".to_owned(),
            ));
        }
        let mut names = BTreeSet::new();
        for table in &self.table_hashes {
            if table.table.is_empty()
                || !names.insert(table.table.as_str())
                || !is_sha256(&table.sha256)
            {
                return Err(StoreError::Migration(
                    "Hiqlite activation marker has invalid table hashes".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Install the process-level rustls provider every TLS path here depends on.
///
/// `rustls` panics rather than erroring when a process reaches TLS with no
/// default provider, and the crate is built with more than one provider feature
/// reachable, so it will not choose for us. `plurxd run` used to be covered only
/// by accident: `hiqlite::start_node` installs one on the server side, which the
/// maintenance commands never call, so `reset-password` and `refresh-metadata`
/// aborted on every activated node. Installing here rather than in one binary's
/// entry point keeps a future caller from reintroducing that gap.
///
/// Idempotent: a losing race or an already-installed provider returns `Err`,
/// which is the same end state as winning.
#[cfg(feature = "hiqlite-store")]
fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(feature = "hiqlite-store")]
async fn open_active_store(
    config: &Config,
    daemon_lock: File,
) -> Result<SelectedStore, StoreError> {
    open_active_store_with_key(config, daemon_lock, None).await
}

#[cfg(feature = "hiqlite-store")]
async fn open_active_store_with_key(
    config: &Config,
    daemon_lock: File,
    credential_key: Option<Arc<CredentialKey>>,
) -> Result<SelectedStore, StoreError> {
    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    require_real_directory(&active)?;
    let marker = read_activation_marker(&active)?;
    let identity = super::initialize_identity(&config.storage.data_dir, &marker.cluster_id)?;
    let secrets = read_existing_secrets(&config.storage.data_dir)?;
    let client = start_local_voter(config, &active, &secrets, true).await?;
    let store = match HiqliteAuthStore::open(client.clone(), &active.join("telemetry.db")).await {
        Ok(store) => store,
        Err(error) => {
            let shutdown = client.shutdown().await.err();
            return Err(with_shutdown_error(error, shutdown));
        }
    };
    if let Err(error) = verify_store_identity(&store, &marker.cluster_id).await {
        drop(store);
        let shutdown = client.shutdown().await.err();
        return Err(with_shutdown_error(error, shutdown));
    }
    // Written here rather than beside the rename so a crash in between still
    // converges: any boot that successfully opens an active target re-asserts
    // it, and this is the only place that can be reached without one.
    if let Err(error) = ensure_activated_source_record(&config.storage.data_dir, &marker) {
        drop(store);
        let shutdown = client.shutdown().await.err();
        return Err(with_shutdown_error(error, shutdown));
    }
    let credential_key = match credential_key {
        Some(key) => key,
        None => match open_active_credential_key(config, &store).await {
            Ok(key) => key,
            Err(error) => {
                drop(store);
                let shutdown = client.shutdown().await.err();
                return Err(with_shutdown_error(error, shutdown));
            }
        },
    };
    Ok(SelectedStore {
        store: Arc::new(store),
        identity,
        credential_key,
        backend: SelectedBackend::Replicated,
        local_client: Some(client),
        _daemon_lock: daemon_lock,
    })
}

/// Resolve the key against the authoritative replicated rows on every reopen.
///
/// The retained SQLite source cannot provide this census after activation: a
/// household may link Trakt later, and a lost or replaced key must refuse at
/// startup rather than surface as an unrelated outbound-sync failure.
#[cfg(feature = "hiqlite-store")]
async fn open_active_credential_key(
    config: &Config,
    store: &HiqliteAuthStore,
) -> Result<Arc<CredentialKey>, StoreError> {
    let mut census = SealedRowCensus::default();
    for auth in store.list_trakt_auth().await? {
        census.observe_row(&auth.access_token, &auth.refresh_token);
    }
    let path = config.cluster.credential_key_path(&config.storage.data_dir);
    let key = secrets::open_credential_key(&path, &census)
        .map_err(|error| StoreError::Identity(error.to_string()))?;
    tracing::debug!(
        key_id = %key.id(),
        wrapped_rows = census.sealed_rows(),
        "opened the node-local credential key for the replicated store"
    );
    Ok(Arc::new(key))
}

#[cfg(feature = "hiqlite-store")]
fn with_shutdown_error(error: StoreError, shutdown: Option<hiqlite::Error>) -> StoreError {
    match shutdown {
        Some(shutdown) => StoreError::Database(format!(
            "{error}; additionally failed to stop one-voter Hiqlite: {shutdown}"
        )),
        None => error,
    }
}

#[cfg(feature = "hiqlite-store")]
fn acquire_daemon_lock(data_dir: &Path) -> Result<File, StoreError> {
    let path = data_dir.join(DAEMON_LOCK_FILENAME);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&path)
        .map_err(|error| migration_io("opening", &path, error))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Migration(format!(
            "another plurxd process already owns the data directory {}",
            data_dir.display()
        ))),
        Err(std::fs::TryLockError::Error(error)) => Err(migration_io("locking", &path, error)),
    }
}

#[cfg(feature = "hiqlite-store")]
async fn verify_store_identity(store: &HiqliteAuthStore, expected: &str) -> Result<(), StoreError> {
    let actual = store.instance_id().await?;
    if actual != expected {
        return Err(StoreError::Identity(format!(
            "active Hiqlite instance.id is {actual}, activation marker expects {expected}"
        )));
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
struct ClusterSecrets {
    raft: String,
    api: String,
}

#[cfg(feature = "hiqlite-store")]
fn load_or_create_secrets(data_dir: &Path) -> Result<ClusterSecrets, StoreError> {
    Ok(ClusterSecrets {
        raft: load_or_create_secret(data_dir, RAFT_SECRET_FILENAME)?,
        api: load_or_create_secret(data_dir, API_SECRET_FILENAME)?,
    })
}

#[cfg(feature = "hiqlite-store")]
async fn start_local_voter(
    config: &Config,
    target: &Path,
    secrets: &ClusterSecrets,
    active_transport: bool,
) -> Result<Client, StoreError> {
    let raft_address = local_client_address(config.cluster.raft_bind);
    // Both listeners stay loopback-only for the same reason: M2 has no peer.
    // The API listener's only consumer is `connect_activated_store` on this
    // machine, which dials `local_client_address(api_bind)`, so binding the
    // configured `0.0.0.0` default would open a LAN port on every existing
    // single-node install at upgrade with nothing on the other end of it. M3
    // opens it deliberately when membership gives it a peer to talk to.
    let api_address = local_client_address(config.cluster.api_bind);
    let node = Node {
        id: super::SINGLE_VOTER_RAFT_ID,
        // Besides removing an unnecessary exposed port, Hiqlite's plain
        // listener receives its graceful-shutdown signal; M3 will replace both
        // addresses when it adds authenticated membership.
        addr_raft: raft_address.to_string(),
        addr_api: api_address.to_string(),
    };
    // The daemon lock guards one data directory, but these ports are host-wide
    // and default to fixed values, so two data directories on one host collide.
    // Without this check the node does not report a bind conflict: it reaches a
    // foreign voter's listener, retries the handshake for the full start
    // timeout, and then blames the migration. Check first and say which port.
    for (role, address) in [("raft", raft_address), ("cluster API", api_address)] {
        if let Err(error) = std::net::TcpListener::bind(address) {
            return Err(StoreError::Migration(format!(
                "the {role} port {address} is not available: {error}. Another plurxd on \
                 this host is using it — cluster ports are host-wide, so a second data \
                 directory needs its own raft_bind and api_bind"
            )));
        }
    }
    let node_config = NodeConfig {
        node_id: super::SINGLE_VOTER_RAFT_ID,
        nodes: vec![node],
        listen_addr_api: Cow::Owned(api_address.ip().to_string()),
        listen_addr_raft: Cow::Owned(raft_address.ip().to_string()),
        data_dir: Cow::Owned(target.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed(HIQLITE_DATABASE_FILENAME),
        secret_raft: secrets.raft.clone(),
        secret_api: secrets.api.clone(),
        tls_raft: None,
        tls_api: active_transport.then_some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: 2 * 1024 * 1024,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    };
    let client = hiqlite::start_node(node_config)
        .await
        .map_err(|error| StoreError::Database(format!("starting one-voter Hiqlite: {error}")))?;
    if tokio::time::timeout(HIQLITE_START_TIMEOUT, client.wait_until_healthy_db())
        .await
        .is_err()
    {
        let _ = client.shutdown().await;
        return Err(StoreError::Database(format!(
            "one-voter Hiqlite did not become healthy within {HIQLITE_START_TIMEOUT:?}"
        )));
    }
    Ok(client)
}

#[cfg(feature = "hiqlite-store")]
async fn abort_incoming(
    client: Client,
    incoming: &Path,
    error: StoreError,
) -> Result<SelectedStore, StoreError> {
    let shutdown = client.shutdown().await.err();
    drop(client);
    let mut message = error.to_string();
    if let Some(shutdown) = shutdown {
        message.push_str(&format!(
            "; additionally failed to stop incoming voter: {shutdown}"
        ));
    }
    Err(cleanup_incoming_failure(
        incoming,
        StoreError::Migration(message),
    ))
}

#[cfg(feature = "hiqlite-store")]
fn cleanup_incoming_failure(incoming: &Path, error: StoreError) -> StoreError {
    let mut message = error.to_string();
    if let Err(cleanup) = remove_abandoned_incoming(
        incoming
            .parent()
            .expect("the incoming directory always has a data-dir parent"),
    ) {
        message.push_str(&format!(
            "; additionally failed to remove incoming target: {cleanup}"
        ));
    }
    StoreError::Migration(message)
}

#[cfg(feature = "hiqlite-store")]
fn activation_failure(data_dir: &Path, error: StoreError) -> StoreError {
    StoreError::Migration(format!(
        "{error}; SQLite source {} remains available; rollback command: plurxd run",
        data_dir.join(SQLITE_FILENAME).display()
    ))
}

#[cfg(feature = "hiqlite-store")]
fn ensure_sqlite_source(data_dir: &Path) -> Result<(), StoreError> {
    let source = data_dir.join(SQLITE_FILENAME);
    if !source.exists() {
        drop(SqliteStore::open(&source)?);
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn verify_legacy_ownership(
    backup: &Path,
    identity: &super::ClusterIdentity,
) -> Result<(), StoreError> {
    if identity.node_id == identity.cluster_id {
        return Ok(());
    }
    let connection = open_source(backup)?;
    let count: i64 = connection
        .query_row(
            "SELECT \
                (SELECT COUNT(*) FROM transcode_cache_locations WHERE node_id = ?1) + \
                (SELECT COUNT(*) FROM offline_packages WHERE node_id = ?1)",
            [&identity.cluster_id],
            |row| row.get(0),
        )
        .map_err(|error| {
            StoreError::Migration(format!("checking legacy byte ownership: {error}"))
        })?;
    if count > 0 {
        return Err(StoreError::Identity(format!(
            "node.id {} differs from instance.id {}, but {count} cache or offline row(s) still \
             use instance.id; refusing to strand owned bytes",
            identity.node_id, identity.cluster_id
        )));
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn inspect_sqlite_source(path: &Path) -> Result<(i64, String), StoreError> {
    let source = open_source(path)?;
    let schema_version = read_schema_version(&source)?;
    if schema_version > SQLITE_SCHEMA_VERSION {
        return Err(StoreError::Migration(format!(
            "source database schema is v{schema_version}, but this binary only knows \
             v{SQLITE_SCHEMA_VERSION}; refusing clustering import without changing {}",
            path.parent().unwrap_or_else(|| Path::new(".")).display()
        )));
    }
    let cluster_id = read_cluster_id(&source)?;
    Ok((schema_version, cluster_id))
}

// `config.cluster.advertise_host` has no reader while both listeners are
// loopback-only: there is no peer to advertise an address to. M3 reintroduces
// the resolution helper alongside the membership that needs it, rather than
// keeping an unreachable one here.
#[cfg(feature = "hiqlite-store")]
fn local_client_address(bind: SocketAddr) -> SocketAddr {
    let ip = match bind.ip() {
        IpAddr::V4(_) => IpAddr::from([127, 0, 0, 1]),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(ip, bind.port())
}

#[cfg(feature = "hiqlite-store")]
fn load_or_create_secret(data_dir: &Path, filename: &str) -> Result<String, StoreError> {
    let path = data_dir.join(filename);
    if path_exists(&path)? {
        return read_secret(&path);
    }

    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| StoreError::Migration(format!("generating {filename}: {error}")))?;
    let secret = hex::encode(bytes);
    let temporary = data_dir.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4()));
    write_private_file(&temporary, format!("{secret}\n").as_bytes())?;
    match std::fs::hard_link(&temporary, &path) {
        Ok(()) => sync_directory(data_dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            publish_secret_with_rename(data_dir, &temporary, &path)?;
        }
        Err(error) => return Err(migration_io("publishing", &path, error)),
    }
    remove_file_if_present(&temporary)?;
    read_secret(&path)
}

#[cfg(feature = "hiqlite-store")]
fn read_existing_secrets(data_dir: &Path) -> Result<ClusterSecrets, StoreError> {
    Ok(ClusterSecrets {
        raft: read_secret(&data_dir.join(RAFT_SECRET_FILENAME))?,
        api: read_secret(&data_dir.join(API_SECRET_FILENAME))?,
    })
}

#[cfg(feature = "hiqlite-store")]
fn publish_secret_with_rename(
    data_dir: &Path,
    temporary: &Path,
    destination: &Path,
) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(destination) {
        Ok(placeholder) => {
            drop(placeholder);
            std::fs::rename(temporary, destination)
                .map_err(|error| migration_io("publishing", destination, error))?;
            sync_directory(data_dir)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(migration_io("reserving", destination, error)),
    }
}

#[cfg(feature = "hiqlite-store")]
fn read_secret(path: &Path) -> Result<String, StoreError> {
    let raw =
        std::fs::read_to_string(path).map_err(|error| migration_io("reading", path, error))?;
    let secret = raw.trim();
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StoreError::Migration(format!(
            "cluster secret {} is malformed",
            path.display()
        )));
    }
    Ok(secret.to_owned())
}

#[cfg(feature = "hiqlite-store")]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|error| migration_io("creating", path, error))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| migration_io("writing", path, error))
}

#[cfg(feature = "hiqlite-store")]
fn write_activation_attempt(data_dir: &Path) -> Result<(), StoreError> {
    let migration_dir = data_dir.join(MIGRATION_DIRNAME);
    std::fs::create_dir_all(&migration_dir)
        .map_err(|error| migration_io("creating", &migration_dir, error))?;
    sync_directory(data_dir)?;
    write_atomic_private(
        &migration_dir,
        ACTIVATION_ATTEMPT_FILENAME,
        b"one-voter activation in progress\n",
    )
}

#[cfg(feature = "hiqlite-store")]
fn activation_attempt_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(MIGRATION_DIRNAME)
        .join(ACTIVATION_ATTEMPT_FILENAME)
}

#[cfg(feature = "hiqlite-store")]
fn remove_activation_attempt(data_dir: &Path) -> Result<(), StoreError> {
    let path = activation_attempt_path(data_dir);
    let existed = path_exists(&path)?;
    remove_file_if_present(&path)?;
    if existed {
        sync_directory(&data_dir.join(MIGRATION_DIRNAME))?;
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn write_activation_marker(target: &Path, marker: &ActivationMarker) -> Result<(), StoreError> {
    marker.validate()?;
    let mut bytes = serde_json::to_vec_pretty(marker).map_err(|error| {
        StoreError::Migration(format!("serializing Hiqlite activation marker: {error}"))
    })?;
    bytes.push(b'\n');
    write_atomic_private(target, ACTIVATION_MARKER_FILENAME, &bytes)
}

/// Write the already-activated breadcrumb unless it is already correct.
///
/// Fsynced through the same atomic path as the marker: a torn breadcrumb would
/// fail closed on the next boot and demand an operator decision for a directory
/// that is actually fine.
#[cfg(feature = "hiqlite-store")]
fn ensure_activated_source_record(
    data_dir: &Path,
    marker: &ActivationMarker,
) -> Result<(), StoreError> {
    let record = ActivatedSourceRecord {
        cluster_id: marker.cluster_id.clone(),
        source_backup_sha256: marker.source_backup_sha256.clone(),
    };
    if read_activated_source_record(data_dir)?.as_ref() == Some(&record) {
        return Ok(());
    }
    let mut bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
        StoreError::Migration(format!("serializing the activated-source record: {error}"))
    })?;
    bytes.push(b'\n');
    write_atomic_private(data_dir, ACTIVATED_SOURCE_FILENAME, &bytes)
}

/// Read the already-activated breadcrumb; `None` means this directory never has.
///
/// A present but undecodable record is an error rather than a `None`: treating
/// it as absent would re-import the stale source, which is the exact loss the
/// record exists to prevent.
#[cfg(feature = "hiqlite-store")]
fn read_activated_source_record(
    data_dir: &Path,
) -> Result<Option<ActivatedSourceRecord>, StoreError> {
    let path = data_dir.join(ACTIVATED_SOURCE_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(migration_io("reading", &path, error)),
    };
    let record: ActivatedSourceRecord = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::Migration(format!(
            "decoding the activated-source record {}: {error}. It records that this \
             directory already handed authority to Hiqlite; repair or remove it \
             deliberately rather than letting a re-import discard replicated state",
            path.display()
        ))
    })?;
    Ok(Some(record))
}

#[cfg(feature = "hiqlite-store")]
fn read_activation_marker(target: &Path) -> Result<ActivationMarker, StoreError> {
    let path = target.join(ACTIVATION_MARKER_FILENAME);
    let bytes = std::fs::read(&path).map_err(|error| migration_io("reading", &path, error))?;
    let marker: ActivationMarker = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::Migration(format!(
            "decoding Hiqlite activation marker {}: {error}",
            path.display()
        ))
    })?;
    marker.validate()?;
    Ok(marker)
}

#[cfg(feature = "hiqlite-store")]
fn write_atomic_private(directory: &Path, filename: &str, bytes: &[u8]) -> Result<(), StoreError> {
    let destination = directory.join(filename);
    let temporary = directory.join(format!(".{filename}.{}.incoming", uuid::Uuid::new_v4()));
    write_private_file(&temporary, bytes)?;
    if let Err(error) = std::fs::rename(&temporary, &destination) {
        let _ = remove_file_if_present(&temporary);
        return Err(migration_io("publishing", &destination, error));
    }
    sync_directory(directory)
}

#[cfg(feature = "hiqlite-store")]
fn path_exists(path: &Path) -> Result<bool, StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(migration_io("inspecting", path, error)),
    }
}

#[cfg(feature = "hiqlite-store")]
fn require_real_directory(path: &Path) -> Result<(), StoreError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| migration_io("inspecting", path, error))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::Migration(format!(
            "active Hiqlite target {} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Validate and durably snapshot the legacy SQLite source.
///
/// The daemon will call this only from `plurxd run`, before any listener or
/// background task starts. A future-schema database is refused before any
/// cleanup or backup write. SQLite's online-backup API is used instead of a
/// filesystem copy so committed pages still resident in `plurx.db-wal` are
/// included in the snapshot.
pub fn prepare_sqlite_import(data_dir: &Path) -> Result<PreparedSqliteImport, StoreError> {
    let source_path = data_dir.join(SQLITE_FILENAME);
    let source = open_source(&source_path)?;
    let schema_version = read_schema_version(&source)?;
    if schema_version > SQLITE_SCHEMA_VERSION {
        return Err(StoreError::Migration(format!(
            "source database schema is v{schema_version}, but this binary only knows \
             v{SQLITE_SCHEMA_VERSION}; refusing clustering import without changing {}",
            data_dir.display()
        )));
    }
    let cluster_id = read_cluster_id(&source)?;

    remove_abandoned_incoming(data_dir)?;

    let migration_dir = data_dir.join(MIGRATION_DIRNAME);
    std::fs::create_dir_all(&migration_dir)
        .map_err(|error| migration_io("creating", &migration_dir, error))?;
    sync_directory(data_dir)?;
    remove_abandoned_backup_temps(&migration_dir)?;

    let temporary_path = migration_dir.join(format!(
        ".{SQLITE_FILENAME}.{}.incoming",
        uuid::Uuid::new_v4()
    ));
    let prepared = create_backup(
        &source,
        &source_path,
        schema_version,
        &cluster_id,
        &migration_dir,
        &temporary_path,
    );
    if let Err(error) = prepared {
        return match remove_file_if_present(&temporary_path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(StoreError::Migration(format!(
                "{error}; additionally failed to clean temporary backup: {cleanup}"
            ))),
        };
    }
    let prepared = prepared?;
    prune_migration_backups(&migration_dir, &prepared.backup_path)?;
    Ok(prepared)
}

/// Keep the newest few source backups and delete the rest.
///
/// Backups are content-addressed, so a repeated attempt against an unchanged
/// source reuses one file. But a failed activation leaves a recovery boot that
/// serves traffic and mutates `plurx.db`, so the next retry addresses different
/// content and writes another full-size copy. Unbounded, a persistently failing
/// activation fills the media server's data volume with complete database
/// copies. Retention is by modification time and never removes the backup this
/// attempt is about to import.
fn prune_migration_backups(migration_dir: &Path, keep: &Path) -> Result<(), StoreError> {
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(migration_dir)
        .map_err(|error| migration_io("reading", migration_dir, error))?
    {
        let entry = entry.map_err(|error| migration_io("reading", migration_dir, error))?;
        let path = entry.path();
        if path == keep || path.extension().is_none_or(|ext| ext != "db") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| migration_io("reading", &path, error))?;
        backups.push((modified, path));
    }
    if backups.len() < MIGRATION_BACKUP_RETENTION {
        return Ok(());
    }
    // Newest first, so the tail past the retained window is what goes. `keep`
    // is excluded above rather than counted, so retention is "this attempt's
    // backup plus the previous MIGRATION_BACKUP_RETENTION - 1".
    backups.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in backups.drain(MIGRATION_BACKUP_RETENTION - 1..) {
        remove_file_if_present(&path)?;
    }
    sync_directory(migration_dir)
}

fn open_source(path: &Path) -> Result<Connection, StoreError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StoreError::Migration(format!(
            "opening SQLite import source {}: {error}",
            path.display()
        ))
    })
}

fn read_schema_version(connection: &Connection) -> Result<i64, StoreError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| StoreError::Migration(format!("reading source schema version: {error}")))
}

fn read_cluster_id(connection: &Connection) -> Result<String, StoreError> {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'instance.id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| StoreError::Migration(format!("reading source instance.id: {error}")))
        .and_then(|cluster_id| {
            if cluster_id.trim().is_empty() {
                Err(StoreError::Migration(
                    "source instance.id is empty".to_owned(),
                ))
            } else {
                Ok(cluster_id)
            }
        })
}

fn remove_abandoned_incoming(data_dir: &Path) -> Result<(), StoreError> {
    let incoming = data_dir.join(HIQLITE_INCOMING_DIRNAME);
    let metadata = match std::fs::symlink_metadata(&incoming) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(migration_io("inspecting", &incoming, error)),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(&incoming)
            .map_err(|error| migration_io("removing abandoned", &incoming, error))?;
    } else {
        std::fs::remove_file(&incoming)
            .map_err(|error| migration_io("removing abandoned", &incoming, error))?;
    }
    sync_directory(data_dir)
}

fn remove_abandoned_backup_temps(migration_dir: &Path) -> Result<(), StoreError> {
    let mut removed = false;
    let entries = std::fs::read_dir(migration_dir)
        .map_err(|error| migration_io("reading", migration_dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| migration_io("reading", migration_dir, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_backup_temp_artifact(&name) {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| migration_io("inspecting", &path, error))?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                return Err(StoreError::Migration(format!(
                    "refusing to recursively remove unexpected backup staging directory {}",
                    path.display()
                )));
            }
            std::fs::remove_file(&path)
                .map_err(|error| migration_io("removing abandoned", &path, error))?;
            removed = true;
        }
    }
    if removed {
        sync_directory(migration_dir)?;
    }
    Ok(())
}

fn is_backup_temp_artifact(name: &str) -> bool {
    let Some(remainder) = name.strip_prefix(&format!(".{SQLITE_FILENAME}.")) else {
        return false;
    };
    let Some((identifier, suffix)) = remainder.split_once(".incoming") else {
        return false;
    };
    !identifier.is_empty() && matches!(suffix, "" | "-wal" | "-shm")
}

fn create_backup(
    source: &Connection,
    source_path: &Path,
    schema_version: i64,
    cluster_id: &str,
    migration_dir: &Path,
    temporary_path: &Path,
) -> Result<PreparedSqliteImport, StoreError> {
    create_private_file(temporary_path)?;
    let mut destination = Connection::open(temporary_path).map_err(|error| {
        StoreError::Migration(format!(
            "opening temporary SQLite backup {}: {error}",
            temporary_path.display()
        ))
    })?;
    {
        let backup = Backup::new(source, &mut destination).map_err(|error| {
            StoreError::Migration(format!("starting SQLite online backup: {error}"))
        })?;
        backup
            .run_to_completion(256, Duration::from_millis(10), None)
            .map_err(|error| {
                StoreError::Migration(format!("copying SQLite import source: {error}"))
            })?;
    }
    destination
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(|error| {
            StoreError::Migration(format!(
                "canonicalizing SQLite backup journal mode: {error}"
            ))
        })?;
    let copied_version = read_schema_version(&destination)?;
    if copied_version != schema_version {
        return Err(StoreError::Migration(format!(
            "SQLite backup schema changed during import preparation: \
             source v{schema_version}, backup v{copied_version}"
        )));
    }
    let integrity: String = destination
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| StoreError::Migration(format!("checking SQLite backup: {error}")))?;
    if integrity != "ok" {
        return Err(StoreError::Migration(format!(
            "SQLite backup failed quick_check: {integrity}"
        )));
    }
    drop(destination);

    File::open(temporary_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| migration_io("syncing", temporary_path, error))?;
    let backup_sha256 = sha256_file(temporary_path)?;
    let backup_path = migration_dir.join(format!("plurx-v{schema_version}-{backup_sha256}.db"));

    if backup_path.exists() {
        let existing_sha256 = sha256_file(&backup_path)?;
        if existing_sha256 != backup_sha256 {
            return Err(StoreError::Migration(format!(
                "existing migration backup {} does not match its content-addressed name",
                backup_path.display()
            )));
        }
        remove_file_if_present(temporary_path)?;
    } else {
        match std::fs::rename(temporary_path, &backup_path) {
            Ok(()) => {}
            Err(error) if backup_path.exists() => {
                let existing_sha256 = sha256_file(&backup_path)?;
                if existing_sha256 != backup_sha256 {
                    return Err(migration_io("publishing", &backup_path, error));
                }
                remove_file_if_present(temporary_path)?;
            }
            Err(error) => return Err(migration_io("publishing", &backup_path, error)),
        }
        sync_directory(migration_dir)?;
    }

    Ok(PreparedSqliteImport {
        source_path: source_path.to_owned(),
        backup_path,
        backup_sha256,
        schema_version,
        cluster_id: cluster_id.to_owned(),
    })
}

fn create_private_file(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|error| migration_io("creating", path, error))?;
    file.write_all(&[])
        .map_err(|error| migration_io("initializing", path, error))
}

fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let mut file = File::open(path).map_err(|error| migration_io("opening", path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| migration_io("hashing", path, error))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn remove_file_if_present(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(migration_io("removing temporary", path, error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| migration_io("syncing directory", path, error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn migration_io(action: &str, path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Migration(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SettingsStore, SqliteStore};

    /// M2 has no remote voter, so an operator's future-facing M3 host setting
    /// must not expose either listener before membership exists. The port and
    /// address family remain configurable so parallel and IPv6-only installs
    /// still work.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn m2_listener_addresses_ignore_explicitly_configured_hosts() {
        let configured_v4: SocketAddr = "192.0.2.40:32401".parse().expect("IPv4 bind");
        assert_eq!(
            local_client_address(configured_v4),
            "127.0.0.1:32401".parse().expect("IPv4 loopback")
        );

        let configured_v6: SocketAddr = "[2001:db8::40]:32402".parse().expect("IPv6 bind");
        assert_eq!(
            local_client_address(configured_v6),
            "[::1]:32402".parse().expect("IPv6 loopback")
        );
    }

    #[tokio::test]
    async fn backup_includes_committed_wal_state_and_removes_abandoned_incoming() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let store = SqliteStore::open(&source_path).expect("source store");
        store
            .put_setting("migration.wal", "committed")
            .await
            .expect("write source");

        let incoming = data.path().join(HIQLITE_INCOMING_DIRNAME);
        std::fs::create_dir_all(incoming.join("partial")).expect("incoming tree");
        std::fs::write(incoming.join("partial/state"), b"not complete").expect("incoming state");

        let prepared = prepare_sqlite_import(data.path()).expect("prepare import");
        assert!(!incoming.exists(), "abandoned target was removed");
        assert_eq!(prepared.schema_version, SQLITE_SCHEMA_VERSION);
        assert_eq!(
            prepared.backup_sha256,
            sha256_file(&prepared.backup_path).expect("hash published backup")
        );

        let backup = Connection::open_with_flags(
            &prepared.backup_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("open backup");
        let value: String = backup
            .query_row(
                "SELECT value FROM settings WHERE key = 'migration.wal'",
                [],
                |row| row.get(0),
            )
            .expect("backed-up setting");
        assert_eq!(value, "committed");
        let journal_mode: String = backup
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read backup journal mode");
        assert_eq!(journal_mode, "delete");
    }

    #[test]
    fn future_schema_refusal_changes_nothing() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let source = Connection::open(&source_path).expect("source");
        source
            .pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION + 1)
            .expect("future version");
        drop(source);
        let incoming = data.path().join(HIQLITE_INCOMING_DIRNAME);
        std::fs::create_dir(&incoming).expect("incoming");
        std::fs::write(incoming.join("state"), b"preserve").expect("state");

        let error = prepare_sqlite_import(data.path()).expect_err("future schema refused");
        assert!(error.to_string().contains("refusing clustering import"));
        assert!(incoming.join("state").exists());
        assert!(!data.path().join(MIGRATION_DIRNAME).exists());
    }

    #[tokio::test]
    async fn content_addressed_backups_are_reused_and_old_backups_are_kept() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let store = SqliteStore::open(&source_path).expect("source store");
        let migration_dir = data.path().join(MIGRATION_DIRNAME);
        std::fs::create_dir(&migration_dir).expect("migration dir");
        let abandoned = migration_dir.join(format!(
            ".{SQLITE_FILENAME}.{}.incoming",
            uuid::Uuid::new_v4()
        ));
        let abandoned_wal = PathBuf::from(format!("{}-wal", abandoned.display()));
        let abandoned_shm = PathBuf::from(format!("{}-shm", abandoned.display()));
        std::fs::write(&abandoned, b"partial backup").expect("abandoned backup");
        std::fs::write(&abandoned_wal, b"partial wal").expect("abandoned backup wal");
        std::fs::write(&abandoned_shm, b"partial shm").expect("abandoned backup shm");
        let first = prepare_sqlite_import(data.path()).expect("first backup");
        assert!(!abandoned.exists(), "abandoned backup temp was removed");
        assert!(!abandoned_wal.exists(), "abandoned backup WAL was removed");
        assert!(!abandoned_shm.exists(), "abandoned backup SHM was removed");
        let repeated = prepare_sqlite_import(data.path()).expect("repeated backup");
        assert_eq!(first, repeated);

        store
            .put_setting("migration.changed", "yes")
            .await
            .expect("change source");
        let changed = prepare_sqlite_import(data.path()).expect("changed backup");
        assert_ne!(changed.backup_path, first.backup_path);
        assert!(first.backup_path.exists(), "original backup is retained");
        assert!(changed.backup_path.exists(), "new backup is retained");
        assert!(
            std::fs::read_dir(data.path().join(MIGRATION_DIRNAME))
                .expect("migration dir")
                .all(|entry| {
                    let entry = entry.expect("migration entry");
                    !is_backup_temp_artifact(&entry.file_name().to_string_lossy())
                }),
            "no temporary backup remains"
        );
    }

    /// A failing activation must not fill the data volume with database copies.
    ///
    /// Content addressing alone does not bound this: each failed attempt leaves
    /// a recovery boot that serves traffic and mutates the source, so the next
    /// retry addresses different content and writes another full-size copy.
    #[tokio::test]
    async fn repeated_activation_attempts_bound_retained_source_backups() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let store = SqliteStore::open(&source_path).expect("source store");

        let mut published = Vec::new();
        // Comfortably past the retention window, each with different content,
        // standing in for the mutation a recovery boot makes between retries.
        for attempt in 0..MIGRATION_BACKUP_RETENTION + 3 {
            store
                .put_setting("migration.attempt", &attempt.to_string())
                .await
                .expect("change source between attempts");
            published.push(
                prepare_sqlite_import(data.path())
                    .expect("attempt backup")
                    .backup_path,
            );
        }

        let retained: Vec<_> = std::fs::read_dir(data.path().join(MIGRATION_DIRNAME))
            .expect("migration dir")
            .map(|entry| entry.expect("migration entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
            .collect();
        assert_eq!(
            retained.len(),
            MIGRATION_BACKUP_RETENTION,
            "retained backups: {retained:?}"
        );
        // The window keeps the newest, and above all the one this attempt is
        // about to import — deleting that would break the activation it serves.
        for recent in published.iter().rev().take(MIGRATION_BACKUP_RETENTION) {
            assert!(retained.contains(recent), "{recent:?} must be retained");
        }
        for old in published
            .iter()
            .take(published.len() - MIGRATION_BACKUP_RETENTION)
        {
            assert!(!old.exists(), "{old:?} must have been pruned");
        }
    }

    #[test]
    fn corrupt_content_addressed_backup_is_refused() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        SqliteStore::open(&source_path).expect("source store");

        let prepared = prepare_sqlite_import(data.path()).expect("first backup");
        std::fs::write(&prepared.backup_path, b"not a SQLite backup")
            .expect("poison published backup");

        let error = prepare_sqlite_import(data.path()).expect_err("corrupt backup refused");
        assert!(
            error
                .to_string()
                .contains("does not match its content-addressed name"),
            "unexpected error: {error}"
        );
    }
}

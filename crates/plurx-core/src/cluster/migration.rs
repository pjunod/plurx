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
use std::io::Seek;
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
    Store, TraktStore, AUTH_SCHEMA_MIGRATION_SOURCE, AUTH_SCHEMA_VERSION,
};
#[cfg(feature = "hiqlite-store")]
use hiqlite::tls::ServerTlsConfig;
#[cfg(feature = "hiqlite-store")]
use hiqlite::{Client, Node, NodeConfig};
#[cfg(feature = "hiqlite-store")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "hiqlite-store")]
use super::membership::{
    decode_join_token, join_token_digest, ClusterPeer, FinalizeJoinRequest, JoinPayload,
    JoinSecrets, LocalMembership, MembershipManager, RedeemJoinRequest,
};

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
const LOCAL_MEMBERSHIP_FILENAME: &str = "membership.json";
#[cfg(feature = "hiqlite-store")]
const HIQLITE_READDRESS_BACKUP_DIRNAME: &str = "hiqlite.before-readdress";
#[cfg(feature = "hiqlite-store")]
const HIQLITE_READDRESS_MARKER_FILENAME: &str = "hiqlite-readdress.json";
#[cfg(feature = "hiqlite-store")]
const HIQLITE_START_TIMEOUT: Duration = Duration::from_secs(45);
/// Hiqlite Raft WAL segment size used by every plurx voter.
///
/// Hiqlite 0.14 accepts one serialized Raft entry up to `wal_size - 34` bytes:
/// its WAL writer subtracts the fixed segment metadata before checking each
/// entry independently. The 16 MiB segment therefore leaves 16,777,182 usable
/// bytes. OpenRaft's configured `max_payload_entries = 128` limits the number
/// of entries in one append request, not the size of an individual entry, and
/// Hiqlite carries that request in a WebSocket binary frame whose length field
/// supports this size without a smaller application cap.
///
/// Import transactions deliberately stay far below the usable entry capacity
/// at the replication-time ceiling measured before this headroom increase.
/// Keep all voters and import bounds tied to this constant so a retune cannot
/// make the contract exercise a different limit than production.
#[cfg(feature = "hiqlite-store")]
pub const HIQLITE_WAL_SIZE_BYTES: u32 = 16 * 1024 * 1024;
/// Bytes Hiqlite WAL 0.14 reserves before one serialized log entry.
#[cfg(feature = "hiqlite-store")]
const HIQLITE_WAL_SEGMENT_RESERVED_BYTES: usize = 34;
/// Largest serialized Raft entry accepted by the configured production WAL.
#[cfg(feature = "hiqlite-store")]
pub const HIQLITE_WAL_USABLE_PAYLOAD_BYTES: usize =
    HIQLITE_WAL_SIZE_BYTES as usize - HIQLITE_WAL_SEGMENT_RESERVED_BYTES;
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

/// Crash-recovery record for the one-voter loopback-to-advertised transition.
///
/// Hiqlite 0.14 cannot replace a voter's address in-place. Before the first
/// peer is admitted, plurx therefore snapshots the state machine and rebuilds
/// only the single-node Raft metadata. This record makes the two directory
/// renames recoverable without ever falling back to the stale legacy source.
#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReaddressRecord {
    version: u32,
    membership: LocalMembership,
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
    membership: MembershipManager,
    local_client: Option<Client>,
    _daemon_lock: File,
}

#[cfg(feature = "hiqlite-store")]
impl SelectedStore {
    /// Read-only watch-state replication projection for the server API.
    #[must_use]
    pub fn replication_monitor(&self) -> status::ReplicationMonitor {
        match self.backend {
            SelectedBackend::Replicated => status::ReplicationMonitor::replicated(
                self.local_client
                    .as_ref()
                    .expect("replicated backend must carry its local client")
                    .clone(),
            ),
            SelectedBackend::SqliteRecovery => {
                debug_assert!(self.local_client.is_none());
                status::ReplicationMonitor::sqlite()
            }
        }
    }

    /// Membership lifecycle and privacy-safe node health for the daemon API.
    #[must_use]
    pub fn membership_manager(&self) -> MembershipManager {
        self.membership.clone()
    }

    /// Finish daemon shutdown before the Tokio runtime tears down the voter.
    ///
    /// Hiqlite 0.14 drains Raft, the WAL, and the SQL writer before it reaches
    /// a known TLS-listener notification defect. `shutdown_voter` contains that
    /// final panic only after matching its exact message; it never substitutes
    /// process exit for the durable-writer drain.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        if let Some(client) = &self.local_client {
            shutdown_voter(client, true).await?;
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
    let daemon_lock = acquire_daemon_lock(&config.storage.data_dir).await?;
    recover_interrupted_readdress(&config.storage.data_dir)?;

    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    if path_exists(&active)? {
        let pending_join = !config.cluster.join_token_file.as_os_str().is_empty()
            && path_exists(&config.cluster.join_token_file)?
            && !path_exists(&active.join(ACTIVATION_MARKER_FILENAME))?;
        if pending_join {
            return join_fresh_store(config, daemon_lock).await;
        }
        readdress_single_voter_if_needed(config)?;
        let selected = open_active_store(config, daemon_lock).await?;
        finalize_pending_join_best_effort(config, &selected).await;
        // A crash immediately after rename may expose the target before its
        // parent-directory entry is durable. Observing it on recovery lets us
        // finish that durability boundary before clearing attempt artifacts.
        sync_directory(&config.storage.data_dir)?;
        remove_abandoned_incoming(&config.storage.data_dir)?;
        remove_activation_attempt(&config.storage.data_dir)?;
        finish_readdress(&config.storage.data_dir)?;
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

    if !config.cluster.join_token_file.as_os_str().is_empty() {
        return join_fresh_store(config, daemon_lock).await;
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
            membership: MembershipManager::unavailable(),
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

#[cfg(feature = "hiqlite-store")]
async fn join_fresh_store(config: &Config, daemon_lock: File) -> Result<SelectedStore, StoreError> {
    let source = config.storage.data_dir.join(SQLITE_FILENAME);
    if path_exists(&source)? {
        return Err(StoreError::Migration(format!(
            "refusing to join with existing local database {}; a cluster join consumes a fresh \
             data directory and never overwrites an installation's library",
            source.display()
        )));
    }
    let token = read_join_token_file(&config.cluster.join_token_file)?;
    let payload = decode_join_token(&token)
        .map_err(|error| StoreError::Migration(format!("{}: {}", error.code(), error)))?;
    // The coordinator owns the expiry verdict. Locally, an unused expired
    // token and an interrupted join already reserved to this node have the
    // same payload; only the replicated token record can distinguish them.
    // Rechecking the embedded timestamp here would strand an identity-bound
    // voter that failed after redemption and restarted after the token TTL.
    if payload.schema_version != AUTH_SCHEMA_VERSION
        || payload.protocol_version != crate::store::AUTH_PROTOCOL_VERSION
    {
        return Err(StoreError::Migration(
            "join_incompatible: joining binary does not match the cluster schema/protocol"
                .to_owned(),
        ));
    }
    let remote = Client::remote(
        payload
            .bootstrap
            .iter()
            .map(|peer| peer.api_address.clone())
            .collect(),
        true,
        true,
        payload.secrets.api.clone(),
        true,
        None,
    )
    .await
    .map_err(|error| {
        StoreError::Database(format!("connecting to cluster for join preflight: {error}"))
    })?;
    HiqliteAuthStore::preflight_voter(&remote, crate::store::ClusterCompatibility::CURRENT).await?;

    let existing_membership = read_local_membership(&config.storage.data_dir)?;
    let identity = match &existing_membership {
        Some(membership) => {
            if membership.cluster_id != payload.cluster_id || membership.raft_id != payload.raft_id
            {
                return Err(StoreError::Identity(
                    "join token does not match the interrupted local membership".to_owned(),
                ));
            }
            super::ClusterIdentity {
                cluster_id: membership.cluster_id.clone(),
                node_id: membership.node_id.clone(),
                raft_id: membership.raft_id,
            }
        }
        None => super::initialize_join_identity(
            &config.storage.data_dir,
            &payload.cluster_id,
            payload.raft_id,
        )?,
    };
    let local = configured_local_peer(config, payload.raft_id)?;
    let token_digest = join_token_digest(&token);
    let membership = LocalMembership {
        version: 1,
        cluster_id: payload.cluster_id.clone(),
        node_id: identity.node_id.clone(),
        raft_id: payload.raft_id,
        local: local.clone(),
        bootstrap: payload.bootstrap.clone(),
        join_token_digest: Some(token_digest.clone()),
    };
    redeem_remote_join(
        &payload,
        RedeemJoinRequest {
            token_digest,
            raft_id: payload.raft_id,
            node_id: identity.node_id.clone(),
            raft_address: local.raft_address.clone(),
            api_address: local.api_address.clone(),
            schema_version: AUTH_SCHEMA_VERSION,
            protocol_version: crate::store::AUTH_PROTOCOL_VERSION,
        },
    )
    .await?;

    persist_join_secret(
        &config.storage.data_dir.join(RAFT_SECRET_FILENAME),
        &payload.secrets.raft,
    )?;
    persist_join_secret(
        &config.storage.data_dir.join(API_SECRET_FILENAME),
        &payload.secrets.api,
    )?;
    persist_join_secret(
        &config.cluster.credential_key_path(&config.storage.data_dir),
        &payload.secrets.credential_key,
    )?;
    let secrets = read_existing_secrets(&config.storage.data_dir)?;
    // A verified activation marker, not the directory name, is the commit bit.
    // Hiqlite 0.14 cannot stop and rebind fully-TLS listeners in one process,
    // so the joiner starts at its final path. A crash before the marker leaves
    // the token and node id in place; the next boot re-enters this path, redeems
    // idempotently for the same node, and resumes the partial voter.
    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    std::fs::create_dir_all(&active).map_err(|error| migration_io("creating", &active, error))?;
    sync_directory(&config.storage.data_dir)?;
    let (client, _) = start_voter(
        config,
        &active,
        &secrets,
        &identity,
        Some(&membership),
        true,
        false,
    )
    .await?;
    let store = HiqliteAuthStore::open(client.clone(), &active.join("telemetry.db")).await?;
    verify_store_identity(&store, &payload.cluster_id).await?;
    write_activation_marker(&active, &payload.activation_marker)?;
    write_local_membership(&config.storage.data_dir, &membership)?;
    sync_directory(&active)?;
    sync_directory(&config.storage.data_dir)?;
    ensure_activated_source_record(&config.storage.data_dir, &payload.activation_marker)?;

    // Keep the caught-up voter alive. Fully-TLS Hiqlite listeners have no
    // graceful-shutdown handle, and no stop/rebind boundary is needed because
    // every durable file already lives at the final path.
    let credential_key = open_active_credential_key(config, &store).await?;
    let store: Arc<dyn Store> = Arc::new(store);
    let membership_manager = MembershipManager::replicated(
        client.clone(),
        Arc::clone(&store),
        identity.clone(),
        local,
        configured_join_url(config)?,
        JoinSecrets {
            raft: secrets.raft,
            api: secrets.api,
            credential_key: read_secret(
                &config.cluster.credential_key_path(&config.storage.data_dir),
            )?,
        },
        payload.activation_marker,
    )
    .await
    .map_err(|error| StoreError::Database(error.to_string()))?;
    let selected = SelectedStore {
        store,
        identity,
        credential_key,
        backend: SelectedBackend::Replicated,
        membership: membership_manager,
        local_client: Some(client),
        _daemon_lock: daemon_lock,
    };
    finalize_pending_join_best_effort(config, &selected).await;
    Ok(selected)
}

#[cfg(feature = "hiqlite-store")]
async fn finalize_pending_join_best_effort(config: &Config, selected: &SelectedStore) {
    if let Err(error) = finalize_pending_join(config, selected).await {
        // The voter and activation marker are already durable at this point.
        // Finalization only consumes the coordinator's one-time record, so a
        // temporarily unavailable coordinator must not take this voter back
        // offline. The identity-bound token file remains for the next boot.
        tracing::warn!(
            error = %error,
            "joined voter is healthy but token finalization is pending; startup will retry"
        );
    }
}

#[cfg(feature = "hiqlite-store")]
async fn finalize_pending_join(
    config: &Config,
    selected: &SelectedStore,
) -> Result<(), StoreError> {
    let path = &config.cluster.join_token_file;
    if path.as_os_str().is_empty() || !path_exists(path)? {
        return Ok(());
    }
    let Some(membership) = read_local_membership(&config.storage.data_dir)? else {
        tracing::warn!("join-token file is present but this node has no staged join; ignoring it");
        return Ok(());
    };
    let Some(expected_digest) = membership.join_token_digest.as_deref() else {
        tracing::warn!(
            "join-token file is present on an initial voter that was never admitted; ignoring it"
        );
        return Ok(());
    };
    let token = read_join_token_file(path)?;
    let token_digest = join_token_digest(&token);
    if token_digest != expected_digest {
        tracing::warn!("join-token file does not match this node's staged join; ignoring it");
        return Ok(());
    }
    let payload = decode_join_token(&token)
        .map_err(|error| StoreError::Migration(format!("{}: {}", error.code(), error)))?;
    if payload.cluster_id != membership.cluster_id || payload.raft_id != membership.raft_id {
        return Err(StoreError::Identity(
            "staged join token does not match local membership identity".to_owned(),
        ));
    }
    finalize_remote_join(
        &payload,
        FinalizeJoinRequest {
            token_digest,
            raft_id: membership.raft_id,
            node_id: selected.identity.node_id.clone(),
        },
    )
    .await?;
    std::fs::remove_file(path)
        .map_err(|error| migration_io("removing redeemed token", path, error))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn read_join_token_file(path: &Path) -> Result<String, StoreError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| migration_io("reading join token", path, error))?;
    let token = raw.trim();
    if token.is_empty() || token.lines().count() != 1 {
        return Err(StoreError::Migration(
            "join_token_invalid: join-token file must contain exactly one token".to_owned(),
        ));
    }
    Ok(token.to_owned())
}

#[cfg(feature = "hiqlite-store")]
fn persist_join_secret(path: &Path, secret: &str) -> Result<(), StoreError> {
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StoreError::Migration(
            "join_token_invalid: encrypted cluster secret is malformed".to_owned(),
        ));
    }
    if path_exists(path)? {
        if read_secret(path)? != secret {
            return Err(StoreError::Identity(
                "join token secrets do not match the interrupted local join".to_owned(),
            ));
        }
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        StoreError::Migration("join secret path has no parent directory".to_owned())
    })?;
    std::fs::create_dir_all(parent).map_err(|error| migration_io("creating", parent, error))?;
    write_private_file(path, format!("{secret}\n").as_bytes())?;
    sync_directory(parent)
}

#[cfg(feature = "hiqlite-store")]
async fn redeem_remote_join(
    payload: &JoinPayload,
    request: RedeemJoinRequest,
) -> Result<(), StoreError> {
    post_join_request(payload, "/api/v1/cluster/join/redeem", &request).await
}

#[cfg(feature = "hiqlite-store")]
async fn finalize_remote_join(
    payload: &JoinPayload,
    request: FinalizeJoinRequest,
) -> Result<(), StoreError> {
    post_join_request(payload, "/api/v1/cluster/join/finalize", &request).await
}

#[cfg(feature = "hiqlite-store")]
async fn post_join_request<T: Serialize>(
    payload: &JoinPayload,
    path: &str,
    request: &T,
) -> Result<(), StoreError> {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", payload.bootstrap_http))
        .json(request)
        .send()
        .await
        .map_err(|error| StoreError::Database(format!("contacting join coordinator: {error}")))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let error = response
        .json::<JoinApiError>()
        .await
        .unwrap_or(JoinApiError {
            code: "membership_internal".to_owned(),
            message: "join coordinator refused the request".to_owned(),
        });
    Err(StoreError::Migration(format!(
        "{}: {} (HTTP {status})",
        error.code, error.message
    )))
}

#[cfg(feature = "hiqlite-store")]
#[derive(Deserialize)]
struct JoinApiError {
    code: String,
    message: String,
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
    let identity = super::initialize_identity(&config.storage.data_dir, &marker.cluster_id)?;
    let secret_api = read_secret(&config.storage.data_dir.join(API_SECRET_FILENAME))?;
    let address = local_voter_api_address(config, &identity)?;
    let client = Client::remote(vec![address], true, true, secret_api, true, None)
        .await
        .map_err(|error| {
            StoreError::Database(format!(
                "connecting to the running local Hiqlite voter: {error}"
            ))
        })?;
    let store = HiqliteAuthStore::open(client, &active.join("telemetry.db")).await?;
    verify_store_identity(&store, &marker.cluster_id).await?;
    Ok(Arc::new(store))
}

#[cfg(feature = "hiqlite-store")]
fn local_voter_api_address(
    config: &Config,
    identity: &super::ClusterIdentity,
) -> Result<String, StoreError> {
    match read_local_membership(&config.storage.data_dir)? {
        Some(membership)
            if membership.cluster_id == identity.cluster_id
                && membership.node_id == identity.node_id =>
        {
            Ok(membership.local.api_address)
        }
        Some(_) => Err(StoreError::Identity(
            "membership.json does not match activation.json and node.id".to_owned(),
        )),
        None => Ok(local_client_address(config.cluster.api_bind).to_string()),
    }
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

    // The temporary import voter always stays on loopback. This preserves the
    // pre-M3 activation failpoints and lets cleartext staging shut down cleanly.
    // Once the atomic active target exists, the closed-store readdress path
    // below rebuilds sole-voter metadata with an explicitly advertised address.
    let client = match start_voter(config, &incoming, &secrets, &identity, None, false, true).await
    {
        Ok((client, _)) => client,
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
        Err(error) => return abort_incoming(client, &incoming, error, false).await,
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
            return abort_incoming(client, &incoming, error, false).await;
        }
    };
    let marker = ActivationMarker::from_report(&prepared.cluster_id, report);
    if let Err(error) = write_activation_marker(&incoming, &marker) {
        drop(store);
        return abort_incoming(client, &incoming, error, false).await;
    }
    ActivationFailpoint::crash_if(failpoint, ActivationFailpoint::Marker);

    drop(store);
    if let Err(error) = shutdown_voter(&client, false).await {
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

    readdress_single_voter_if_needed(config)?;
    let selected = open_active_store_with_key(config, daemon_lock, Some(credential_key)).await?;
    finish_readdress(&config.storage.data_dir)?;
    Ok(selected)
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
            || !(AUTH_SCHEMA_MIGRATION_SOURCE..=AUTH_SCHEMA_VERSION)
                .contains(&self.replicated_schema_version)
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

/// Rebuild a sole voter's local Raft metadata around its unchanged state
/// machine, for the two cases that need it before Hiqlite opens the target.
///
/// **Address change.** Openraft supports an atomic `SetNodes` address update,
/// but Hiqlite 0.14 does not expose it. Rebuilding only the one-voter Raft
/// metadata from Hiqlite's metadata-reset backup is safe while there is exactly
/// one voter; doing the same after a peer exists would fork membership history
/// and is refused.
///
/// **Ungraceful shutdown.** Hiqlite's `auto-heal` deletes the whole
/// state-machine database whenever it finds its own lock file at startup, and
/// expects the Raft log to rebuild it. Activation imports the operator's SQLite
/// data straight into that database rather than through Raft, and nothing
/// snapshots before `logs_until_snapshot`, so for an activated node that
/// replay reconstructs nothing and the install is lost. Running first, while
/// the database is still intact, turns that into an ordinary crash recovery:
/// the committed state machine is preserved and only the one-voter Raft
/// metadata is rebuilt around it. A sole voter acks a write only after applying
/// it, so the discarded log tail holds nothing a client was told was durable.
/// A node with peers is left to Hiqlite, whose catch-up from the leader is the
/// correct recovery there and does not fork membership.
#[cfg(feature = "hiqlite-store")]
fn readdress_single_voter_if_needed(config: &Config) -> Result<(), StoreError> {
    let active = config.storage.data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    let ungraceful_shutdown = path_exists(&active.join("state_machine").join("lock"))?;
    let advertised = !config.cluster.advertise_host.trim().is_empty();
    if !advertised && !ungraceful_shutdown {
        return Ok(());
    }
    let existing_membership = read_local_membership(&config.storage.data_dir)?;
    // Settle on whether the committed address already matches configuration,
    // not on whether it looks like loopback. A loopback-literal advertise_host
    // is a legitimate configuration - two daemons on one host, and the whole
    // cluster harness - and testing loopback-ness there never reaches a fixed
    // point, so every boot would rebuild the state machine and every joined
    // follower would refuse to start.
    let address_changed = match &existing_membership {
        Some(membership) if advertised => {
            membership.local != configured_local_peer(config, membership.raft_id)?
        }
        Some(_) => false,
        None => advertised,
    };
    if !address_changed && !ungraceful_shutdown {
        return Ok(());
    }

    let marker = read_activation_marker(&active)?;
    let mut identity = super::initialize_identity(&config.storage.data_dir, &marker.cluster_id)?;
    if let Some(membership) = &existing_membership {
        if membership.cluster_id != marker.cluster_id || membership.node_id != identity.node_id {
            return Err(StoreError::Identity(
                "membership.json does not match activation.json and node.id".to_owned(),
            ));
        }
        identity.raft_id = membership.raft_id;
    }
    // Decide sole-voter-ness from replicated cluster_nodes rows in the closed
    // state machine, not from membership.json. redeem() writes an admitted peer
    // there before it answers the joining node, whereas membership.json records
    // the bootstrap list once and is never refreshed, so a coordinator that has
    // since admitted a peer still reads as a lone voter and would rebuild
    // one-node Raft metadata that silently ejects it.
    let admitted_peers = admitted_peer_count(&active, identity.raft_id)?;
    if admitted_peers > 0 {
        if address_changed {
            return Err(StoreError::Migration(format!(
                "cluster.advertise_host cannot change after another voter or learner exists \
                 ({admitted_peers} other node(s) are admitted); use a future online \
                 membership-reconfiguration path instead"
            )));
        }
        // Ungraceful shutdown with peers: rebuilding one-node Raft metadata
        // here would eject them. Hiqlite rebuilds this node from the leader.
        tracing::warn!(
            admitted_peers,
            "recovering from an ungraceful shutdown by catching up from the cluster"
        );
        return Ok(());
    }
    let desired = configured_local_peer(config, identity.raft_id)?;
    let backup = config
        .storage
        .data_dir
        .join(HIQLITE_READDRESS_BACKUP_DIRNAME);
    if path_exists(&backup)? {
        return Err(StoreError::Migration(format!(
            "refusing readdress because recovery directory {} already exists",
            backup.display()
        )));
    }

    // The daemon lock proves no voter is running from this directory. Snapshot
    // the closed SQLite state machine directly and clear only Hiqlite's private
    // applied-log metadata, matching its own backup implementation. The next
    // open initializes a new one-voter Raft log around unchanged application
    // rows and validates instance.id before the old target is removed.
    remove_abandoned_incoming(&config.storage.data_dir)?;
    let incoming = config.storage.data_dir.join(HIQLITE_INCOMING_DIRNAME);
    std::fs::create_dir_all(&incoming)
        .map_err(|error| migration_io("creating", &incoming, error))?;
    let state_target = incoming
        .join("state_machine")
        .join("db")
        .join(HIQLITE_DATABASE_FILENAME);
    if let Err(error) = snapshot_hiqlite_state_machine(
        &active
            .join("state_machine")
            .join("db")
            .join(HIQLITE_DATABASE_FILENAME),
        &state_target,
    ) {
        let _ = remove_abandoned_incoming(&config.storage.data_dir);
        return Err(error);
    }

    // telemetry.db is node-local, but it still contains useful continuity for
    // status and should survive a membership-address repair.
    let telemetry_target = incoming.join("telemetry.db");
    if let Err(error) = snapshot_sqlite_file(&active.join("telemetry.db"), &telemetry_target) {
        let _ = remove_abandoned_incoming(&config.storage.data_dir);
        return Err(error);
    }

    write_activation_marker(&incoming, &marker)?;
    verify_state_machine_snapshot_identity(&state_target, &marker.cluster_id)?;
    let membership = LocalMembership {
        version: 1,
        cluster_id: identity.cluster_id.clone(),
        node_id: identity.node_id.clone(),
        raft_id: identity.raft_id,
        local: desired.clone(),
        bootstrap: vec![desired],
        join_token_digest: existing_membership.and_then(|membership| membership.join_token_digest),
    };
    sync_directory(&incoming)?;

    write_readdress_record(&config.storage.data_dir, &membership)?;
    std::fs::rename(&active, &backup)
        .map_err(|error| migration_io("parking pre-readdress target", &backup, error))?;
    if let Err(error) = std::fs::rename(&incoming, &active) {
        let restore = std::fs::rename(&backup, &active);
        return Err(match restore {
            Ok(()) => migration_io("activating readdressed target", &active, error),
            Err(restore) => StoreError::Migration(format!(
                "activating readdressed target {} failed: {error}; restoring {} also failed: \
                 {restore}",
                active.display(),
                backup.display()
            )),
        });
    }
    sync_directory(&config.storage.data_dir)?;
    write_local_membership(&config.storage.data_dir, &membership)?;
    Ok(())
}

/// Count admitted cluster nodes other than this one, read directly from the
/// closed replicated state machine.
///
/// The daemon lock proves no voter is running, so this is a consistent read of
/// committed state: `redeem` inserts an admitted peer into `cluster_nodes`
/// before answering the joining node, and `remove_voter` tombstones it with
/// `removed_at`. A store activated before the membership schema existed has no
/// such table, which means no peer was ever admitted.
#[cfg(feature = "hiqlite-store")]
fn admitted_peer_count(active: &Path, raft_id: u64) -> Result<u64, StoreError> {
    let database = active
        .join("state_machine")
        .join("db")
        .join(HIQLITE_DATABASE_FILENAME);
    if !path_exists(&database)? {
        return Ok(0);
    }
    let connection = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StoreError::Migration(format!(
            "opening state machine {} to check admitted nodes: {error}",
            database.display()
        ))
    })?;
    let table_exists = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cluster_nodes'",
            (),
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            StoreError::Migration(format!("inspecting replicated membership schema: {error}"))
        })?;
    if table_exists == 0 {
        return Ok(0);
    }
    let peers = connection
        .query_row(
            "SELECT COUNT(*) FROM cluster_nodes WHERE removed_at IS NULL AND raft_id != ?1",
            [raft_id as i64],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            StoreError::Migration(format!("reading replicated cluster node records: {error}"))
        })?;
    Ok(peers.max(0) as u64)
}

#[cfg(feature = "hiqlite-store")]
fn snapshot_hiqlite_state_machine(source: &Path, target: &Path) -> Result<(), StoreError> {
    snapshot_sqlite_file(source, target)?;
    let connection = Connection::open(target).map_err(|error| {
        StoreError::Migration(format!(
            "opening readdressed state-machine snapshot {}: {error}",
            target.display()
        ))
    })?;
    connection
        .execute("DELETE FROM _metadata", ())
        .map_err(|error| {
            StoreError::Migration(format!(
                "resetting readdressed Raft state-machine metadata: {error}"
            ))
        })?;
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(|error| {
            StoreError::Migration(format!(
                "canonicalizing readdressed state-machine snapshot: {error}"
            ))
        })?;
    drop(connection);
    File::open(target)
        .and_then(|file| file.sync_all())
        .map_err(|error| migration_io("syncing", target, error))
}

#[cfg(feature = "hiqlite-store")]
fn verify_state_machine_snapshot_identity(
    snapshot: &Path,
    expected: &str,
) -> Result<(), StoreError> {
    let connection = Connection::open_with_flags(
        snapshot,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StoreError::Migration(format!(
            "opening readdressed state-machine snapshot {}: {error}",
            snapshot.display()
        ))
    })?;
    let actual = connection
        .query_row(
            "SELECT value FROM settings WHERE key = 'instance.id'",
            (),
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| {
            StoreError::Migration(format!(
                "reading instance.id from readdressed state-machine snapshot: {error}"
            ))
        })?;
    if actual != expected {
        return Err(StoreError::Identity(format!(
            "readdressed state-machine instance.id is {actual}, expected {expected}"
        )));
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn snapshot_sqlite_file(source: &Path, target: &Path) -> Result<(), StoreError> {
    let parent = target.parent().expect("SQLite snapshot target has parent");
    std::fs::create_dir_all(parent).map_err(|error| migration_io("creating", parent, error))?;
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StoreError::Migration(format!(
            "opening node-local SQLite {}: {error}",
            source.display()
        ))
    })?;
    create_private_file(target)?;
    let mut target_connection = Connection::open(target).map_err(|error| {
        StoreError::Migration(format!(
            "opening node-local snapshot {}: {error}",
            target.display()
        ))
    })?;
    {
        let backup = Backup::new(&source_connection, &mut target_connection).map_err(|error| {
            StoreError::Migration(format!("starting node-local SQLite snapshot: {error}"))
        })?;
        backup
            .run_to_completion(256, Duration::from_millis(10), None)
            .map_err(|error| {
                StoreError::Migration(format!("copying node-local SQLite snapshot: {error}"))
            })?;
    }
    drop(target_connection);
    File::open(target)
        .and_then(|file| file.sync_all())
        .map_err(|error| migration_io("syncing", target, error))
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
    let mut marker = read_activation_marker(&active)?;
    let mut local_membership = read_local_membership(&config.storage.data_dir)?;
    let mut identity = super::initialize_identity(&config.storage.data_dir, &marker.cluster_id)?;
    if let Some(membership) = &local_membership {
        if membership.cluster_id != marker.cluster_id || membership.node_id != identity.node_id {
            return Err(StoreError::Identity(
                "membership.json does not match activation.json and node.id".to_owned(),
            ));
        }
        identity.raft_id = membership.raft_id;
    }
    let secrets = read_existing_secrets(&config.storage.data_dir)?;
    let force_loopback = should_force_loopback(config, local_membership.as_ref());
    let (client, local) = start_voter(
        config,
        &active,
        &secrets,
        &identity,
        local_membership.as_ref(),
        true,
        force_loopback,
    )
    .await?;
    let store =
        match HiqliteAuthStore::open_or_migrate(client.clone(), &active.join("telemetry.db")).await
        {
            Ok(store) => store,
            Err(error) => return Err(error),
        };
    if marker.replicated_schema_version != AUTH_SCHEMA_VERSION {
        marker.replicated_schema_version = AUTH_SCHEMA_VERSION;
        write_activation_marker(&active, &marker)?;
        sync_directory(&active)?;
    }
    if let Err(error) = verify_store_identity(&store, &marker.cluster_id).await {
        drop(store);
        return Err(error);
    }
    // Written here rather than beside the rename so a crash in between still
    // converges: any boot that successfully opens an active target re-asserts
    // it, and this is the only place that can be reached without one.
    if let Err(error) = ensure_activated_source_record(&config.storage.data_dir, &marker) {
        drop(store);
        return Err(error);
    }
    let credential_key = match credential_key {
        Some(key) => key,
        None => match open_active_credential_key(config, &store).await {
            Ok(key) => key,
            Err(error) => {
                drop(store);
                return Err(error);
            }
        },
    };
    let store: Arc<dyn Store> = Arc::new(store);
    let membership_file = match local_membership.take() {
        Some(mut membership) => {
            if membership.local != local {
                membership.local = local.clone();
                for peer in &mut membership.bootstrap {
                    if peer.raft_id == local.raft_id {
                        *peer = local.clone();
                    }
                }
                write_local_membership(&config.storage.data_dir, &membership)?;
            }
            membership
        }
        None => {
            let metrics = client.metrics_db().await.map_err(|error| {
                StoreError::Database(format!("reading initial cluster membership: {error}"))
            })?;
            let membership = LocalMembership {
                version: 1,
                cluster_id: identity.cluster_id.clone(),
                node_id: identity.node_id.clone(),
                raft_id: identity.raft_id,
                local: local.clone(),
                bootstrap: metrics
                    .membership_config
                    .nodes()
                    .map(|(_, node)| ClusterPeer::from(node))
                    .collect(),
                join_token_digest: None,
            };
            write_local_membership(&config.storage.data_dir, &membership)?;
            membership
        }
    };
    let credential_key_secret =
        read_secret(&config.cluster.credential_key_path(&config.storage.data_dir))?;
    let membership = MembershipManager::replicated(
        client.clone(),
        Arc::clone(&store),
        identity.clone(),
        membership_file.local,
        configured_join_url(config)?,
        JoinSecrets {
            raft: secrets.raft,
            api: secrets.api,
            credential_key: credential_key_secret,
        },
        marker,
    )
    .await
    .map_err(|error| StoreError::Database(error.to_string()))?;
    Ok(SelectedStore {
        store,
        identity,
        credential_key,
        backend: SelectedBackend::Replicated,
        membership,
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

/// Who the lock file says is holding the data directory.
///
/// This never decides whether to refuse — the advisory lock is the only
/// authority for that, and a recorded id can be stale, truncated, or written
/// by a process that has since died. It exists so a refusal can say which of
/// two very different conditions it actually saw, because they call for
/// opposite responses from whoever reads the message.
#[cfg(feature = "hiqlite-store")]
#[derive(Debug, PartialEq, Eq)]
enum DaemonLockHolder {
    /// The recorded owner is this very process.
    ///
    /// Not a second daemon under any reading: an earlier activation inside
    /// this process still has its lock handle open. In production that is a
    /// leak to fix here; in an in-process test it is the previous activation
    /// not having finished dropping.
    ThisProcess(u32),
    /// A different process. This is the genuine double-start.
    OtherProcess(u32),
    /// The lock file carried no usable owner record — it predates this
    /// bookkeeping, or the holder was interrupted between taking the lock and
    /// recording itself. Treated as another process, because the lock is held
    /// and nothing says it is ours.
    Unidentified,
}

/// How long startup re-attempts the data-directory lock before concluding that
/// another daemon owns it.
///
/// The lock lives exactly as long as the owner's open handle, and closing that
/// handle is the last thing a departing owner does. A container restarted by
/// `Restart=always`, a `systemctl restart`, and an in-process test that
/// re-activates the same directory all hand the incoming owner a predecessor
/// that is still finishing. Refusing on the first `WouldBlock` reported every
/// one of them as "another plurxd process already owns the data directory" —
/// a message that in production means a genuine double-start and is a correct,
/// serious refusal.
///
/// Waiting a bounded window costs nothing on a quiet host, where the first
/// attempt succeeds, and it cannot let a second live daemon through: a running
/// owner holds the lock for its entire lifetime, so it is still holding it when
/// the window expires. The window only converts "not free at this instant" into
/// "not free for five seconds", which is the claim the refusal actually makes.
#[cfg(feature = "hiqlite-store")]
const DAEMON_LOCK_ACQUIRE_WINDOW: Duration = Duration::from_secs(5);

/// Gap between attempts inside [`DAEMON_LOCK_ACQUIRE_WINDOW`].
///
/// Short enough that a predecessor closing its handle is picked up promptly,
/// long enough that the wait is not a spin. There is no OS notification for an
/// advisory lock being released without blocking on it, and blocking would give
/// up the bound this whole path exists to keep.
#[cfg(feature = "hiqlite-store")]
const DAEMON_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(feature = "hiqlite-store")]
async fn acquire_daemon_lock(data_dir: &Path) -> Result<File, StoreError> {
    acquire_daemon_lock_within(data_dir, DAEMON_LOCK_ACQUIRE_WINDOW).await
}

#[cfg(feature = "hiqlite-store")]
async fn acquire_daemon_lock_within(data_dir: &Path, window: Duration) -> Result<File, StoreError> {
    let path = data_dir.join(DAEMON_LOCK_FILENAME);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|error| migration_io("opening", &path, error))?;
    let mut waited = Duration::ZERO;
    loop {
        match file.try_lock() {
            Ok(()) => {
                record_daemon_lock_holder(&mut file, &path);
                return Ok(file);
            }
            Err(std::fs::TryLockError::WouldBlock) if waited < window => {
                // Async, not `thread::sleep`: this runs on the daemon runtime,
                // and the predecessor whose handle we are waiting on may need
                // that same runtime to finish dropping it.
                let step = DAEMON_LOCK_RETRY_INTERVAL.min(window - waited);
                tokio::time::sleep(step).await;
                waited += step;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(daemon_lock_conflict(
                    data_dir,
                    &read_daemon_lock_holder(&path),
                    waited,
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(migration_io("locking", &path, error))
            }
        }
    }
}

/// Record this process as the owner, so a later contender's refusal can name
/// it. Written under the lock we just took, so no two writers race here.
///
/// Best effort on purpose. The record only ever improves somebody else's error
/// message, and the lock — which is the thing that actually protects the data
/// directory — is already held by the time this runs. Refusing startup because
/// a seven-byte cosmetic write failed would trade a working server for a nicer
/// diagnostic; a contender falls back to
/// [`DaemonLockHolder::Unidentified`] instead.
#[cfg(feature = "hiqlite-store")]
fn record_daemon_lock_holder(file: &mut File, path: &Path) {
    if let Err(error) = file
        .set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|_| writeln!(file, "{}", std::process::id()))
        .and_then(|()| file.flush())
    {
        tracing::debug!(
            path = %path.display(),
            %error,
            "could not record this process as the data-directory lock owner"
        );
    }
}

/// Read the recorded owner without taking the lock. Every failure is
/// [`DaemonLockHolder::Unidentified`]: this only ever improves a message, so a
/// missing or malformed record must not turn into a startup error of its own.
#[cfg(feature = "hiqlite-store")]
fn read_daemon_lock_holder(path: &Path) -> DaemonLockHolder {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|recorded| recorded.trim().parse::<u32>().ok())
    {
        Some(pid) if pid == std::process::id() => DaemonLockHolder::ThisProcess(pid),
        Some(pid) => DaemonLockHolder::OtherProcess(pid),
        None => DaemonLockHolder::Unidentified,
    }
}

/// Describe a lock that stayed held for the whole acquire window.
///
/// The two conditions are not variants of one complaint. Another live process
/// means the operator started a second daemon on one data directory and must
/// stop one of them; this process means our own handle leaked and no operator
/// action would help. Printing the first text for the second condition sends
/// whoever reads it looking for a process that does not exist.
#[cfg(feature = "hiqlite-store")]
fn daemon_lock_conflict(
    data_dir: &Path,
    holder: &DaemonLockHolder,
    waited: Duration,
) -> StoreError {
    let data_dir = data_dir.display();
    let waited = waited.as_secs_f32();
    StoreError::Migration(match holder {
        DaemonLockHolder::ThisProcess(pid) => format!(
            "the data directory {data_dir} is still locked inside this plurxd process \
             (pid {pid}) after waiting {waited:.1}s for the previous owner's handle to \
             close. No second plurxd process is involved: an earlier activation in this \
             process never dropped its data-directory lock"
        ),
        DaemonLockHolder::OtherProcess(pid) => format!(
            "another plurxd process already owns the data directory {data_dir} \
             (pid {pid}) and still held it after waiting {waited:.1}s"
        ),
        DaemonLockHolder::Unidentified => format!(
            "another plurxd process already owns the data directory {data_dir} \
             (owner pid not recorded) and still held it after waiting {waited:.1}s"
        ),
    })
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
async fn start_voter(
    config: &Config,
    target: &Path,
    secrets: &ClusterSecrets,
    identity: &super::ClusterIdentity,
    local_membership: Option<&LocalMembership>,
    active_transport: bool,
    force_loopback: bool,
) -> Result<(Client, ClusterPeer), StoreError> {
    let raft_bind = if force_loopback {
        local_client_address(config.cluster.raft_bind)
    } else {
        config.cluster.raft_bind
    };
    let api_bind = if force_loopback {
        local_client_address(config.cluster.api_bind)
    } else {
        config.cluster.api_bind
    };
    validate_cluster_listener("raft", raft_bind, active_transport)?;
    validate_cluster_listener("cluster API", api_bind, active_transport)?;

    let local = match local_membership {
        Some(membership) => {
            if membership.version != 1
                || membership.cluster_id != identity.cluster_id
                || membership.node_id != identity.node_id
                || membership.raft_id != identity.raft_id
                || membership.local.raft_id != identity.raft_id
            {
                return Err(StoreError::Identity(
                    "local cluster membership does not match node.id or activation marker"
                        .to_owned(),
                ));
            }
            configured_or_persisted_local_peer(config, membership, force_loopback)?
        }
        None if force_loopback => ClusterPeer {
            raft_id: identity.raft_id,
            raft_address: raft_bind.to_string(),
            api_address: api_bind.to_string(),
        },
        None => configured_local_peer(config, identity.raft_id)?,
    };
    let mut nodes = local_membership
        .map(|membership| membership.bootstrap.clone())
        .unwrap_or_default();
    nodes.retain(|peer| peer.raft_id != local.raft_id);
    nodes.push(local.clone());
    nodes.sort_by_key(|peer| peer.raft_id);
    nodes.dedup_by_key(|peer| peer.raft_id);
    let nodes = hiqlite_nodes_for_local(nodes, &local)?;

    // The daemon lock guards one data directory, but these ports are host-wide
    // and default to fixed values, so two data directories on one host collide.
    // Without this check the node does not report a bind conflict: it reaches a
    // foreign voter's listener, retries the handshake for the full start
    // timeout, and then blames the migration. Check first and say which port.
    for (role, address) in [("raft", raft_bind), ("cluster API", api_bind)] {
        if let Err(error) = std::net::TcpListener::bind(address) {
            return Err(StoreError::Migration(format!(
                "the {role} port {address} is not available: {error}. Another plurxd on \
                 this host is using it — cluster ports are host-wide, so a second data \
                 directory needs its own raft_bind and api_bind"
            )));
        }
    }
    let node_config = NodeConfig {
        node_id: identity.raft_id,
        nodes,
        listen_addr_api: Cow::Owned(api_bind.ip().to_string()),
        listen_addr_raft: Cow::Owned(raft_bind.ip().to_string()),
        data_dir: Cow::Owned(target.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed(HIQLITE_DATABASE_FILENAME),
        secret_raft: secrets.raft.clone(),
        secret_api: secrets.api.clone(),
        tls_raft: active_transport.then_some(ServerTlsConfig::TlsAutoCertificates),
        tls_api: active_transport.then_some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: HIQLITE_WAL_SIZE_BYTES,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    };
    let client = hiqlite::start_node(node_config)
        .await
        .map_err(|error| StoreError::Database(format!("starting Hiqlite voter: {error}")))?;
    if tokio::time::timeout(HIQLITE_START_TIMEOUT, client.wait_until_healthy_db())
        .await
        .is_err()
    {
        let _ = shutdown_voter(&client, active_transport).await;
        return Err(StoreError::Database(format!(
            "Hiqlite voter did not become healthy within {HIQLITE_START_TIMEOUT:?}"
        )));
    }
    let promotion_deadline = tokio::time::Instant::now() + HIQLITE_START_TIMEOUT;
    loop {
        let metrics = client
            .metrics_db()
            .await
            .map_err(|error| StoreError::Database(format!("reading voter membership: {error}")))?;
        if metrics
            .membership_config
            .voter_ids()
            .any(|raft_id| raft_id == identity.raft_id)
        {
            break;
        }
        if tokio::time::Instant::now() >= promotion_deadline {
            let is_learner = metrics
                .membership_config
                .nodes()
                .any(|(raft_id, _)| *raft_id == identity.raft_id);
            let _ = shutdown_voter(&client, active_transport).await;
            let reason = if is_learner {
                "is still a learner and was not promoted before the start timeout"
            } else {
                "is absent from committed membership; it may have been removed"
            };
            return Err(StoreError::Identity(format!(
                "node {} {reason} in cluster {}",
                identity.node_id, identity.cluster_id
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok((client, local))
}

/// Adapt OpenRaft's sparse node ids to Hiqlite 0.14's positional config.
///
/// Hiqlite validates `node_id <= nodes.len()` and selects this node with
/// `nodes[node_id - 1]`. Membership ids are not vector positions, and join
/// tokens can legitimately reserve id 2 while id 3 is redeemed first. Pad a
/// sparse vector with copies of the local peer so the required slot exists and
/// contains this node. The copies retain the same id, so they add no phantom
/// node to committed Raft membership; Hiqlite also skips every entry whose id
/// is `this_node` while asking an existing voter to admit the joiner.
#[cfg(feature = "hiqlite-store")]
fn hiqlite_nodes_for_local(
    mut peers: Vec<ClusterPeer>,
    local: &ClusterPeer,
) -> Result<Vec<Node>, StoreError> {
    let local_slot = usize::try_from(local.raft_id.saturating_sub(1)).map_err(|_| {
        StoreError::Migration(format!(
            "Raft id {} cannot be represented as a local Hiqlite node slot",
            local.raft_id
        ))
    })?;
    let local_index = peers
        .iter()
        .position(|peer| peer.raft_id == local.raft_id)
        .ok_or_else(|| {
            StoreError::Identity(format!(
                "local Raft id {} is absent from the configured peer list",
                local.raft_id
            ))
        })?;

    if peers.len() <= local_slot {
        peers.resize(local_slot + 1, local.clone());
    } else if local_index != local_slot {
        peers.swap(local_index, local_slot);
    }

    debug_assert_eq!(peers[local_slot].raft_id, local.raft_id);
    Ok(peers.iter().map(Node::from).collect())
}

#[cfg(feature = "hiqlite-store")]
fn should_force_loopback(config: &Config, membership: Option<&LocalMembership>) -> bool {
    if !config.cluster.advertise_host.trim().is_empty() {
        return false;
    }
    membership.is_none_or(|membership| cluster_peer_is_loopback(&membership.local))
}

#[cfg(feature = "hiqlite-store")]
fn configured_or_persisted_local_peer(
    config: &Config,
    membership: &LocalMembership,
    force_loopback: bool,
) -> Result<ClusterPeer, StoreError> {
    if force_loopback {
        return if cluster_peer_is_loopback(&membership.local) {
            Ok(ClusterPeer {
                raft_id: membership.raft_id,
                raft_address: local_client_address(config.cluster.raft_bind).to_string(),
                api_address: local_client_address(config.cluster.api_bind).to_string(),
            })
        } else {
            // Fresh activation can bind its temporary staging listener on
            // loopback while committing the explicitly advertised address.
            Ok(membership.local.clone())
        };
    }

    if cluster_peer_is_loopback(&membership.local) {
        // Setting advertise_host is the explicit opt-in that opens an existing
        // one-voter install to peers. The readdress path replaces its persisted
        // loopback membership before the active voter reaches this function.
        return configured_local_peer(config, membership.raft_id);
    }

    let raft_port = stored_peer_port("Raft", &membership.local.raft_address)?;
    let api_port = stored_peer_port("cluster API", &membership.local.api_address)?;
    if raft_port != config.cluster.raft_bind.port() || api_port != config.cluster.api_bind.port() {
        return Err(StoreError::Migration(format!(
            "cluster listener port drift: membership.json records Raft/API ports \
             {raft_port}/{api_port}, but configuration requests {}/{}; changing a joined \
             voter's ports requires a membership reconfiguration and is refused at startup",
            config.cluster.raft_bind.port(),
            config.cluster.api_bind.port()
        )));
    }

    if config.cluster.advertise_host.trim().is_empty() {
        return Ok(membership.local.clone());
    }
    let configured = configured_local_peer(config, membership.raft_id)?;
    if configured != membership.local {
        return Err(StoreError::Migration(
            "cluster.advertise_host differs from membership.json; changing a joined voter's \
             advertised address requires a membership reconfiguration and is refused at startup"
                .to_owned(),
        ));
    }
    Ok(configured)
}

#[cfg(feature = "hiqlite-store")]
fn cluster_peer_is_loopback(peer: &ClusterPeer) -> bool {
    [&peer.raft_address, &peer.api_address]
        .into_iter()
        .all(|address| {
            address
                .parse::<SocketAddr>()
                .is_ok_and(|address| address.ip().is_loopback())
        })
}

#[cfg(feature = "hiqlite-store")]
fn stored_peer_port(role: &str, address: &str) -> Result<u16, StoreError> {
    let port = if let Some(rest) = address.strip_prefix('[') {
        rest.rsplit_once("]:").map(|(_, port)| port)
    } else {
        address.rsplit_once(':').map(|(_, port)| port)
    };
    port.and_then(|port| port.parse().ok()).ok_or_else(|| {
        StoreError::Migration(format!(
            "membership.json contains an invalid {role} address; refusing to guess a listener port"
        ))
    })
}

#[cfg(feature = "hiqlite-store")]
async fn shutdown_voter(client: &Client, active_transport: bool) -> Result<(), StoreError> {
    const TLS_SHUTDOWN_ASSERTION: &str =
        "The global Hiqlite shutdown handler to always listen: SendError { .. }";

    if !active_transport {
        return client
            .shutdown()
            .await
            .map_err(|error| StoreError::Database(format!("stopping Hiqlite voter: {error}")));
    }

    // The assertion is caught below, but Rust's default panic hook would still
    // print a frightening backtrace during an otherwise clean daemon stop.
    // Serialize the short process-global hook swap and suppress only the exact
    // upstream assertion; every unrelated panic still reaches the prior hook.
    static PANIC_HOOK_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    let _hook_guard = PANIC_HOOK_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let previous_hook = Arc::new(std::panic::take_hook());
    let delegated_hook = Arc::clone(&previous_hook);
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str));
        if message == Some(TLS_SHUTDOWN_ASSERTION) {
            return;
        }
        delegated_hook(info);
    }));

    let owned = client.clone();
    let outcome = tokio::spawn(async move { owned.shutdown().await }).await;
    drop(std::panic::take_hook());
    let previous_hook = match Arc::try_unwrap(previous_hook) {
        Ok(hook) => hook,
        Err(_) => unreachable!("serialized panic-hook owner"),
    };
    std::panic::set_hook(previous_hook);

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(StoreError::Database(format!(
            "stopping TLS Hiqlite voter: {error}"
        ))),
        Err(join_error) if join_error.is_panic() => {
            let payload = join_error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic");
            if message == TLS_SHUTDOWN_ASSERTION {
                // In 0.14 this exact assertion is after Raft shutdown, WAL
                // drain, SQL-writer drain, and client-stream shutdown. TLS
                // listeners own no receiver, so only their final notification
                // is missing; runtime teardown closes those listener tasks.
                tracing::debug!("contained Hiqlite 0.14 TLS listener shutdown assertion");
                Ok(())
            } else {
                Err(StoreError::Database(format!(
                    "TLS Hiqlite voter panicked during shutdown: {message}"
                )))
            }
        }
        Err(join_error) => Err(StoreError::Database(format!(
            "TLS Hiqlite voter shutdown task failed: {join_error}"
        ))),
    }
}

#[cfg(feature = "hiqlite-store")]
fn validate_cluster_listener(role: &str, bind: SocketAddr, tls: bool) -> Result<(), StoreError> {
    if !tls && !bind.ip().is_loopback() {
        return Err(StoreError::Migration(format!(
            "refusing public cleartext {role} bind {bind}; use the automatic cluster TLS \
             transport or bind the listener to loopback"
        )));
    }
    Ok(())
}

#[cfg(feature = "hiqlite-store")]
fn configured_local_peer(config: &Config, raft_id: u64) -> Result<ClusterPeer, StoreError> {
    let host = configured_advertise_host(config)?;
    Ok(ClusterPeer {
        raft_id,
        raft_address: host_port(&host, config.cluster.raft_bind.port()),
        api_address: host_port(&host, config.cluster.api_bind.port()),
    })
}

#[cfg(feature = "hiqlite-store")]
fn configured_advertise_host(config: &Config) -> Result<String, StoreError> {
    let host = if !config.cluster.advertise_host.trim().is_empty() {
        config.cluster.advertise_host.trim().to_owned()
    } else if !config.cluster.api_bind.ip().is_unspecified() {
        config.cluster.api_bind.ip().to_string()
    } else if !config.server.bind.ip().is_unspecified() {
        config.server.bind.ip().to_string()
    } else {
        "127.0.0.1".to_owned()
    };
    if host.contains("//") || host.contains('/') || host.contains(char::is_whitespace) {
        return Err(StoreError::Migration(
            "cluster.advertise_host must be a host or IP address, not a URL".to_owned(),
        ));
    }
    Ok(host)
}

#[cfg(feature = "hiqlite-store")]
fn configured_join_url(config: &Config) -> Result<String, StoreError> {
    let configured = config.cluster.join_url.trim().trim_end_matches('/');
    if !configured.is_empty() {
        if !(configured.starts_with("http://") || configured.starts_with("https://")) {
            return Err(StoreError::Migration(
                "cluster.join_url must start with http:// or https://".to_owned(),
            ));
        }
        return Ok(configured.to_owned());
    }
    Ok(format!(
        "http://{}",
        host_port(
            &configured_advertise_host(config)?,
            config.server.bind.port()
        )
    ))
}

#[cfg(feature = "hiqlite-store")]
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(feature = "hiqlite-store")]
async fn abort_incoming(
    client: Client,
    incoming: &Path,
    error: StoreError,
    active_transport: bool,
) -> Result<SelectedStore, StoreError> {
    let shutdown = shutdown_voter(&client, active_transport).await.err();
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

#[cfg(feature = "hiqlite-store")]
fn read_local_membership(data_dir: &Path) -> Result<Option<LocalMembership>, StoreError> {
    let path = data_dir.join(LOCAL_MEMBERSHIP_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(migration_io("reading", &path, error)),
    };
    let membership: LocalMembership = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::Identity(format!(
            "decoding local cluster membership {}: {error}",
            path.display()
        ))
    })?;
    if membership.version != 1
        || membership.cluster_id.trim().is_empty()
        || membership.node_id.trim().is_empty()
        || membership.raft_id == 0
    {
        return Err(StoreError::Identity(format!(
            "local cluster membership {} is incomplete",
            path.display()
        )));
    }
    Ok(Some(membership))
}

#[cfg(feature = "hiqlite-store")]
fn write_local_membership(data_dir: &Path, membership: &LocalMembership) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec_pretty(membership).map_err(|error| {
        StoreError::Identity(format!("serializing local cluster membership: {error}"))
    })?;
    bytes.push(b'\n');
    write_atomic_private(data_dir, LOCAL_MEMBERSHIP_FILENAME, &bytes)
}

#[cfg(feature = "hiqlite-store")]
fn write_readdress_record(data_dir: &Path, membership: &LocalMembership) -> Result<(), StoreError> {
    let record = ReaddressRecord {
        version: 1,
        membership: membership.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
        StoreError::Migration(format!("serializing voter-readdress record: {error}"))
    })?;
    bytes.push(b'\n');
    write_atomic_private(data_dir, HIQLITE_READDRESS_MARKER_FILENAME, &bytes)
}

#[cfg(feature = "hiqlite-store")]
fn read_readdress_record(data_dir: &Path) -> Result<Option<ReaddressRecord>, StoreError> {
    let path = data_dir.join(HIQLITE_READDRESS_MARKER_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(migration_io("reading", &path, error)),
    };
    let record: ReaddressRecord = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::Migration(format!(
            "decoding voter-readdress record {}: {error}",
            path.display()
        ))
    })?;
    if record.version != 1
        || record.membership.version != 1
        || record.membership.cluster_id.trim().is_empty()
        || record.membership.node_id.trim().is_empty()
        || record.membership.raft_id == 0
    {
        return Err(StoreError::Migration(format!(
            "voter-readdress record {} is incomplete",
            path.display()
        )));
    }
    Ok(Some(record))
}

/// Recover the only non-atomic boundary in the one-voter directory swap.
#[cfg(feature = "hiqlite-store")]
fn recover_interrupted_readdress(data_dir: &Path) -> Result<(), StoreError> {
    let Some(record) = read_readdress_record(data_dir)? else {
        return Ok(());
    };
    let active = data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    let incoming = data_dir.join(HIQLITE_INCOMING_DIRNAME);
    let backup = data_dir.join(HIQLITE_READDRESS_BACKUP_DIRNAME);
    match (path_exists(&active)?, path_exists(&backup)?) {
        // Both renames committed. Publish the matching local record before the
        // replacement target is opened; cleanup waits for successful open.
        (true, true) => write_local_membership(data_dir, &record.membership),
        // The prepared target never replaced the authoritative directory.
        (true, false) => {
            remove_directory_if_present(&incoming)?;
            remove_file_if_present(&data_dir.join(HIQLITE_READDRESS_MARKER_FILENAME))?;
            sync_directory(data_dir)
        }
        // Crash between the two renames: restore the old authoritative target.
        (false, true) => {
            std::fs::rename(&backup, &active)
                .map_err(|error| migration_io("restoring pre-readdress target", &active, error))?;
            remove_directory_if_present(&incoming)?;
            remove_file_if_present(&data_dir.join(HIQLITE_READDRESS_MARKER_FILENAME))?;
            sync_directory(data_dir)
        }
        (false, false) => Err(StoreError::Migration(format!(
            "{} exists but neither the active nor recovery Hiqlite target exists",
            data_dir.join(HIQLITE_READDRESS_MARKER_FILENAME).display()
        ))),
    }
}

#[cfg(feature = "hiqlite-store")]
fn finish_readdress(data_dir: &Path) -> Result<(), StoreError> {
    if read_readdress_record(data_dir)?.is_none() {
        return Ok(());
    }
    remove_directory_if_present(&data_dir.join(HIQLITE_READDRESS_BACKUP_DIRNAME))?;
    remove_file_if_present(&data_dir.join(HIQLITE_READDRESS_MARKER_FILENAME))?;
    sync_directory(data_dir)
}

#[cfg(feature = "hiqlite-store")]
fn remove_directory_if_present(path: &Path) -> Result<(), StoreError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(migration_io("inspecting", path, error)),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::Migration(format!(
            "refusing to recursively remove non-directory recovery path {}",
            path.display()
        )));
    }
    std::fs::remove_dir_all(path).map_err(|error| migration_io("removing", path, error))
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

    #[cfg(feature = "hiqlite-store")]
    use axum::extract::State;
    #[cfg(feature = "hiqlite-store")]
    use axum::http::StatusCode;
    #[cfg(feature = "hiqlite-store")]
    use axum::response::{IntoResponse, Response};
    #[cfg(feature = "hiqlite-store")]
    use axum::routing::post;
    #[cfg(feature = "hiqlite-store")]
    use axum::{Json, Router};
    #[cfg(feature = "hiqlite-store")]
    use serde_json::json;

    /// The claim a data-directory refusal is entitled to make. Nothing that did
    /// not observe a *different* live process may print it.
    #[cfg(feature = "hiqlite-store")]
    const SECOND_DAEMON_VERDICT: &str = "another plurxd process already owns the data directory";

    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn activation_marker_accepts_only_the_supported_upgrade_window() {
        let marker = |replicated_schema_version| ActivationMarker {
            marker_version: ACTIVATION_MARKER_VERSION,
            cluster_id: "cluster-a".to_owned(),
            source_backup_sha256: "0".repeat(64),
            source_schema_version: 20,
            replicated_schema_version,
            imported_rows: 0,
            table_hashes: vec![SqliteImportTableDigest {
                table: "settings".to_owned(),
                row_count: 0,
                sha256: "0".repeat(64),
            }],
        };
        marker(AUTH_SCHEMA_MIGRATION_SOURCE)
            .validate()
            .expect("v5 marker must reach the complete daemon migration chain");
        marker(AUTH_SCHEMA_MIGRATION_SOURCE + 1)
            .validate()
            .expect("v6 marker must reach the book-facts migration");
        marker(AUTH_SCHEMA_VERSION)
            .validate()
            .expect("current marker");
        for unsupported in [AUTH_SCHEMA_MIGRATION_SOURCE - 1, AUTH_SCHEMA_VERSION + 1] {
            let error = marker(unsupported)
                .validate()
                .expect_err("unsupported marker must fail before voter startup");
            assert!(error.to_string().contains("incomplete"), "{error}");
        }
    }

    /// A predecessor still closing its handle is not a second daemon.
    ///
    /// Regression for #374. `acquire_daemon_lock` took the advisory lock
    /// non-blockingly, so
    /// `daemon_join_refuses_occupied_and_expired_targets_then_resumes_finalization`
    /// re-activating the same data directory printed [`SECOND_DAEMON_VERDICT`]
    /// whenever the previous activation's handle had not finished dropping.
    /// Unloaded the drop won that race; under gate-parallel CI load it did not,
    /// and `rust-gate` failed on diffs that cannot reach this code — a load
    /// artifact reported as a production-grade double-start.
    ///
    /// Exercises the real production entry point and its real window, because
    /// the window is the correction. Reverting to a single non-blocking attempt
    /// fails here immediately.
    #[cfg(feature = "hiqlite-store")]
    #[tokio::test]
    async fn a_still_closing_predecessor_lock_is_not_reported_as_a_second_daemon() {
        let data = tempfile::tempdir().expect("daemon lock dir");
        let holder = acquire_daemon_lock(data.path())
            .await
            .expect("the first owner takes the lock");

        // Well inside DAEMON_LOCK_ACQUIRE_WINDOW, and the whole point of it:
        // the incoming owner cannot know this handle is about to close.
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            drop(holder);
        });

        let acquired = acquire_daemon_lock(data.path()).await;
        release.await.expect("predecessor release task");
        let error = match acquired {
            Ok(_lock) => return,
            Err(error) => error,
        };
        panic!(
            "a predecessor that was still closing its handle was refused: {error}. The \
             data-directory lock must wait out a departing owner rather than read a \
             not-yet-closed handle as a second running daemon"
        );
    }

    /// The refusal survives, and it does not conflate the two conditions.
    ///
    /// The owner here is this test process, so the lock is genuinely held for
    /// the whole window and must still be refused — but calling that "another
    /// plurxd process" would send a reader hunting a process that does not
    /// exist. A short explicit window keeps this from costing five seconds;
    /// the production window is exercised by the regression above.
    #[cfg(feature = "hiqlite-store")]
    #[tokio::test]
    async fn a_lock_held_for_the_whole_window_is_refused_without_blaming_another_process() {
        let data = tempfile::tempdir().expect("daemon lock dir");
        let _holder = acquire_daemon_lock(data.path())
            .await
            .expect("the owner takes the lock");
        assert_eq!(
            read_daemon_lock_holder(&data.path().join(DAEMON_LOCK_FILENAME)),
            DaemonLockHolder::ThisProcess(std::process::id()),
            "an owner must record itself so a contender's refusal can name it"
        );

        let message = acquire_daemon_lock_within(data.path(), Duration::from_millis(200))
            .await
            .expect_err("a lock held for the whole window must still be refused")
            .to_string();
        assert!(
            !message.contains(SECOND_DAEMON_VERDICT),
            "our own still-open handle was reported as a second daemon: {message}"
        );
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "a refusal must name the recorded holder: {message}"
        );
    }

    /// A genuinely different owner keeps the operator-facing verdict.
    ///
    /// `crates/plurxd/tests/cluster_activation.rs` proves this end to end with
    /// two real processes; this pins the text and the classification so the
    /// #374 correction cannot quietly stop calling a real double-start what it
    /// is. An unrecorded owner is treated the same way: the lock is held and
    /// nothing says it is ours.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn a_different_owner_is_still_reported_as_a_second_daemon() {
        let data = tempfile::tempdir().expect("daemon lock dir");
        let path = data.path().join(DAEMON_LOCK_FILENAME);

        let foreign = std::process::id().wrapping_add(1);
        std::fs::write(&path, format!("{foreign}\n")).expect("record a foreign owner");
        assert_eq!(
            read_daemon_lock_holder(&path),
            DaemonLockHolder::OtherProcess(foreign)
        );
        std::fs::write(&path, "not-a-pid").expect("record an unusable owner");
        assert_eq!(
            read_daemon_lock_holder(&path),
            DaemonLockHolder::Unidentified
        );
        assert_eq!(
            read_daemon_lock_holder(&data.path().join("absent.lock")),
            DaemonLockHolder::Unidentified,
            "a missing record must degrade to a message, never to a startup error"
        );

        for holder in [
            DaemonLockHolder::OtherProcess(foreign),
            DaemonLockHolder::Unidentified,
        ] {
            let message =
                daemon_lock_conflict(data.path(), &holder, DAEMON_LOCK_ACQUIRE_WINDOW).to_string();
            assert!(
                message.contains(SECOND_DAEMON_VERDICT),
                "{holder:?} must keep the double-start verdict: {message}"
            );
        }
    }

    /// Staging import never needs a remotely reachable address. Active M3
    /// voters and maintenance clients use persisted membership addresses.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn staging_fallback_addresses_are_always_loopback() {
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

    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn maintenance_uses_the_persisted_local_api_address() {
        let data = tempfile::tempdir().expect("maintenance address dir");
        let mut config = Config::default();
        config.storage.data_dir = data.path().to_owned();
        let identity = super::super::ClusterIdentity {
            cluster_id: "cluster-a".to_owned(),
            node_id: "node-a".to_owned(),
            raft_id: 1,
        };
        write_local_membership(
            data.path(),
            &LocalMembership {
                version: 1,
                cluster_id: identity.cluster_id.clone(),
                node_id: identity.node_id.clone(),
                raft_id: identity.raft_id,
                local: ClusterPeer {
                    raft_id: 1,
                    raft_address: "192.0.2.40:32401".to_owned(),
                    api_address: "192.0.2.40:32402".to_owned(),
                },
                bootstrap: Vec::new(),
                join_token_digest: None,
            },
        )
        .expect("write local membership");
        assert_eq!(
            local_voter_api_address(&config, &identity).expect("maintenance address"),
            "192.0.2.40:32402"
        );
    }

    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn public_cleartext_cluster_listeners_are_refused() {
        let public: SocketAddr = "0.0.0.0:32401".parse().expect("public bind");
        let error = validate_cluster_listener("raft", public, false)
            .expect_err("public cleartext must be refused");
        assert!(error.to_string().contains("refusing public cleartext raft"));
        validate_cluster_listener("raft", public, true).expect("public TLS is allowed");
        validate_cluster_listener("raft", "127.0.0.1:32401".parse().expect("loopback"), false)
            .expect("loopback cleartext is allowed for staging");
    }

    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn default_voters_stay_loopback_and_joined_port_drift_is_refused() {
        let config = Config::default();
        assert!(should_force_loopback(&config, None));

        let mut enabled = config.clone();
        enabled.cluster.advertise_host = "plurx-a.lan".to_owned();
        assert!(!should_force_loopback(&enabled, None));

        let membership = LocalMembership {
            version: 1,
            cluster_id: "cluster-a".to_owned(),
            node_id: "node-a".to_owned(),
            raft_id: 1,
            local: ClusterPeer {
                raft_id: 1,
                raft_address: "plurx-a.lan:32401".to_owned(),
                api_address: "plurx-a.lan:32402".to_owned(),
            },
            bootstrap: Vec::new(),
            join_token_digest: None,
        };
        let mut drifted = enabled;
        drifted.cluster.api_bind.set_port(32502);
        let error = configured_or_persisted_local_peer(&drifted, &membership, false)
            .expect_err("joined listener port drift must fail closed");
        assert!(error.to_string().contains("listener port drift"), "{error}");
    }

    /// Build an activated data directory whose persisted membership is
    /// `membership`, so readdress decisions can be exercised without a store.
    #[cfg(feature = "hiqlite-store")]
    fn activated_dir_for_readdress(membership: &LocalMembership) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("activated data dir");
        let active = dir.path().join(HIQLITE_ACTIVE_DIRNAME);
        std::fs::create_dir_all(&active).expect("active target");
        write_activation_marker(
            &active,
            &ActivationMarker {
                marker_version: ACTIVATION_MARKER_VERSION,
                cluster_id: membership.cluster_id.clone(),
                source_backup_sha256: "0".repeat(64),
                source_schema_version: AUTH_SCHEMA_VERSION,
                replicated_schema_version: AUTH_SCHEMA_VERSION,
                imported_rows: 0,
                table_hashes: vec![SqliteImportTableDigest {
                    table: "settings".to_owned(),
                    row_count: 0,
                    sha256: "0".repeat(64),
                }],
            },
        )
        .expect("activation marker");
        std::fs::write(
            dir.path().join("node.id"),
            format!("{}\n", membership.node_id),
        )
        .expect("node identity");
        write_local_membership(dir.path(), membership).expect("persisted membership");
        dir
    }

    /// A loopback-literal `advertise_host` is a legitimate configuration - two
    /// daemons on one host - and must reach a fixed point. Deciding on
    /// loopback-ness instead of on "differs from configured" never settles, so
    /// a sole voter rebuilt its state machine on every boot and a joined
    /// follower refused to start at all.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn readdress_is_a_no_op_once_the_committed_address_matches_configuration() {
        for (raft_id, bootstrap_raft_id) in [(1_u64, 1_u64), (2, 1)] {
            let dir = tempfile::tempdir().expect("port probe dir");
            let config_probe = membership_test_config(dir.path());
            let raft_port = config_probe.cluster.raft_bind.port();
            let api_port = config_probe.cluster.api_bind.port();
            let local = ClusterPeer {
                raft_id,
                raft_address: format!("127.0.0.1:{raft_port}"),
                api_address: format!("127.0.0.1:{api_port}"),
            };
            let membership = LocalMembership {
                version: 1,
                cluster_id: "11111111-1111-4111-8111-111111111111".to_owned(),
                node_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                raft_id,
                local: local.clone(),
                // A joined follower carries the coordinator's bootstrap list,
                // which is exactly what the old peer-existence guard rejected.
                bootstrap: vec![ClusterPeer {
                    raft_id: bootstrap_raft_id,
                    raft_address: format!("127.0.0.1:{raft_port}"),
                    api_address: format!("127.0.0.1:{api_port}"),
                }],
                join_token_digest: None,
            };
            let activated = activated_dir_for_readdress(&membership);
            let mut config = membership_test_config(activated.path());
            config.cluster.advertise_host = "127.0.0.1".to_owned();
            config.cluster.raft_bind = format!("127.0.0.1:{raft_port}").parse().expect("Raft bind");
            config.cluster.api_bind = format!("127.0.0.1:{api_port}")
                .parse()
                .expect("cluster API bind");

            readdress_single_voter_if_needed(&config)
                .unwrap_or_else(|error| panic!("raft_id {raft_id} must boot unchanged: {error}"));

            assert_eq!(
                read_local_membership(activated.path())
                    .expect("read membership")
                    .map(|membership| membership.local),
                Some(local),
                "raft_id {raft_id} committed address was rewritten"
            );
            assert!(
                !activated
                    .path()
                    .join(HIQLITE_READDRESS_BACKUP_DIRNAME)
                    .exists()
                    && !activated.path().join(HIQLITE_INCOMING_DIRNAME).exists(),
                "raft_id {raft_id} rebuilt its state machine with nothing to repair"
            );
        }
    }

    /// `membership.json` records the bootstrap list once and is never
    /// refreshed, so a coordinator that has admitted a peer still reads as a
    /// lone voter there. Deciding sole-voter-ness from that file let the
    /// readdress rebuild one-node Raft metadata that silently ejected the peer.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn readdress_refuses_once_a_peer_is_admitted_even_if_local_membership_is_stale() {
        let membership = LocalMembership {
            version: 1,
            cluster_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            node_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            raft_id: 1,
            local: ClusterPeer {
                raft_id: 1,
                raft_address: "127.0.0.1:32401".to_owned(),
                api_address: "127.0.0.1:32402".to_owned(),
            },
            // Written before the peer joined, and never updated since.
            bootstrap: vec![ClusterPeer {
                raft_id: 1,
                raft_address: "127.0.0.1:32401".to_owned(),
                api_address: "127.0.0.1:32402".to_owned(),
            }],
            join_token_digest: None,
        };
        let activated = activated_dir_for_readdress(&membership);
        let database = activated
            .path()
            .join(HIQLITE_ACTIVE_DIRNAME)
            .join("state_machine")
            .join("db")
            .join(HIQLITE_DATABASE_FILENAME);
        std::fs::create_dir_all(database.parent().expect("state machine dir"))
            .expect("state machine dir");
        let connection = Connection::open(&database).expect("state machine");
        connection
            .execute_batch(
                "CREATE TABLE cluster_nodes (node_id TEXT PRIMARY KEY, raft_id INTEGER NOT NULL, \
                 raft_address TEXT NOT NULL, api_address TEXT NOT NULL, \
                 last_seen_at INTEGER NOT NULL, removed_at INTEGER);\
                 INSERT INTO cluster_nodes VALUES \
                   ('node-a', 1, '127.0.0.1:32401', '127.0.0.1:32402', 1, NULL),\
                   ('node-b', 2, '127.0.0.1:32411', '127.0.0.1:32412', 1, NULL);",
            )
            .expect("admitted peer record");
        drop(connection);

        let mut config = membership_test_config(activated.path());
        config.cluster.advertise_host = "plurx-a.lan".to_owned();
        config.cluster.raft_bind = "127.0.0.1:32401".parse().expect("Raft bind");
        config.cluster.api_bind = "127.0.0.1:32402".parse().expect("cluster API bind");

        let error = readdress_single_voter_if_needed(&config)
            .expect_err("readdress must refuse once a peer is admitted");
        assert!(
            error
                .to_string()
                .contains("cannot change after another voter or learner exists"),
            "{error}"
        );
        assert!(
            !activated
                .path()
                .join(HIQLITE_READDRESS_BACKUP_DIRNAME)
                .exists()
                && !activated.path().join(HIQLITE_INCOMING_DIRNAME).exists(),
            "refused readdress still touched the active target"
        );
    }

    /// A removed node is tombstoned rather than deleted, so it must stop
    /// blocking a sole survivor's readdress while still reserving its Raft id.
    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn admitted_peer_count_ignores_tombstones_and_missing_schema() {
        let dir = tempfile::tempdir().expect("state machine dir");
        let active = dir.path().join(HIQLITE_ACTIVE_DIRNAME);
        let database = active
            .join("state_machine")
            .join("db")
            .join(HIQLITE_DATABASE_FILENAME);
        std::fs::create_dir_all(database.parent().expect("db dir")).expect("db dir");

        // A store activated before the membership schema existed never admitted
        // a peer, and an absent state machine cannot contradict that either.
        assert_eq!(admitted_peer_count(&active, 1).expect("absent database"), 0);
        let connection = Connection::open(&database).expect("state machine");
        assert_eq!(admitted_peer_count(&active, 1).expect("absent table"), 0);
        connection
            .execute_batch(
                "CREATE TABLE cluster_nodes (node_id TEXT PRIMARY KEY, raft_id INTEGER NOT NULL, \
                 raft_address TEXT NOT NULL, api_address TEXT NOT NULL, \
                 last_seen_at INTEGER NOT NULL, removed_at INTEGER);\
                 INSERT INTO cluster_nodes VALUES \
                   ('node-a', 1, 'a:1', 'a:2', 1, NULL),\
                   ('node-b', 2, 'b:1', 'b:2', 1, 99);",
            )
            .expect("tombstoned peer");
        drop(connection);

        assert_eq!(admitted_peer_count(&active, 1).expect("tombstoned peer"), 0);
        assert_eq!(admitted_peer_count(&active, 2).expect("live peer"), 1);
    }

    #[cfg(feature = "hiqlite-store")]
    #[test]
    fn readdress_directory_swap_recovers_both_crash_boundaries() {
        let membership = LocalMembership {
            version: 1,
            cluster_id: "cluster-a".to_owned(),
            node_id: "node-a".to_owned(),
            raft_id: 1,
            local: ClusterPeer {
                raft_id: 1,
                raft_address: "plurx-a.lan:32401".to_owned(),
                api_address: "plurx-a.lan:32402".to_owned(),
            },
            bootstrap: Vec::new(),
            join_token_digest: None,
        };

        let between = tempfile::tempdir().expect("between-renames recovery dir");
        std::fs::create_dir(between.path().join(HIQLITE_READDRESS_BACKUP_DIRNAME))
            .expect("old target");
        std::fs::create_dir(between.path().join(HIQLITE_INCOMING_DIRNAME))
            .expect("prepared target");
        write_readdress_record(between.path(), &membership).expect("readdress marker");
        recover_interrupted_readdress(between.path()).expect("restore old target");
        assert!(between.path().join(HIQLITE_ACTIVE_DIRNAME).exists());
        assert!(!between
            .path()
            .join(HIQLITE_READDRESS_BACKUP_DIRNAME)
            .exists());
        assert!(!between.path().join(HIQLITE_INCOMING_DIRNAME).exists());
        assert!(!between
            .path()
            .join(HIQLITE_READDRESS_MARKER_FILENAME)
            .exists());

        let after = tempfile::tempdir().expect("after-renames recovery dir");
        std::fs::create_dir(after.path().join(HIQLITE_ACTIVE_DIRNAME)).expect("new target");
        std::fs::create_dir(after.path().join(HIQLITE_READDRESS_BACKUP_DIRNAME))
            .expect("old recovery target");
        write_readdress_record(after.path(), &membership).expect("readdress marker");
        recover_interrupted_readdress(after.path()).expect("publish new membership");
        assert_eq!(
            read_local_membership(after.path()).expect("read membership"),
            Some(membership)
        );
        assert!(after.path().join(HIQLITE_READDRESS_BACKUP_DIRNAME).exists());
        finish_readdress(after.path()).expect("finish readdress cleanup");
        assert!(!after.path().join(HIQLITE_READDRESS_BACKUP_DIRNAME).exists());
        assert!(!after
            .path()
            .join(HIQLITE_READDRESS_MARKER_FILENAME)
            .exists());
    }

    #[cfg(feature = "hiqlite-store")]
    fn free_test_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind test port")
            .local_addr()
            .expect("test port address")
            .port()
    }

    #[cfg(feature = "hiqlite-store")]
    fn membership_test_config(data_dir: &Path) -> Config {
        let mut config = Config::default();
        config.storage.data_dir = data_dir.to_owned();
        config.cluster.raft_bind = format!("127.0.0.1:{}", free_test_port())
            .parse()
            .expect("Raft bind");
        config.cluster.api_bind = format!("127.0.0.1:{}", free_test_port())
            .parse()
            .expect("cluster API bind");
        // `localhost` is reachable on this machine but is not parsed as a
        // loopback SocketAddr. Tests can therefore distinguish committed
        // advertised membership from the old hard-coded 127.0.0.1 record.
        config.cluster.advertise_host = "localhost".to_owned();
        config
    }

    #[cfg(feature = "hiqlite-store")]
    fn membership_http_error(error: super::super::membership::MembershipError) -> Response {
        (
            StatusCode::CONFLICT,
            Json(json!({ "code": error.code(), "message": error.to_string() })),
        )
            .into_response()
    }

    #[cfg(feature = "hiqlite-store")]
    async fn redeem_join_for_test(
        State(manager): State<MembershipManager>,
        Json(request): Json<RedeemJoinRequest>,
    ) -> Response {
        match manager.redeem(&request).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => membership_http_error(error),
        }
    }

    #[cfg(feature = "hiqlite-store")]
    async fn finalize_join_for_test(
        State(manager): State<MembershipManager>,
        Json(request): Json<FinalizeJoinRequest>,
    ) -> Response {
        match manager.finalize(&request).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => membership_http_error(error),
        }
    }

    /// Exercise the operator's actual daemon join entry point, including the
    /// token file, public redeem/finalize wire, local membership state, and
    /// fully-TLS voter startup. The in-process membership harness alone cannot
    /// cover any of these pre-store decisions.
    #[cfg(feature = "hiqlite-store")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn daemon_join_refuses_occupied_and_expired_targets_then_resumes_finalization() {
        install_default_crypto_provider();

        let source_dir = tempfile::tempdir().expect("source data dir");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("join coordinator listener");
        let coordinator_addr = listener.local_addr().expect("coordinator address");
        let mut source_config = membership_test_config(source_dir.path());
        source_config.cluster.advertise_host.clear();
        source_config.server.bind = coordinator_addr;
        source_config.cluster.join_url = format!("http://{coordinator_addr}");
        drop(SqliteStore::open(&source_dir.path().join(SQLITE_FILENAME)).expect("source SQLite"));
        let source = select_daemon_store(&source_config)
            .await
            .expect("activate source voter");
        let cluster_id = source.identity.cluster_id.clone();
        source
            .store
            .put_setting("membership.readdress", "preserved")
            .await
            .expect("write state before readdress");
        let original_metrics = source
            .local_client
            .as_ref()
            .expect("source client")
            .metrics_db()
            .await
            .expect("source membership");
        let original = original_metrics
            .membership_config
            .nodes()
            .find(|(raft_id, _)| **raft_id == 1)
            .map(|(_, node)| node.clone())
            .expect("source voter record");
        assert!(original.addr_api.starts_with("127.0.0.1:"));
        source.shutdown().await.expect("stop loopback source");
        drop(source);

        // This is the actual upgrade path: an already-running one-voter install
        // opts into membership later. Its committed Raft address, not only
        // membership.json, must become remotely usable without losing data.
        // A real daemon restart drops the old runtime and frees these ports;
        // this in-process test uses fresh ports because Hiqlite 0.14 leaves its
        // TLS listener task alive until the test runtime itself exits.
        source_config.cluster.raft_bind = format!("127.0.0.1:{}", free_test_port())
            .parse()
            .expect("readdressed Raft bind");
        source_config.cluster.api_bind = format!("127.0.0.1:{}", free_test_port())
            .parse()
            .expect("readdressed cluster API bind");
        source_config.cluster.advertise_host = "localhost".to_owned();
        let source = select_daemon_store(&source_config)
            .await
            .expect("readdress existing source voter");
        assert_eq!(source.identity.cluster_id, cluster_id);
        assert_eq!(
            source
                .store
                .get_setting("membership.readdress")
                .await
                .expect("read state after readdress")
                .as_deref(),
            Some("preserved")
        );
        let readdressed_metrics = source
            .local_client
            .as_ref()
            .expect("readdressed source client")
            .metrics_db()
            .await
            .expect("readdressed membership");
        let readdressed = readdressed_metrics
            .membership_config
            .nodes()
            .find(|(raft_id, _)| **raft_id == 1)
            .map(|(_, node)| node.clone())
            .expect("readdressed voter record");
        assert!(
            readdressed.addr_api.starts_with("localhost:"),
            "committed membership remained loopback: {readdressed:?}"
        );
        assert!(!source_dir
            .path()
            .join(HIQLITE_READDRESS_BACKUP_DIRNAME)
            .exists());
        assert!(!source_dir
            .path()
            .join(HIQLITE_READDRESS_MARKER_FILENAME)
            .exists());
        let coordinator = source.membership_manager();
        let app = Router::new()
            .route("/api/v1/cluster/join/redeem", post(redeem_join_for_test))
            .route(
                "/api/v1/cluster/join/finalize",
                post(finalize_join_for_test),
            )
            .with_state(coordinator.clone());
        let http_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve join coordinator");
        });

        let occupied = tempfile::tempdir().expect("occupied join dir");
        let occupied_token = occupied.path().join("join.token");
        std::fs::write(&occupied_token, "unused\n").expect("occupied token file");
        File::create(occupied.path().join(SQLITE_FILENAME)).expect("occupied SQLite path");
        let mut occupied_config = membership_test_config(occupied.path());
        occupied_config.cluster.join_token_file = occupied_token;
        let occupied_error = select_daemon_store(&occupied_config)
            .await
            .err()
            .expect("existing SQLite must refuse daemon join")
            .to_string();
        assert!(occupied_error.contains("refusing to join with existing local database"));

        let expired = coordinator
            .issue_token(Duration::from_secs(1))
            .await
            .expect("issue expiring token");
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let expired_dir = tempfile::tempdir().expect("expired join dir");
        let expired_path = expired_dir.path().join("join.token");
        std::fs::write(&expired_path, format!("{}\n", expired.token)).expect("expired token file");
        let mut expired_config = membership_test_config(expired_dir.path());
        expired_config.cluster.join_token_file = expired_path;
        let expired_error = select_daemon_store(&expired_config)
            .await
            .err()
            .expect("expired token must refuse daemon join")
            .to_string();
        assert!(
            expired_error.contains("join_token_expired"),
            "{expired_error}"
        );
        assert!(!expired_dir.path().join(HIQLITE_ACTIVE_DIRNAME).exists());

        // A still-live token may reserve the next Raft id while another
        // machine starts first. OpenRaft permits that sparse membership, and
        // the production daemon must not confuse the assigned id with the
        // length of its bootstrap vector (Hiqlite 0.14 does exactly that
        // without the compatibility adapter in `start_voter`).
        let held_gap = coordinator
            .issue_token(Duration::from_secs(120))
            .await
            .expect("reserve the preceding Raft id");
        let issued = coordinator
            .issue_token(Duration::from_secs(1))
            .await
            .expect("issue daemon join token");
        assert_eq!(
            issued.raft_id,
            held_gap.raft_id + 1,
            "daemon join fixture must exercise a sparse assigned id"
        );
        let joining_dir = tempfile::tempdir().expect("joining data dir");
        let token_path = joining_dir.path().join("join.token");
        std::fs::write(&token_path, format!("{}\n", issued.token)).expect("joining token file");
        let mut joining_config = membership_test_config(joining_dir.path());
        joining_config.cluster.join_token_file = token_path.clone();
        let staged_identity = crate::cluster::initialize_join_identity(
            joining_dir.path(),
            &cluster_id,
            issued.raft_id,
        )
        .expect("stage the joining node identity");
        let staged_local = configured_local_peer(&joining_config, issued.raft_id)
            .expect("configure the staged peer");
        let issued_digest = join_token_digest(&issued.token);
        coordinator
            .redeem(&RedeemJoinRequest {
                token_digest: issued_digest.clone(),
                raft_id: issued.raft_id,
                node_id: staged_identity.node_id,
                raft_address: staged_local.raft_address,
                api_address: staged_local.api_address,
                schema_version: AUTH_SCHEMA_VERSION,
                protocol_version: crate::store::AUTH_PROTOCOL_VERSION,
            })
            .await
            .expect("reserve the token to the staged node before its failed start");
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let joined = select_daemon_store(&joining_config)
            .await
            .expect("resume an expired identity-bound join through daemon store selection");
        assert_eq!(joined.identity.cluster_id, cluster_id);
        assert_eq!(joined.identity.raft_id, issued.raft_id);
        assert!(
            !token_path.exists(),
            "successful finalization removes the token"
        );
        let local = read_local_membership(joining_dir.path())
            .expect("read joined local membership")
            .expect("joined node persists local membership");
        assert_eq!(
            local.join_token_digest.as_deref(),
            Some(issued_digest.as_str())
        );

        // A crash after finalization but before unlink leaves exactly this
        // shape: active target + membership.json + the original token. The
        // retry is idempotent and removes it on the next pass.
        std::fs::write(&token_path, format!("{}\n", issued.token))
            .expect("restore interrupted-finalization token");
        finalize_pending_join(&joining_config, &joined)
            .await
            .expect("resume finalized join");
        assert!(!token_path.exists());

        // An unrelated copied token is not this node's pending state. It is
        // ignored rather than taking a healthy voter offline.
        let foreign = coordinator
            .issue_token(Duration::from_secs(120))
            .await
            .expect("issue foreign token");
        std::fs::write(&token_path, format!("{}\n", foreign.token)).expect("foreign token file");
        finalize_pending_join(&joining_config, &joined)
            .await
            .expect("foreign token is a warning, not a startup failure");
        assert!(
            token_path.exists(),
            "foreign token remains for operator inspection"
        );

        // An operator staging two machines, or replacing a token they lost,
        // must not be made to wait out the first token's TTL. Allocation walks
        // past ids that live tokens are holding instead of recomputing the one
        // id an outstanding token already reserved.
        let concurrent = coordinator
            .issue_token(Duration::from_secs(120))
            .await
            .expect("second token while the first is still outstanding");
        assert_ne!(
            concurrent.raft_id, foreign.raft_id,
            "concurrent tokens reserved the same Raft id"
        );

        // Once voter state is durable, a coordinator outage may delay token
        // consumption but must not take the healthy joined daemon offline.
        std::fs::write(&token_path, format!("{}\n", issued.token))
            .expect("restore own token before coordinator outage");
        http_task.abort();
        finalize_pending_join_best_effort(&joining_config, &joined).await;
        assert!(
            token_path.exists(),
            "pending finalization keeps the identity-bound token for retry"
        );

        let local_client = joined.local_client.clone().expect("joined local client");
        joined.shutdown().await.expect("drain joined voter");
        let post_shutdown = tokio::time::timeout(
            Duration::from_secs(2),
            local_client.execute(
                "CREATE TABLE IF NOT EXISTS shutdown_must_have_closed_writers (id INTEGER)",
                hiqlite::macros::params!(),
            ),
        )
        .await;
        assert!(
            !matches!(post_shutdown, Ok(Ok(_))),
            "shutdown returned while the replicated writer still accepted work"
        );

        source.shutdown().await.expect("drain source voter");
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

#[cfg(feature = "hiqlite-store")]
pub mod status {
    //! User-facing replication status derived from the selected durable backend.
    //!
    //! This is deliberately a read-only projection. Membership changes belong to
    //! M3; this module only turns the backend and Raft metrics the daemon already
    //! has into an honest answer about watch-state convergence.

    use std::time::{SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Serialize};

    use hiqlite::Client;
    use std::sync::{Arc, Mutex};

    /// The durable backend serving this daemon process.
    #[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ReplicationBackend {
        Sqlite,
        Replicated,
    }

    /// The conclusion an operator should act on, not a raw Raft server state.
    #[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ReplicationHealth {
        /// SQLite has no replication peer and must not be labelled "synced".
        SingleNode,
        InSync,
        Degraded,
    }

    /// Safe, user-visible watch-state replication status.
    ///
    /// It carries no node address, media path, account, token, or library data.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
    pub struct ReplicationStatus {
        pub backend: ReplicationBackend,
        pub health: ReplicationHealth,
        /// More than one voter is present. The membership roster belongs to M3.
        pub clustered: bool,
        /// Raft term/index of the last entry applied to this node.
        pub last_applied_term: Option<u64>,
        pub last_applied_index: Option<u64>,
        /// Largest observed gap, or a conservative upper bound for a missing peer.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub behind_by: Option<u64>,
        /// Unix seconds when this process last positively observed convergence.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub last_converged_at: Option<i64>,
        /// Process-local applied-index baseline retained across degraded samples.
        ///
        /// This is classifier state, not part of the public JSON contract.
        #[serde(skip)]
        last_converged_index: Option<u64>,
        /// Unix seconds when this projection was sampled.
        pub checked_at: i64,
        /// Plain-language interpretation for clients that do not speak Raft.
        pub explanation: String,
    }

    /// Backend-neutral facts needed to classify one replicated-store sample.
    ///
    /// The cluster harness constructs this from real three-voter metrics too, so
    /// the degraded contract exercises this production classifier rather than a
    /// test-only imitation.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ReplicationObservation {
        pub running: bool,
        pub leader_known: bool,
        pub voter_count: usize,
        pub last_log_index: Option<u64>,
        pub last_applied_term: Option<u64>,
        pub last_applied_index: Option<u64>,
        /// Leader-known match index for every other voter. `None` means the leader
        /// has not observed that voter at any index yet. Followers have no map.
        pub peer_matched_indexes: Option<Vec<Option<u64>>>,
    }

    /// Live status reader kept beside the selected store in daemon state.
    #[derive(Clone)]
    pub struct ReplicationMonitor {
        backend: ReplicationBackend,
        client: Option<Client>,
        previous: Arc<Mutex<Option<ReplicationStatus>>>,
    }

    impl ReplicationMonitor {
        /// A local SQLite install: truthful single-node status, never "synced".
        #[must_use]
        pub fn sqlite() -> Self {
            Self {
                backend: ReplicationBackend::Sqlite,
                client: None,
                previous: Arc::new(Mutex::new(None)),
            }
        }

        /// Monitor a local Hiqlite voter through the same client the Store uses.
        #[must_use]
        pub fn replicated(client: Client) -> Self {
            Self {
                backend: ReplicationBackend::Replicated,
                client: Some(client),
                previous: Arc::new(Mutex::new(None)),
            }
        }

        /// Read the current projection without changing membership or durable data.
        pub async fn status(&self) -> ReplicationStatus {
            let checked_at = unix_seconds();
            if self.backend == ReplicationBackend::Sqlite {
                return ReplicationStatus {
                    backend: self.backend,
                    health: ReplicationHealth::SingleNode,
                    clustered: false,
                    last_applied_term: None,
                    last_applied_index: None,
                    behind_by: None,
                    last_converged_at: None,
                    last_converged_index: None,
                    checked_at,
                    explanation: "Watch progress is stored only on this server; this SQLite node is not clustered."
                        .to_owned(),
                };
            }

            let previous = self.previous();
            let client = self
                .client
                .as_ref()
                .expect("replicated monitor must carry a client");
            let status = match client.metrics_db().await {
                Ok(metrics) => {
                    let peer_matched_indexes = metrics.replication.as_ref().map(|replication| {
                        replication
                            .iter()
                            .filter(|(node_id, _)| **node_id != metrics.id)
                            .map(|(_, applied)| applied.as_ref().map(|log| log.index))
                            .collect()
                    });
                    let observation = ReplicationObservation {
                        running: metrics.running_state.is_ok(),
                        leader_known: metrics.current_leader.is_some(),
                        voter_count: metrics.membership_config.voter_ids().count(),
                        last_log_index: metrics.last_log_index,
                        last_applied_term: metrics
                            .last_applied
                            .as_ref()
                            .map(|log| log.leader_id.term),
                        last_applied_index: metrics.last_applied.as_ref().map(|log| log.index),
                        peer_matched_indexes,
                    };
                    classify_replicated(&observation, previous.as_ref(), checked_at)
                }
                Err(error) => {
                    tracing::warn!(%error, "reading replicated-store status failed");
                    unavailable(previous.as_ref(), checked_at)
                }
            };
            self.remember(status.clone());
            status
        }

        fn previous(&self) -> Option<ReplicationStatus> {
            self.previous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn remember(&self, status: ReplicationStatus) {
            *self
                .previous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status);
        }
    }

    fn classify_replicated(
        observation: &ReplicationObservation,
        previous: Option<&ReplicationStatus>,
        checked_at: i64,
    ) -> ReplicationStatus {
        let clustered = observation.voter_count > 1;
        let two_voter_reconfiguration = observation.voter_count == 2;
        let local_lag = match (observation.last_log_index, observation.last_applied_index) {
            (Some(log), Some(applied)) => log.saturating_sub(applied),
            (Some(log), None) => log,
            _ => 0,
        };

        let expected_peers = observation.voter_count.saturating_sub(1);
        let mut unknown_peer = false;
        let mut peer_lag = 0_u64;
        if let Some(peers) = &observation.peer_matched_indexes {
            unknown_peer = peers.len() < expected_peers;
            if let Some(target) = observation.last_applied_index {
                for applied in peers {
                    match applied {
                        Some(index) => peer_lag = peer_lag.max(target.saturating_sub(*index)),
                        None => unknown_peer = true,
                    }
                }
            }
        }

        // Once every voter was positively converged, the local applied-index
        // advance bounds how far behind a now-unreported peer could be. The peer
        // may have received unseen entries before disappearing, so this is a
        // conservative upper bound rather than a proven or exact peer index.
        let inferred_peer_lag = if unknown_peer {
            previous
                .and_then(|status| status.last_converged_index)
                .zip(observation.last_applied_index)
                .map_or(0, |(previous_index, current_index)| {
                    current_index.saturating_sub(previous_index)
                })
        } else {
            0
        };

        let degraded = !observation.running
            || !observation.leader_known
            || two_voter_reconfiguration
            || local_lag > 0
            || peer_lag > 0
            || (observation.peer_matched_indexes.is_some() && unknown_peer);
        let behind_by = local_lag.max(peer_lag).max(inferred_peer_lag);
        let last_converged_at = if degraded {
            previous.and_then(|status| status.last_converged_at)
        } else {
            Some(checked_at)
        };
        let last_converged_index = if degraded {
            previous.and_then(|status| status.last_converged_index)
        } else {
            observation.last_applied_index
        };
        let explanation = if two_voter_reconfiguration {
            "Two voters are a degraded reconfiguration state, not supported HA. Keep both voters online and add a third before treating the cluster as redundant."
                .to_owned()
        } else if !degraded && clustered && observation.peer_matched_indexes.is_some() {
            "Watch progress is replicated; this node has applied the latest known change and every reporting peer has received it."
                .to_owned()
        } else if !degraded && clustered {
            "Watch progress is replicated; this node has applied every change sent by the leader. Replication status for the other nodes is visible on the leader."
                .to_owned()
        } else if !degraded {
            "Replicated storage is active, but this is currently a one-node cluster; there is no second node to sync with."
                .to_owned()
        } else if local_lag > 0 {
            "Watch progress replication is behind on this node. Keep it running while it catches up."
                .to_owned()
        } else if peer_lag > 0 || unknown_peer {
            "Watch progress replication is behind on one or more nodes. Keep the cluster online while it catches up."
                .to_owned()
        } else {
            "Replication status cannot be confirmed, so watch progress may not yet appear on every node."
                .to_owned()
        };

        ReplicationStatus {
            backend: ReplicationBackend::Replicated,
            health: if degraded {
                ReplicationHealth::Degraded
            } else {
                ReplicationHealth::InSync
            },
            clustered,
            last_applied_term: observation.last_applied_term,
            last_applied_index: observation.last_applied_index,
            behind_by: (behind_by > 0).then_some(behind_by),
            last_converged_at,
            last_converged_index,
            checked_at,
            explanation,
        }
    }

    fn unavailable(previous: Option<&ReplicationStatus>, checked_at: i64) -> ReplicationStatus {
        ReplicationStatus {
            backend: ReplicationBackend::Replicated,
            health: ReplicationHealth::Degraded,
            clustered: previous.is_some_and(|status| status.clustered),
            last_applied_term: previous.and_then(|status| status.last_applied_term),
            last_applied_index: previous.and_then(|status| status.last_applied_index),
            behind_by: previous.and_then(|status| status.behind_by),
            last_converged_at: previous.and_then(|status| status.last_converged_at),
            last_converged_index: previous.and_then(|status| status.last_converged_index),
            checked_at,
            explanation:
                "Replication status cannot be confirmed, so watch progress may not yet appear on every node."
                    .to_owned(),
        }
    }

    fn unix_seconds() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn converged(voters: usize) -> ReplicationObservation {
            ReplicationObservation {
                running: true,
                leader_known: true,
                voter_count: voters,
                last_log_index: Some(42),
                last_applied_term: Some(7),
                last_applied_index: Some(42),
                peer_matched_indexes: (voters > 1).then(|| vec![Some(42); voters - 1]),
            }
        }

        #[tokio::test]
        async fn sqlite_is_single_node_instead_of_misleadingly_synced() {
            let status = ReplicationMonitor::sqlite().status().await;
            assert_eq!(status.backend, ReplicationBackend::Sqlite);
            assert_eq!(status.health, ReplicationHealth::SingleNode);
            assert!(!status.clustered);
            assert_eq!(status.last_converged_at, None);
            assert!(status.explanation.contains("stored only on this server"));
        }

        #[test]
        fn one_voter_replicated_store_is_in_sync_but_not_clustered() {
            let status = classify_replicated(&converged(1), None, 100);
            assert_eq!(status.health, ReplicationHealth::InSync);
            assert!(!status.clustered);
            assert_eq!(status.last_applied_term, Some(7));
            assert_eq!(status.last_applied_index, Some(42));
            assert_eq!(status.last_converged_at, Some(100));
            assert!(status.explanation.contains("one-node cluster"));
        }

        #[test]
        fn two_voters_are_degraded_reconfiguration_not_healthy_ha() {
            let status = classify_replicated(&converged(2), None, 100);
            assert_eq!(status.health, ReplicationHealth::Degraded);
            assert!(status.clustered);
            assert_eq!(status.behind_by, None);
            assert!(status.explanation.contains("not supported HA"));
        }

        #[test]
        fn local_apply_lag_is_degraded_and_keeps_the_last_convergence() {
            let prior = classify_replicated(&converged(3), None, 100);
            let status = classify_replicated(
                &ReplicationObservation {
                    last_log_index: Some(47),
                    last_applied_index: Some(42),
                    peer_matched_indexes: None,
                    ..converged(3)
                },
                Some(&prior),
                200,
            );
            assert_eq!(status.health, ReplicationHealth::Degraded);
            assert_eq!(status.behind_by, Some(5));
            assert_eq!(status.last_converged_at, Some(100));
            assert!(status.explanation.contains("on this node"));
        }

        #[test]
        fn a_lagging_peer_degrades_the_cluster_surface() {
            let status = classify_replicated(
                &ReplicationObservation {
                    peer_matched_indexes: Some(vec![Some(42), Some(37)]),
                    ..converged(3)
                },
                None,
                200,
            );
            assert_eq!(status.health, ReplicationHealth::Degraded);
            assert_eq!(status.behind_by, Some(5));
            assert!(status.clustered);
            assert!(status.explanation.contains("one or more nodes"));
        }

        #[test]
        fn an_unknown_peer_is_degraded_instead_of_silently_healthy() {
            let status = classify_replicated(
                &ReplicationObservation {
                    peer_matched_indexes: Some(vec![Some(42)]),
                    ..converged(3)
                },
                None,
                200,
            );
            assert_eq!(status.health, ReplicationHealth::Degraded);
            assert_eq!(status.behind_by, None);
        }

        #[test]
        fn a_missing_peer_after_a_committed_write_has_a_conservative_lag_bound() {
            let prior = classify_replicated(&converged(3), None, 100);
            let status = classify_replicated(
                &ReplicationObservation {
                    last_log_index: Some(45),
                    last_applied_index: Some(45),
                    peer_matched_indexes: Some(vec![Some(45)]),
                    ..converged(3)
                },
                Some(&prior),
                200,
            );
            assert_eq!(status.health, ReplicationHealth::Degraded);
            assert_eq!(status.behind_by, Some(3));
            assert_eq!(status.last_converged_at, Some(100));
        }

        #[test]
        fn a_caught_up_follower_claims_only_the_visibility_it_has() {
            let status = classify_replicated(
                &ReplicationObservation {
                    peer_matched_indexes: None,
                    ..converged(3)
                },
                None,
                100,
            );

            assert_eq!(status.health, ReplicationHealth::InSync);
            assert!(status.clustered);
            assert!(status
                .explanation
                .contains("every change sent by the leader"));
            assert!(status
                .explanation
                .contains("other nodes is visible on the leader"));
            assert!(!status.explanation.contains("every reporting peer"));
        }

        #[test]
        fn missing_peer_lag_survives_repeated_samples_and_tracks_new_writes() {
            let prior = classify_replicated(&converged(3), None, 100);
            let first_degraded = classify_replicated(
                &ReplicationObservation {
                    last_log_index: Some(45),
                    last_applied_index: Some(45),
                    peer_matched_indexes: Some(vec![Some(45)]),
                    ..converged(3)
                },
                Some(&prior),
                200,
            );
            let later_degraded = classify_replicated(
                &ReplicationObservation {
                    last_log_index: Some(50),
                    last_applied_index: Some(50),
                    peer_matched_indexes: Some(vec![Some(50)]),
                    ..converged(3)
                },
                Some(&first_degraded),
                300,
            );

            assert_eq!(first_degraded.behind_by, Some(3));
            assert_eq!(later_degraded.health, ReplicationHealth::Degraded);
            assert_eq!(later_degraded.behind_by, Some(8));
            assert_eq!(later_degraded.last_converged_at, Some(100));
        }

        #[test]
        fn replicated_json_has_only_the_privacy_safe_status_contract() {
            let prior = classify_replicated(&converged(3), None, 100);
            let status = classify_replicated(
                &ReplicationObservation {
                    last_log_index: Some(45),
                    last_applied_index: Some(45),
                    peer_matched_indexes: Some(vec![Some(45)]),
                    ..converged(3)
                },
                Some(&prior),
                200,
            );
            let serialized = serde_json::to_value(status).expect("serialize status");
            let keys = serialized
                .as_object()
                .expect("status object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();

            assert_eq!(
                keys,
                [
                    "backend",
                    "behind_by",
                    "checked_at",
                    "clustered",
                    "explanation",
                    "health",
                    "last_applied_index",
                    "last_applied_term",
                    "last_converged_at",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                "replicated status must not expose users, media, paths, tokens, addresses, or membership"
            );
        }
    }
}

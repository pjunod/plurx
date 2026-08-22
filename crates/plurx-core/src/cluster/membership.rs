//! M3 cluster membership lifecycle.
//!
//! Membership is cluster infrastructure, not a second application store. The
//! Hiqlite client remains the one replicated write path; this coordinator owns
//! only the small amount of state needed to admit nodes, describe them without
//! exposing listener ports or secrets, and remove a voter safely.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CStr;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hiqlite::macros::params;
use hiqlite::{Client, Node, Row};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{OfflinePackage, OfflineRemovalPlanEntry, OfflineRemovalReport};
use crate::store::{Store, AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION};

use super::migration::status::{ReplicationMonitor, ReplicationStatus};
use super::migration::ActivationMarker;
use super::ClusterIdentity;

const JOIN_TOKEN_PREFIX: &str = "plxjoin:v1";
const JOIN_TOKEN_AAD: &[u8] = b"plurx-cluster-join-v1";
const JOIN_TOKEN_VERSION: u32 = 1;
const MEMBERSHIP_SCHEMA_VERSION: i64 = 1;
const NODE_REACHABLE_WINDOW_MS: i64 = 30_000;
const ARTWORK_AUTH_WINDOW_MS: i64 = 60_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// How long a removal waits for survivors to answer a source probe before
/// treating silence as "cannot prove it". Long enough for a healthy node's
/// poll plus a `stat` on a sleeping NAS; short enough that an operator gets an
/// answer rather than a hung request.
const PROBE_WAIT: Duration = Duration::from_secs(15);
const PROBE_POLL: Duration = Duration::from_millis(500);
/// A probe nobody answered stops being worth answering. This bounds how far
/// back a node looks so an abandoned removal cannot leave work queued forever.
const PROBE_REQUEST_TTL: i64 = 300;
/// Matches the activity surface's notion of a transfer in progress: a lease
/// touched this recently is a downloader that is still fetching.
const TRANSFER_ACTIVE_SECONDS: i64 = 60;
/// How many times a removal re-reads and resolves newly created offline work
/// before it refuses instead. Each round costs at most one [`PROBE_WAIT`], so
/// this bounds the operator's wait as well as the loop.
const OFFLINE_RESOLVE_ROUNDS: u32 = 3;

const MEMBERSHIP_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cluster_membership_meta (\
         singleton INTEGER PRIMARY KEY CHECK (singleton = 1), \
         schema_version INTEGER NOT NULL) STRICT",
    "CREATE TABLE IF NOT EXISTS cluster_join_tokens (\
         token_hash TEXT PRIMARY KEY, \
         raft_id INTEGER NOT NULL CHECK (raft_id > 0), \
         expires_at INTEGER NOT NULL, \
         state TEXT NOT NULL CHECK (state IN ('issued', 'redeeming', 'redeemed')), \
         node_id TEXT, \
         created_at INTEGER NOT NULL, \
         redeemed_at INTEGER) STRICT",
    "CREATE TABLE IF NOT EXISTS cluster_nodes (\
         node_id TEXT PRIMARY KEY, \
         raft_id INTEGER NOT NULL UNIQUE CHECK (raft_id > 0), \
         raft_address TEXT NOT NULL, \
         api_address TEXT NOT NULL, \
         last_seen_at INTEGER NOT NULL, \
         removed_at INTEGER) STRICT",
    // Kept separate from `cluster_nodes` so this patch is rolling-compatible
    // with M3 binaries that still write the original six-column row. Public
    // HTTP addressing belongs to the node rather than the logical server: it
    // is where another voter can retrieve node-local materialized bytes.
    "CREATE TABLE IF NOT EXISTS cluster_node_http (\
         node_id TEXT PRIMARY KEY, \
         public_http_url TEXT NOT NULL) STRICT",
    // Additive so a rolling upgrade can teach old membership rows their
    // machine names without rewriting the cluster_nodes table underneath an
    // older voter. Every new daemon creates this table before its heartbeat.
    "CREATE TABLE IF NOT EXISTS cluster_node_hostnames (\
         node_id TEXT PRIMARY KEY, \
         hostname TEXT NOT NULL) STRICT",
];

#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    #[error("cluster membership is unavailable while this node uses SQLite recovery")]
    Unavailable,
    #[error("join token is invalid")]
    InvalidToken,
    #[error("join token expired before it was redeemed")]
    ExpiredToken,
    #[error("join token has already been redeemed")]
    ReusedToken,
    #[error("join token is already reserved for another node")]
    ReservedToken,
    #[error("joining binary is incompatible with this cluster")]
    Incompatible,
    #[error("node was not found in current cluster membership")]
    NodeNotFound,
    #[error("the current Raft leader cannot be removed; retry after leadership moves")]
    LeaderRemoval,
    #[error("removal would leave fewer than two voters and lose the reconfiguration quorum")]
    QuorumLoss,
    /// The removal was refused because this node's offline work could not be
    /// resolved by the §6.7 rule. The payload is the operator-visible reason;
    /// lifting the blanket refusal must not turn removal into "always
    /// succeeds", so what stopped it has to be sayable.
    #[error("node owns offline work that could not be resolved: {0}")]
    OfflineWork(String),
    #[error("cluster membership operation failed: {0}")]
    Internal(String),
}

impl MembershipError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => "membership_unavailable",
            Self::InvalidToken => "join_token_invalid",
            Self::ExpiredToken => "join_token_expired",
            Self::ReusedToken => "join_token_reused",
            Self::ReservedToken => "join_token_reserved",
            Self::Incompatible => "join_incompatible",
            Self::NodeNotFound => "cluster_node_not_found",
            Self::LeaderRemoval => "cluster_leader_removal_refused",
            Self::QuorumLoss => "removal_would_lose_quorum",
            Self::OfflineWork(_) => "node_owns_offline_work",
            Self::Internal(_) => "membership_internal",
        }
    }
}

impl From<hiqlite::Error> for MembershipError {
    fn from(error: hiqlite::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterPeer {
    pub raft_id: u64,
    pub raft_address: String,
    pub api_address: String,
}

impl From<&Node> for ClusterPeer {
    fn from(node: &Node) -> Self {
        Self {
            raft_id: node.id,
            raft_address: node.addr_raft.clone(),
            api_address: node.addr_api.clone(),
        }
    }
}

impl From<&ClusterPeer> for Node {
    fn from(peer: &ClusterPeer) -> Self {
        Self {
            id: peer.raft_id,
            addr_raft: peer.raft_address.clone(),
            addr_api: peer.api_address.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalMembership {
    pub version: u32,
    pub cluster_id: String,
    pub node_id: String,
    pub raft_id: u64,
    pub local: ClusterPeer,
    pub bootstrap: Vec<ClusterPeer>,
    /// Digest of the one-time token that admitted this node. Initial voters
    /// have no digest. This is the local gate that keeps an unrelated token
    /// file from turning an otherwise healthy restart into a failed join.
    #[serde(default)]
    pub join_token_digest: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemJoinRequest {
    /// SHA-256 of the bearer held by the joining node. The coordinator needs
    /// only proof of possession; sending the self-contained token would put
    /// every cluster secret on the public HTTP wire.
    pub token_digest: String,
    pub raft_id: u64,
    pub node_id: String,
    /// The joining machine's OS hostname, reduced to its first DNS label.
    /// Defaulting keeps an interrupted request from an older binary resumable;
    /// its first current heartbeat fills the additive hostname table.
    #[serde(default)]
    pub hostname: String,
    pub raft_address: String,
    pub api_address: String,
    pub schema_version: i64,
    pub protocol_version: i64,
}

impl std::fmt::Debug for RedeemJoinRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedeemJoinRequest")
            .field("token_digest", &"<redacted>")
            .field("raft_id", &self.raft_id)
            .field("node_id", &self.node_id)
            .field("hostname", &self.hostname)
            .field("raft_address", &self.raft_address)
            .field("api_address", &self.api_address)
            .field("schema_version", &self.schema_version)
            .field("protocol_version", &self.protocol_version)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeJoinRequest {
    pub token_digest: String,
    pub raft_id: u64,
    pub node_id: String,
}

impl std::fmt::Debug for FinalizeJoinRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FinalizeJoinRequest")
            .field("token_digest", &"<redacted>")
            .field("raft_id", &self.raft_id)
            .field("node_id", &self.node_id)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedJoinToken {
    pub token: String,
    pub expires_at: i64,
    pub raft_id: u64,
}

impl std::fmt::Debug for IssuedJoinToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedJoinToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("raft_id", &self.raft_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Voter,
    Learner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNodeRecord {
    pub node_id: String,
    /// Actual short machine hostname reported by the node itself.
    pub hostname: String,
    /// Host portion of the advertised cluster API address. Listener ports are
    /// still private; loopback is rendered as `localhost`, not a raw IP.
    pub advertised_host: String,
    pub raft_id: u64,
    pub role: NodeRole,
    pub is_leader: bool,
    pub reachable: bool,
    pub last_seen_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterAvailability {
    SingleNode,
    DegradedReconfiguration,
    HighAvailability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipStatus {
    pub availability: ClusterAvailability,
    pub nodes: Vec<ClusterNodeRecord>,
    /// The one canonical lag answer introduced by #233.
    pub replication: ReplicationStatus,
}

#[derive(Clone)]
pub struct MembershipManager {
    inner: Option<Arc<ReplicatedMembership>>,
}

/// Short-lived proof that one admitted voter requested a node-local artwork
/// file. The shared API secret signs this shape but never crosses the public
/// HTTP listener; a captured proof expires after one minute and names exactly
/// one filename.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkPeerAuth {
    pub node_id: String,
    pub timestamp_ms: i64,
    pub signature: String,
}

struct ReplicatedMembership {
    client: Client,
    store: Arc<dyn Store>,
    identity: ClusterIdentity,
    local: ClusterPeer,
    local_hostname: String,
    bootstrap_http: String,
    secrets: JoinSecrets,
    activation_marker: ActivationMarker,
    replication: ReplicationMonitor,
}

#[derive(Clone)]
pub struct JoinSecrets {
    pub raft: String,
    pub api: String,
    pub credential_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinPayload {
    version: u32,
    pub cluster_id: String,
    pub raft_id: u64,
    pub expires_at: i64,
    pub bootstrap_http: String,
    pub bootstrap: Vec<ClusterPeer>,
    pub secrets: JoinSecretPayload,
    pub activation_marker: ActivationMarker,
    pub schema_version: i64,
    pub protocol_version: i64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinSecretPayload {
    pub raft: String,
    pub api: String,
    pub credential_key: String,
}

impl std::fmt::Debug for JoinSecretPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JoinSecretPayload")
            .field("raft", &"<redacted>")
            .field("api", &"<redacted>")
            .field("credential_key", &"<redacted>")
            .finish()
    }
}

impl MembershipManager {
    #[must_use]
    pub fn unavailable() -> Self {
        Self { inner: None }
    }

    #[must_use]
    pub fn is_replicated(&self) -> bool {
        self.inner.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn replicated(
        client: Client,
        store: Arc<dyn Store>,
        identity: ClusterIdentity,
        local: ClusterPeer,
        bootstrap_http: String,
        secrets: JoinSecrets,
        activation_marker: ActivationMarker,
    ) -> Result<Self, MembershipError> {
        let replication = ReplicationMonitor::replicated(client.clone());
        let local_hostname = membership_hostname(
            system_short_hostname().as_deref().unwrap_or_default(),
            &local.api_address,
        );
        let manager = Self {
            inner: Some(Arc::new(ReplicatedMembership {
                client,
                store,
                identity,
                local,
                local_hostname,
                bootstrap_http,
                secrets,
                activation_marker,
                replication,
            })),
        };
        manager.initialize().await?;
        Ok(manager)
    }

    fn replicated_inner(&self) -> Result<&ReplicatedMembership, MembershipError> {
        self.inner.as_deref().ok_or(MembershipError::Unavailable)
    }

    async fn initialize(&self) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        for statement in MEMBERSHIP_SCHEMA {
            inner.client.execute(*statement, params!()).await?;
        }
        let current = inner
            .client
            .execute(
                "INSERT INTO cluster_membership_meta (singleton, schema_version) VALUES (1, $1) \
                 ON CONFLICT(singleton) DO UPDATE SET schema_version = excluded.schema_version \
                 WHERE cluster_membership_meta.schema_version = excluded.schema_version",
                params!(MEMBERSHIP_SCHEMA_VERSION),
            )
            .await?;
        if current == 0 {
            let rows = inner
                .client
                .query_consistent_map::<MembershipSchemaRow, _>(
                    "SELECT schema_version FROM cluster_membership_meta WHERE singleton = 1",
                    params!(),
                )
                .await?;
            if rows.first().map(|row| row.schema_version) != Some(MEMBERSHIP_SCHEMA_VERSION) {
                return Err(MembershipError::Incompatible);
            }
        }
        self.heartbeat().await
    }

    pub async fn issue_token(&self, ttl: Duration) -> Result<IssuedJoinToken, MembershipError> {
        let inner = self.replicated_inner()?;
        let now = unix_ms()?;
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let expires_at = now.saturating_add(ttl_ms.clamp(1_000, 86_400_000));
        let metrics = inner.client.metrics_db().await?;
        let bootstrap = metrics
            .membership_config
            .nodes()
            .map(|(_, node)| ClusterPeer::from(node))
            .collect::<Vec<_>>();
        if bootstrap.is_empty() {
            return Err(MembershipError::Internal(
                "cluster has no bootstrap member".to_owned(),
            ));
        }

        for _ in 0..8 {
            // Allocate above durable node records *and* the ids that live
            // tokens are currently holding, so a second token can be minted
            // while the first is still outstanding. Expired and redeemed rows
            // are excluded, so an abandoned token stops reserving its id once
            // it lapses rather than consuming the id space permanently.
            let rows = inner
                .client
                .query_consistent_map::<MaxRaftIdRow, _>(
                    "SELECT MAX(raft_id) AS max_raft_id FROM (\
                       SELECT raft_id FROM cluster_nodes \
                       UNION ALL \
                       SELECT raft_id FROM cluster_join_tokens \
                       WHERE state IN ('issued', 'redeeming') AND expires_at > $1\
                     )",
                    params!(now),
                )
                .await?;
            let raft_id = rows
                .first()
                .and_then(|row| row.max_raft_id)
                .unwrap_or(1)
                .saturating_add(1) as u64;
            let payload = JoinPayload {
                version: JOIN_TOKEN_VERSION,
                cluster_id: inner.identity.cluster_id.clone(),
                raft_id,
                expires_at,
                bootstrap_http: inner.bootstrap_http.clone(),
                bootstrap: bootstrap.clone(),
                secrets: JoinSecretPayload {
                    raft: inner.secrets.raft.clone(),
                    api: inner.secrets.api.clone(),
                    credential_key: inner.secrets.credential_key.clone(),
                },
                activation_marker: inner.activation_marker.clone(),
                schema_version: AUTH_SCHEMA_VERSION,
                protocol_version: AUTH_PROTOCOL_VERSION,
            };
            let token = encode_join_token(&payload)?;
            let token_hash = join_token_digest(&token);
            let inserted = inner
                .client
                .execute(
                    "INSERT INTO cluster_join_tokens \
                     (token_hash, raft_id, expires_at, state, node_id, created_at, redeemed_at) \
                     SELECT $1, $2, $3, 'issued', NULL, $4, NULL \
                     WHERE NOT EXISTS (\
                       SELECT 1 FROM cluster_join_tokens \
                       WHERE raft_id = $2 AND state IN ('issued', 'redeeming') AND expires_at > $4\
                     )",
                    params!(token_hash, raft_id as i64, expires_at, now),
                )
                .await?;
            if inserted == 1 {
                return Ok(IssuedJoinToken {
                    token,
                    expires_at,
                    raft_id,
                });
            }
        }
        Err(MembershipError::Internal(
            "could not allocate a unique Raft id".to_owned(),
        ))
    }

    pub async fn redeem(&self, request: &RedeemJoinRequest) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        if request.schema_version != AUTH_SCHEMA_VERSION
            || request.protocol_version != AUTH_PROTOCOL_VERSION
        {
            return Err(MembershipError::Incompatible);
        }
        if !is_join_token_digest(&request.token_digest) {
            return Err(MembershipError::InvalidToken);
        }
        let now = unix_ms()?;
        let record = self.token_record(&request.token_digest).await?;
        if record.raft_id != request.raft_id as i64 {
            return Err(MembershipError::InvalidToken);
        }
        match record.state.as_str() {
            "redeemed" => return Err(MembershipError::ReusedToken),
            "redeeming" if record.node_id.as_deref() != Some(&request.node_id) => {
                return Err(MembershipError::ReservedToken)
            }
            // Redemption reserves the credential to one generated node id.
            // That same staged node may resume after the original TTL; expiry
            // still refuses an unused token below, and a different node id is
            // refused above, so this does not restore bearer authority.
            "redeeming" => {
                self.upsert_hostname(
                    &request.node_id,
                    &membership_hostname(&request.hostname, &request.api_address),
                )
                .await?;
                return Ok(());
            }
            "issued" if record.expires_at <= now => return Err(MembershipError::ExpiredToken),
            "issued" => {}
            _ => return Err(MembershipError::InvalidToken),
        }

        let changed = inner
            .client
            .execute(
                "UPDATE cluster_join_tokens SET state = 'redeeming', node_id = $1 \
                 WHERE token_hash = $2 AND state = 'issued' AND expires_at > $3",
                params!(request.node_id.as_str(), request.token_digest.as_str(), now),
            )
            .await?;
        if changed != 1 {
            let latest = self.token_record(&request.token_digest).await?;
            return if latest.expires_at <= now {
                Err(MembershipError::ExpiredToken)
            } else if latest.state == "redeemed" {
                Err(MembershipError::ReusedToken)
            } else {
                Err(MembershipError::ReservedToken)
            };
        }
        inner
            .client
            .execute(
                "INSERT INTO cluster_nodes \
                 (node_id, raft_id, raft_address, api_address, last_seen_at, removed_at) \
                 VALUES ($1, $2, $3, $4, $5, NULL) \
                 ON CONFLICT(node_id) DO UPDATE SET \
                   raft_id = excluded.raft_id, raft_address = excluded.raft_address, \
                   api_address = excluded.api_address, last_seen_at = excluded.last_seen_at, \
                   removed_at = NULL",
                params!(
                    request.node_id.as_str(),
                    request.raft_id as i64,
                    request.raft_address.as_str(),
                    request.api_address.as_str(),
                    now
                ),
            )
            .await?;
        self.upsert_hostname(
            &request.node_id,
            &membership_hostname(&request.hostname, &request.api_address),
        )
        .await?;
        Ok(())
    }

    async fn upsert_hostname(&self, node_id: &str, hostname: &str) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        inner
            .client
            .execute(
                "INSERT INTO cluster_node_hostnames (node_id, hostname) VALUES ($1, $2) \
                 ON CONFLICT(node_id) DO UPDATE SET hostname = excluded.hostname",
                params!(node_id, hostname),
            )
            .await?;
        Ok(())
    }

    pub async fn finalize(&self, request: &FinalizeJoinRequest) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        if !is_join_token_digest(&request.token_digest) {
            return Err(MembershipError::InvalidToken);
        }
        let record = self.token_record(&request.token_digest).await?;
        if record.node_id.as_deref() != Some(&request.node_id)
            || record.raft_id != request.raft_id as i64
        {
            return Err(MembershipError::ReservedToken);
        }
        if record.state == "redeemed" {
            return Ok(());
        }
        let metrics = inner.client.metrics_db().await?;
        if !metrics
            .membership_config
            .voter_ids()
            .any(|id| id == request.raft_id)
        {
            return Err(MembershipError::Internal(
                "joining node has not committed voter membership".to_owned(),
            ));
        }
        let now = unix_ms()?;
        let changed = inner
            .client
            .execute(
                "UPDATE cluster_join_tokens SET state = 'redeemed', redeemed_at = $1 \
                 WHERE token_hash = $2 AND state = 'redeeming' AND node_id = $3",
                params!(now, request.token_digest.as_str(), request.node_id.as_str()),
            )
            .await?;
        if changed != 1 {
            return Err(MembershipError::ReusedToken);
        }
        Ok(())
    }

    async fn token_record(&self, token_hash: &str) -> Result<JoinTokenRow, MembershipError> {
        let inner = self.replicated_inner()?;
        let rows = inner
            .client
            .query_consistent_map::<JoinTokenRow, _>(
                "SELECT raft_id, expires_at, state, node_id FROM cluster_join_tokens \
                 WHERE token_hash = $1",
                params!(token_hash),
            )
            .await?;
        rows.into_iter().next().ok_or(MembershipError::InvalidToken)
    }

    pub async fn heartbeat(&self) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        let now = unix_ms()?;
        inner
            .client
            .execute(
                "INSERT INTO cluster_nodes \
                 (node_id, raft_id, raft_address, api_address, last_seen_at, removed_at) \
                 VALUES ($1, $2, $3, $4, $5, NULL) \
                 ON CONFLICT(node_id) DO UPDATE SET last_seen_at = excluded.last_seen_at, \
                   removed_at = NULL WHERE cluster_nodes.removed_at IS NULL",
                params!(
                    inner.identity.node_id.as_str(),
                    inner.identity.raft_id as i64,
                    inner.local.raft_address.as_str(),
                    inner.local.api_address.as_str(),
                    now
                ),
            )
            .await?;
        inner
            .client
            .execute(
                "INSERT INTO cluster_node_http (node_id, public_http_url) VALUES ($1, $2) \
                 ON CONFLICT(node_id) DO UPDATE SET \
                   public_http_url = excluded.public_http_url",
                params!(
                    inner.identity.node_id.as_str(),
                    inner.bootstrap_http.as_str()
                ),
            )
            .await?;
        self.upsert_hostname(&inner.identity.node_id, &inner.local_hostname)
            .await?;
        Ok(())
    }

    /// Public HTTP bases for currently reachable peers, used only to recover
    /// node-local materialized bytes such as artwork. The ordinary membership
    /// status deliberately continues to omit addresses.
    pub async fn reachable_peer_http_urls(&self) -> Result<Vec<String>, MembershipError> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(Vec::new());
        };
        let reachable_after = unix_ms()?.saturating_sub(NODE_REACHABLE_WINDOW_MS);
        let rows = inner
            .client
            .query_map::<HttpUrlRow, _>(
                "SELECT http.public_http_url \
                 FROM cluster_node_http http \
                 JOIN cluster_nodes node ON node.node_id = http.node_id \
                 WHERE node.removed_at IS NULL AND node.node_id != $1 \
                   AND node.last_seen_at >= $2 \
                 ORDER BY node.last_seen_at DESC, node.raft_id",
                params!(inner.identity.node_id.as_str(), reachable_after),
            )
            .await?;
        Ok(rows.into_iter().map(|row| row.public_http_url).collect())
    }

    /// Sign a one-file materialization request without exposing the shared
    /// cluster API secret to the HTTP transport.
    pub fn artwork_peer_auth(&self, filename: &str) -> Result<ArtworkPeerAuth, MembershipError> {
        let inner = self.replicated_inner()?;
        let timestamp_ms = unix_ms()?;
        let message = artwork_auth_message(&inner.identity.node_id, timestamp_ms, filename);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(inner.secrets.api.as_bytes())
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        mac.update(message.as_bytes());
        Ok(ArtworkPeerAuth {
            node_id: inner.identity.node_id.clone(),
            timestamp_ms,
            signature: hex::encode(mac.finalize().into_bytes()),
        })
    }

    /// Verify the proof and the sender's live membership. A removed node still
    /// knows the old cluster secret, so signature validity alone is not enough
    /// authority to read bytes from the surviving voters.
    pub async fn verify_artwork_peer_auth(
        &self,
        filename: &str,
        auth: &ArtworkPeerAuth,
    ) -> Result<bool, MembershipError> {
        let inner = self.replicated_inner()?;
        let now = unix_ms()?;
        if now.abs_diff(auth.timestamp_ms) > ARTWORK_AUTH_WINDOW_MS as u64 {
            return Ok(false);
        }
        let signature = match hex::decode(&auth.signature) {
            Ok(signature) => signature,
            Err(_) => return Ok(false),
        };
        let message = artwork_auth_message(&auth.node_id, auth.timestamp_ms, filename);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(inner.secrets.api.as_bytes())
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        mac.update(message.as_bytes());
        if mac.verify_slice(&signature).is_err() {
            return Ok(false);
        }

        let reachable_after = now.saturating_sub(NODE_REACHABLE_WINDOW_MS);
        let rows = inner
            .client
            .query_map::<CountRow, _>(
                "SELECT COUNT(*) AS count FROM cluster_nodes \
                 WHERE node_id = $1 AND removed_at IS NULL AND last_seen_at >= $2",
                params!(auth.node_id.as_str(), reachable_after),
            )
            .await?;
        Ok(rows.first().is_some_and(|row| row.count == 1))
    }

    pub async fn heartbeat_loop(self) {
        if self.inner.is_none() {
            return;
        }
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            if let Err(error) = self.heartbeat().await {
                tracing::warn!(code = error.code(), "cluster node heartbeat failed");
            }
        }
    }

    pub async fn status(&self) -> Result<MembershipStatus, MembershipError> {
        let inner = self.replicated_inner()?;
        let now = unix_ms()?;
        let metrics = inner.client.metrics_db().await?;
        let voters = metrics
            .membership_config
            .voter_ids()
            .collect::<BTreeSet<_>>();
        let members = metrics
            .membership_config
            .nodes()
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>();
        let rows = inner
            .client
            .query_map::<NodeRow, _>(
                "SELECT n.node_id, n.raft_id, n.api_address, n.last_seen_at, \
                        COALESCE(h.hostname, '') AS hostname \
                 FROM cluster_nodes n \
                 LEFT JOIN cluster_node_hostnames h ON h.node_id = n.node_id \
                 WHERE n.removed_at IS NULL ORDER BY n.raft_id",
                params!(),
            )
            .await?;
        let nodes = rows
            .into_iter()
            .filter(|row| members.contains(&(row.raft_id as u64)))
            .map(|row| ClusterNodeRecord {
                node_id: row.node_id,
                hostname: membership_hostname(&row.hostname, &row.api_address),
                advertised_host: advertised_host(&row.api_address),
                raft_id: row.raft_id as u64,
                role: if voters.contains(&(row.raft_id as u64)) {
                    NodeRole::Voter
                } else {
                    NodeRole::Learner
                },
                is_leader: metrics.current_leader == Some(row.raft_id as u64),
                reachable: now.saturating_sub(row.last_seen_at) <= NODE_REACHABLE_WINDOW_MS,
                last_seen_at: row.last_seen_at,
            })
            .collect::<Vec<_>>();
        let availability = match voters.len() {
            0 | 1 => ClusterAvailability::SingleNode,
            2 => ClusterAvailability::DegradedReconfiguration,
            _ => ClusterAvailability::HighAvailability,
        };
        Ok(MembershipStatus {
            availability,
            nodes,
            replication: inner.replication.status().await,
        })
    }

    pub async fn remove_voter(&self, node_id: &str) -> Result<MembershipStatus, MembershipError> {
        let inner = self.replicated_inner()?;
        let status = self.status().await?;
        let target = status
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .ok_or(MembershipError::NodeNotFound)?;
        let metrics = inner.client.metrics_db().await?;
        let voters = metrics
            .membership_config
            .voter_ids()
            .collect::<BTreeSet<_>>();
        if !voters.contains(&target.raft_id) {
            return Err(MembershipError::NodeNotFound);
        }
        if metrics.current_leader == Some(target.raft_id) {
            return Err(MembershipError::LeaderRemoval);
        }
        if voters.len() < 3 {
            return Err(MembershipError::QuorumLoss);
        }
        // Offline work is resolved before the membership change commits, never
        // after. A removal that half-commits leaves packages owned by a node
        // that no longer exists, which `CLUSTERING-PLAN.md` §6.7 calls an
        // activation blocker and which is strictly worse than refusing.
        let mut resolved = self.settle_offline_work(node_id).await?;
        let remaining = voters
            .into_iter()
            .filter(|id| *id != target.raft_id)
            .collect::<BTreeSet<_>>();
        let leader_id = metrics
            .current_leader
            .ok_or_else(|| MembershipError::Internal("cluster has no current leader".to_owned()))?;
        let leader = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)
            .ok_or_else(|| MembershipError::Internal("leader has no node record".to_owned()))?;
        change_membership(&leader.addr_api, &inner.secrets.api, &remaining).await?;
        inner
            .client
            .execute(
                "UPDATE cluster_nodes SET removed_at = $1 WHERE node_id = $2",
                params!(unix_ms()?, node_id),
            )
            .await?;
        // The membership change has committed, so refusing is no longer an
        // available answer for anything that slipped through the last
        // re-read. Fail it instead of leaving a row owned by a node that is
        // now a tombstone.
        match self.fail_offline_work_left_behind(node_id).await {
            Ok(failed) => resolved.failed += failed,
            Err(error) => tracing::error!(
                %error,
                "offline work created while a node removal committed is still owned by the \
                 removed node and will hold its reservation until it expires"
            ),
        }
        if resolved.requeued + resolved.failed > 0 {
            tracing::info!(
                requeued = resolved.requeued,
                failed = resolved.failed,
                "resolved offline packages owned by a removed node"
            );
        }
        self.status().await
    }

    /// Remove this process's own voter, including when it is the leader.
    ///
    /// Arbitrary leader removal remains refused by [`Self::remove_voter`]. A
    /// self-leave is different: the departing process is still alive to settle
    /// its owned work, commit a membership that excludes itself, observe a
    /// surviving leader, and then let the HTTP layer drain the daemon.
    pub async fn leave_voter(&self) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        let node_id = inner.identity.node_id.clone();
        let status = self.status().await?;
        let target = status
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .ok_or(MembershipError::NodeNotFound)?;
        let metrics = inner.client.metrics_db().await?;
        let voters = metrics
            .membership_config
            .voter_ids()
            .collect::<BTreeSet<_>>();
        if !voters.contains(&target.raft_id) {
            return Err(MembershipError::NodeNotFound);
        }
        if voters.len() < 3 {
            return Err(MembershipError::QuorumLoss);
        }

        let mut resolved = self.settle_offline_work(&node_id).await?;
        let remaining = voters
            .into_iter()
            .filter(|id| *id != target.raft_id)
            .collect::<BTreeSet<_>>();
        let leader_id = metrics
            .current_leader
            .ok_or_else(|| MembershipError::Internal("cluster has no current leader".to_owned()))?;
        let leader = metrics
            .membership_config
            .membership()
            .get_node(&leader_id)
            .ok_or_else(|| MembershipError::Internal("leader has no node record".to_owned()))?;
        let survivor_nodes = metrics
            .membership_config
            .membership()
            .nodes()
            .filter(|(raft_id, _)| remaining.contains(raft_id))
            .map(|(raft_id, node)| (*raft_id, node.addr_api.clone()))
            .collect::<Vec<_>>();
        let mut commit_leader_api = leader.addr_api.clone();

        // OpenRaft 0.9 has no dedicated transfer-leader call. Triggering an
        // election on one survivor advances the term and makes this leader
        // step down before its membership entry is removed. Waiting until
        // every survivor reports that same leader turns the trigger into a
        // verified handoff rather than a hopeful delay.
        if leader_id == target.raft_id {
            let candidate = survivor_nodes.first().ok_or_else(|| {
                MembershipError::Internal("cluster has no leadership successor".to_owned())
            })?;
            trigger_election(&candidate.1, &inner.secrets.api).await?;
            let new_leader = self
                .await_survivor_leader(
                    target.raft_id,
                    &remaining,
                    &survivor_nodes,
                    &inner.secrets.api,
                )
                .await?;
            commit_leader_api = survivor_nodes
                .iter()
                .find(|(raft_id, _)| *raft_id == new_leader)
                .map(|(_, api)| api.clone())
                .ok_or_else(|| {
                    MembershipError::Internal("new leader has no node record".to_owned())
                })?;
        }
        change_membership(&commit_leader_api, &inner.secrets.api, &remaining).await?;

        // Membership is already committed, so cleanup failures must not keep
        // this now-nonmember process serving. The HTTP layer will drain after
        // this method returns; report cleanup failures loudly and preserve the
        // one success answer that makes shutdown inevitable.
        if let Err(error) = inner
            .client
            .execute(
                "UPDATE cluster_nodes SET removed_at = $1 WHERE node_id = $2",
                params!(unix_ms()?, node_id.as_str()),
            )
            .await
        {
            tracing::error!(%error, %node_id, "self-removal committed but node tombstone write failed");
        }
        match self.fail_offline_work_left_behind(&node_id).await {
            Ok(failed) => resolved.failed += failed,
            Err(error) => tracing::error!(
                %error,
                %node_id,
                "self-removal committed but late offline work could not be failed"
            ),
        }
        tracing::info!(
            %node_id,
            requeued = resolved.requeued,
            failed = resolved.failed,
            "local voter left the cluster"
        );
        Ok(())
    }

    /// Wait long enough to distinguish a committed self-removal from a
    /// cluster still electing. This is deliberately best-effort: once the
    /// membership entry committed, keeping the removed daemon online because
    /// an observation timed out is strictly less safe than draining it.
    async fn await_survivor_leader(
        &self,
        departed: u64,
        remaining: &BTreeSet<u64>,
        survivor_nodes: &[(u64, String)],
        api_secret: &str,
    ) -> Result<u64, MembershipError> {
        let Ok(client) = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(1))
            .build()
        else {
            return Err(MembershipError::Internal(
                "building leader handoff client".to_owned(),
            ));
        };
        let mut stable_rounds = 0_u8;
        let mut stable_leader = None;
        for _ in 0..40 {
            let mut elected = None;
            let mut all_confirmed = !survivor_nodes.is_empty();
            for (_, api) in survivor_nodes {
                let response = client
                    .get(format!("https://{api}/cluster/metrics/sqlite"))
                    .header("X-API-SECRET", api_secret)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .send()
                    .await;
                let leader = match response {
                    Ok(response) => response
                        .json::<LeaderMetrics>()
                        .await
                        .ok()
                        .and_then(|metrics| metrics.current_leader),
                    Err(_) => None,
                };
                if !leader.is_some_and(|leader| {
                    leader != departed
                        && remaining.contains(&leader)
                        && elected.is_none_or(|expected| expected == leader)
                }) {
                    all_confirmed = false;
                    break;
                }
                elected = leader;
            }
            if all_confirmed {
                if elected == stable_leader {
                    stable_rounds += 1;
                } else {
                    stable_leader = elected;
                    stable_rounds = 1;
                }
                if stable_rounds >= 3 {
                    return stable_leader.ok_or_else(|| {
                        MembershipError::Internal("leader handoff lost its winner".to_owned())
                    });
                }
            } else {
                stable_rounds = 0;
                stable_leader = None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(MembershipError::Internal(format!(
            "leadership did not move away from Raft voter {departed}"
        )))
    }

    /// Resolve the node's offline work, then keep going until a fresh read
    /// finds nothing left to resolve.
    ///
    /// One snapshot only describes the work that existed when the plan was
    /// drawn. The departing node keeps serving its HTTP API throughout, and a
    /// package is created owned by whichever node answered the request — so a
    /// download requested during the bounded probe wait would exit the removal
    /// owned by a node that no longer exists, holding its reservation until
    /// the seven-day expiry. That is precisely the stranded state §6.7 calls
    /// an activation blocker, so the plan is re-read rather than trusted.
    ///
    /// Rounds are bounded because each one is itself bounded by [`PROBE_WAIT`]:
    /// a node that keeps admitting new downloads faster than they can be
    /// resolved gets a refusal naming that reason, not an unbounded wait.
    async fn settle_offline_work(
        &self,
        node_id: &str,
    ) -> Result<OfflineRemovalReport, MembershipError> {
        let inner = self.replicated_inner()?;
        let mut total = OfflineRemovalReport::default();
        let mut round = 0_u32;
        loop {
            let report = self.resolve_offline_work(node_id).await?;
            total.requeued += report.requeued;
            total.failed += report.failed;
            round += 1;
            let remaining = inner
                .store
                .unresolved_offline_packages(node_id)
                .await
                .map_err(|error| MembershipError::Internal(error.to_string()))?
                .len();
            if remaining == 0 {
                return Ok(total);
            }
            if round >= OFFLINE_RESOLVE_ROUNDS {
                return Err(MembershipError::OfflineWork(format!(
                    "{remaining} offline download(s) were requested on this node while its \
                     existing work was being resolved; stop sending it new downloads and retry \
                     the removal"
                )));
            }
        }
    }

    /// The last narrow window: a package created between the final re-read and
    /// the committed membership change.
    ///
    /// It cannot be requeued — no survivor was ever asked about its source, and
    /// asking now would re-home on an unproven mount, which §6.7 forbids. So it
    /// gets the same `node_removed` failure an unverifiable package gets, which
    /// releases its reservation and lets the client ask again.
    ///
    /// This is the removal's last act, not a permanent fence. Leaving the raft
    /// roster does not stop the node's writes: hiqlite forwards a non-leader
    /// client's write to the leader on the shared API secret, which removal does
    /// not rotate, and [`crate::store::OfflinePackageStore::create_offline_package`]
    /// consults no tombstone. A removed node still running and still answering
    /// download requests can therefore create packages owned by a tombstone,
    /// after every refusal and failure path this removal has run. Those are
    /// outside the removal's guarantee until the operator stops that process or
    /// repoints its clients, which `docs/OPERATIONS.md` tells them to do.
    async fn fail_offline_work_left_behind(&self, node_id: &str) -> Result<u64, MembershipError> {
        let inner = self.replicated_inner()?;
        let plan = inner
            .store
            .unresolved_offline_packages(node_id)
            .await
            .map_err(|error| MembershipError::Internal(error.to_string()))?
            .into_iter()
            .map(|package| OfflineRemovalPlanEntry {
                package_id: package.id,
                requeue_to: None,
            })
            .collect::<Vec<_>>();
        if plan.is_empty() {
            return Ok(0);
        }
        inner
            .store
            .resolve_offline_packages_for_removal(node_id, &plan, unix_seconds()?)
            .await
            .map(|report| report.failed)
            .map_err(|error| MembershipError::Internal(error.to_string()))
    }

    /// Resolve every offline package the departing node owns, per §6.7.
    ///
    /// The rule has exactly two allowed outcomes and one allowed refusal:
    ///
    /// * **Requeue** a `queued` or `preparing` package on a survivor that
    ///   answered a source probe by actually opening the snapshotted bytes.
    /// * **Fail** it with `node_removed` when no survivor proved that, which
    ///   releases the reservation so the client can simply ask again.
    /// * **Refuse** the removal while a client is mid-transfer, because
    ///   cutting a download off is neither of the two outcomes above.
    ///
    /// A `ready` package is never requeued. Its bytes exist only in the
    /// departing node's cache, and §7 non-goal 4 explicitly declines to
    /// promise byte-identical transcodes across mixed encoders — so a
    /// re-produced package behind the same stable lease URL could hand a
    /// resuming downloader a different generation's bytes. Failing it stops
    /// the cluster advertising a promise it can no longer keep, and the
    /// client's retry gets a fresh package with a fresh URL.
    async fn resolve_offline_work(
        &self,
        node_id: &str,
    ) -> Result<OfflineRemovalReport, MembershipError> {
        let inner = self.replicated_inner()?;
        let now = unix_seconds()?;
        let packages = inner
            .store
            .unresolved_offline_packages(node_id)
            .await
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        if packages.is_empty() {
            return Ok(OfflineRemovalReport::default());
        }

        let in_flight = inner
            .store
            .offline_transfers_in_flight(node_id, now, now - TRANSFER_ACTIVE_SECONDS)
            .await
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        if in_flight > 0 {
            return Err(MembershipError::OfflineWork(format!(
                "{in_flight} offline download(s) are still transferring from this node; \
                 retry the removal once they finish or delete those packages"
            )));
        }

        // Only `queued` and `preparing` work can move, so only that work is
        // worth asking about. Candidates are the other nodes still in the
        // roster; the departing node is excluded because its answer is about
        // to stop being true.
        let movable = packages
            .iter()
            .filter(|package| package.state == "queued" || package.state == "preparing")
            .map(|package| package.id.clone())
            .collect::<Vec<_>>();
        let candidates = self
            .status()
            .await?
            .nodes
            .iter()
            .filter(|node| node.node_id != node_id && node.reachable)
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();

        let requested_at = unix_seconds()?;
        if !movable.is_empty() && !candidates.is_empty() {
            inner
                .store
                .request_offline_source_probes(&movable, &candidates, requested_at)
                .await
                .map_err(|error| MembershipError::Internal(error.to_string()))?;
            self.await_source_probes(requested_at).await?;
        }

        let mut plan = Vec::with_capacity(packages.len());
        for package in &packages {
            let requeue_to = if movable.contains(&package.id) {
                inner
                    .store
                    .verified_offline_source_nodes(&package.id, requested_at)
                    .await
                    .map_err(|error| MembershipError::Internal(error.to_string()))?
                    .into_iter()
                    .next()
            } else {
                None
            };
            plan.push(OfflineRemovalPlanEntry {
                package_id: package.id.clone(),
                requeue_to,
            });
        }

        inner
            .store
            .resolve_offline_packages_for_removal(node_id, &plan, unix_seconds()?)
            .await
            .map_err(|error| {
                MembershipError::OfflineWork(format!(
                    "this node's offline work changed while it was being resolved, so the \
                     removal was refused rather than committing on a stale plan; retry it: \
                     {error}"
                ))
            })
    }

    /// Give the surviving nodes a bounded window to answer, then stop waiting.
    ///
    /// Not answering is a valid answer: a node that is wedged, busy, or simply
    /// does not have the mount fails to prove anything, and an unproven node
    /// is not a re-homing target. The wait exists to give a healthy node time
    /// to reply, not to keep the operator hanging until every node agrees.
    async fn await_source_probes(&self, requested_at: i64) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        let deadline = tokio::time::Instant::now() + PROBE_WAIT;
        loop {
            let outstanding = inner
                .store
                .outstanding_offline_source_probes(requested_at)
                .await
                .map_err(|error| MembershipError::Internal(error.to_string()))?;
            if outstanding == 0 || tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(PROBE_POLL).await;
        }
    }

    /// The node-side half of the removal protocol: answer every source probe
    /// addressed to this node by actually reading the bytes.
    ///
    /// Every node runs this continuously, not just during a removal, because
    /// the asking node cannot reach into a peer's filesystem and a probe is
    /// only useful if somebody is listening when it is asked.
    pub async fn answer_offline_source_probes(&self) -> Result<u64, MembershipError> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(0);
        };
        let now = unix_seconds()?;
        let pending = inner
            .store
            .pending_offline_source_probes(&inner.identity.node_id, now - PROBE_REQUEST_TTL)
            .await
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        let mut answered = 0;
        for package in pending {
            let package_id = package.id.clone();
            let readable = tokio::task::spawn_blocking(move || source_is_readable(&package))
                .await
                .map_err(|error| MembershipError::Internal(error.to_string()))?;
            if inner
                .store
                .answer_offline_source_probe(
                    &package_id,
                    &inner.identity.node_id,
                    readable,
                    unix_seconds()?,
                )
                .await
                .map_err(|error| MembershipError::Internal(error.to_string()))?
            {
                answered += 1;
            }
        }
        Ok(answered)
    }

    /// Poll for source probes far more often than the heartbeat ticks.
    ///
    /// A removal blocks an operator while it waits for these answers, so the
    /// cadence is the responsiveness of node removal itself, not a background
    /// housekeeping interval. Idle passes cost one indexed lookup that
    /// normally returns nothing.
    pub async fn offline_source_probe_loop(self) {
        if self.inner.is_none() {
            return;
        }
        loop {
            tokio::time::sleep(PROBE_POLL).await;
            match self.answer_offline_source_probes().await {
                Ok(0) => {}
                Ok(answered) => {
                    tracing::info!(
                        answered,
                        "answered offline source probes for a node removal"
                    )
                }
                Err(error) => {
                    tracing::warn!(code = error.code(), "offline source probe pass failed")
                }
            }
        }
    }
}

/// Positive proof that *this* machine can read a package's exact source.
///
/// The whole point of the probe protocol is that a replicated `source_path` is
/// a string, not a mount. So this opens the file and reads from it rather than
/// asking whether the path exists: a stale automount entry, a directory, and a
/// permission-denied share all "exist".
///
/// Size and mtime must match the snapshot the package took at request time for
/// the same reason [`crate::store::OfflinePackageStore::create_offline_package`]
/// snapshots them: a survivor holding a *different* file at the same path is
/// not an equivalent source, and re-homing onto it would silently prepare
/// different media than the traveller asked for.
///
/// Blocking. Callers hand it to a blocking pool — a dead NAS can hold an
/// `open` for a long time, and that must not stall an async runtime.
#[must_use]
pub fn source_is_readable(package: &OfflinePackage) -> bool {
    use std::io::Read as _;

    let Ok(metadata) = std::fs::metadata(&package.source_path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != package.source_size as u64 {
        return false;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs());
    if modified != Some(package.source_mtime as u64) {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(&package.source_path) else {
        return false;
    };
    let mut probe = [0_u8; 1];
    file.read(&mut probe).is_ok()
}

pub fn decode_join_token(token: &str) -> Result<JoinPayload, MembershipError> {
    let mut parts = token.split(':');
    if parts.next() != Some("plxjoin") || parts.next() != Some("v1") || parts.clone().count() != 2 {
        return Err(MembershipError::InvalidToken);
    }
    let key_bytes = hex::decode(parts.next().ok_or(MembershipError::InvalidToken)?)
        .map_err(|_| MembershipError::InvalidToken)?;
    let encrypted = hex::decode(parts.next().ok_or(MembershipError::InvalidToken)?)
        .map_err(|_| MembershipError::InvalidToken)?;
    if key_bytes.len() != 32 || encrypted.len() <= 24 {
        return Err(MembershipError::InvalidToken);
    }
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&encrypted[..24]),
            chacha20poly1305::aead::Payload {
                msg: &encrypted[24..],
                aad: JOIN_TOKEN_AAD,
            },
        )
        .map_err(|_| MembershipError::InvalidToken)?;
    let payload: JoinPayload =
        serde_json::from_slice(&plaintext).map_err(|_| MembershipError::InvalidToken)?;
    if payload.version != JOIN_TOKEN_VERSION || payload.cluster_id.trim().is_empty() {
        return Err(MembershipError::InvalidToken);
    }
    Ok(payload)
}

fn encode_join_token(payload: &JoinPayload) -> Result<String, MembershipError> {
    let mut key_bytes = [0_u8; 32];
    let mut nonce = [0_u8; 24];
    getrandom::getrandom(&mut key_bytes)
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    getrandom::getrandom(&mut nonce)
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    let plaintext = serde_json::to_vec(payload)
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &plaintext,
                aad: JOIN_TOKEN_AAD,
            },
        )
        .map_err(|_| MembershipError::Internal("encrypting join token".to_owned()))?;
    let mut encrypted = nonce.to_vec();
    encrypted.extend(ciphertext);
    Ok(format!(
        "{JOIN_TOKEN_PREFIX}:{}:{}",
        hex::encode(key_bytes),
        hex::encode(encrypted)
    ))
}

#[must_use]
pub fn join_token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn is_join_token_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn change_membership(
    leader_api: &str,
    api_secret: &str,
    voters: &BTreeSet<u64>,
) -> Result<(), MembershipError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    let response = client
        .post(format!("https://{leader_api}/cluster/membership/sqlite"))
        .header("X-API-SECRET", api_secret)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(voters)
        .send()
        .await
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    if !response.status().is_success() {
        return Err(MembershipError::Internal(format!(
            "Hiqlite refused voter reconfiguration with HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

async fn trigger_election(candidate_api: &str, api_secret: &str) -> Result<(), MembershipError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    let response = client
        .post(format!("https://{candidate_api}/cluster/elect/sqlite"))
        .header("X-API-SECRET", api_secret)
        .send()
        .await
        .map_err(|error| MembershipError::Internal(error.to_string()))?;
    if !response.status().is_success() {
        return Err(MembershipError::Internal(format!(
            "Hiqlite refused leader handoff with HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

fn unix_ms() -> Result<i64, MembershipError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| MembershipError::Internal(error.to_string()))?
        .as_millis();
    i64::try_from(millis).map_err(|_| MembershipError::Internal("clock overflow".to_owned()))
}

fn unix_seconds() -> Result<i64, MembershipError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| MembershipError::Internal(error.to_string()))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| MembershipError::Internal("clock overflow".to_owned()))
}

struct MembershipSchemaRow {
    schema_version: i64,
}

impl From<&mut Row<'_>> for MembershipSchemaRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            schema_version: row.get("schema_version"),
        }
    }
}

struct MaxRaftIdRow {
    max_raft_id: Option<i64>,
}

impl From<&mut Row<'_>> for MaxRaftIdRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            max_raft_id: row.get("max_raft_id"),
        }
    }
}

struct JoinTokenRow {
    raft_id: i64,
    expires_at: i64,
    state: String,
    node_id: Option<String>,
}

impl From<&mut Row<'_>> for JoinTokenRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            raft_id: row.get("raft_id"),
            expires_at: row.get("expires_at"),
            state: row.get("state"),
            node_id: row.get("node_id"),
        }
    }
}

struct NodeRow {
    node_id: String,
    raft_id: i64,
    api_address: String,
    hostname: String,
    last_seen_at: i64,
}

struct HttpUrlRow {
    public_http_url: String,
}

struct CountRow {
    count: i64,
}

#[derive(Deserialize)]
struct LeaderMetrics {
    current_leader: Option<u64>,
}

impl From<&mut Row<'_>> for CountRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            count: row.get("count"),
        }
    }
}

impl From<&mut Row<'_>> for HttpUrlRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            public_http_url: row.get("public_http_url"),
        }
    }
}

impl From<&mut Row<'_>> for NodeRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            node_id: row.get("node_id"),
            raft_id: row.get("raft_id"),
            api_address: row.get("api_address"),
            hostname: row.get("hostname"),
            last_seen_at: row.get("last_seen_at"),
        }
    }
}

fn artwork_auth_message(node_id: &str, timestamp_ms: i64, filename: &str) -> String {
    format!("plurx-artwork-v1\n{node_id}\n{timestamp_ms}\n{filename}")
}

/// Strip the listener port while preserving DNS names, IPv4, and bracketed
/// IPv6. Loopback is a location, not a useful numeric identity, so every
/// loopback literal has the one operator-facing spelling `localhost`.
fn advertised_host(address: &str) -> String {
    let host = if let Some(bracketed) = address.strip_prefix('[') {
        bracketed.rsplit_once("]:")
    } else {
        address.rsplit_once(':')
    };
    let host = host
        .filter(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
        .map(|(host, _)| host)
        .unwrap_or(address);
    if host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
    {
        "localhost".to_owned()
    } else {
        host.to_owned()
    }
}

/// Reduce a machine name or FQDN to the short hostname people use at a shell.
/// An IP address is deliberately not accepted as a machine name.
fn short_hostname(raw: &str) -> Option<String> {
    let hostname = raw.trim().trim_end_matches('.');
    if hostname.parse::<IpAddr>().is_ok() {
        return None;
    }
    let label = hostname.split('.').next().unwrap_or_default().trim();
    if label.is_empty()
        || label.eq_ignore_ascii_case("localhost")
        || label.eq_ignore_ascii_case("unknown-host")
        || looks_like_container_id(label)
        || label.chars().any(char::is_control)
    {
        return None;
    }
    Some(label.chars().take(63).collect())
}

/// Docker's default hostname is the container id truncated to twelve hex
/// digits. It is ephemeral runtime plumbing, not a machine name, and should
/// never become the primary identity in the cluster roster.
fn looks_like_container_id(label: &str) -> bool {
    matches!(label.len(), 12 | 64) && label.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn membership_hostname(reported: &str, api_address: &str) -> String {
    membership_hostname_with_lookup(reported, api_address, cached_reverse_hostname)
}

fn membership_hostname_with_lookup(
    reported: &str,
    api_address: &str,
    reverse_lookup: impl FnOnce(IpAddr) -> Option<String>,
) -> String {
    let advertised = advertised_host(api_address);
    short_hostname(reported)
        .or_else(|| short_hostname(&advertised))
        .or_else(|| {
            advertised
                .parse::<IpAddr>()
                .ok()
                .and_then(reverse_lookup)
                .and_then(|hostname| short_hostname(&hostname))
        })
        .unwrap_or_else(|| "unknown-host".to_owned())
}

fn cached_reverse_hostname(address: IpAddr) -> Option<String> {
    static CACHE: OnceLock<Mutex<BTreeMap<IpAddr, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(hostname) = cache.get(&address) {
            return Some(hostname.clone());
        }
    }

    let hostname = reverse_hostname(address)?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(address, hostname.clone());
    }
    Some(hostname)
}

#[cfg(unix)]
fn reverse_hostname(address: IpAddr) -> Option<String> {
    match address {
        IpAddr::V4(address) => {
            // SAFETY: an all-zero sockaddr is a valid base value. The family
            // and address fields below are initialized before libc reads it.
            let mut socket: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            socket.sin_family = libc::AF_INET as libc::sa_family_t;
            socket.sin_addr.s_addr = u32::from_ne_bytes(address.octets());
            #[cfg(any(
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "ios",
                target_os = "macos",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "tvos",
                target_os = "visionos",
                target_os = "watchos"
            ))]
            {
                socket.sin_len = std::mem::size_of_val(&socket) as u8;
            }
            reverse_sockaddr(
                (&raw const socket).cast(),
                std::mem::size_of_val(&socket) as libc::socklen_t,
            )
        }
        IpAddr::V6(address) => {
            // SAFETY: an all-zero sockaddr is a valid base value. The family
            // and address fields below are initialized before libc reads it.
            let mut socket: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            socket.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            socket.sin6_addr.s6_addr = address.octets();
            #[cfg(any(
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "ios",
                target_os = "macos",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "tvos",
                target_os = "visionos",
                target_os = "watchos"
            ))]
            {
                socket.sin6_len = std::mem::size_of_val(&socket) as u8;
            }
            reverse_sockaddr(
                (&raw const socket).cast(),
                std::mem::size_of_val(&socket) as libc::socklen_t,
            )
        }
    }
}

#[cfg(unix)]
fn reverse_sockaddr(
    address: *const libc::sockaddr,
    address_len: libc::socklen_t,
) -> Option<String> {
    let mut hostname = [0 as libc::c_char; 1025];
    // SAFETY: `address` points to a fully initialized sockaddr whose concrete
    // length is supplied alongside it. `hostname` is writable for its full
    // declared size and remains alive until the C string is copied below.
    let result = unsafe {
        libc::getnameinfo(
            address,
            address_len,
            hostname.as_mut_ptr(),
            hostname.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if result != 0 {
        return None;
    }
    // SAFETY: successful getnameinfo writes a NUL-terminated hostname into
    // the provided buffer.
    unsafe { CStr::from_ptr(hostname.as_ptr()) }
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

#[cfg(not(unix))]
fn reverse_hostname(_address: IpAddr) -> Option<String> {
    None
}

#[cfg(unix)]
pub(crate) fn system_short_hostname() -> Option<String> {
    if let Some(configured) = std::env::var("PLURX_NODE_HOSTNAME")
        .ok()
        .and_then(|hostname| short_hostname(&hostname))
    {
        return Some(configured);
    }
    let mut buffer = [0_u8; 256];
    // SAFETY: `buffer` is writable for exactly the length passed to libc and
    // stays alive until the returned bytes have been copied into a String.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    short_hostname(&String::from_utf8_lossy(&buffer[..end]))
}

#[cfg(not(unix))]
pub(crate) fn system_short_hostname() -> Option<String> {
    std::env::var("PLURX_NODE_HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .and_then(|hostname| short_hostname(&hostname))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> JoinPayload {
        JoinPayload {
            version: JOIN_TOKEN_VERSION,
            cluster_id: "cluster-a".to_owned(),
            raft_id: 2,
            expires_at: 123,
            bootstrap_http: "http://127.0.0.1:32400".to_owned(),
            bootstrap: vec![ClusterPeer {
                raft_id: 1,
                raft_address: "127.0.0.1:32401".to_owned(),
                api_address: "127.0.0.1:32402".to_owned(),
            }],
            secrets: JoinSecretPayload {
                raft: "r".repeat(64),
                api: "a".repeat(64),
                credential_key: "c".repeat(64),
            },
            activation_marker: ActivationMarker {
                marker_version: 1,
                cluster_id: "cluster-a".to_owned(),
                source_backup_sha256: "f".repeat(64),
                source_schema_version: 18,
                replicated_schema_version: AUTH_SCHEMA_VERSION,
                imported_rows: 1,
                table_hashes: vec![crate::store::SqliteImportTableDigest {
                    table: "settings".to_owned(),
                    row_count: 1,
                    sha256: "e".repeat(64),
                }],
            },
            schema_version: AUTH_SCHEMA_VERSION,
            protocol_version: AUTH_PROTOCOL_VERSION,
        }
    }

    #[test]
    fn join_token_round_trips_without_plaintext_payload() {
        let expected = payload();
        let token = encode_join_token(&expected).expect("encode token");
        assert!(token.starts_with(JOIN_TOKEN_PREFIX));
        assert!(!token.contains("cluster-a"));
        assert!(!token.contains(&"r".repeat(64)));
        assert_eq!(decode_join_token(&token).expect("decode token"), expected);
    }

    #[test]
    fn malformed_and_tampered_tokens_have_one_stable_refusal() {
        assert_eq!(
            decode_join_token("not-a-token")
                .expect_err("malformed")
                .code(),
            "join_token_invalid"
        );
        let token = encode_join_token(&payload()).expect("encode token");
        let mut bytes = token.into_bytes();
        let last = bytes.last_mut().expect("token byte");
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(bytes).expect("ascii token");
        assert_eq!(
            decode_join_token(&tampered).expect_err("tampered").code(),
            "join_token_invalid"
        );
    }

    #[test]
    fn stable_membership_refusal_codes_are_operator_visible() {
        assert_eq!(MembershipError::ExpiredToken.code(), "join_token_expired");
        assert_eq!(MembershipError::ReusedToken.code(), "join_token_reused");
        assert_eq!(
            MembershipError::QuorumLoss.code(),
            "removal_would_lose_quorum"
        );
    }

    #[test]
    fn advertised_host_hides_the_port_and_names_loopback() {
        assert_eq!(advertised_host("plurx-a.lan:32402"), "plurx-a.lan");
        assert_eq!(advertised_host("192.0.2.40:32402"), "192.0.2.40");
        assert_eq!(advertised_host("127.0.0.1:32402"), "localhost");
        assert_eq!(advertised_host("[::1]:32402"), "localhost");
        assert_eq!(advertised_host("[2001:db8::40]:32402"), "2001:db8::40");
        assert_eq!(advertised_host("legacy-host"), "legacy-host");
        assert_eq!(advertised_host("legacy:host"), "legacy:host");
    }

    #[test]
    fn hostname_is_short_and_never_an_ip_address() {
        assert_eq!(
            short_hostname("living-room.example.net"),
            Some("living-room".to_owned())
        );
        assert_eq!(short_hostname("nuc4.local."), Some("nuc4".to_owned()));
        assert_eq!(short_hostname("192.0.2.40"), None);
        assert_eq!(membership_hostname("", "plurx-a.lan:32402"), "plurx-a");
        assert_eq!(membership_hostname("", "127.0.0.1:32402"), "unknown-host");
    }

    #[test]
    fn docker_container_ids_are_not_machine_hostnames() {
        assert_eq!(short_hostname("1cb4bdb624dc"), None);
        assert_eq!(short_hostname(&"a".repeat(64)), None);
    }

    #[test]
    fn reverse_dns_supplies_a_short_name_for_an_ip_only_node() {
        let hostname =
            membership_hostname_with_lookup("unknown-host", "192.168.4.7:32402", |address| {
                assert_eq!(address, "192.168.4.7".parse::<IpAddr>().expect("ip"));
                Some("nuc3.home.arpa".to_owned())
            });

        assert_eq!(hostname, "nuc3");
    }

    #[test]
    fn reported_hostname_wins_without_a_reverse_lookup() {
        let hostname =
            membership_hostname_with_lookup("nuc4.example.net", "192.168.4.8:32402", |_| {
                panic!("reported hostnames must not trigger reverse DNS")
            });

        assert_eq!(hostname, "nuc4");
    }

    #[test]
    fn join_credentials_and_cluster_secrets_are_redacted_from_debug() {
        let payload = payload();
        let token = encode_join_token(&payload).expect("encode token");
        let digest = join_token_digest(&token);
        let issued = IssuedJoinToken {
            token: token.clone(),
            expires_at: payload.expires_at,
            raft_id: payload.raft_id,
        };
        let redeem = RedeemJoinRequest {
            token_digest: digest.clone(),
            raft_id: payload.raft_id,
            node_id: "node-b".to_owned(),
            hostname: "node-b.example.net".to_owned(),
            raft_address: "node-b:32401".to_owned(),
            api_address: "node-b:32402".to_owned(),
            schema_version: payload.schema_version,
            protocol_version: payload.protocol_version,
        };
        let finalize = FinalizeJoinRequest {
            token_digest: digest.clone(),
            raft_id: payload.raft_id,
            node_id: "node-b".to_owned(),
        };

        for rendered in [
            format!("{issued:?}"),
            format!("{redeem:?}"),
            format!("{finalize:?}"),
            format!("{payload:?}"),
        ] {
            assert!(rendered.contains("<redacted>"), "{rendered}");
            assert!(!rendered.contains(&token), "{rendered}");
            assert!(!rendered.contains(&digest), "{rendered}");
            assert!(!rendered.contains(&"r".repeat(64)), "{rendered}");
            assert!(!rendered.contains(&"a".repeat(64)), "{rendered}");
            assert!(!rendered.contains(&"c".repeat(64)), "{rendered}");
        }
    }
}

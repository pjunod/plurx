//! M3 cluster membership lifecycle.
//!
//! Membership is cluster infrastructure, not a second application store. The
//! Hiqlite client remains the one replicated write path; this coordinator owns
//! only the small amount of state needed to admit nodes, describe them without
//! exposing addresses, and remove a voter safely.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hiqlite::macros::params;
use hiqlite::{Client, Node, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{Store, AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION};

use super::migration::status::{ReplicationMonitor, ReplicationStatus};
use super::migration::ActivationMarker;
use super::ClusterIdentity;

const JOIN_TOKEN_PREFIX: &str = "plxjoin:v1";
const JOIN_TOKEN_AAD: &[u8] = b"plurx-cluster-join-v1";
const JOIN_TOKEN_VERSION: u32 = 1;
const MEMBERSHIP_SCHEMA_VERSION: i64 = 1;
const NODE_REACHABLE_WINDOW_MS: i64 = 30_000;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

const MEMBERSHIP_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cluster_membership_meta (\
         singleton INTEGER PRIMARY KEY CHECK (singleton = 1), \
         schema_version INTEGER NOT NULL) STRICT",
    "CREATE TABLE IF NOT EXISTS cluster_join_tokens (\
         token_hash TEXT PRIMARY KEY, \
         raft_id INTEGER NOT NULL UNIQUE CHECK (raft_id > 0), \
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
    #[error("node owns offline work; remove or expire its offline packages before removing it")]
    OfflineWork,
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
            Self::OfflineWork => "node_owns_offline_work",
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemJoinRequest {
    pub token: String,
    pub node_id: String,
    pub raft_address: String,
    pub api_address: String,
    pub schema_version: i64,
    pub protocol_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeJoinRequest {
    pub token: String,
    pub node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssuedJoinToken {
    pub token: String,
    pub expires_at: i64,
    pub raft_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Voter,
    Learner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClusterNodeRecord {
    pub node_id: String,
    pub raft_id: u64,
    pub role: NodeRole,
    pub reachable: bool,
    pub last_seen_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterAvailability {
    SingleNode,
    DegradedReconfiguration,
    HighAvailability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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

struct ReplicatedMembership {
    client: Client,
    store: Arc<dyn Store>,
    identity: ClusterIdentity,
    local: ClusterPeer,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinSecretPayload {
    pub raft: String,
    pub api: String,
    pub credential_key: String,
}

impl MembershipManager {
    #[must_use]
    pub fn unavailable() -> Self {
        Self { inner: None }
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
        let manager = Self {
            inner: Some(Arc::new(ReplicatedMembership {
                client,
                store,
                identity,
                local,
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
            let rows = inner
                .client
                .query_consistent_map::<MaxRaftIdRow, _>(
                    "SELECT MAX(raft_id) AS max_raft_id FROM (\
                         SELECT raft_id FROM cluster_join_tokens UNION ALL \
                         SELECT raft_id FROM cluster_nodes)",
                    params!(),
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
            let token_hash = hash_join_token(&token);
            let inserted = inner
                .client
                .execute(
                    "INSERT OR IGNORE INTO cluster_join_tokens \
                     (token_hash, raft_id, expires_at, state, node_id, created_at, redeemed_at) \
                     VALUES ($1, $2, $3, 'issued', NULL, $4, NULL)",
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
        let payload = decode_join_token(&request.token)?;
        if request.schema_version != AUTH_SCHEMA_VERSION
            || request.protocol_version != AUTH_PROTOCOL_VERSION
            || payload.schema_version != AUTH_SCHEMA_VERSION
            || payload.protocol_version != AUTH_PROTOCOL_VERSION
            || payload.cluster_id != inner.identity.cluster_id
        {
            return Err(MembershipError::Incompatible);
        }
        let now = unix_ms()?;
        let token_hash = hash_join_token(&request.token);
        let record = self.token_record(&token_hash).await?;
        if record.expires_at <= now || payload.expires_at <= now {
            return Err(MembershipError::ExpiredToken);
        }
        if record.raft_id != payload.raft_id as i64 {
            return Err(MembershipError::InvalidToken);
        }
        match record.state.as_str() {
            "redeemed" => return Err(MembershipError::ReusedToken),
            "redeeming" if record.node_id.as_deref() != Some(&request.node_id) => {
                return Err(MembershipError::ReservedToken)
            }
            "redeeming" => return Ok(()),
            "issued" => {}
            _ => return Err(MembershipError::InvalidToken),
        }

        let changed = inner
            .client
            .execute(
                "UPDATE cluster_join_tokens SET state = 'redeeming', node_id = $1 \
                 WHERE token_hash = $2 AND state = 'issued' AND expires_at > $3",
                params!(request.node_id.as_str(), token_hash.as_str(), now),
            )
            .await?;
        if changed != 1 {
            let latest = self.token_record(&token_hash).await?;
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
                    payload.raft_id as i64,
                    request.raft_address.as_str(),
                    request.api_address.as_str(),
                    now
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn finalize(&self, request: &FinalizeJoinRequest) -> Result<(), MembershipError> {
        let inner = self.replicated_inner()?;
        let payload = decode_join_token(&request.token)?;
        let token_hash = hash_join_token(&request.token);
        let record = self.token_record(&token_hash).await?;
        if record.node_id.as_deref() != Some(&request.node_id)
            || record.raft_id != payload.raft_id as i64
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
            .any(|id| id == payload.raft_id)
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
                params!(now, token_hash, request.node_id.as_str()),
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
                   removed_at = NULL",
                params!(
                    inner.identity.node_id.as_str(),
                    inner.identity.raft_id as i64,
                    inner.local.raft_address.as_str(),
                    inner.local.api_address.as_str(),
                    now
                ),
            )
            .await?;
        Ok(())
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
        self.heartbeat().await?;
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
            .query_consistent_map::<NodeRow, _>(
                "SELECT node_id, raft_id, last_seen_at FROM cluster_nodes \
                 WHERE removed_at IS NULL ORDER BY raft_id",
                params!(),
            )
            .await?;
        let nodes = rows
            .into_iter()
            .filter(|row| members.contains(&(row.raft_id as u64)))
            .map(|row| ClusterNodeRecord {
                node_id: row.node_id,
                raft_id: row.raft_id as u64,
                role: if voters.contains(&(row.raft_id as u64)) {
                    NodeRole::Voter
                } else {
                    NodeRole::Learner
                },
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
        let offline = inner
            .store
            .offline_package_stats(node_id, unix_seconds()?)
            .await
            .map_err(|error| MembershipError::Internal(error.to_string()))?;
        if offline.queued + offline.preparing + offline.ready + offline.failed > 0 {
            return Err(MembershipError::OfflineWork);
        }
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
        self.status().await
    }
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

fn hash_join_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
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
    last_seen_at: i64,
}

impl From<&mut Row<'_>> for NodeRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            node_id: row.get("node_id"),
            raft_id: row.get("raft_id"),
            last_seen_at: row.get("last_seen_at"),
        }
    }
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
    fn token_debug_types_do_not_exist_for_the_manager_secrets() {
        assert_eq!(MembershipError::ExpiredToken.code(), "join_token_expired");
        assert_eq!(MembershipError::ReusedToken.code(), "join_token_reused");
        assert_eq!(
            MembershipError::QuorumLoss.code(),
            "removal_would_lose_quorum"
        );
    }
}

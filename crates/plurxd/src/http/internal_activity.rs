//! Narrow cluster-only activity snapshot transport.
//!
//! This route is deliberately outside `/api/v1`: household credentials do
//! not authorize it, and its peer addresses and authority never enter a
//! public response. The aggregation/UI remains in `system.rs`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

pub const PATH: &str = "/_internal/v1/activity-snapshot";
#[allow(dead_code)] // aggregation child #326 calls the pre-wired client
pub const TIMEOUT: Duration = Duration::from_secs(2);
const TIMESTAMP_HEADER: &str = "x-plurx-cluster-time";
const SIGNATURE_HEADER: &str = "x-plurx-cluster-signature";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActivitySnapshot {
    pub node_id: String,
    /// Existing HLS-session shape, kept opaque here so this transport cannot
    /// accidentally become a second public contract.
    pub sessions: serde_json::Value,
    pub deliveries: Vec<ActivityDelivery>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityDelivery {
    pub method: String,
    pub user: String,
    pub file_id: i64,
    pub item_id: i64,
    pub title: String,
    pub started_unix: i64,
    pub idle_seconds: u64,
    pub session_id: Option<String>,
    pub delivered_bytes: Option<i64>,
    pub delivered_bps: Option<i64>,
}

#[allow(dead_code)] // consumed by #326 once this dependency merges
#[derive(Clone, Debug, PartialEq)]
pub enum PeerActivityOutcome {
    Answered(ActivitySnapshot),
    Unhealthy,
    Unreachable,
    TimedOut,
}

#[allow(dead_code)] // wired now so #326 does not need state/main territory
#[derive(Clone)]
pub struct PeerActivityClient {
    membership: plurx_core::cluster::membership::MembershipManager,
    client: reqwest::Client,
}

#[allow(dead_code)]
impl PeerActivityClient {
    #[must_use]
    pub fn new(membership: plurx_core::cluster::membership::MembershipManager) -> Self {
        Self {
            membership,
            client: reqwest::Client::new(),
        }
    }

    pub async fn snapshots(
        &self,
    ) -> Result<Vec<(String, PeerActivityOutcome)>, plurx_core::cluster::membership::MembershipError>
    {
        let peers = self.membership.activity_peers().await?;
        let mut outcomes = Vec::with_capacity(peers.len());
        for peer in peers {
            let node_id = peer.node_id.clone();
            let outcome = if !peer.reachable {
                PeerActivityOutcome::Unhealthy
            } else if let Some(http_base) = peer.http_base {
                self.snapshot(&http_base).await
            } else {
                PeerActivityOutcome::Unreachable
            };
            outcomes.push((node_id, outcome));
        }
        Ok(outcomes)
    }

    async fn snapshot(&self, base: &str) -> PeerActivityOutcome {
        let timestamp = unix_seconds();
        let Ok(signature) = self.membership.sign_activity_request(timestamp) else {
            return PeerActivityOutcome::Unreachable;
        };
        let request = self
            .client
            .get(format!("{}{PATH}", base.trim_end_matches('/')))
            .timeout(TIMEOUT)
            .header(TIMESTAMP_HEADER, timestamp)
            .header(SIGNATURE_HEADER, signature)
            .send()
            .await;
        match request {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(snapshot) => PeerActivityOutcome::Answered(snapshot),
                Err(_) => PeerActivityOutcome::Unreachable,
            },
            Err(error) if error.is_timeout() => PeerActivityOutcome::TimedOut,
            Ok(_) | Err(_) => PeerActivityOutcome::Unreachable,
        }
    }
}

pub async fn snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ActivitySnapshot>, StatusCode> {
    let timestamp = headers
        .get(TIMESTAMP_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !state
        .membership
        .authorize_activity_request(timestamp, signature)
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(local_snapshot(&state).await))
}

async fn local_snapshot(state: &AppState) -> ActivitySnapshot {
    let sessions = state.transcode.list_deliveries().await;
    let session_rows = sessions
        .iter()
        .map(|(session, _)| session.clone())
        .collect::<Vec<_>>();
    let mut deliveries = sessions
        .into_iter()
        .map(|(session, method)| ActivityDelivery {
            method: method.as_str().to_owned(),
            user: session.user_name,
            file_id: session.file_id,
            item_id: session.item_id,
            title: session.item_title,
            started_unix: session.started_unix,
            idle_seconds: session.idle_seconds,
            session_id: Some(session.id),
            delivered_bytes: Some(session.delivered_bytes),
            delivered_bps: session.delivered_bps,
        })
        .collect::<Vec<_>>();
    let mut titles = HashMap::new();
    for stream in state.streams.list() {
        let item_id = state
            .store
            .get_file(stream.file_id)
            .await
            .ok()
            .flatten()
            .map_or(0, |file| file.item_id);
        let title = title(state, item_id, &mut titles).await;
        deliveries.push(ActivityDelivery {
            method: "remux".to_owned(),
            user: stream.user_name,
            file_id: stream.file_id,
            item_id,
            title,
            started_unix: stream.started_unix,
            idle_seconds: (stream.delivered_idle_ms.max(0) / 1_000) as u64,
            session_id: None,
            delivered_bytes: Some(stream.delivered_bytes),
            delivered_bps: stream.delivered_bps,
        });
    }
    for play in state.direct_plays.list() {
        let title = title(state, play.item_id, &mut titles).await;
        deliveries.push(ActivityDelivery {
            method: "direct".to_owned(),
            user: play.user_name,
            file_id: play.file_id,
            item_id: play.item_id,
            title,
            started_unix: play.started_unix,
            idle_seconds: play.idle_seconds,
            session_id: None,
            delivered_bytes: None,
            delivered_bps: None,
        });
    }
    deliveries.sort_by(|left, right| {
        right
            .started_unix
            .cmp(&left.started_unix)
            .then(left.method.cmp(&right.method))
            .then(left.file_id.cmp(&right.file_id))
            .then(left.user.cmp(&right.user))
    });
    ActivitySnapshot {
        node_id: state.node_id.clone(),
        sessions: serde_json::to_value(session_rows).unwrap_or_else(|_| serde_json::json!([])),
        deliveries,
    }
}

async fn title(state: &AppState, item_id: i64, titles: &mut HashMap<i64, String>) -> String {
    if let Some(title) = titles.get(&item_id) {
        return title.clone();
    }
    let title = state
        .store
        .get_item(item_id)
        .await
        .ok()
        .flatten()
        .map_or_else(String::new, |item| item.title);
    titles.insert(item_id, title.clone());
    title
}

#[allow(dead_code)]
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ordinary_bearer_and_missing_cluster_authority_are_rejected() {
        let (_, state) = crate::http::tests::test_app_with_state();
        let mut household = HeaderMap::new();
        household.insert(
            "authorization",
            "Bearer household-token"
                .parse()
                .expect("static household authorization is a valid header value"),
        );
        assert_eq!(
            snapshot(State(state.clone()), household)
                .await
                .expect_err("a household bearer must not authorize a cluster snapshot"),
            StatusCode::UNAUTHORIZED
        );

        let mut forged = HeaderMap::new();
        forged.insert(
            TIMESTAMP_HEADER,
            unix_seconds()
                .to_string()
                .parse()
                .expect("a decimal timestamp is a valid header value"),
        );
        forged.insert(
            SIGNATURE_HEADER,
            "00".repeat(32)
                .parse()
                .expect("a hexadecimal signature is a valid header value"),
        );
        assert_eq!(
            snapshot(State(state), forged)
                .await
                .expect_err("a forged signature must not authorize a cluster snapshot"),
            StatusCode::UNAUTHORIZED
        );
    }
}

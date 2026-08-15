//! Admin cluster-membership API and the narrow join-token redemption path.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use plurx_core::cluster::membership::{
    FinalizeJoinRequest, IssuedJoinToken, MembershipError, MembershipStatus, RedeemJoinRequest,
};
use serde::Deserialize;

use super::error::ApiError;
use super::extract::AdminUser;
use crate::state::AppState;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct IssueJoinTokenRequest {
    /// Short by default because the token carries the complete authority to
    /// enter the cluster. Bounds prevent a typo from minting a near-permanent
    /// credential or one that expires before it can be copied.
    pub expires_in_seconds: Option<u64>,
}

pub async fn issue_join_token(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(request): Json<IssueJoinTokenRequest>,
) -> Result<Json<IssuedJoinToken>, ApiError> {
    let seconds = request.expires_in_seconds.unwrap_or(600).clamp(60, 3_600);
    state
        .membership
        .issue_token(Duration::from_secs(seconds))
        .await
        .map(Json)
        .map_err(api_error)
}

pub async fn nodes(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<MembershipStatus>, ApiError> {
    state.membership.status().await.map(Json).map_err(api_error)
}

pub async fn remove_node(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<MembershipStatus>, ApiError> {
    state
        .membership
        .remove_voter(&node_id)
        .await
        .map(Json)
        .map_err(api_error)
}

/// The join token is the credential for this route. Requiring an account token
/// would make a fresh node impossible to admit; accepting either would widen
/// the admin token into a node credential. The two doors stay separate.
pub async fn redeem_join(
    State(state): State<AppState>,
    Json(request): Json<RedeemJoinRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .membership
        .redeem(&request)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_error)
}

pub async fn finalize_join(
    State(state): State<AppState>,
    Json(request): Json<FinalizeJoinRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .membership
        .finalize(&request)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_error)
}

fn api_error(error: MembershipError) -> ApiError {
    let status = match error {
        MembershipError::Unavailable => StatusCode::CONFLICT,
        MembershipError::InvalidToken | MembershipError::Incompatible => StatusCode::BAD_REQUEST,
        MembershipError::ExpiredToken => StatusCode::GONE,
        MembershipError::ReusedToken
        | MembershipError::ReservedToken
        | MembershipError::LeaderRemoval
        | MembershipError::QuorumLoss
        | MembershipError::OfflineWork => StatusCode::CONFLICT,
        MembershipError::NodeNotFound => StatusCode::NOT_FOUND,
        MembershipError::Internal(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    ApiError::typed(status, error.code(), error.to_string())
}

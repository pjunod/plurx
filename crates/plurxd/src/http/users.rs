//! User management (admin only). The rules exist to make lockouts
//! impossible: the last admin can be neither deleted nor demoted, and you
//! cannot delete yourself. Password resets revoke the target's sessions.
//! (A forgotten *admin* password is handled by `plurxd reset-password` on
//! the server console, not by HTTP.)

use axum::extract::{Path, State};
use axum::Json;
use plurx_core::auth;
use serde::Deserialize;

use super::dto::UserDto;
use super::error::ApiError;
use super::extract::{AdminUser, AuthUser};
use crate::state::AppState;

/// GET /api/v1/users (admin)
pub async fn list(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let users = state.store.list_users().await?;
    Ok(Json(users.into_iter().map(Into::into).collect()))
}

#[derive(Deserialize)]
pub struct CreateUser {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// POST /api/v1/users (admin)
pub async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUser>,
) -> Result<Json<UserDto>, ApiError> {
    let username = req.username.trim();
    if username.is_empty() || req.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "username required and password must be at least 8 characters".into(),
        ));
    }
    if state.store.get_user_by_username(username).await?.is_some() {
        return Err(ApiError::Conflict(format!(
            "a user named `{username}` already exists"
        )));
    }
    let hash = auth::hash_password(&req.password).map_err(|e| ApiError::Internal(e.to_string()))?;
    let user = state.store.create_user(username, &hash, req.is_admin).await?;
    Ok(Json(user.into()))
}

#[derive(Deserialize)]
pub struct UpdateUser {
    /// New password (resets the user's sessions). Absent = unchanged.
    pub password: Option<String>,
    /// Grant or revoke admin. Absent = unchanged.
    pub is_admin: Option<bool>,
}

/// PUT /api/v1/users/:id (admin)
pub async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUser>,
) -> Result<Json<UserDto>, ApiError> {
    let target = state
        .store
        .get_user(id)
        .await?
        .ok_or(ApiError::NotFound("user"))?;

    if let Some(is_admin) = req.is_admin {
        // Never demote the last admin — that would orphan the server.
        if target.is_admin && !is_admin && state.store.count_admins().await? <= 1 {
            return Err(ApiError::Conflict(
                "cannot remove admin from the last admin account".into(),
            ));
        }
        state.store.set_admin(id, is_admin).await?;
    }
    if let Some(password) = req.password {
        if password.len() < 8 {
            return Err(ApiError::BadRequest(
                "password must be at least 8 characters".into(),
            ));
        }
        let hash = auth::hash_password(&password).map_err(|e| ApiError::Internal(e.to_string()))?;
        state.store.set_password(id, &hash).await?;
        // Old sessions die with the old password.
        state.store.delete_tokens_for_user(id).await?;
    }

    let user = state
        .store
        .get_user(id)
        .await?
        .ok_or(ApiError::NotFound("user"))?;
    Ok(Json(user.into()))
}

/// DELETE /api/v1/users/:id (admin)
pub async fn delete(
    _admin: AdminUser,
    AuthUser(caller): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = state
        .store
        .get_user(id)
        .await?
        .ok_or(ApiError::NotFound("user"))?;
    if target.id == caller.id {
        return Err(ApiError::BadRequest(
            "you cannot delete the account you are signed in with".into(),
        ));
    }
    if target.is_admin && state.store.count_admins().await? <= 1 {
        return Err(ApiError::Conflict("cannot delete the last admin".into()));
    }
    // Tokens and watch state go with the user (ON DELETE CASCADE).
    state.store.delete_user(id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

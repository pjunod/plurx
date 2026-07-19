//! Login, logout, and current-user endpoints.

use std::sync::LazyLock;

use axum::extract::State;
use axum::Json;
use plurx_core::auth;
use serde::{Deserialize, Serialize};

use super::dto::UserDto;
use super::error::ApiError;
use super::extract::{AuthUser, RawToken};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: UserDto,
}

/// POST /api/v1/auth/login
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let user = state.store.get_user_by_username(&req.username).await?;
    // Verify even on unknown user to keep timing uniform.
    let (ok, user) = match user {
        Some(u) => (
            auth::verify_password(&req.password, &u.password_hash),
            Some(u),
        ),
        None => {
            let _ = auth::verify_password(&req.password, &DUMMY_HASH);
            (false, None)
        }
    };
    let user = match (ok, user) {
        (true, Some(u)) => u,
        _ => return Err(ApiError::Unauthorized),
    };

    let token = auth::generate_token().map_err(|e| ApiError::Internal(e.to_string()))?;
    let hash = auth::hash_token(&token);
    state
        .store
        .create_token(&hash, user.id, req.device.as_deref())
        .await?;

    Ok(Json(LoginResponse {
        token,
        user: user.into(),
    }))
}

/// POST /api/v1/auth/logout — invalidate the presented token.
pub async fn logout(
    State(state): State<AppState>,
    RawToken(token): RawToken,
) -> Result<Json<serde_json::Value>, ApiError> {
    let hash = auth::hash_token(&token);
    state.store.delete_token(&hash).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/me
pub async fn me(AuthUser(user): AuthUser) -> Json<UserDto> {
    Json(user.into())
}

/// A real Argon2 hash (of a throwaway password), computed once, used to spend
/// the same verification time on unknown usernames (mitigates user enumeration
/// via login timing). Verifying against it always fails for real passwords.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    auth::hash_password("plurx-timing-placeholder")
        .unwrap_or_else(|_| "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned())
});

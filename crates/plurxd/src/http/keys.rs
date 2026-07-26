//! Scoped API keys: create, list, revoke (admin only).
//!
//! A key is what another application holds instead of a login token. The
//! difference matters: a token IS a user, so an admin token handed to a
//! neighbouring app also hands over every secret in `GET /api/v1/settings`.
//! A key carries a scope list and cannot widen itself.

use axum::extract::{Path, State};
use axum::Json;
use plurx_core::auth;
use plurx_core::domain::{scopes, ApiKey};
use serde::{Deserialize, Serialize};

use super::error::ApiError;
use super::extract::AdminUser;
use crate::state::AppState;

#[derive(Serialize)]
pub struct KeyDto {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub disabled: bool,
}

impl From<ApiKey> for KeyDto {
    fn from(k: ApiKey) -> Self {
        // Note what is NOT here: `key_hash`. Listing keys is a routine,
        // frequently-open settings screen; the stored hash has no business
        // on it, and leaving it out means it cannot leak from there.
        KeyDto {
            id: k.id,
            name: k.name,
            scopes: k.scopes,
            created_at: k.created_at,
            last_used_at: k.last_used_at,
            disabled: k.disabled,
        }
    }
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// The one and only response that carries the secret.
#[derive(Serialize)]
pub struct CreateKeyResponse {
    #[serde(flatten)]
    pub key: KeyDto,
    /// Shown exactly once. Not retrievable afterwards — losing it means
    /// issuing a new key, which is the correct cost of never storing it.
    pub key_secret: String,
}

/// POST /api/v1/keys (admin)
pub async fn create(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Json(req): Json<CreateKeyRequest>,
) -> Result<Json<CreateKeyResponse>, ApiError> {
    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::BadRequest(
            "a key needs a name — it is how you will know which one to revoke".to_owned(),
        ));
    }
    if req.scopes.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "a key with no scopes can do nothing; pick from: {}",
            scopes::ALL.join(", ")
        )));
    }
    // Reject unknown scopes rather than storing them. A typo'd scope would
    // otherwise produce a key that looks correct in the UI and authorizes
    // nothing, and the failure would surface as a 403 in another
    // application, hours later.
    for s in &req.scopes {
        if !scopes::ALL.contains(&s.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "unknown scope {s:?}; valid scopes are: {}",
                scopes::ALL.join(", ")
            )));
        }
    }

    let secret = auth::generate_api_key().map_err(|e| ApiError::Internal(e.to_string()))?;
    let key = state
        .store
        .create_api_key(&name, &auth::hash_token(&secret), &req.scopes)
        .await?;
    tracing::info!(
        target: "plurxd::integrate",
        key = key.id, name = %key.name, scopes = ?key.scopes,
        "api key created"
    );
    Ok(Json(CreateKeyResponse {
        key: key.into(),
        key_secret: secret,
    }))
}

/// GET /api/v1/keys (admin) — never includes secrets.
pub async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<KeyDto>>, ApiError> {
    let keys = state.store.list_api_keys().await?;
    Ok(Json(keys.into_iter().map(KeyDto::from).collect()))
}

/// DELETE /api/v1/keys/{id} (admin)
pub async fn delete(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.delete_api_key(id).await? {
        return Err(ApiError::NotFound("api key"));
    }
    tracing::info!(target: "plurxd::integrate", key = id, "api key revoked");
    Ok(Json(serde_json::json!({ "ok": true })))
}

//! Auth extractors. Any handler taking [`AuthUser`] requires a valid token;
//! [`AdminUser`] additionally requires the admin flag.
//!
//! Tokens arrive either as `Authorization: Bearer <token>` (API clients) or as
//! a `?token=` query parameter — the latter because `<img>` and `<video>` tags
//! can't set headers, so image and stream URLs carry the token inline.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use plurx_core::auth;
use plurx_core::domain::User;

use super::error::ApiError;
use crate::state::AppState;

pub struct AuthUser(pub User);
/// Guard that requires the caller be an admin. Carries no data — handlers that
/// need the admin's identity can also take [`AuthUser`].
pub struct AdminUser;
/// The raw bearer token, for endpoints that operate on the token itself
/// (e.g. logout). Does not validate the token against the store.
pub struct RawToken(pub String);

fn token_from_parts(parts: &Parts) -> Option<String> {
    // Authorization: Bearer <token>
    if let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(token) = s.strip_prefix("Bearer ") {
                return Some(token.trim().to_owned());
            }
        }
    }
    // ?token=<token>
    parts
        .uri
        .query()
        .and_then(|q| url_decode_lookup(q, "token"))
}

/// Minimal `application/x-www-form-urlencoded` lookup for a single key.
fn url_decode_lookup(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = token_from_parts(parts).ok_or(ApiError::Unauthorized)?;
        let hash = auth::hash_token(&token);
        let user = state
            .store
            .user_for_token(&hash)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        Ok(AuthUser(user))
    }
}

impl FromRequestParts<AppState> for RawToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        token_from_parts(parts)
            .map(RawToken)
            .ok_or(ApiError::Unauthorized)
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthUser(user) = AuthUser::from_request_parts(parts, state).await?;
        if user.is_admin {
            Ok(AdminUser)
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("hello"), "hello");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2Fpath"), "/path");
    }
}

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
/// Guard that requires the caller be an admin; carries the admin's identity.
pub struct AdminUser(pub User);

/// Guard for machine callers: a scoped API key, and nothing else.
///
/// **Two credential kinds, two doors.** A login token does not open a
/// key-scoped route and a key does not open a user route, deliberately and
/// in both directions:
///
/// - A token must not pass here, because then "monarr can trigger scans"
///   would be satisfiable by handing it an admin token — which can also
///   read the TMDB/Trakt secrets out of `GET /api/v1/settings`. The narrow
///   credential only helps if the narrow route insists on it.
/// - A key must not pass a user route, because a key has no user. There is
///   no "who" to attribute a watch state or a playback session to, and
///   inventing one would be worse than refusing.
///
/// Build with [`ScopedKey::require`] inside a handler rather than as an
/// extractor generic, so the scope a route needs is written in that route's
/// own body where it can be read.
// Unused until the `/api/v1/scan` routes it guards land (integration plan
// P3). It lives here now rather than arriving with them because the auth
// matrix — key-only, scope-exact, revocation-first — is the security claim
// keys exist to make, and it is written and tested beside the credential it
// belongs to. `ApiKey::allows` is covered by the tests below.
#[allow(dead_code)]
pub struct ScopedKey(pub plurx_core::domain::ApiKey);

#[allow(dead_code)]
impl ScopedKey {
    /// Extract a key from the request and require `scope`.
    ///
    /// `last_used_at` is bumped on every successful check. It is the only
    /// way an operator can tell a key that is working from one that was
    /// issued and forgotten — and the forgotten ones are what should be
    /// revoked.
    pub async fn require(
        parts: &mut Parts,
        state: &AppState,
        scope: &'static str,
    ) -> Result<ScopedKey, ApiError> {
        let secret = token_from_parts(parts).ok_or(ApiError::Unauthorized)?;
        if !auth::is_api_key(&secret) {
            // A user token on a machine route. Unauthorized rather than
            // Forbidden: the credential is the wrong KIND, and saying
            // "forbidden" would suggest the right token could work.
            return Err(ApiError::Unauthorized);
        }
        let key = state
            .store
            .api_key_for_hash(&auth::hash_token(&secret))
            .await?
            .ok_or(ApiError::Unauthorized)?;
        if key.disabled {
            // Revoked is indistinguishable from never-existed, on purpose.
            return Err(ApiError::Unauthorized);
        }
        if !key.allows(scope) {
            // A real key that lacks this scope: Forbidden, because the
            // credential is valid and the answer is about permission.
            return Err(ApiError::Forbidden);
        }
        let _ = state.store.touch_api_key(key.id).await;
        Ok(ScopedKey(key))
    }
}
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
            Ok(AdminUser(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::percent_decode;
    use plurx_core::auth;
    use plurx_core::domain::{scopes, ApiKey};

    fn key(scopes: &[&str], disabled: bool) -> ApiKey {
        ApiKey {
            id: 1,
            name: "monarr".into(),
            key_hash: "h".into(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            created_at: 0,
            last_used_at: None,
            disabled,
        }
    }

    // The routing rule the whole two-credential design rests on: the prefix
    // decides WHICH table to look in, before any lookup happens. Without it
    // a key and a token would have to be tried against each other's store,
    // and "not found" would stop meaning anything.
    #[test]
    fn the_prefix_tells_a_key_from_a_login_token() {
        let secret = auth::generate_api_key().expect("key");
        assert!(auth::is_api_key(&secret));
        let token = auth::generate_token().expect("token");
        assert!(
            !auth::is_api_key(&token),
            "a login token must never be mistaken for a key"
        );
    }

    #[test]
    fn scope_checks_are_exact_and_revocation_beats_all_of_them() {
        let k = key(&[scopes::SCAN_TRIGGER], false);
        assert!(k.allows(scopes::SCAN_TRIGGER));
        assert!(!k.allows(scopes::STATUS_READ));
        assert!(!k.allows("scan:trigger "), "no fuzzy matching on scopes");

        let revoked = key(&[scopes::SCAN_TRIGGER, scopes::STATUS_READ], true);
        for s in scopes::ALL {
            assert!(!revoked.allows(s), "a revoked key still allows {s}");
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("hello"), "hello");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2Fpath"), "/path");
    }
}

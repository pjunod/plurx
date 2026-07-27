//! `GET /api/v1/coming-soon` — what monarr expects to have soon (plan §11.2).
//!
//! plurx does not know what is on the way; monarr does, and already answers
//! `GET /api/v1/calendar`. So this is a proxy, and the proxying is the
//! feature: the monarr API key stays on the server. A browser calling monarr
//! directly would need that key in its own JavaScript, which means every
//! logged-in viewer holds a credential that can edit the whole library. One
//! server-side hop removes that entirely.
//!
//! Read-only, one endpoint, no monarr changes. Unset settings mean no rail,
//! which is the default: a plurx that has never heard of monarr behaves
//! exactly as it did.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use plurx_core::store::keys;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

/// How long an answer is reused.
///
/// A calendar changes when a release date changes — days, not seconds — and
/// the home screen is the most-loaded page there is. Fifteen minutes means a
/// household of six opening plurx all evening costs monarr four requests an
/// hour rather than several hundred.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// How far ahead to look. Four weeks is the horizon a "coming soon" rail is
/// for; past that it is a calendar, and monarr already has one of those.
const HORIZON_DAYS: i64 = 28;

/// One thing monarr expects. Deliberately a subset of monarr's own
/// `CalendarEntry`: `mediaItemId` is monarr's id and means nothing here, and
/// forwarding an id from another application invites somebody to build on
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComingSoon {
    pub date: String,
    /// `episode` | `movie` | `book`.
    pub kind: String,
    pub title: String,
    /// "S01E03 — Pilot", "by Author", or empty.
    pub detail: String,
    /// True when monarr already has the file. Kept because "expected
    /// tomorrow" and "arrived early" are different things to look at.
    pub has_file: bool,
}

#[derive(Deserialize)]
struct MonarrEntry {
    date: String,
    kind: String,
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(default, rename = "hasFile")]
    has_file: bool,
}

#[derive(Serialize)]
pub struct ComingSoonResponse {
    /// Empty and `configured: false` when no monarr is paired — an absent
    /// rail rather than an error, because not pairing is a valid choice and
    /// the home screen must not show a red box for it.
    pub configured: bool,
    pub entries: Vec<ComingSoon>,
}

/// The last good answer and when it was fetched.
#[derive(Default)]
pub struct ComingSoonCache {
    inner: Mutex<Option<(Instant, Vec<ComingSoon>)>>,
}

impl ComingSoonCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn get(&self) -> Option<Vec<ComingSoon>> {
        let guard = self.inner.lock().await;
        let (at, entries) = guard.as_ref()?;
        (at.elapsed() < CACHE_TTL).then(|| entries.clone())
    }

    async fn put(&self, entries: Vec<ComingSoon>) {
        *self.inner.lock().await = Some((Instant::now(), entries));
    }
}

/// GET /api/v1/coming-soon (any logged-in user)
pub async fn coming_soon(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<ComingSoonResponse>, ApiError> {
    let url = state
        .store
        .get_setting(keys::MONARR_URL)
        .await?
        .unwrap_or_default();
    let key = state
        .store
        .get_setting(keys::MONARR_API_KEY)
        .await?
        .unwrap_or_default();
    if url.is_empty() || key.is_empty() {
        return Ok(Json(ComingSoonResponse {
            configured: false,
            entries: Vec::new(),
        }));
    }

    if let Some(entries) = state.coming_soon.get().await {
        return Ok(Json(ComingSoonResponse {
            configured: true,
            entries,
        }));
    }

    let entries = fetch(&url, &key).await.unwrap_or_else(|e| {
        // A monarr that is down must not take the home screen with it. The
        // rail simply has nothing in it this quarter-hour, and the reason is
        // in the log rather than in the viewer's face.
        tracing::warn!(target: "plurxd::integrate", error = %e, "coming-soon fetch failed");
        Vec::new()
    });
    state.coming_soon.put(entries.clone()).await;
    Ok(Json(ComingSoonResponse {
        configured: true,
        entries,
    }))
}

async fn fetch(url: &str, key: &str) -> Result<Vec<ComingSoon>, String> {
    // `date_from_unix` is the home scanner's, reused rather than pulling in
    // a date crate for two calls.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today = plurx_core::scan::home::date_from_unix(now);
    let end = plurx_core::scan::home::date_from_unix(now + HORIZON_DAYS * 86_400);
    let target = format!(
        "{}/api/v1/calendar?start={}&end={}",
        url.trim_end_matches('/'),
        today,
        end
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&target)
        .header("X-Api-Key", key)
        .send()
        .await
        .map_err(|e| format!("cannot reach monarr at {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("monarr returned {}", resp.status()));
    }
    let raw: Vec<MonarrEntry> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .map(|e| ComingSoon {
            date: e.date,
            kind: e.kind,
            title: e.title,
            detail: e.detail,
            has_file: e.has_file,
        })
        .collect())
}

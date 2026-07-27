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
use plurx_core::domain::ItemKind;
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
    /// monarr's ids for the item — the SHOW's, for an episode. Not rendered;
    /// they are how the entry is resolved against plurx's own library.
    #[serde(skip_serializing)]
    pub tmdb_id: Option<i64>,
    #[serde(skip_serializing)]
    pub imdb_id: Option<String>,
    /// plurx's own artwork for this title, when it is already in the library.
    ///
    /// Resolved by id against the local library rather than fetched from
    /// monarr: for a show whose next episode is airing, plurx already has the
    /// poster cached, and proxying an image out of another application to
    /// display art we are holding anyway would be a new network surface for
    /// no gain. A film not yet in the library has no local artwork, and the
    /// card falls back to initials rather than pretending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// The local item, so the card can be clicked through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<i64>,
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
    #[serde(default, rename = "tmdbId")]
    tmdb_id: Option<i64>,
    #[serde(default, rename = "imdbId")]
    imdb_id: Option<String>,
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

/// The state of the monarr pairing, for the settings page.
#[derive(Serialize)]
pub struct MonarrStatus {
    pub configured: bool,
    /// True once monarr has answered a real request.
    pub reachable: bool,
    /// monarr's version, when it said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Watch notifications waiting, delivered, and given up on.
    pub watched_pending: i64,
    pub watched_sent: i64,
    pub watched_failed: i64,
}

/// GET /api/v1/monarr/status (admin) — is the pairing actually working?
///
/// Deliberately an active probe rather than a cached flag. Somebody opening
/// this page has just typed a URL and a key and wants to know whether they
/// typed them right; an answer from fifteen minutes ago cannot tell them.
/// It reads monarr's own status endpoint, which changes nothing.
pub async fn monarr_status(
    _admin: super::extract::AdminUser,
    State(state): State<AppState>,
) -> Result<Json<MonarrStatus>, ApiError> {
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
    let (pending, sent, failed) = state.store.watched_outbox_counts().await?;

    let mut out = MonarrStatus {
        configured: !url.is_empty() && !key.is_empty(),
        reachable: false,
        version: None,
        error: None,
        watched_pending: pending,
        watched_sent: sent,
        watched_failed: failed,
    };
    if !out.configured {
        return Ok(Json(out));
    }
    match probe_monarr(&url, &key).await {
        Ok(version) => {
            out.reachable = true;
            out.version = version;
        }
        Err(e) => out.error = Some(e),
    }
    Ok(Json(out))
}

/// The completed form of what a person typed, or their input unchanged when
/// it cannot be completed honestly.
///
/// A bare `host.docker.internal` is what an operator reaches for, and it is
/// not a URL: reqwest rejects it as a relative address and reports "builder
/// error", which says nothing about what to change. So fill in what is
/// missing and let the settings screen show the result — a normalization you
/// can see is a normalization you can correct.
///
/// The rule that matters is the second half. A first version of this
/// string-matched on `://`, so `http:/monarr:7676` — one mistyped slash —
/// did not look like it had a scheme, was treated as a bare hostname, and
/// came back as `http://http:/monarr:7676:7676`. Turning a typo into nonsense
/// is worse than refusing it, because the person can no longer see what they
/// typed. Everything below is arranged so that cannot happen: repair only
/// what is unambiguous, parse to prove the result is real, and otherwise hand
/// the input straight back so the failure names *their* string.
pub fn normalize_monarr_url(raw: &str) -> String {
    const MONARR_DEFAULT_PORT: u16 = 7676;
    let typed = raw.trim();
    if typed.is_empty() {
        return String::new();
    }

    // `http:/host` — one slash. Unambiguous: a scheme followed by a single
    // slash has no other meaningful reading, and it is the commonest way to
    // mistype a URL. Note this runs on the untrimmed string, so `http://`
    // (scheme, nothing else) is NOT reshaped into something parseable —
    // trimming its slashes first would leave `http:` and invite exactly the
    // guesswork this function exists to avoid.
    let repaired = match typed.split_once(':') {
        Some((scheme, rest))
            if is_scheme(scheme) && rest.starts_with('/') && !rest.starts_with("//") =>
        {
            format!("{scheme}://{}", rest.trim_start_matches('/'))
        }
        _ => typed.to_owned(),
    };

    let had_scheme = repaired.contains("://");
    let candidate = if had_scheme {
        repaired
    } else if typed
        .split_once(':')
        .is_some_and(|(head, rest)| is_scheme(head) && (rest.is_empty() || rest.starts_with('/')))
    {
        // Looks like a scheme, and is followed by nothing that could be an
        // authority. Not ours to guess at. The `rest` test is what keeps
        // `monarr:9000` on the bare-host path — a colon followed by digits
        // is a port, not a scheme, however scheme-shaped the word before it.
        return typed.to_owned();
    } else {
        // A bare host, which is what an operator reaches for. Assume http,
        // and monarr's own default port when none was given — there is
        // nothing else it could sensibly mean.
        format!("http://{repaired}")
    };

    let Ok(mut url) = reqwest::Url::parse(&candidate) else {
        return typed.to_owned();
    };
    if url.host_str().unwrap_or_default().is_empty() {
        return typed.to_owned();
    }
    // Only fill in a port we invented, never one they chose, and never onto a
    // scheme they supplied — guessing :7676 onto an https URL behind a
    // reverse proxy would break a setup that was already correct.
    if url.port().is_none() && !had_scheme {
        let _ = url.set_port(Some(MONARR_DEFAULT_PORT));
    }
    url.as_str().trim_end_matches('/').to_owned()
}

/// RFC 3986 scheme shape: alpha, then alphanumerics and `+-.`.
fn is_scheme(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The real reason a request failed, not the outermost wrapper.
///
/// reqwest's own Display for a send failure is "error sending request for url
/// (…)" — it names the URL you already know and drops the cause you don't.
/// Whether this was a DNS failure or a refused connection is the entire
/// diagnosis, and it lives one or more levels down the source chain. In a
/// two-container setup those two answers mean completely different things:
/// DNS means the other container is not resolvable from this one (a missing
/// `extra_hosts` or a network they do not share), refused means the name
/// resolved and nothing was listening.
fn root_cause(e: &dyn std::error::Error) -> String {
    let mut cause: &dyn std::error::Error = e;
    let mut deepest = cause.to_string();
    while let Some(next) = cause.source() {
        deepest = next.to_string();
        cause = next;
    }
    deepest
}

async fn probe_monarr(url: &str, key: &str) -> Result<Option<String>, String> {
    let url = normalize_monarr_url(url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{url}/api/v1/system/status"))
        .header("X-Api-Key", key)
        .send()
        .await
        .map_err(|e| format!("cannot reach monarr at {url}: {}", root_cause(&e)))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err("monarr rejected the API key".to_owned());
    }
    if !status.is_success() {
        return Err(format!("monarr returned {status}"));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body
        .get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned()))
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
            entries: with_local_artwork(&state, entries).await,
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
        entries: with_local_artwork(&state, entries).await,
    }))
}

/// Attach plurx's own poster to every entry it can resolve by id.
///
/// Done AFTER the cache read, not before it: the cache holds monarr's answer
/// for fifteen minutes, and artwork resolved into it would be frozen there
/// too — so a show that finished scanning two minutes ago would stay
/// pictureless for the rest of the quarter-hour. A handful of indexed lookups
/// per request is the cheaper mistake.
async fn with_local_artwork(state: &AppState, entries: Vec<ComingSoon>) -> Vec<ComingSoon> {
    let mut out = Vec::with_capacity(entries.len());
    for mut e in entries {
        // An episode's artwork is its SHOW's — an episode's own id is not what
        // identifies the series, and the poster a person expects beside
        // "S04E02" is the show's.
        let kind = match e.kind.as_str() {
            "episode" => ItemKind::Show,
            "movie" => ItemKind::Movie,
            // Books are not a thing plurx has (guardrail §10.7), so there is
            // nothing local to find and nothing to look up.
            _ => {
                out.push(e);
                continue;
            }
        };
        match state
            .store
            .item_by_external_id(kind, e.tmdb_id, e.imdb_id.as_deref())
            .await
        {
            Ok(Some(item)) => {
                e.poster = item
                    .poster_path
                    .as_ref()
                    .map(|f| format!("/api/v1/images/{f}"));
                e.item_id = Some(item.id);
            }
            Ok(None) => {}
            Err(err) => {
                // A lookup failure costs a picture, never the rail.
                tracing::warn!(target: "plurxd::integrate", error = %err,
                    "coming-soon artwork lookup failed");
            }
        }
        out.push(e);
    }
    out
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
    let url = normalize_monarr_url(url);
    let target = format!("{url}/api/v1/calendar?start={today}&end={end}");
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
        .map_err(|e| format!("cannot reach monarr at {url}: {}", root_cause(&e)))?;
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
            tmdb_id: e.tmdb_id,
            imdb_id: e.imdb_id,
            poster: None,
            item_id: None,
        })
        .collect())
}

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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use futures_util::stream::{self, StreamExt};
use plurx_core::domain::ItemKind;
use plurx_core::store::keys;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Bound one provider image before it reaches memory or disk. Posters are
/// normally hundreds of kilobytes; 15 MiB leaves room for an extravagant
/// source image without allowing a calendar entry to become an unbounded
/// download.
const MAX_ARTWORK_BYTES: u64 = 15 * 1024 * 1024;

/// A home load may discover several new titles at once. Fetch a small batch
/// in parallel so one slow provider does not serialize the whole rail, while
/// keeping the request count low enough to remain polite to those providers.
const ARTWORK_DOWNLOAD_CONCURRENCY: usize = 4;

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
    /// Artwork served from plurx's own cache. A local library poster wins;
    /// otherwise plurx downloads the provider path monarr named and keeps the
    /// browser/native app off that external network surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// The local item, so the card can be clicked through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<i64>,
    /// The provider artwork reference from monarr. It is input to the local
    /// cache only and is never handed to a client.
    #[serde(skip)]
    source_poster: Option<String>,
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
    /// TMDB-relative for films/most shows; HTTPS for provider-chain shows and
    /// books. The downloader below accepts only the providers monarr uses.
    #[serde(default, rename = "posterPath")]
    poster_path: Option<String>,
}

impl From<MonarrEntry> for ComingSoon {
    fn from(e: MonarrEntry) -> Self {
        ComingSoon {
            date: e.date,
            kind: e.kind,
            title: e.title,
            detail: e.detail,
            has_file: e.has_file,
            tmdb_id: e.tmdb_id,
            imdb_id: e.imdb_id,
            poster: None,
            item_id: None,
            source_poster: e.poster_path.filter(|p| !p.trim().is_empty()),
        }
    }
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
            entries: with_artwork(&state, entries).await,
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
        entries: with_artwork(&state, entries).await,
    }))
}

/// Attach a locally served poster to every entry whose provider named one.
///
/// Done AFTER the cache read, not before it: the cache holds monarr's answer
/// for fifteen minutes, and artwork resolved into it would be frozen there
/// too — so a show that finished scanning two minutes ago would stay
/// pictureless for the rest of the quarter-hour. Local library artwork wins;
/// provider artwork is the fallback for titles that have not arrived yet or
/// series identified outside TMDB's id space.
async fn with_artwork(state: &AppState, entries: Vec<ComingSoon>) -> Vec<ComingSoon> {
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
                e.item_id = Some(item.id);
                if let Some(filename) = item.poster_path.as_deref() {
                    // A stale database path must not suppress the provider
                    // fallback. This can happen after an artwork volume is
                    // replaced independently of the database volume.
                    let present = tokio::fs::metadata(state.artwork_dir.join(filename))
                        .await
                        .is_ok_and(|m| m.is_file() && m.len() > 0);
                    if present {
                        e.poster = Some(format!("/api/v1/images/{filename}"));
                    }
                }
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

    // One poster can appear several times in the four-week window (successive
    // episodes of one show). De-duplicate before starting network work, then
    // fetch distinct images concurrently without changing the entry order.
    let mut sources = BTreeMap::<String, reqwest::Url>::new();
    for entry in &out {
        if entry.poster.is_some() {
            continue;
        }
        if let Some(source) = entry.source_poster.as_deref() {
            if let Some(url) = calendar_artwork_url(source) {
                sources.entry(source.to_owned()).or_insert(url);
            } else {
                tracing::warn!(
                    target: "plurxd::integrate",
                    title = %entry.title,
                    "coming-soon artwork source is not an approved provider URL"
                );
            }
        }
    }
    if sources.is_empty() {
        return out;
    }

    let client = match calendar_artwork_client() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(target: "plurxd::integrate", error = %err,
                "coming-soon artwork client could not be built");
            return out;
        }
    };
    let artwork_dir = state.artwork_dir.clone();
    let downloaded = stream::iter(sources)
        .map(|(source, url)| {
            let client = client.clone();
            let artwork_dir = artwork_dir.clone();
            async move {
                let result = cache_artwork_url(&client, &artwork_dir, &url).await;
                (source, result)
            }
        })
        .buffered(ARTWORK_DOWNLOAD_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut cached = HashMap::<String, String>::new();
    for (source, result) in downloaded {
        match result {
            Ok(filename) => {
                cached.insert(source, filename);
            }
            Err(err) => tracing::warn!(
                target: "plurxd::integrate",
                error = %err,
                "coming-soon artwork download failed"
            ),
        }
    }
    for entry in &mut out {
        if entry.poster.is_some() {
            continue;
        }
        if let Some(filename) = entry
            .source_poster
            .as_ref()
            .and_then(|source| cached.get(source))
        {
            entry.poster = Some(format!("/api/v1/images/{filename}"));
        }
    }
    out
}

/// Turn monarr's provider artwork reference into the one URL plurx may fetch.
///
/// The allowlist is the SSRF boundary. monarr normally sends a TMDB-relative
/// path, but provider-chain shows and books carry absolute TVmaze/Open Library
/// URLs. Accepting arbitrary absolute URLs from a peer would let that peer
/// make plurxd probe private services, so only those three public artwork
/// hosts are valid, on standard HTTPS.
fn calendar_artwork_url(source: &str) -> Option<reqwest::Url> {
    let source = source.trim();
    let raw = if source.starts_with('/') && !source.starts_with("//") {
        format!("https://image.tmdb.org/t/p/w500{source}")
    } else {
        source.to_owned()
    };
    let url = reqwest::Url::parse(&raw).ok()?;
    approved_artwork_url(&url).then_some(url)
}

fn approved_artwork_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("image.tmdb.org" | "static.tvmaze.com" | "covers.openlibrary.org")
        )
}

fn calendar_artwork_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
        // Redirects are checked against the same allowlist. Without this, an
        // approved public host could redirect the downloader to a private
        // address and step around the boundary above.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if approved_artwork_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
}

pub(super) fn artwork_cache_filename(url: &reqwest::Url) -> String {
    let digest = Sha256::digest(url.as_str().as_bytes());
    let extension = Path::new(url.path())
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| matches!(e.as_str(), "jpg" | "jpeg" | "png" | "webp"))
        .map(|e| if e == "jpeg" { "jpg".to_owned() } else { e })
        .unwrap_or_else(|| "jpg".to_owned());
    format!("coming-soon-{}.{}", hex::encode(&digest[..12]), extension)
}

/// Download one already-approved URL into the local artwork cache. This
/// helper deliberately accepts a parsed URL without re-validating it so its
/// file/cache behavior can be tested against a loopback HTTP server; every
/// production caller reaches it through [`calendar_artwork_url`].
async fn cache_artwork_url(
    client: &reqwest::Client,
    artwork_dir: &Path,
    url: &reqwest::Url,
) -> Result<String, String> {
    tokio::fs::create_dir_all(artwork_dir)
        .await
        .map_err(|e| format!("create artwork directory: {e}"))?;
    let filename = artwork_cache_filename(url);
    let destination = artwork_dir.join(&filename);
    if tokio::fs::metadata(&destination)
        .await
        .is_ok_and(|m| m.is_file() && m.len() > 0)
    {
        return Ok(filename);
    }

    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("provider returned {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARTWORK_BYTES)
    {
        return Err(format!("image exceeds {MAX_ARTWORK_BYTES} bytes"));
    }
    let is_image = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("image/"));
    if !is_image {
        return Err("provider response is not an image".to_owned());
    }
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("read image: {e}"))?;
        if bytes.len() as u64 + chunk.len() as u64 > MAX_ARTWORK_BYTES {
            return Err(format!("image exceeds {MAX_ARTWORK_BYTES} bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err("provider returned an empty image".to_owned());
    }

    // Rename only after the complete response is on disk. The image route
    // therefore sees either the previous complete file or the new complete
    // file, never a half-written JPEG while two home requests overlap.
    let temporary: PathBuf = artwork_dir.join(format!(".{filename}.{}.part", uuid::Uuid::new_v4()));
    if let Err(err) = tokio::fs::write(&temporary, &bytes).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(format!("write image: {err}"));
    }
    match tokio::fs::rename(&temporary, &destination).await {
        Ok(()) => Ok(filename),
        Err(_)
            if tokio::fs::metadata(&destination)
                .await
                .is_ok_and(|m| m.is_file() && m.len() > 0) =>
        {
            // Another request won the same race. Its complete file is the
            // desired result; our private temporary file is expendable.
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(filename)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(format!("install image: {err}"))
        }
    }
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
    Ok(raw.into_iter().map(ComingSoon::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn monarr_poster_is_cache_input_not_public_api() {
        let raw: MonarrEntry = serde_json::from_value(json!({
            "date": "2026-08-04",
            "kind": "episode",
            "title": "Lucky",
            "detail": "S01E05",
            "hasFile": false,
            "posterPath": "https://static.tvmaze.com/uploads/images/medium_portrait/1/2.jpg"
        }))
        .expect("calendar entry");
        let entry = ComingSoon::from(raw);
        assert!(entry
            .source_poster
            .as_deref()
            .is_some_and(|p| p.contains("tvmaze")));

        let public = serde_json::to_value(entry).expect("serialize");
        assert!(public.get("source_poster").is_none());
        assert!(public.get("posterPath").is_none());
    }

    #[test]
    fn calendar_artwork_accepts_only_monarrs_public_providers() {
        assert_eq!(
            calendar_artwork_url("/poster.jpg").expect("tmdb").as_str(),
            "https://image.tmdb.org/t/p/w500/poster.jpg"
        );
        assert!(calendar_artwork_url(
            "https://static.tvmaze.com/uploads/images/medium_portrait/1/2.jpg"
        )
        .is_some());
        assert!(calendar_artwork_url("https://covers.openlibrary.org/b/id/1-L.jpg").is_some());

        assert!(calendar_artwork_url("http://image.tmdb.org/poster.jpg").is_none());
        assert!(calendar_artwork_url("https://example.com/poster.jpg").is_none());
        assert!(calendar_artwork_url("https://image.tmdb.org:8443/poster.jpg").is_none());
        assert!(calendar_artwork_url("https://user@image.tmdb.org/poster.jpg").is_none());
    }

    #[tokio::test]
    async fn calendar_artwork_is_downloaded_once_then_reused() {
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let app = axum::Router::new().route(
            "/poster.jpg",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    (
                        [(header::CONTENT_TYPE, "image/jpeg")],
                        b"jpeg bytes".to_vec(),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = reqwest::Url::parse(&format!(
            "http://{}/poster.jpg",
            listener.local_addr().expect("addr")
        ))
        .expect("url");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let artwork = tempfile::tempdir().expect("artwork");
        let client = reqwest::Client::builder().build().expect("client");
        let first = cache_artwork_url(&client, artwork.path(), &url)
            .await
            .expect("first download");
        let second = cache_artwork_url(&client, artwork.path(), &url)
            .await
            .expect("cache hit");

        assert_eq!(first, second);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(artwork.path().join(first)).expect("file"),
            b"jpeg bytes"
        );
    }
}

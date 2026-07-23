//! Trakt.tv integration: device-code OAuth, live scrobbling, and two-way
//! watched/playback sync (docs/FEATURES.md §9).
//!
//! All HTTP lives in [`TraktClient`]; the sync *planning* is a pure function
//! ([`plan_sync`]) over local candidates and remote snapshots, unit-tested
//! without a network. The API base URL is injectable so tests (and
//! `PLURX_TRAKT_BASE`) can point at a mock server.
//!
//! Design notes against the 2026 Trakt limits: watch history (100k) and
//! scrobbling are safe territory; collection/"offline library" sync is capped
//! at 100 items for third-party apps and is deliberately NOT implemented.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

pub const DEFAULT_BASE: &str = "https://api.trakt.tv";
/// Refresh access tokens this many seconds before they actually expire.
pub const REFRESH_MARGIN_SECS: i64 = 24 * 3600;

#[derive(Debug, Error)]
pub enum TraktError {
    #[error("http error: {0}")]
    Http(String),

    #[error("trakt returned status {0}")]
    Status(u16),

    #[error("could not parse trakt response: {0}")]
    Parse(String),

    /// The access token was rejected and could not be refreshed — the account
    /// must be re-linked.
    #[error("trakt authorization expired — re-link the account")]
    AuthExpired,
}

// ---------------------------------------------------------------------------
// OAuth types
// ---------------------------------------------------------------------------

/// Response to a device-code request: show `user_code` and send the person to
/// `verification_url` (trakt.tv/activate), then poll every `interval` seconds.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: i64,
    pub interval: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds the access token lives from `created_at`.
    pub expires_in: i64,
    pub created_at: i64,
}

impl Token {
    pub fn expires_at(&self) -> i64 {
        self.created_at + self.expires_in
    }
}

/// One poll of the device-token endpoint while the user types the code.
#[derive(Debug)]
pub enum DevicePoll {
    /// Approved — tokens issued.
    Ready(Token),
    /// Not approved yet; keep polling.
    Pending,
    /// Polling too fast; add to the interval.
    SlowDown,
    /// The user rejected the code.
    Denied,
    /// The code expired before approval — start over.
    Expired,
}

// ---------------------------------------------------------------------------
// Sync payload types (deserialized defensively — Trakt adds fields freely)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Ids {
    pub trakt: Option<i64>,
    pub tmdb: Option<i64>,
    pub imdb: Option<String>,
}

/// Identity of something watchable, keyed the way Trakt and plurx both can:
/// movies by TMDB id, episodes by show TMDB id + season/episode numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ident {
    Movie {
        tmdb: i64,
    },
    Episode {
        show_tmdb: i64,
        season: i32,
        episode: i32,
    },
}

/// A remote "watched" fact: the newest watch timestamp Trakt has.
pub type RemoteWatched = HashMap<Ident, i64>;
/// Remote in-progress positions: percent complete + when it was paused.
pub type RemotePlayback = HashMap<Ident, (f64, i64)>;

// ---------------------------------------------------------------------------
// Local candidates and the pure sync planner
// ---------------------------------------------------------------------------

/// Local watch state for one item, as the store reports it.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalWatch {
    pub watched: bool,
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
    pub updated_at: i64,
}

/// One library item eligible for sync (movie or episode with a usable ident).
#[derive(Debug, Clone)]
pub struct SyncCandidate {
    pub item_id: i64,
    pub ident: Ident,
    pub watch: Option<LocalWatch>,
    /// Duration from the item's best file (for translating percent → ms).
    pub file_duration_ms: Option<i64>,
}

/// What a sync run should do, split by direction.
#[derive(Debug, Default)]
pub struct SyncPlan {
    /// Mark these watched locally (item id, remote watched_at for the stamp).
    pub mark_watched: Vec<(i64, i64)>,
    /// Add these to Trakt history (ident, watched_at).
    pub push_add: Vec<(Ident, i64)>,
    /// Remove these from Trakt history (explicit local un-watch wins).
    pub push_remove: Vec<Ident>,
    /// Set local resume points (item id, position ms, duration ms, remote ts).
    pub set_progress: Vec<(i64, i64, i64, i64)>,
}

/// Decide what to do for every candidate. Pure — no IO, fully unit-tested.
///
/// Rules (deliberately conservative):
/// - Remote watched, local not: mark watched locally — unless the local row is
///   an *explicit un-watch* that is newer than both the remote watch and the
///   last sync, in which case the un-watch wins and is pushed as a removal.
/// - Local watched, remote missing: push to Trakt. Trakt-side history
///   *removals* therefore do not propagate here (safer: a matching failure on
///   Trakt's side can never erase local history).
/// - Remote playback (resume points): applied only when newer than the local
///   row and the item isn't watched; percent maps onto the local file's
///   duration.
pub fn plan_sync(
    candidates: &[SyncCandidate],
    remote_watched: &RemoteWatched,
    remote_playback: &RemotePlayback,
    last_sync: i64,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    for cand in candidates {
        let remote = remote_watched.get(&cand.ident).copied();
        let local = cand.watch;
        match (local, remote) {
            (Some(l), Some(remote_at)) if !l.watched => {
                // Local row exists but isn't watched; remote says watched.
                if l.updated_at > remote_at && l.updated_at > last_sync {
                    // Explicit newer local un-watch → remove on Trakt.
                    plan.push_remove.push(cand.ident);
                } else {
                    plan.mark_watched.push((cand.item_id, remote_at));
                }
            }
            (None, Some(remote_at)) => plan.mark_watched.push((cand.item_id, remote_at)),
            (Some(l), None) if l.watched => {
                plan.push_add.push((cand.ident, l.updated_at.max(1)));
            }
            _ => {}
        }

        // Resume points: only for items that aren't (about to be) watched.
        let watched_now = matches!(local, Some(l) if l.watched)
            || plan
                .mark_watched
                .last()
                .is_some_and(|(id, _)| *id == cand.item_id);
        if watched_now {
            continue;
        }
        if let Some((pct, paused_at)) = remote_playback.get(&cand.ident) {
            let newer = local.is_none_or(|l| *paused_at > l.updated_at);
            let dur = local
                .and_then(|l| l.duration_ms)
                .or(cand.file_duration_ms)
                .unwrap_or(0);
            if newer && dur > 0 && *pct > 0.0 && *pct < 100.0 {
                let pos = ((pct / 100.0) * dur as f64) as i64;
                plan.set_progress.push((cand.item_id, pos, dur, *paused_at));
            }
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// ISO-8601 (Trakt speaks RFC3339 UTC; we speak unix seconds)
// ---------------------------------------------------------------------------

/// Days → civil date, Howard Hinnant's algorithm (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds → `2026-07-22T20:15:00.000Z`.
pub fn iso8601(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Parse an RFC3339 UTC timestamp (fractional seconds optional) to unix
/// seconds. Returns `None` for anything malformed — sync treats that as 0.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let (y, m, d) = (
        dp.next()?.parse::<i64>().ok()?,
        dp.next()?.parse::<u32>().ok()?,
        dp.next()?.parse::<u32>().ok()?,
    );
    let time = time.split('.').next()?;
    let mut tp = time.split(':');
    let (hh, mm, ss) = (
        tp.next()?.parse::<i64>().ok()?,
        tp.next()?.parse::<i64>().ok()?,
        tp.next()?.parse::<i64>().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

// ---------------------------------------------------------------------------
// The HTTP client
// ---------------------------------------------------------------------------

pub struct TraktClient {
    http: reqwest::Client,
    base: String,
    client_id: String,
    client_secret: String,
}

/// What to tell Trakt about a playback session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleAction {
    Start,
    Pause,
    Stop,
}

impl ScrobbleAction {
    fn path(self) -> &'static str {
        match self {
            ScrobbleAction::Start => "/scrobble/start",
            ScrobbleAction::Pause => "/scrobble/pause",
            ScrobbleAction::Stop => "/scrobble/stop",
        }
    }
}

impl TraktClient {
    pub fn new(client_id: &str, client_secret: &str, base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(25))
                .user_agent("plurx")
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// The three headers every API call needs; `access` adds the bearer token.
    fn api(&self, req: reqwest::RequestBuilder, access: Option<&str>) -> reqwest::RequestBuilder {
        let req = req
            .header("trakt-api-version", "2")
            .header("trakt-api-key", &self.client_id)
            .header("content-type", "application/json");
        match access {
            Some(tok) => req.bearer_auth(tok),
            None => req,
        }
    }

    async fn parse<T: serde::de::DeserializeOwned>(
        res: reqwest::Response,
    ) -> Result<T, TraktError> {
        let status = res.status().as_u16();
        if status == 401 || status == 403 {
            return Err(TraktError::AuthExpired);
        }
        if !(200..300).contains(&status) {
            return Err(TraktError::Status(status));
        }
        res.json::<T>()
            .await
            .map_err(|e| TraktError::Parse(e.to_string()))
    }

    // -- OAuth --------------------------------------------------------------

    pub async fn device_code(&self) -> Result<DeviceCode, TraktError> {
        let res = self
            .api(self.http.post(self.url("/oauth/device/code")), None)
            .json(&json!({ "client_id": self.client_id }))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        Self::parse(res).await
    }

    pub async fn poll_device(&self, device_code: &str) -> Result<DevicePoll, TraktError> {
        let res = self
            .api(self.http.post(self.url("/oauth/device/token")), None)
            .json(&json!({
                "code": device_code,
                "client_id": self.client_id,
                "client_secret": self.client_secret,
            }))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        match res.status().as_u16() {
            200 => Ok(DevicePoll::Ready(
                res.json()
                    .await
                    .map_err(|e| TraktError::Parse(e.to_string()))?,
            )),
            400 => Ok(DevicePoll::Pending),
            429 => Ok(DevicePoll::SlowDown),
            418 => Ok(DevicePoll::Denied),
            410 => Ok(DevicePoll::Expired),
            404 => Ok(DevicePoll::Expired), // unknown code — restart the flow
            s => Err(TraktError::Status(s)),
        }
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Token, TraktError> {
        let res = self
            .api(self.http.post(self.url("/oauth/token")), None)
            .json(&json!({
                "refresh_token": refresh_token,
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
                "grant_type": "refresh_token",
            }))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        // A dead refresh token means the link is gone for good.
        if res.status().as_u16() == 400 || res.status().as_u16() == 401 {
            return Err(TraktError::AuthExpired);
        }
        Self::parse(res).await
    }

    /// The linked account's username (for the settings page).
    pub async fn username(&self, access: &str) -> Result<String, TraktError> {
        let res = self
            .api(self.http.get(self.url("/users/settings")), Some(access))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        let v: Value = Self::parse(res).await?;
        Ok(v.pointer("/user/username")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_owned())
    }

    // -- Scrobbling ---------------------------------------------------------

    pub async fn scrobble(
        &self,
        access: &str,
        action: ScrobbleAction,
        ident: Ident,
        progress: f64,
    ) -> Result<(), TraktError> {
        let mut body = match ident {
            Ident::Movie { tmdb } => json!({ "movie": { "ids": { "tmdb": tmdb } } }),
            Ident::Episode {
                show_tmdb,
                season,
                episode,
            } => json!({
                "show": { "ids": { "tmdb": show_tmdb } },
                "episode": { "season": season, "number": episode },
            }),
        };
        body["progress"] = json!(progress.clamp(0.0, 100.0));
        let res = self
            .api(self.http.post(self.url(action.path())), Some(access))
            .json(&body)
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        // 409 = a stop was already recorded moments ago — not an error for us.
        if res.status().as_u16() == 409 {
            return Ok(());
        }
        Self::parse::<Value>(res).await.map(|_| ())
    }

    // -- Sync pulls ----------------------------------------------------------

    /// Raw `/sync/last_activities` JSON — compared as an opaque change gate.
    pub async fn last_activities(&self, access: &str) -> Result<String, TraktError> {
        let res = self
            .api(
                self.http.get(self.url("/sync/last_activities")),
                Some(access),
            )
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        let v: Value = Self::parse(res).await?;
        Ok(v.to_string())
    }

    pub async fn watched(&self, access: &str) -> Result<RemoteWatched, TraktError> {
        let mut map = RemoteWatched::new();

        let res = self
            .api(
                self.http.get(self.url("/sync/watched/movies")),
                Some(access),
            )
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        let movies: Vec<Value> = Self::parse(res).await?;
        for m in movies {
            let tmdb = m.pointer("/movie/ids/tmdb").and_then(Value::as_i64);
            let at = m
                .get("last_watched_at")
                .and_then(Value::as_str)
                .and_then(parse_iso8601)
                .unwrap_or(0);
            if let Some(tmdb) = tmdb {
                map.insert(Ident::Movie { tmdb }, at);
            }
        }

        let res = self
            .api(self.http.get(self.url("/sync/watched/shows")), Some(access))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        let shows: Vec<Value> = Self::parse(res).await?;
        for s in shows {
            let Some(show_tmdb) = s.pointer("/show/ids/tmdb").and_then(Value::as_i64) else {
                continue;
            };
            for season in s
                .get("seasons")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(sn) = season.get("number").and_then(Value::as_i64) else {
                    continue;
                };
                for ep in season
                    .get("episodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(en) = ep.get("number").and_then(Value::as_i64) else {
                        continue;
                    };
                    let at = ep
                        .get("last_watched_at")
                        .and_then(Value::as_str)
                        .and_then(parse_iso8601)
                        .unwrap_or(0);
                    map.insert(
                        Ident::Episode {
                            show_tmdb,
                            season: sn as i32,
                            episode: en as i32,
                        },
                        at,
                    );
                }
            }
        }
        Ok(map)
    }

    /// In-progress resume points from `/sync/playback` (percent + paused_at).
    pub async fn playback(&self, access: &str) -> Result<RemotePlayback, TraktError> {
        let res = self
            .api(self.http.get(self.url("/sync/playback")), Some(access))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        let entries: Vec<Value> = Self::parse(res).await?;
        let mut map = RemotePlayback::new();
        for e in entries {
            let pct = e.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
            let at = e
                .get("paused_at")
                .and_then(Value::as_str)
                .and_then(parse_iso8601)
                .unwrap_or(0);
            let ident = match e.get("type").and_then(Value::as_str) {
                Some("movie") => e
                    .pointer("/movie/ids/tmdb")
                    .and_then(Value::as_i64)
                    .map(|tmdb| Ident::Movie { tmdb }),
                Some("episode") => {
                    let show = e.pointer("/show/ids/tmdb").and_then(Value::as_i64);
                    let sn = e.pointer("/episode/season").and_then(Value::as_i64);
                    let en = e.pointer("/episode/number").and_then(Value::as_i64);
                    match (show, sn, en) {
                        (Some(show_tmdb), Some(sn), Some(en)) => Some(Ident::Episode {
                            show_tmdb,
                            season: sn as i32,
                            episode: en as i32,
                        }),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(ident) = ident {
                map.insert(ident, (pct, at));
            }
        }
        Ok(map)
    }

    // -- Sync pushes ----------------------------------------------------------

    /// Build the `/sync/history` body for a set of watches. Episodes group by
    /// show → seasons → episode numbers, movies ride flat; `watched_at` is
    /// included when adding, omitted when removing.
    fn history_body(items: &[(Ident, Option<i64>)]) -> Value {
        let mut movies = Vec::new();
        // show_tmdb → season → [(episode, watched_at)]
        type SeasonMap = HashMap<i32, Vec<(i32, Option<i64>)>>;
        let mut shows: HashMap<i64, SeasonMap> = HashMap::new();
        for (ident, at) in items {
            match ident {
                Ident::Movie { tmdb } => {
                    let mut m = json!({ "ids": { "tmdb": tmdb } });
                    if let Some(at) = at {
                        m["watched_at"] = json!(iso8601(*at));
                    }
                    movies.push(m);
                }
                Ident::Episode {
                    show_tmdb,
                    season,
                    episode,
                } => {
                    shows
                        .entry(*show_tmdb)
                        .or_default()
                        .entry(*season)
                        .or_default()
                        .push((*episode, *at));
                }
            }
        }
        let mut show_list = Vec::new();
        let mut show_ids: Vec<_> = shows.keys().copied().collect();
        show_ids.sort_unstable();
        for show_tmdb in show_ids {
            let seasons = &shows[&show_tmdb];
            let mut season_nums: Vec<_> = seasons.keys().copied().collect();
            season_nums.sort_unstable();
            let seasons_json: Vec<Value> = season_nums
                .iter()
                .map(|sn| {
                    let mut eps = seasons[sn].clone();
                    eps.sort_unstable_by_key(|(n, _)| *n);
                    let eps_json: Vec<Value> = eps
                        .iter()
                        .map(|(n, at)| {
                            let mut e = json!({ "number": n });
                            if let Some(at) = at {
                                e["watched_at"] = json!(iso8601(*at));
                            }
                            e
                        })
                        .collect();
                    json!({ "number": sn, "episodes": eps_json })
                })
                .collect();
            show_list.push(json!({ "ids": { "tmdb": show_tmdb }, "seasons": seasons_json }));
        }
        json!({ "movies": movies, "shows": show_list })
    }

    pub async fn history_add(
        &self,
        access: &str,
        items: &[(Ident, i64)],
    ) -> Result<(), TraktError> {
        if items.is_empty() {
            return Ok(());
        }
        let tagged: Vec<_> = items.iter().map(|(i, at)| (*i, Some(*at))).collect();
        let res = self
            .api(self.http.post(self.url("/sync/history")), Some(access))
            .json(&Self::history_body(&tagged))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        Self::parse::<Value>(res).await.map(|_| ())
    }

    pub async fn history_remove(&self, access: &str, items: &[Ident]) -> Result<(), TraktError> {
        if items.is_empty() {
            return Ok(());
        }
        let tagged: Vec<_> = items.iter().map(|i| (*i, None)).collect();
        let res = self
            .api(
                self.http.post(self.url("/sync/history/remove")),
                Some(access),
            )
            .json(&Self::history_body(&tagged))
            .send()
            .await
            .map_err(|e| TraktError::Http(e.to_string()))?;
        Self::parse::<Value>(res).await.map(|_| ())
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(tmdb: i64) -> Ident {
        Ident::Movie { tmdb }
    }

    fn cand(item_id: i64, ident: Ident, watch: Option<LocalWatch>) -> SyncCandidate {
        SyncCandidate {
            item_id,
            ident,
            watch,
            file_duration_ms: Some(6_000_000),
        }
    }

    fn watched_at(updated_at: i64) -> LocalWatch {
        LocalWatch {
            watched: true,
            position_ms: 0,
            duration_ms: Some(6_000_000),
            updated_at,
        }
    }

    fn unwatched_at(updated_at: i64) -> LocalWatch {
        LocalWatch {
            watched: false,
            position_ms: 0,
            duration_ms: Some(6_000_000),
            updated_at,
        }
    }

    #[test]
    fn iso8601_roundtrip() {
        for ts in [
            0,
            1,
            86_399,
            86_400,
            951_827_696,
            1_784_755_419,
            4_102_444_800,
        ] {
            assert_eq!(parse_iso8601(&iso8601(ts)), Some(ts), "ts {ts}");
        }
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            parse_iso8601("2026-07-22T12:00:00.500Z"),
            parse_iso8601("2026-07-22T12:00:00Z")
        );
        assert_eq!(parse_iso8601("garbage"), None);
    }

    #[test]
    fn pulls_remote_watch_into_empty_local() {
        let cands = [cand(1, movie(100), None)];
        let remote: RemoteWatched = [(movie(100), 1000)].into();
        let plan = plan_sync(&cands, &remote, &RemotePlayback::new(), 0);
        assert_eq!(plan.mark_watched, vec![(1, 1000)]);
        assert!(plan.push_add.is_empty() && plan.push_remove.is_empty());
    }

    #[test]
    fn pushes_local_watch_missing_remotely() {
        let cands = [cand(1, movie(100), Some(watched_at(2000)))];
        let plan = plan_sync(&cands, &RemoteWatched::new(), &RemotePlayback::new(), 500);
        assert_eq!(plan.push_add, vec![(movie(100), 2000)]);
    }

    #[test]
    fn newer_local_unwatch_wins_and_removes_remote() {
        let cands = [cand(1, movie(100), Some(unwatched_at(5000)))];
        let remote: RemoteWatched = [(movie(100), 1000)].into();
        let plan = plan_sync(&cands, &remote, &RemotePlayback::new(), 2000);
        assert_eq!(plan.push_remove, vec![movie(100)]);
        assert!(plan.mark_watched.is_empty());
    }

    #[test]
    fn older_local_unwatch_loses_to_remote_watch() {
        // The un-watch predates the remote watch → remote wins.
        let cands = [cand(1, movie(100), Some(unwatched_at(500)))];
        let remote: RemoteWatched = [(movie(100), 1000)].into();
        let plan = plan_sync(&cands, &remote, &RemotePlayback::new(), 100);
        assert_eq!(plan.mark_watched, vec![(1, 1000)]);
        assert!(plan.push_remove.is_empty());
    }

    #[test]
    fn agreement_is_a_noop() {
        let cands = [cand(1, movie(100), Some(watched_at(1000)))];
        let remote: RemoteWatched = [(movie(100), 900)].into();
        let plan = plan_sync(&cands, &remote, &RemotePlayback::new(), 0);
        assert!(plan.mark_watched.is_empty());
        assert!(plan.push_add.is_empty());
        assert!(plan.push_remove.is_empty());
    }

    #[test]
    fn playback_pull_maps_percent_onto_duration() {
        let cands = [cand(1, movie(100), Some(unwatched_at(100)))];
        let playback: RemotePlayback = [(movie(100), (50.0, 900))].into();
        let plan = plan_sync(&cands, &RemoteWatched::new(), &playback, 0);
        assert_eq!(plan.set_progress, vec![(1, 3_000_000, 6_000_000, 900)]);
    }

    #[test]
    fn playback_pull_skips_stale_and_watched() {
        // Local row is newer than the remote pause → keep local.
        let cands = [cand(1, movie(100), Some(unwatched_at(2000)))];
        let playback: RemotePlayback = [(movie(100), (50.0, 900))].into();
        let plan = plan_sync(&cands, &RemoteWatched::new(), &playback, 0);
        assert!(plan.set_progress.is_empty());

        // Watched items never get a resume point.
        let cands = [cand(1, movie(100), Some(watched_at(100)))];
        let remote: RemoteWatched = [(movie(100), 90)].into();
        let plan = plan_sync(&cands, &remote, &playback, 0);
        assert!(plan.set_progress.is_empty());
    }

    #[test]
    fn episode_history_groups_by_show_and_season() {
        let items = [
            (
                Ident::Episode {
                    show_tmdb: 7,
                    season: 2,
                    episode: 4,
                },
                Some(100),
            ),
            (
                Ident::Episode {
                    show_tmdb: 7,
                    season: 2,
                    episode: 5,
                },
                Some(200),
            ),
            (
                Ident::Episode {
                    show_tmdb: 7,
                    season: 1,
                    episode: 1,
                },
                Some(50),
            ),
            (movie(9), Some(300)),
        ];
        let body = TraktClient::history_body(&items);
        let arr = |v: &Value| v.as_array().expect("array").len();
        assert_eq!(arr(&body["movies"]), 1);
        let shows = body["shows"].as_array().expect("shows array");
        assert_eq!(shows.len(), 1);
        let seasons = shows[0]["seasons"].as_array().expect("seasons array");
        assert_eq!(seasons.len(), 2);
        assert_eq!(seasons[0]["number"], 1);
        assert_eq!(arr(&seasons[1]["episodes"]), 2);
        assert!(seasons[1]["episodes"][0]["watched_at"].is_string());
    }
}

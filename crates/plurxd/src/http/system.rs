//! Server identity, first-run setup, settings, and scan status.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use plurx_core::auth;
use plurx_core::domain::{PlaybackEvent, PlaybackEventQuery};
use plurx_core::metadata::genres::GenreBackfillReport;
use plurx_core::store::{keys, Store};
use serde::{Deserialize, Serialize};

use super::auth::LoginResponse;
use super::error::ApiError;
use super::extract::{AdminUser, AuthUser};
use crate::state::{AppState, ScanStatus};

#[derive(Serialize)]
pub struct ServerInfo {
    pub name: String,
    /// Bare semver — clients compare this.
    pub version: &'static str,
    /// Git description of the exact build ("v0.1.0-14-gc0ffee"), for support.
    pub build: &'static str,
    /// Compile time, always present — the fallback when `build` is "unknown".
    pub built_at: &'static str,
    pub instance_id: String,
    pub uptime_seconds: u64,
    /// True when no users exist yet — the web app shows first-run setup.
    pub setup_required: bool,
    /// True when an Android APK is published (so the web UI shows the download
    /// link on Android). See `web::android_apk_path`.
    pub android_app: bool,
}

/// GET /api/v1/server — public; drives the client's setup-vs-login decision.
pub async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfo>, ApiError> {
    let instance_id = state.store.instance_id().await?;
    let setup_required = state.store.count_users().await? == 0;
    let android_app = super::web::android_apk_path(&state.system.data_dir).is_some();
    Ok(Json(ServerInfo {
        name: state.server_name.clone(),
        version: crate::version::SEMVER,
        build: crate::version::BUILD,
        built_at: crate::version::BUILT_AT,
        instance_id,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        setup_required,
        android_app,
    }))
}

#[derive(Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

/// POST /api/v1/setup — create the first (admin) user. Allowed only while no
/// users exist; auto-logs-in on success.
pub async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    if state.store.count_users().await? > 0 {
        return Err(ApiError::Conflict("setup already completed".into()));
    }
    if req.username.trim().is_empty() || req.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "username required and password must be at least 8 characters".into(),
        ));
    }
    let hash = auth::hash_password(&req.password).map_err(|e| ApiError::Internal(e.to_string()))?;
    let user = state
        .store
        .create_user(req.username.trim(), &hash, true)
        .await?;

    let token = auth::generate_token().map_err(|e| ApiError::Internal(e.to_string()))?;
    let token_hash = auth::hash_token(&token);
    state
        .store
        .create_token(&token_hash, user.id, Some("setup"))
        .await?;
    Ok(Json(LoginResponse {
        token,
        user: user.into(),
    }))
}

#[derive(Serialize)]
pub struct SystemDto {
    pub name: String,
    pub version: &'static str,
    pub build: &'static str,
    pub built_at: &'static str,
    pub instance_id: String,
    pub uptime_seconds: u64,
    pub users: i64,
    pub libraries: usize,
    pub active_transcodes: usize,
    /// Hardware encoder slots in use, and the cap
    /// (`transcode.max_hw_sessions`). Reported as a pair because either number
    /// alone is unreadable: a start refused while this says 0 of 2 is a very
    /// different bug from one refused at 2 of 2.
    pub hw_slots_in_use: usize,
    pub hw_slots_max: usize,
    /// Recent targeted-scan requests from other applications, newest last.
    /// The one place an operator can see that monarr is actually talking to
    /// plurx — and what it asked for — without reading the log.
    pub scan_requests: Vec<crate::state::ScanRequestRecord>,
    /// The integration at a glance: has another application ever reached
    /// plurx, and when did it last.
    ///
    /// `scan_requests` above only holds requests that got as far as a
    /// library — a path-mapping mistake is rejected before one exists, so a
    /// server being called constantly and rejecting everything looks
    /// identical there to one nobody is calling. These counters tell those
    /// two apart, which is the difference between "fix monarr's path
    /// mapping" and "check monarr's URL and key".
    pub integration: IntegrationDto,
    /// What the libraries' storage reads at, last time anyone measured. The
    /// input side of the pipeline, and until this existed the only side that
    /// had never been measured — which is why a source the box could not read
    /// fast enough surfaced as a *client* stall and sent everyone to look at
    /// the encoder.
    pub storage: crate::storeprobe::StorageReport,
    #[serde(flatten)]
    pub info: crate::state::SystemInfo,
}

#[derive(Serialize)]
pub struct IntegrationDto {
    /// Scan requests received from other applications, ever (this process).
    pub notifications_received: u64,
    /// Scans started, by what asked for one.
    pub scans_by_trigger: std::collections::BTreeMap<String, u64>,
    /// When the last targeted scan request arrived, and who said it was
    /// from. `None` means none has, this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_notification_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_notification_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_correlation_id: Option<String>,
}

/// GET /api/v1/system (admin) — environment diagnostics for the settings
/// page: paths, ffmpeg, detected encoders, counts.
pub async fn system_info(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<SystemDto>, ApiError> {
    let requests = state.jobs.scan_requests().await;
    let last = requests.last();
    let (by_trigger, notifications) = state.jobs.metrics().snapshot();
    let (hw_in_use, hw_max) = state.transcode.hardware_slots().await;
    Ok(Json(SystemDto {
        name: state.server_name.clone(),
        version: crate::version::SEMVER,
        build: crate::version::BUILD,
        built_at: crate::version::BUILT_AT,
        instance_id: state.store.instance_id().await?,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        users: state.store.count_users().await?,
        libraries: state.store.list_libraries().await?.len(),
        active_transcodes: state.transcode.active_sessions().await,
        hw_slots_in_use: hw_in_use,
        hw_slots_max: hw_max,
        scan_requests: requests.clone(),
        integration: IntegrationDto {
            notifications_received: notifications,
            scans_by_trigger: by_trigger
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            last_notification_at: last.map(|r| r.at),
            last_notification_source: last.and_then(|r| r.source.clone()),
            last_correlation_id: last.and_then(|r| r.correlation_id.clone()),
        },
        storage: state.storage.read().await.clone(),
        info: (*state.system).clone(),
    }))
}

#[derive(Serialize)]
pub struct MediaShapeDto {
    pub probed: i64,
    pub unprobed: i64,
    pub hdr: Vec<(String, i64)>,
    pub hdr_4k: Vec<(String, i64)>,
    pub codecs: Vec<(String, i64)>,
    pub over_segmented_floor: i64,
    pub max_bitrate: Option<i64>,
}

/// GET /api/v1/system/library-shape (admin) — what the libraries actually hold.
///
/// Its own route rather than a field on `/system` because it is a table scan.
/// `/system` is polled by the settings page every few seconds; a census that
/// rides along with it would put a scan of every file behind a UI timer.
pub async fn library_shape(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<MediaShapeDto>, ApiError> {
    let s = state.store.media_shape().await?;
    Ok(Json(MediaShapeDto {
        probed: s.probed,
        unprobed: s.unprobed,
        hdr: s.hdr,
        hdr_4k: s.hdr_4k,
        codecs: s.codecs,
        over_segmented_floor: s.over_segmented_floor,
        max_bitrate: s.max_bitrate,
    }))
}

/// Measure every library's storage and record the result on `state`.
///
/// Shared by the boot task and the admin re-run so there is one definition of
/// what "the storage numbers" are. Roots come from the library table rather
/// than from a config path: what matters is the storage plurx will actually
/// read media from, which is exactly the set of paths libraries point at.
pub async fn probe_storage(state: AppState, sustained_secs: f64) {
    let roots: Vec<std::path::PathBuf> = match state.store.list_libraries().await {
        Ok(libs) => libs.into_iter().flat_map(|l| l.paths).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "storage probe: could not list libraries");
            return;
        }
    };
    if roots.is_empty() {
        return;
    }
    let mut report = crate::storeprobe::probe(roots, sustained_secs).await;
    report.judge();
    for m in &report.mounts {
        // One line per mount, at info: this is the kind of fact someone reads
        // the log to find, and burying it at debug would mean it is only ever
        // seen by someone who already suspected storage.
        tracing::info!(
            roots = %m.roots.join(", "),
            read_mbps = m.read_bps.map(|b| b / 1e6),
            seek_ms = m.seek_ms,
            cache_suspect = m.cache_suspect,
            note = m.note.as_deref().unwrap_or(""),
            "storage probe"
        );
    }
    *state.storage.write().await = report;
}

#[derive(Deserialize, Default)]
pub struct StorageQuery {
    /// Seconds of continuous reading, per mount, on top of the quick probe.
    /// Absent or `0` is the quick probe alone. This costs real I/O — seconds
    /// × the read rate × the number of mounts — so it is opt-in and never
    /// happens at boot.
    #[serde(default)]
    pub sustained: Option<f64>,
}

/// The longest a sustained probe may be asked to run, per mount. A diagnostic
/// that can be told to read for an hour is a denial-of-service with a nice UI.
const SUSTAINED_MAX_SECS: f64 = 120.0;

/// POST /api/v1/system/storage (admin) — re-measure and return the new
/// numbers. Synchronous, because the caller asked for a measurement and an
/// immediate 200 with the *old* figures would be worse than a slow one.
pub async fn remeasure_storage(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(q): Query<StorageQuery>,
) -> Result<Json<crate::storeprobe::StorageReport>, ApiError> {
    let sustained = q.sustained.unwrap_or(0.0).clamp(0.0, SUSTAINED_MAX_SECS);
    probe_storage(state.clone(), sustained).await;
    Ok(Json(state.storage.read().await.clone()))
}

/// POST /api/v1/system/search-index/rebuild (admin) — recreate the derived
/// search index from cluster-authoritative item rows on every voter.
pub async fn rebuild_search_index(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let indexed_items = state.store.rebuild_search_index().await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "indexed_items": indexed_items
    })))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    /// Minimum severity to include ("error" … "trace"). Default: everything
    /// the server's log filter captured.
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_limit")]
    pub limit: usize,
}

fn default_log_level() -> String {
    "trace".to_owned()
}
fn default_log_limit() -> usize {
    500
}

/// GET /api/v1/system/logs (admin) — recent log lines, oldest first.
pub async fn logs(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> Json<Vec<crate::logbuf::LogEntry>> {
    Json(state.logs.tail(&q.level, q.limit.min(2000)))
}

#[derive(Deserialize)]
pub struct PlaybackEventsQuery {
    pub since: Option<i64>,
    pub event: Option<String>,
    #[serde(default = "default_playback_event_limit")]
    pub limit: i64,
}

fn default_playback_event_limit() -> i64 {
    500
}

/// GET /api/v1/system/playback-events (admin) — newest node-local playback
/// observations first. The Store applies the final 2,000-row cap too, so a
/// future caller cannot bypass this handler's bound.
pub async fn playback_events(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(query): Query<PlaybackEventsQuery>,
) -> Result<Json<Vec<PlaybackEvent>>, ApiError> {
    Ok(Json(
        state
            .store
            .playback_events(&bounded_playback_query(query))
            .await?,
    ))
}

fn bounded_playback_query(query: PlaybackEventsQuery) -> PlaybackEventQuery {
    PlaybackEventQuery {
        since_ms: query.since,
        event: query.event,
        limit: query.limit.clamp(1, 2_000),
    }
}

/// A client-side playback problem the browser reports back to the server.
///
/// Why this exists: when a browser refuses a stream — Safari rejecting a codec,
/// or a direct-play file it won't progressive-play — *nothing runs server-side
/// to fail*, so `Settings → Logs` stays empty and the failure is invisible
/// unless the user opens dev tools. Forwarding the browser's own error here puts
/// it in the same log the admin already reads. All fields optional so the client
/// can send only what's relevant; everything is length-clipped before logging.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ClientLog {
    /// "error" | "warn" — anything but "error" logs at WARN.
    pub level: String,
    /// Short machine tag: "playback_failed" | "stream_rejected" | "hls_fatal" |
    /// "stall" | "stall_recovery".
    pub event: String,
    /// Human-readable summary (e.g. "format not supported by this browser").
    pub message: String,
    /// Delivery path in play at the time: "direct_play" | "remux" | "transcode".
    pub method: Option<String>,
    /// `HTMLMediaElement.error.code` (1..=4), when the failure is a media error.
    pub code: Option<i64>,
    /// Title being played, for cross-referencing with the library.
    pub title: Option<String>,
    /// File id being played.
    pub file_id: Option<i64>,
    /// Source video codec the decision picked, e.g. "hevc" — the usual Safari culprit.
    pub vcodec: Option<String>,
    /// Stream URL (query/token stripped by the client).
    pub src: Option<String>,
    /// Extra detail (hls.js error type, stall verdict, …).
    pub detail: Option<String>,
    /// Browser label the client computed ("Safari" | "Chrome" | …).
    pub ua: Option<String>,
    /// Whether this browser will decode this stream in hardware, as reported
    /// by `navigator.mediaCapabilities` for the real codec, resolution and
    /// bitrate — not for a codec string alone.
    ///
    /// The one thing that separates two failures every other field here
    /// renders identically: a full buffer with late frames because the GPU is
    /// doing the work and something upstream hiccuped, versus a full buffer
    /// with late frames because a CPU is software-decoding 4K. `null` from a
    /// browser without the API is honest; `false` is the finding.
    pub decode_hw: Option<bool>,
    /// The browser's own guess at whether it can keep up. `false` alongside
    /// `decode_hw: false` is the browser saying so before it even started.
    pub decode_smooth: Option<bool>,
    // -- playback measurements (M0) ------------------------------------------
    // These are the point of the beacons. Without them the log records THAT a
    // stream stalled and not the one number that says why, which is how much
    // was buffered when it did. Unknown fields are dropped by serde, so a
    // measurement the client sends and this struct doesn't name is a
    // measurement nobody ever sees — the reason these are listed explicitly.
    /// Which playback attempt this belongs to (one per stream start/restart),
    /// so a seek's numbers never contaminate a cold start's.
    pub attempt: Option<String>,
    /// Why that attempt began: cold-start · resume · seek · audio · quality ·
    /// fallback.
    pub reason: Option<String>,
    /// Seconds of decoded video ahead of the playhead at the moment reported.
    pub runway: Option<f64>,
    /// Time from click to first frame, in ms (the `ttff` event).
    pub ms: Option<i64>,
    /// hls.js's bandwidth estimate, in kb/s.
    pub bandwidth: Option<i64>,
    /// Target height of the stream in play.
    pub height: Option<i64>,
    /// Encoder the server reported for this session.
    pub encoder: Option<String>,
    /// Live HLS session to join with node-local server telemetry. Web clients
    /// historically call this `session`; the alias keeps that field additive.
    #[serde(alias = "session")]
    pub session_id: Option<String>,
}

/// Sustained rate and burst allowance for `/client-log`, in reports per minute.
///
/// The log ring holds 2000 lines, so a browser that hits an error in a loop can
/// erase every other line in it in seconds — destroying precisely the history an
/// operator opened the page to read. That is not hypothetical: a stranded
/// hls.js instance polling a playlist that never ends fires a fatal error on a
/// timer, and there was no bound on how many of those could pile up.
const CLIENT_LOG_USER_PER_MIN: u32 = 240;
const CLIENT_LOG_GLOBAL_PER_MIN: u32 = 1_000;
const CLIENT_LOG_USER_BUCKETS_MAX: usize = 4_096;

/// One bucket in the two-tier limiter. Per-user buckets prevent one viewer
/// silencing another; the global bucket bounds total pressure on the log ring.
struct LogBucket {
    tokens: f64,
    per_minute: u32,
    /// `None` until the first report — `Instant` has no const constructor.
    last: Option<std::time::Instant>,
    /// Reports dropped since the last admitted one, so the gap is reported
    /// rather than silently swallowed.
    suppressed: u64,
}

impl LogBucket {
    /// Take a token. `Some(n)` means record this report and mention that `n`
    /// were dropped before it; `None` means drop it.
    fn admit(&mut self, now: std::time::Instant) -> Option<u64> {
        let elapsed = self
            .last
            .map(|t| now.saturating_duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        self.last = Some(now);
        self.tokens =
            (self.tokens + elapsed * (self.per_minute as f64 / 60.0)).min(self.per_minute as f64);
        if self.tokens < 1.0 {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.tokens -= 1.0;
        Some(std::mem::take(&mut self.suppressed))
    }
}

impl LogBucket {
    fn new(per_minute: u32) -> Self {
        Self {
            tokens: per_minute as f64,
            per_minute,
            last: None,
            suppressed: 0,
        }
    }
}

struct ClientLogLimiter {
    global: LogBucket,
    users: HashMap<i64, LogBucket>,
}

impl ClientLogLimiter {
    fn new() -> Self {
        Self {
            global: LogBucket::new(CLIENT_LOG_GLOBAL_PER_MIN),
            users: HashMap::new(),
        }
    }

    fn admit(&mut self, user_id: i64, now: std::time::Instant) -> Option<u64> {
        if !self.users.contains_key(&user_id) && self.users.len() >= CLIENT_LOG_USER_BUCKETS_MAX {
            let oldest_id = self
                .users
                .iter()
                .min_by_key(|(_, bucket)| bucket.last)
                .map(|(id, _)| *id);
            if let Some(id) = oldest_id {
                if let Some(evicted) = self.users.remove(&id) {
                    self.global.suppressed =
                        self.global.suppressed.saturating_add(evicted.suppressed);
                }
            }
        }
        let user_suppressed = self
            .users
            .entry(user_id)
            .or_insert_with(|| LogBucket::new(CLIENT_LOG_USER_PER_MIN))
            .admit(now)?;
        match self.global.admit(now) {
            Some(global_suppressed) => Some(user_suppressed.saturating_add(global_suppressed)),
            None => {
                // The global bucket counted this report. Preserve any older
                // per-user gap that was just collected, so the next admitted
                // report still prints every suppressed event exactly once.
                if let Some(bucket) = self.users.get_mut(&user_id) {
                    bucket.suppressed = bucket.suppressed.saturating_add(user_suppressed);
                }
                None
            }
        }
    }
}

static CLIENT_LOG_LIMITER: std::sync::LazyLock<std::sync::Mutex<ClientLogLimiter>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(ClientLogLimiter::new()));

/// POST /api/v1/client-log — any signed-in user. Records one browser playback
/// error into the server log ring so it surfaces in `Settings → Logs`. Bounded
/// by per-field clipping and by a global rate limit (this is diagnostics, not an
/// audit trail), and tagged with the `plurxd::client` target so it's visibly a
/// client report.
pub async fn client_log(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    super::network::RemoteAddress(remote): super::network::RemoteAddress,
    Json(ev): Json<ClientLog>,
) -> StatusCode {
    let suppressed = match CLIENT_LOG_LIMITER.lock() {
        Ok(mut limiter) => limiter.admit(user.id, std::time::Instant::now()),
        // Fail open: a poisoned lock must not silence diagnostics.
        Err(_) => Some(0),
    };
    // Still 204 when dropped. The client is reporting, not asking, and an error
    // response would only give it something new to report about.
    let Some(suppressed) = suppressed else {
        return StatusCode::NO_CONTENT;
    };
    let line = client_log_line(&ev, suppressed);

    // Both WARN and ERROR clear the default `info` filter, so either shows in
    // the admin log without the operator touching PLURX_LOG.
    if ev.level.eq_ignore_ascii_case("error") {
        tracing::error!(target: "plurxd::client", "{line}");
    } else {
        tracing::warn!(target: "plurxd::client", "{line}");
    }

    let event = client_playback_event(&ev, user.id);
    let network = super::network::identity(&headers, remote, ev.ua.as_deref());
    let transcode = Arc::clone(&state.transcode);
    let store = Arc::clone(&state.store);
    tokio::spawn(async move {
        let session_id = event.session_id.clone();
        let info = match session_id.as_deref() {
            Some(session_id) => transcode.session_status(session_id).await,
            None => None,
        };
        emit_client_playback_event(store, event, info.as_ref(), network);
    });
    StatusCode::NO_CONTENT
}

fn join_session_truth(event: &mut PlaybackEvent, info: &crate::transcode::SessionInfo) {
    event.file_id = Some(info.file_id);
    event.encoder = Some(info.encoder.to_owned());
    event.height = Some(info.target_height);
    event.speed_recent = info.recent_speed;
    event.ahead_seconds = info.ahead_seconds;
    event.suspended = Some(info.suspended);
    event.hold_reason = info.hold_reason.map(|reason| match reason {
        crate::transcode::AheadHoldReason::Time => "time".to_owned(),
        crate::transcode::AheadHoldReason::Bytes => "bytes".to_owned(),
        crate::transcode::AheadHoldReason::Global => "global".to_owned(),
    });
    event.delivered_bps = info.delivered_bps;
    event.readrate = Some(info.readrate);
}

fn emit_client_playback_event(
    store: Arc<dyn Store>,
    mut event: PlaybackEvent,
    info: Option<&crate::transcode::SessionInfo>,
    network: Option<crate::telemetry::NetworkIdentity>,
) {
    if let Some(info) = info {
        join_session_truth(&mut event, info);
    }
    crate::telemetry::emit_with_network(store, event, network);
}

fn client_playback_event(ev: &ClientLog, user_id: i64) -> PlaybackEvent {
    fn clipped(value: &Option<String>, limit: usize) -> Option<String> {
        let value = value.as_deref()?.trim();
        if value.is_empty() {
            return None;
        }
        match value.char_indices().nth(limit) {
            Some((index, _)) => Some(value[..index].to_owned()),
            None => Some(value.to_owned()),
        }
    }
    let mut extra = serde_json::Map::new();
    for (key, value) in [
        ("message", clipped(&Some(ev.message.clone()), 200)),
        ("title", clipped(&ev.title, 120)),
        ("vcodec", clipped(&ev.vcodec, 16)),
        ("src", clipped(&ev.src, 160)),
    ] {
        if let Some(value) = value {
            extra.insert(key.to_owned(), value.into());
        }
    }
    if let Some(code) = ev.code {
        extra.insert("code".into(), code.into());
    }
    if let Some(value) = ev.decode_hw {
        extra.insert("decode_hw".into(), value.into());
    }
    if let Some(value) = ev.decode_smooth {
        extra.insert("decode_smooth".into(), value.into());
    }
    PlaybackEvent {
        at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0),
        user_id: Some(user_id),
        session_id: clipped(&ev.session_id, 80),
        file_id: ev.file_id,
        event: clipped(&Some(ev.event.clone()), 40).unwrap_or_else(|| "event".to_owned()),
        level: clipped(&Some(ev.level.clone()), 16),
        method: clipped(&ev.method, 16),
        encoder: clipped(&ev.encoder, 32),
        height: ev.height.filter(|value| *value > 0),
        ms: ev.ms.filter(|value| *value >= 0),
        runway_ds: ev
            .runway
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| (value * 10.0).round().min(i64::MAX as f64) as i64),
        bandwidth_kbps: ev.bandwidth.filter(|value| *value > 0),
        detail: clipped(&ev.detail, 200),
        attempt: clipped(&ev.attempt, 24),
        reason: clipped(&ev.reason, 24),
        ua: clipped(&ev.ua, 24),
        extra: (!extra.is_empty()).then(|| serde_json::Value::Object(extra).to_string()),
        ..PlaybackEvent::default()
    }
}

/// One client report as a log line.
///
/// Pure, and separate from the handler for the reason every other pure
/// decision in this codebase is: the thing worth checking here is the *naming*
/// of the measurements, and naming mistakes read perfectly and produce wrong
/// data. This one printed every event's `ms` as `ttff_ms` — so a stall's
/// duration entered the start-time series, and a grep that looked exactly
/// right returned numbers that were part start and part stall.
fn client_log_line(ev: &ClientLog, suppressed: u64) -> String {
    /// Trim and cap one field so a client can't spam oversized log lines.
    fn clip(s: &str, n: usize) -> String {
        let s = s.trim();
        match s.char_indices().nth(n) {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s.to_owned(),
        }
    }
    fn field(v: &Option<String>, n: usize) -> Option<String> {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip(s, n))
    }

    let event = {
        let e = clip(&ev.event, 40);
        if e.is_empty() {
            "event".to_owned()
        } else {
            e
        }
    };
    let mut line = match field(&ev.ua, 24) {
        Some(ua) => format!("client[{ua}] {event}"),
        None => format!("client {event}"),
    };
    if let Some(m) = field(&ev.method, 16) {
        line.push_str(&format!(" method={m}"));
    }
    if let Some(v) = field(&ev.vcodec, 16) {
        line.push_str(&format!(" vcodec={v}"));
    }
    if let Some(c) = ev.code {
        line.push_str(&format!(" code={c}"));
    }
    if let Some(h) = ev.height.filter(|h| *h > 0) {
        line.push_str(&format!(" height={h}"));
    }
    if let Some(e) = field(&ev.encoder, 32) {
        line.push_str(&format!(" encoder={e}"));
    }
    // Printed before the message so it sits with the other facts about the
    // client rather than in the trailing measurements: it describes the
    // machine, not this event.
    if let Some(hw) = ev.decode_hw {
        line.push_str(if hw { " decode=hw" } else { " decode=SOFTWARE" });
        if ev.decode_smooth == Some(false) {
            line.push_str("/not-smooth");
        }
    }
    let msg = clip(&ev.message, 200);
    if !msg.is_empty() {
        line.push_str(&format!(": {msg}"));
    }
    // The measurements, in a fixed order so a grep over the log ring produces
    // a column-alignable series rather than prose.
    //
    // `ms` is named for what the EVENT measured, not for the field it arrived
    // in. It used to be printed as `ttff_ms=` on every event that carried one,
    // which meant a stall's *duration* was logged as a time-to-first-frame —
    // so `grep ttff_ms` returned a distribution that was part start times and
    // part stall lengths, and the tail of it was entirely stalls. That is the
    // one number M0 exists to produce, and it was wrong in the direction that
    // makes the system look worse than it is.
    if let Some(ms) = ev.ms.filter(|v| *v >= 0) {
        let name = match ev.event.as_str() {
            "ttff" => "ttff_ms",
            "stall" => "stall_ms",
            _ => "ms",
        };
        line.push_str(&format!(" {name}={ms}"));
    }
    if let Some(r) = ev.runway.filter(|v| v.is_finite() && *v >= 0.0) {
        line.push_str(&format!(" runway={r:.1}s"));
    }
    if let Some(b) = ev.bandwidth.filter(|v| *v > 0) {
        line.push_str(&format!(" bw={b}kbps"));
    }
    if let Some(t) = field(&ev.title, 120) {
        line.push_str(&format!(" — {t}"));
    }
    if let Some(id) = ev.file_id {
        line.push_str(&format!(" file={id}"));
    }
    if let Some(s) = field(&ev.src, 160) {
        line.push_str(&format!(" src={s}"));
    }
    if let Some(d) = field(&ev.detail, 200) {
        line.push_str(&format!(" [{d}]"));
    }
    // Attempt identity last: it's what you group by when reading back, and
    // putting it at the end keeps the front of every line comparable.
    match (field(&ev.attempt, 24), field(&ev.reason, 24)) {
        (Some(a), Some(r)) => line.push_str(&format!(" attempt={a}/{r}")),
        (Some(a), None) => line.push_str(&format!(" attempt={a}")),
        (None, Some(r)) => line.push_str(&format!(" reason={r}")),
        (None, None) => {}
    }
    if suppressed > 0 {
        line.push_str(&format!(" (+{suppressed} suppressed)"));
    }
    line
}

#[derive(Serialize)]
pub struct SettingsDto {
    pub tmdb_configured: bool,
    /// The stored TMDB key itself. This endpoint is admin-only and the key is
    /// low-sensitivity (read-only metadata), so the admin who set it can see
    /// and copy it back — the web UI masks it until clicked. Empty when unset.
    pub tmdb_api_key: String,
    pub omdb_configured: bool,
    /// The stored OMDb key (Rotten Tomatoes / Metacritic / IMDb ratings). Same
    /// admin-only, mask-until-clicked treatment as the TMDB key.
    pub omdb_api_key: String,
    /// Trakt app credentials (the admin's own API app), same treatment.
    pub trakt_configured: bool,
    pub trakt_client_id: String,
    pub trakt_client_secret: String,
    /// Where monarr lives, and the key plurxd reads its calendar with — the
    /// coming-soon rail (plan §11.2). Server-side only: plurxd proxies the
    /// call, so this key never reaches a browser. Same admin-only,
    /// mask-until-clicked treatment as the others.
    pub monarr_configured: bool,
    pub monarr_url: String,
    pub monarr_api_key: String,
    /// Push watch state to monarr. Off by default, and separate from the
    /// pair above on purpose: reading monarr's calendar and sending it your
    /// household's viewing history are very different consents.
    pub monarr_watched_sync: bool,
    /// Playback language defaults (docs/FEATURES.md §7): ISO 639 codes and the
    /// subtitle mode "auto" | "always" | "off".
    pub default_audio_lang: String,
    pub default_sub_lang: String,
    pub sub_mode: String,
    /// How fast a remux may be delivered, as a multiple of real time. "0" means
    /// unpaced — which lets a single stream take the whole link.
    pub stream_readrate: String,
    /// Requested N1 rate control. The production-effective value may be VBR
    /// when a family refuses quality mode; `/system` capabilities and boot
    /// logs carry that validation result.
    pub transcode_rate_mode: String,
    /// `None` means use the validated family-tuned default.
    pub transcode_quality: Option<u8>,
    /// How an HLS session (transcode or copy-video) is paced: the multiple of
    /// real time it settles at, how many seconds it may deliver flat-out first
    /// (that burst IS the viewer's opening buffer), and how far ahead of the
    /// playhead it may write before being suspended.
    pub hls_readrate: String,
    pub hls_burst_secs: String,
    pub hls_ahead_max_secs: String,
    /// The ahead-window's other two bounds, in bytes: one session's share and
    /// the ceiling across all of them. Safety limits rather than tuning knobs
    /// — they have no dropdown, because the answer is "big enough that you
    /// never meet it, small enough that a runaway stream cannot fill the
    /// disk" and that is a property of the disk, not a preference.
    pub hls_ahead_max_bytes: String,
    pub hls_scratch_max_bytes: String,
    /// Opt-in physical-device experiment: serve new live sessions as typeless
    /// sliding playlists from their first response. Off by default.
    pub hls_typeless_sliding: bool,
    /// Server-wide scheduled maintenance, in minutes; 0 is off (the default).
    /// Per-library scan/refresh intervals are on the library, not here.
    pub probe_retry_mins: i64,
    /// The exception to "0 is the default": on by default, because it repairs
    /// artwork the server itself failed to fetch rather than adding a habit
    /// nobody asked for. See `keys::ARTWORK_RETRY_DEFAULT_MINS`.
    pub artwork_retry_mins: i64,
    pub transcode_cleanup_mins: i64,
    /// How often the pre-transcode producer looks for something worth making,
    /// and how much disk what it makes may occupy. Both off/0 by default: the
    /// producer competes with live playback for the encoder, so it is a thing
    /// an operator turns on, not a thing an upgrade turns on for them.
    pub cache_produce_mins: i64,
    pub cache_max_gb: i64,
    /// What the cache currently holds on this node, in bytes — the number that
    /// makes the budget above mean something.
    pub cache_used_bytes: i64,
    /// Node-local playback event retention. Default 30 days; 0 is fully off.
    pub telemetry_retain_days: i64,
    /// Use coarse, node-local playback history to seed Auto quality.
    /// Explicit opt-in; missing is false.
    pub playback_network_priors: bool,
    /// App-managed offline preparation has a separate reservation budget from
    /// the opportunistic playback cache above.
    pub offline_enabled: bool,
    pub offline_max_gb: i64,
    pub offline_max_gb_per_user: i64,
    pub offline_max_rows_per_user: i64,
    /// Scan every library once, ~30s after the server starts.
    pub scan_on_startup: bool,
    /// Is the one-off genre backfill armed? It disarms itself when it reaches
    /// the end of the catalogue, so this reads `false` again afterwards.
    ///
    /// Off by default and opt-in, unlike the artwork retry: it re-hits the
    /// provider once per title because nothing stored can produce a genre
    /// (see migration v13), and an upgrade that started that on its own is
    /// the failure v9 documents.
    pub genre_backfill: bool,
    /// What the last backfill pass did, or `None` if none has run since boot.
    /// Reported here rather than in the per-library scan status because the
    /// backfill walks item ids, not libraries — and because this page is
    /// where an operator armed it, so it is where they will look for whether
    /// it worked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre_backfill_last: Option<GenreBackfillReport>,
}

async fn settings_dto(state: &AppState) -> Result<SettingsDto, ApiError> {
    let tmdb_api_key = state
        .store
        .get_setting(keys::TMDB_API_KEY)
        .await?
        .unwrap_or_default();
    let omdb_api_key = state
        .store
        .get_setting(keys::OMDB_API_KEY)
        .await?
        .unwrap_or_default();
    let monarr_url = state
        .store
        .get_setting(keys::MONARR_URL)
        .await?
        .unwrap_or_default();
    let monarr_api_key = state
        .store
        .get_setting(keys::MONARR_API_KEY)
        .await?
        .unwrap_or_default();
    let trakt_client_id = state
        .store
        .get_setting(keys::TRAKT_CLIENT_ID)
        .await?
        .unwrap_or_default();
    let trakt_client_secret = state
        .store
        .get_setting(keys::TRAKT_CLIENT_SECRET)
        .await?
        .unwrap_or_default();
    let prefs = state.transcode.lang_prefs().await;
    let stream_readrate = state
        .store
        .get_setting(keys::STREAM_READRATE)
        .await?
        .unwrap_or_else(|| crate::http::stream::READRATE_DEFAULT.to_string());
    let (transcode_rate_mode, transcode_quality) = state
        .store
        .get_setting_pair(keys::TRANSCODE_RATE_MODE, keys::TRANSCODE_QUALITY)
        .await?;
    let (transcode_rate_mode, transcode_quality, _) =
        crate::transcode::normalize_rate_control_request(
            transcode_rate_mode.as_deref(),
            transcode_quality.as_deref(),
        );
    let transcode_rate_mode = transcode_rate_mode.as_str().to_owned();
    let text = |v: Option<String>, default: &str| -> String {
        v.map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_owned())
    };
    let hls_readrate = text(
        state.store.get_setting(keys::HLS_READRATE).await?,
        &crate::transcode::HLS_READRATE_DEFAULT.to_string(),
    );
    let hls_burst_secs = text(
        state.store.get_setting(keys::HLS_BURST_SECS).await?,
        &crate::transcode::HLS_BURST_SECS_DEFAULT.to_string(),
    );
    let hls_ahead_max_secs = text(
        state.store.get_setting(keys::HLS_AHEAD_MAX_SECS).await?,
        &crate::transcode::HLS_AHEAD_MAX_SECS_DEFAULT.to_string(),
    );
    let hls_ahead_max_bytes = text(
        state.store.get_setting(keys::HLS_AHEAD_MAX_BYTES).await?,
        &crate::transcode::HLS_AHEAD_MAX_BYTES_DEFAULT.to_string(),
    );
    let hls_scratch_max_bytes = text(
        state.store.get_setting(keys::HLS_SCRATCH_MAX_BYTES).await?,
        &crate::transcode::HLS_SCRATCH_MAX_BYTES_DEFAULT.to_string(),
    );
    let mins = |v: Option<String>| -> i64 {
        v.and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    };
    let probe_retry_mins = mins(state.store.get_setting(keys::JOB_PROBE_RETRY_MINS).await?);
    // Absent means the default here, not 0 — the settings page must show the
    // interval that is actually in force, or an admin reading "0" would
    // reasonably conclude nothing is retrying their artwork.
    let artwork_retry_mins = state
        .store
        .get_setting(keys::JOB_ARTWORK_RETRY_MINS)
        .await?
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(keys::ARTWORK_RETRY_DEFAULT_MINS)
        .max(0);
    let transcode_cleanup_mins = mins(
        state
            .store
            .get_setting(keys::JOB_TRANSCODE_CLEANUP_MINS)
            .await?,
    );
    let cache_produce_mins = mins(
        state
            .store
            .get_setting(keys::JOB_CACHE_PRODUCE_MINS)
            .await?,
    );
    let cache_max_gb = state
        .store
        .get_setting(keys::CACHE_MAX_GB)
        .await?
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(crate::cachekeep::DEFAULT_MAX_GB)
        .max(0);
    let cache_used_bytes = match state.transcode.cache_location() {
        Some((_, node)) => state.store.cache_bytes(node).await.unwrap_or(0),
        None => 0,
    };
    let telemetry_retain_days = state
        .store
        .get_setting(keys::TELEMETRY_RETAIN_DAYS)
        .await?
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(keys::TELEMETRY_RETAIN_DEFAULT_DAYS)
        .max(0);
    let playback_network_priors = state
        .store
        .get_setting(keys::PLAYBACK_NETWORK_PRIORS)
        .await?
        .is_some_and(|value| value.trim() == "1");
    let offline_enabled = !matches!(
        state
            .store
            .get_setting(keys::OFFLINE_ENABLED)
            .await?
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    );
    let offline_integer = |value: Option<String>, default: i64| {
        value
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(default)
            .max(0)
    };
    let offline_max_gb = offline_integer(
        state.store.get_setting(keys::OFFLINE_MAX_GB).await?,
        super::offline::DEFAULT_GLOBAL_GB,
    );
    let offline_max_gb_per_user = offline_integer(
        state
            .store
            .get_setting(keys::OFFLINE_MAX_GB_PER_USER)
            .await?,
        super::offline::DEFAULT_USER_GB,
    );
    let offline_max_rows_per_user = offline_integer(
        state
            .store
            .get_setting(keys::OFFLINE_MAX_ROWS_PER_USER)
            .await?,
        super::offline::DEFAULT_USER_ROWS,
    );
    let scan_on_startup = state
        .store
        .get_setting(keys::JOB_SCAN_ON_STARTUP)
        .await?
        .is_some_and(|v| v.trim() == "1");
    let genre_backfill = state
        .store
        .get_setting(keys::GENRE_BACKFILL)
        .await?
        .is_some_and(|v| v.trim() == "1");
    Ok(SettingsDto {
        tmdb_configured: !tmdb_api_key.is_empty(),
        tmdb_api_key,
        omdb_configured: !omdb_api_key.is_empty(),
        omdb_api_key,
        monarr_configured: !monarr_url.is_empty() && !monarr_api_key.is_empty(),
        monarr_url,
        monarr_api_key,
        monarr_watched_sync: state
            .store
            .get_setting(keys::MONARR_WATCHED_SYNC)
            .await?
            .unwrap_or_default()
            == "1",
        trakt_configured: !trakt_client_id.is_empty() && !trakt_client_secret.is_empty(),
        trakt_client_id,
        trakt_client_secret,
        default_audio_lang: prefs.audio_lang,
        default_sub_lang: prefs.sub_lang,
        sub_mode: prefs.sub_mode.as_str().to_owned(),
        stream_readrate,
        transcode_rate_mode,
        transcode_quality,
        hls_readrate,
        hls_burst_secs,
        hls_ahead_max_secs,
        hls_ahead_max_bytes,
        hls_scratch_max_bytes,
        hls_typeless_sliding: state
            .store
            .get_setting(keys::HLS_TYPELESS_SLIDING)
            .await?
            .is_some_and(|value| value.trim() == "1"),
        probe_retry_mins,
        artwork_retry_mins,
        transcode_cleanup_mins,
        cache_produce_mins,
        cache_max_gb,
        cache_used_bytes,
        telemetry_retain_days,
        playback_network_priors,
        offline_enabled,
        offline_max_gb,
        offline_max_gb_per_user,
        offline_max_rows_per_user,
        scan_on_startup,
        genre_backfill,
        genre_backfill_last: state.jobs.last_genre_backfill().await,
    })
}

/// GET /api/v1/settings (admin)
pub async fn get_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<SettingsDto>, ApiError> {
    Ok(Json(settings_dto(&state).await?))
}

#[derive(Deserialize)]
pub struct UpdateSettings {
    /// Set the TMDB API key. Empty string clears it. Absent leaves it as-is.
    pub tmdb_api_key: Option<String>,
    /// Set the OMDb API key. Empty string clears it. Absent leaves it as-is.
    pub omdb_api_key: Option<String>,
    /// Trakt app credentials; same empty-clears semantics.
    pub trakt_client_id: Option<String>,
    pub trakt_client_secret: Option<String>,
    /// monarr pairing for the coming-soon rail; same empty-clears semantics.
    pub monarr_url: Option<String>,
    pub monarr_api_key: Option<String>,
    pub monarr_watched_sync: Option<bool>,
    /// Playback language defaults. ISO 639 codes ("eng"); mode is
    /// "auto" | "always" | "off".
    pub default_audio_lang: Option<String>,
    pub default_sub_lang: Option<String>,
    pub sub_mode: Option<String>,
    /// Remux delivery pace, a multiple of real time; "0" disables the limit.
    pub stream_readrate: Option<String>,
    /// N1 requested rate-control family. Whenever either rate-control field is
    /// sent, both are required so a replicated update is one complete pair.
    /// A quality request is behavior-probed before the effective snapshot
    /// changes; a refused driver remains VBR.
    pub transcode_rate_mode: Option<String>,
    /// JSON null clears the override back to the family-tuned default.
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub transcode_quality: Option<Option<u8>>,
    /// HLS session pacing: rate (x real time, "0" unpaced), opening burst in
    /// seconds, and the ahead-of-playhead window in seconds ("0" unbounded).
    pub hls_readrate: Option<String>,
    pub hls_burst_secs: Option<String>,
    pub hls_ahead_max_secs: Option<String>,
    pub hls_ahead_max_bytes: Option<String>,
    pub hls_scratch_max_bytes: Option<String>,
    pub hls_typeless_sliding: Option<bool>,
    /// Server-wide job intervals in minutes; 0 turns one off.
    pub probe_retry_mins: Option<i64>,
    pub artwork_retry_mins: Option<i64>,
    pub transcode_cleanup_mins: Option<i64>,
    pub cache_produce_mins: Option<i64>,
    pub cache_max_gb: Option<i64>,
    pub telemetry_retain_days: Option<i64>,
    pub playback_network_priors: Option<bool>,
    pub offline_enabled: Option<bool>,
    pub offline_max_gb: Option<i64>,
    pub offline_max_gb_per_user: Option<i64>,
    pub offline_max_rows_per_user: Option<i64>,
    pub scan_on_startup: Option<bool>,
    /// Arm or disarm the one-off genre backfill.
    pub genre_backfill: Option<bool>,
}

/// Preserve the distinction between an absent PATCH-style field and an
/// explicit JSON null. Serde's ordinary `Option<Option<T>>` collapses both;
/// the harness needs null to restore an originally-unset quality override.
fn deserialize_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// PUT /api/v1/settings (admin)
pub async fn update_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsDto>, ApiError> {
    match (&req.transcode_rate_mode, req.transcode_quality) {
        (None, None) => {}
        (Some(requested_mode), Some(quality)) => {
            let mode = plurx_core::transcode::RateMode::parse(requested_mode).ok_or_else(|| {
                ApiError::BadRequest("transcode_rate_mode must be bitrate or quality".into())
            })?;
            match state
                .transcode
                .apply_rate_control_settings(mode, quality)
                .await
            {
                Ok(()) => {}
                Err(crate::transcode::ApplyRateControlError::Store(error)) => {
                    return Err(error.into())
                }
                Err(crate::transcode::ApplyRateControlError::Busy) => {
                    return Err(ApiError::Conflict(
                        "rate-control validation deferred while playback, offline/speculative encoding, or encoder capacity is active; retry after the node is idle".into(),
                    ))
                }
            }
        }
        _ => {
            return Err(ApiError::BadRequest(
                "transcode_rate_mode and transcode_quality must be provided together".into(),
            ));
        }
    }
    let pairs: [(&str, &Option<String>); 8] = [
        (keys::TMDB_API_KEY, &req.tmdb_api_key),
        (keys::OMDB_API_KEY, &req.omdb_api_key),
        (keys::TRAKT_CLIENT_ID, &req.trakt_client_id),
        (keys::TRAKT_CLIENT_SECRET, &req.trakt_client_secret),
        (keys::MONARR_URL, &req.monarr_url),
        (keys::MONARR_API_KEY, &req.monarr_api_key),
        (keys::AUDIO_LANG, &req.default_audio_lang),
        (keys::SUB_LANG, &req.default_sub_lang),
    ];
    for (key, value) in pairs {
        if let Some(value) = value {
            // The monarr URL is canonicalized on the way in, not at each use
            // site: a bare `monarr:7676` or `host.docker.internal` is what a
            // person types, and reqwest answers a schemeless address with
            // "builder error" — a message about our HTTP client rather than
            // about their setting. Storing the completed form means every
            // consumer agrees on it and the settings screen shows back
            // exactly what plurx will dial.
            let value = if key == keys::MONARR_URL {
                super::comingsoon::normalize_monarr_url(value)
            } else {
                value.trim().to_owned()
            };
            state.store.put_setting(key, &value).await?;
        }
    }
    if let Some(on) = req.monarr_watched_sync {
        state
            .store
            .put_setting(keys::MONARR_WATCHED_SYNC, if on { "1" } else { "0" })
            .await?;
    }
    if let Some(on) = req.hls_typeless_sliding {
        state
            .store
            .put_setting(keys::HLS_TYPELESS_SLIDING, if on { "1" } else { "0" })
            .await?;
    }
    if let Some(mode) = &req.sub_mode {
        // Normalize through the parser so only valid modes are stored.
        let mode = plurx_core::tracks::SubMode::parse(mode.trim()).as_str();
        state.store.put_setting(keys::SUB_MODE, mode).await?;
    }
    if let Some(rate) = &req.stream_readrate {
        // Store only something the streamer can act on. A garbled value would
        // otherwise fall back to the default silently and leave the settings
        // page showing a number that isn't in force.
        let parsed: f64 = rate
            .trim()
            .parse()
            .map_err(|_| ApiError::BadRequest("stream_readrate must be a number".into()))?;
        if !(0.0..=1000.0).contains(&parsed) {
            return Err(ApiError::BadRequest(
                "stream_readrate must be between 0 and 1000".into(),
            ));
        }
        // Below real time the client can never buffer and playback stalls by
        // construction; refuse rather than let someone quietly break streaming.
        if parsed > 0.0 && parsed < 1.0 {
            return Err(ApiError::BadRequest(
                "stream_readrate below 1.0 cannot keep up with playback; use 0 to disable pacing"
                    .into(),
            ));
        }
        state
            .store
            .put_setting(keys::STREAM_READRATE, &parsed.to_string())
            .await?;
    }
    // HLS pacing. Same "store only what the streamer can act on" rule as the
    // remux rate, with per-key bounds: a rate below real time cannot keep up
    // by construction, a burst is seconds of content (an hour of it is not a
    // burst), and the ahead-window is what stops a 4K session filling the
    // disk — an enormous one is the same as none, so say so rather than
    // silently accept it.
    for (key, label, value, max) in [
        (
            keys::HLS_READRATE,
            "hls_readrate",
            &req.hls_readrate,
            1000.0,
        ),
        (
            keys::HLS_BURST_SECS,
            "hls_burst_secs",
            &req.hls_burst_secs,
            600.0,
        ),
        (
            keys::HLS_AHEAD_MAX_SECS,
            "hls_ahead_max_secs",
            &req.hls_ahead_max_secs,
            3600.0,
        ),
        // Bytes. The ceiling is generous on purpose: this is a guard against a
        // runaway stream, not a quota, and refusing a large disk would be the
        // setting telling the operator they are wrong about their own hardware.
        (
            keys::HLS_AHEAD_MAX_BYTES,
            "hls_ahead_max_bytes",
            &req.hls_ahead_max_bytes,
            1024.0 * 1024.0 * 1024.0 * 1024.0,
        ),
        (
            keys::HLS_SCRATCH_MAX_BYTES,
            "hls_scratch_max_bytes",
            &req.hls_scratch_max_bytes,
            1024.0 * 1024.0 * 1024.0 * 1024.0,
        ),
    ] {
        let Some(raw) = value else { continue };
        let parsed: f64 = raw
            .trim()
            .parse()
            .map_err(|_| ApiError::BadRequest(format!("{label} must be a number")))?;
        if !(0.0..=max).contains(&parsed) {
            return Err(ApiError::BadRequest(format!(
                "{label} must be between 0 and {max:.0}"
            )));
        }
        if key == keys::HLS_READRATE && parsed > 0.0 && parsed < 1.0 {
            return Err(ApiError::BadRequest(
                "hls_readrate below 1.0 cannot keep up with playback; use 0 to disable pacing"
                    .into(),
            ));
        }
        state.store.put_setting(key, &parsed.to_string()).await?;
    }
    // Job intervals: 0 = off, otherwise a floor of 15 minutes, matching the
    // per-library schedule. The scheduler ticks once a minute, so anything
    // shorter would be a lie dressed as a setting.
    for (key, label, value) in [
        (
            keys::JOB_PROBE_RETRY_MINS,
            "probe_retry_mins",
            req.probe_retry_mins,
        ),
        (
            keys::JOB_ARTWORK_RETRY_MINS,
            "artwork_retry_mins",
            req.artwork_retry_mins,
        ),
        (
            keys::JOB_TRANSCODE_CLEANUP_MINS,
            "transcode_cleanup_mins",
            req.transcode_cleanup_mins,
        ),
        (
            keys::JOB_CACHE_PRODUCE_MINS,
            "cache_produce_mins",
            req.cache_produce_mins,
        ),
    ] {
        if let Some(value) = value {
            if value < 0 || (value > 0 && value < 15) {
                return Err(ApiError::BadRequest(format!(
                    "{label} must be 0 (off) or at least 15 minutes"
                )));
            }
            state.store.put_setting(key, &value.to_string()).await?;
        }
    }
    if let Some(gb) = req.cache_max_gb {
        // Ten terabytes is not a policy so much as a typo guard: the field is
        // in gigabytes, and somebody entering bytes would set a budget no disk
        // can reach — which reads as eviction being broken.
        if !(0..=10_240).contains(&gb) {
            return Err(ApiError::BadRequest(
                "cache_max_gb must be between 0 (off) and 10240".into(),
            ));
        }
        state
            .store
            .put_setting(keys::CACHE_MAX_GB, &gb.to_string())
            .await?;
    }
    if let Some(days) = req.telemetry_retain_days {
        if !(0..=3650).contains(&days) {
            return Err(ApiError::BadRequest(
                "telemetry_retain_days must be between 0 (off) and 3650".into(),
            ));
        }
        state
            .store
            .put_setting(keys::TELEMETRY_RETAIN_DAYS, &days.to_string())
            .await?;
    }
    if let Some(enabled) = req.playback_network_priors {
        state
            .store
            .put_setting(
                keys::PLAYBACK_NETWORK_PRIORS,
                if enabled { "1" } else { "0" },
            )
            .await?;
    }
    for (key, label, value) in [
        (keys::OFFLINE_MAX_GB, "offline_max_gb", req.offline_max_gb),
        (
            keys::OFFLINE_MAX_GB_PER_USER,
            "offline_max_gb_per_user",
            req.offline_max_gb_per_user,
        ),
    ] {
        if let Some(gb) = value {
            if !(0..=10_240).contains(&gb) {
                return Err(ApiError::BadRequest(format!(
                    "{label} must be between 0 (disables offline admission) and 10240"
                )));
            }
            state.store.put_setting(key, &gb.to_string()).await?;
        }
    }
    if let Some(rows) = req.offline_max_rows_per_user {
        if !(0..=10_000).contains(&rows) {
            return Err(ApiError::BadRequest(
                "offline_max_rows_per_user must be between 0 (disables offline admission) and 10000"
                    .into(),
            ));
        }
        state
            .store
            .put_setting(keys::OFFLINE_MAX_ROWS_PER_USER, &rows.to_string())
            .await?;
    }
    if let Some(on) = req.offline_enabled {
        state
            .store
            .put_setting(keys::OFFLINE_ENABLED, if on { "1" } else { "0" })
            .await?;
        if !on {
            state.offline.cancel_all().await;
        }
    }
    if let Some(on) = req.scan_on_startup {
        state
            .store
            .put_setting(keys::JOB_SCAN_ON_STARTUP, if on { "1" } else { "0" })
            .await?;
    }
    if let Some(on) = req.genre_backfill {
        // Arming rewinds the cursor. A pass that finished left it at 0 and
        // disarmed itself, so this normally changes nothing; it matters for
        // the operator who disarms a run half way and arms it again later,
        // expecting the titles it already failed on to be retried rather than
        // skipped forever because the cursor is past them.
        if on {
            state
                .store
                .put_setting(keys::GENRE_BACKFILL_CURSOR, "0")
                .await?;
        }
        state
            .store
            .put_setting(keys::GENRE_BACKFILL, if on { "1" } else { "0" })
            .await?;
        tracing::info!(armed = on, "genre backfill");
    }
    Ok(Json(settings_dto(&state).await?))
}

/// GET /api/v1/scan/status — per-library scan status (keyed by library id).
/// Any authenticated user may look; scans aren't a secret, but strangers
/// shouldn't see filesystem paths in problem messages.
pub async fn scan_status(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Json<HashMap<i64, ScanStatus>> {
    Json(state.jobs.all_statuses().await)
}

/// One thing the server is doing right now. Deliberately generic — future
/// task kinds (file moves, renames, backups) reuse the same shape and the
/// same global indicator in every client.
#[derive(Serialize)]
pub struct Activity {
    /// Machine-readable kind: scan, enrich, stream, or offline work.
    pub kind: &'static str,
    /// Short human label, e.g. "Scanning Movies".
    pub label: String,
    /// Optional detail, e.g. "412 of 3801 files".
    pub detail: Option<String>,
    /// 0–100 when a meaningful percentage exists.
    pub percent: Option<u8>,
}

#[derive(Clone, Serialize)]
struct OfflineWork {
    id: String,
    /// `prepare` or `send`; neither is a playback session.
    kind: &'static str,
    user: String,
    file_id: i64,
    item_id: Option<i64>,
    title: String,
    state: String,
    phase: String,
    target_height: i64,
    percent: Option<u8>,
    bytes_sent: Option<u64>,
    bytes_total: i64,
    started_unix: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn offline_work(state: &AppState) -> Result<Vec<OfflineWork>, ApiError> {
    let now = now_unix();
    let rows = state
        .store
        // Lease touches are deliberately throttled to one SQLite write per
        // minute. Fetch a 65-second candidate window, then use the exact
        // process-local response meter below to decide whether it is sending
        // now; this preserves the write bound without a false 40-second gap.
        .offline_activity_packages(&state.node_id, now, now.saturating_sub(65), 50)
        .await?;
    let users: HashMap<i64, String> = state
        .store
        .list_users()
        .await?
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect();
    let mut work = Vec::with_capacity(rows.len());
    for row in rows {
        let package = row.package;
        let transfer_bytes = row
            .lease_active
            .then(|| state.offline.transfer_bytes(&package.id))
            .flatten();
        if package.state == "ready" && transfer_bytes.is_none() {
            continue;
        }
        let file = state.store.get_file(package.file_id).await?;
        let item_id = file.as_ref().map(|file| file.item_id);
        let title = match item_id {
            Some(item_id) => state
                .store
                .get_item(item_id)
                .await?
                .map(|item| item.title)
                .unwrap_or_else(|| "Unavailable media".to_owned()),
            None => "Unavailable media".to_owned(),
        };
        work.push(OfflineWork {
            id: package.id.clone(),
            kind: if transfer_bytes.is_some() {
                "send"
            } else {
                "prepare"
            },
            user: users
                .get(&package.user_id)
                .cloned()
                .unwrap_or_else(|| "Unknown profile".to_owned()),
            file_id: package.file_id,
            item_id,
            title,
            state: package.state,
            phase: package.phase,
            target_height: package.target_height,
            percent: (package.progress_millis > 0)
                .then_some(((package.progress_millis.clamp(0, 1_000) * 100) / 1_000) as u8),
            bytes_sent: transfer_bytes,
            bytes_total: package.actual_bytes.unwrap_or(package.estimated_bytes),
            started_unix: package.created_at,
        });
    }
    Ok(work)
}

fn activity_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MB", bytes.div_ceil(MIB))
    }
}

/// GET /api/v1/activity — everything in flight, for the always-visible
/// indicator in the app header. Empty array = the server is idle.
pub async fn activity(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Activity>>, ApiError> {
    let mut activities = Vec::new();

    let names: HashMap<i64, String> = state
        .store
        .list_libraries()
        .await?
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let mut statuses: Vec<_> = state
        .jobs
        .all_statuses()
        .await
        .into_iter()
        .filter(|(_, s)| s.running)
        .collect();
    statuses.sort_by_key(|(id, _)| *id);
    for (id, status) in statuses {
        let name = names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
        let enriching = status.phase.as_deref() == Some("enriching");
        let (kind, label) = if enriching {
            ("enrich", format!("Fetching metadata for {name}"))
        } else {
            ("scan", format!("Scanning {name}"))
        };
        let (detail, percent) = match status.progress.filter(|_| !enriching) {
            Some(p) if p.found > 0 => (
                Some(format!("{} of {} files", p.processed, p.found)),
                Some(((p.processed * 100 / p.found).min(100)) as u8),
            ),
            _ => (None, None),
        };
        activities.push(Activity {
            kind,
            label,
            detail,
            percent,
        });
    }

    let streams = state.transcode.active_sessions().await;
    if streams > 0 {
        activities.push(Activity {
            kind: "stream",
            label: if streams == 1 {
                "1 active stream".to_owned()
            } else {
                format!("{streams} active streams")
            },
            detail: None,
            percent: None,
        });
    }

    // The pre-transcode pass. It is the only background job that holds an
    // encoder for hours, and it was the only one that never said so: an admin
    // who found a busy ffmpeg had no way, from inside plurx, to learn what it
    // was or why it had chosen that file.
    if let Some(p) = state.jobs.producing_now().await {
        activities.push(Activity {
            kind: "produce",
            label: format!("Pre-transcoding {}", p.title),
            detail: Some(format!("{} · {} of {}", p.reason, p.index, p.total)),
            // Titles done, not percent of this encode — the encoder does not
            // report a percentage and inventing one would be a lie that reads
            // like a measurement.
            percent: None,
        });
    }

    for work in offline_work(&state).await? {
        let sending = work.kind == "send";
        activities.push(Activity {
            kind: if sending {
                "offline_send"
            } else {
                "offline_prepare"
            },
            label: if sending {
                format!("Sending offline · {}", work.title)
            } else {
                format!("Preparing offline · {}", work.title)
            },
            detail: if sending {
                Some(format!(
                    "{} / {}",
                    activity_bytes(work.bytes_sent.unwrap_or(0)),
                    activity_bytes(work.bytes_total.max(0) as u64)
                ))
            } else if work.state == "queued" {
                Some(format!("Waiting for encoder · {}p", work.target_height))
            } else {
                Some(format!(
                    "{} · {}p",
                    work.phase.replace('_', " "),
                    work.target_height
                ))
            },
            percent: (!sending).then_some(work.percent).flatten(),
        });
    }

    if let Some((label, detail)) = state.trakt.activity().await {
        activities.push(Activity {
            kind: "trakt",
            label,
            detail,
            percent: None,
        });
    }

    Ok(Json(activities))
}

/// One live delivery, whatever route it takes to the screen.
///
/// A new array rather than fields grafted onto `sessions`: that array means
/// "HLS sessions" to every native client parsing it today, and two of the four
/// rows here are not sessions at all.
#[derive(Serialize)]
pub struct Delivery {
    /// `direct` · `remux` · `hls-copy` · `transcode`.
    pub method: &'static str,
    /// Who is watching. Exactly what `sessions[].user_name` has always
    /// carried on this endpoint — see the handler's note on who may look. This
    /// array names no one a `sessions` row would not have named.
    pub user: String,
    pub file_id: i64,
    pub item_id: i64,
    pub title: String,
    pub started_unix: i64,
    /// Seconds since this delivery last showed a sign of life: `last_access`
    /// for a session, the last byte handed over for a remux, and for a direct
    /// play the last range request or progress beacon — which is the clock the
    /// idle expiry runs on.
    pub idle_seconds: u64,
    /// The `sessions` row this is the same stream as, where one exists, so a
    /// page can show one line per viewer and still reach the encoder detail.
    pub session_id: Option<String>,
    /// Bytes handed to this client, where anything counts them. Direct play
    /// counts nothing: `serve_file_range` hands a file to axum and never sees
    /// the bytes leave, and metering it would mean wrapping every 206 body.
    pub delivered_bytes: Option<i64>,
    pub delivered_bps: Option<i64>,
}

/// Everything currently being delivered, from the three places that know.
///
/// Deliberately a join at read time rather than one registry. The two session
/// registries own exact lifetimes — a reap, a `StreamGuard` drop — and their
/// locks disagree by necessity: `TranscodeManager` uses an async mutex because
/// its holders await, `progressive::Streams` a sync one because a `Drop`
/// deregisters and a `Drop` cannot await. Merging them would have forced one
/// answer onto the other. Here, in an async handler, both can simply be read.
async fn deliveries(state: &AppState) -> (Vec<crate::transcode::SessionInfo>, Vec<Delivery>) {
    let sessions = state.transcode.list_deliveries().await;
    let mut out: Vec<Delivery> = sessions
        .iter()
        .map(|(s, method)| Delivery {
            method: method.as_str(),
            user: s.user_name.clone(),
            file_id: s.file_id,
            item_id: s.item_id,
            title: s.item_title.clone(),
            started_unix: s.started_unix,
            idle_seconds: s.idle_seconds,
            session_id: Some(s.id.clone()),
            delivered_bytes: Some(s.delivered_bytes),
            delivered_bps: s.delivered_bps,
        })
        .collect();

    // The two routes that hold no session. Titles are resolved here rather
    // than carried in the registries: a registry that stored a title would go
    // stale against a rename, and this page is polled by an admin looking at a
    // handful of rows, not by every player.
    let mut titles: HashMap<i64, String> = HashMap::new();
    let mut item_of_file: HashMap<i64, i64> = HashMap::new();
    for stream in state.streams.list() {
        let item_id = match item_of_file.get(&stream.file_id) {
            Some(id) => *id,
            None => {
                let id = state
                    .store
                    .get_file(stream.file_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|f| f.item_id)
                    .unwrap_or(0);
                item_of_file.insert(stream.file_id, id);
                id
            }
        };
        out.push(Delivery {
            method: crate::delivery::Method::Remux.as_str(),
            user: stream.user_name,
            file_id: stream.file_id,
            item_id,
            title: title_of(state, item_id, &mut titles).await,
            started_unix: stream.started_unix,
            // A remux is one pipe with no `last_access` of its own; how long
            // since a byte left it is the same question.
            idle_seconds: (stream.delivered_idle_ms.max(0) / 1_000) as u64,
            session_id: None,
            delivered_bytes: Some(stream.delivered_bytes),
            delivered_bps: stream.delivered_bps,
        });
    }
    for play in state.direct_plays.list() {
        out.push(Delivery {
            method: crate::delivery::Method::Direct.as_str(),
            user: play.user_name,
            file_id: play.file_id,
            item_id: play.item_id,
            title: title_of(state, play.item_id, &mut titles).await,
            started_unix: play.started_unix,
            idle_seconds: play.idle_seconds,
            session_id: None,
            delivered_bytes: None,
            delivered_bps: None,
        });
    }
    // Newest first, with a total order so a two-second poll does not reshuffle
    // rows that started in the same second.
    out.sort_by(|a, b| {
        b.started_unix
            .cmp(&a.started_unix)
            .then(a.method.cmp(b.method))
            .then(a.file_id.cmp(&b.file_id))
            .then(a.user.cmp(&b.user))
    });
    (sessions.into_iter().map(|(s, _)| s).collect(), out)
}

/// An item's title, read once per request however many rows want it.
async fn title_of(state: &AppState, item_id: i64, seen: &mut HashMap<i64, String>) -> String {
    if let Some(title) = seen.get(&item_id) {
        return title.clone();
    }
    let title = state
        .store
        .get_item(item_id)
        .await
        .ok()
        .flatten()
        .map(|i| i.title)
        // A file whose item was deleted mid-play is still a delivery in
        // progress; the row belongs on the page with an honest gap in it
        // rather than being dropped for want of a name.
        .unwrap_or_default();
    seen.insert(item_id, title.clone());
    title
}

/// GET /api/v1/activity/detail — the activity page: live playback sessions,
/// per-library scan state, and the Trakt sync story, all in one shape. Any
/// authenticated user may look (it's their household server); the stop action
/// below is admin-only.
pub async fn activity_detail(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // `sessions` is untouched — native clients parse it — and `deliveries` is
    // the superset beside it: the same HLS sessions plus the two routes that
    // were never listed at all.
    let (sessions, deliveries) = deliveries(&state).await;
    let offline = offline_work(&state).await?;
    let names: HashMap<i64, String> = state
        .store
        .list_libraries()
        .await?
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let scans: Vec<serde_json::Value> = state
        .jobs
        .all_statuses()
        .await
        .into_iter()
        .map(|(id, st)| {
            serde_json::json!({
                "library_id": id,
                "library": names.get(&id).cloned().unwrap_or_else(|| format!("#{id}")),
                "status": st,
            })
        })
        .collect();
    let trakt = state.trakt.status(0).await; // page shows server-wide state
    let linked = state
        .store
        .list_trakt_auth()
        .await?
        .into_iter()
        .next()
        .map(|a| {
            serde_json::json!({
                "trakt_username": a.trakt_username,
                "last_sync_at": (a.last_sync_at > 0).then_some(a.last_sync_at),
            })
        });
    Ok(Json(serde_json::json!({
        "sessions": sessions,
        "deliveries": deliveries,
        "offline": offline,
        "scans": scans,
        "producing": state.jobs.producing_now().await,
        "trakt": {
            "configured": trakt.configured,
            "linked": linked,
            "syncing": trakt.syncing,
            "note": trakt.note,
        },
    })))
}

/// DELETE /api/v1/activity/producer (admin) — stop the pre-transcode pass.
///
/// It stops after the title it is on rather than mid-encode: the producer
/// resumes from published segment boundaries, so a clean stop keeps the part
/// it has already made and a kill throws it away. The next scheduled pass
/// picks up from there.
pub async fn stop_producer(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.jobs.stop_producing() {
        return Err(ApiError::NotFound("producer"));
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "note": "stopping after the current title" }),
    ))
}

/// DELETE /api/v1/activity/sessions/:id (admin) — stop a transcode session.
pub async fn stop_session(
    _admin: AdminUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stopped = state.transcode.stop_session(&id, "stopped by admin").await;
    if !stopped {
        return Err(ApiError::NotFound("session"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/v1/activity/offline/:id (admin) — cancel one visible package.
pub async fn stop_offline_package(
    _admin: AdminUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = now_unix();
    let package = state
        .store
        .offline_activity_packages(&state.node_id, now, now.saturating_sub(65), 50)
        .await?
        .into_iter()
        .map(|row| row.package)
        .find(|package| package.id == id)
        .ok_or(ApiError::NotFound("offline package"))?;
    state.offline.cancel(&id).await;
    if !state
        .store
        .delete_offline_package(&id, package.user_id)
        .await?
    {
        return Err(ApiError::NotFound("offline package"));
    }
    state.offline.record_cancellation(&package);
    state.offline.forget_transfer(&id);
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /metrics — Prometheus text exposition (unauthenticated; counts only).
pub async fn metrics(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let uptime = state.started_at.elapsed().as_secs();
    let sessions = state.transcode.active_sessions().await;
    let libraries = state
        .store
        .list_libraries()
        .await
        .map(|l| l.len())
        .unwrap_or(0);
    let users = state.store.count_users().await.unwrap_or(0);
    let active_cache_entries = state.transcode.active_cache_entries();
    let offline = state
        .store
        .offline_package_stats(&state.node_id, now_unix())
        .await
        .unwrap_or_default();
    let offline_metrics = format!(
        "# HELP plurx_offline_packages Durable offline packages by state.\n\
         # TYPE plurx_offline_packages gauge\n\
         plurx_offline_packages{{state=\"queued\"}} {}\n\
         plurx_offline_packages{{state=\"preparing\"}} {}\n\
         plurx_offline_packages{{state=\"ready\"}} {}\n\
         plurx_offline_packages{{state=\"failed\"}} {}\n\
         # HELP plurx_offline_bytes Package bytes by state, actual when ready and reserved otherwise.\n\
         # TYPE plurx_offline_bytes gauge\n\
         plurx_offline_bytes{{state=\"queued\"}} {}\n\
         plurx_offline_bytes{{state=\"preparing\"}} {}\n\
         plurx_offline_bytes{{state=\"ready\"}} {}\n\
         plurx_offline_bytes{{state=\"failed\"}} {}\n\
         # HELP plurx_offline_active_leases Unexpired leases for ready offline packages.\n\
         # TYPE plurx_offline_active_leases gauge\n\
         plurx_offline_active_leases {}\n\
         # HELP plurx_cache_pinned_bytes Completed cache bytes protected by offline packages.\n\
         # TYPE plurx_cache_pinned_bytes gauge\n\
         plurx_cache_pinned_bytes{{reason=\"offline\"}} {}\n\
         # HELP plurx_cache_protected_entries Cache entries protected from housekeeping by active playback.\n\
         # TYPE plurx_cache_protected_entries gauge\n\
         plurx_cache_protected_entries{{reason=\"active_playback\"}} {}\n{}",
        offline.queued,
        offline.preparing,
        offline.ready,
        offline.failed,
        offline.queued_bytes,
        offline.preparing_bytes,
        offline.ready_bytes,
        offline.failed_bytes,
        offline.active_leases,
        offline.pinned_bytes,
        active_cache_entries,
        state.offline.prometheus(),
    );

    // Integration counters (plan P6). Scans by what asked for them, and how
    // many times another application has called in at all — the pair that
    // answers "is the fast path actually being used, or is the scheduled
    // sweep quietly carrying everything?".
    let (by_trigger, notifications) = state.jobs.metrics().snapshot();
    let mut scans = String::from(
        "# HELP plurx_scan_total Library scans started, by what asked for one.\n\
         # TYPE plurx_scan_total counter\n",
    );
    for (trigger, count) in by_trigger {
        scans.push_str(&format!(
            "plurx_scan_total{{trigger=\"{trigger}\"}} {count}\n"
        ));
    }
    let (pending, ok, failed) = state
        .store
        .watched_outbox_counts()
        .await
        .unwrap_or((0, 0, 0));
    scans.push_str(&format!(
        "# HELP plurx_watched_outbox Watched notifications queued for monarr, by state.\n\
         # TYPE plurx_watched_outbox gauge\n\
         plurx_watched_outbox{{status=\"pending\"}} {pending}\n\
         plurx_watched_outbox{{status=\"ok\"}} {ok}\n\
         plurx_watched_outbox{{status=\"failed\"}} {failed}\n"
    ));
    scans.push_str(&format!(
        "# HELP plurx_notify_received_total Scan requests received from other applications.\n\
         # TYPE plurx_notify_received_total counter\n\
         plurx_notify_received_total {notifications}\n"
    ));

    let body = format!(
        "# HELP plurx_build_info Build information.\n\
         # TYPE plurx_build_info gauge\n\
         plurx_build_info{{version=\"{version}\",build=\"{build}\"}} 1\n\
         # HELP plurx_uptime_seconds Seconds since this node started.\n\
         # TYPE plurx_uptime_seconds gauge\n\
         plurx_uptime_seconds {uptime}\n\
         # HELP plurx_transcode_sessions_active Live transcode sessions.\n\
         # TYPE plurx_transcode_sessions_active gauge\n\
         plurx_transcode_sessions_active {sessions}\n\
         # HELP plurx_libraries_total Configured libraries.\n\
         # TYPE plurx_libraries_total gauge\n\
         plurx_libraries_total {libraries}\n\
         # HELP plurx_users_total Registered users.\n\
         # TYPE plurx_users_total gauge\n\
         plurx_users_total {users}\n\
         {scans}{offline_metrics}{playback_metrics}",
        version = crate::version::SEMVER,
        build = crate::version::BUILD,
        playback_metrics = crate::telemetry::prometheus(),
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn beacon(event: &str, ms: i64) -> ClientLog {
        ClientLog {
            level: "warn".into(),
            event: event.into(),
            message: "x".into(),
            method: Some("remux".into()),
            code: None,
            title: None,
            file_id: None,
            vcodec: None,
            src: None,
            detail: None,
            ua: None,
            attempt: None,
            reason: None,
            runway: None,
            ms: Some(ms),
            bandwidth: None,
            height: None,
            encoder: None,
            decode_hw: None,
            decode_smooth: None,
            session_id: None,
        }
    }

    /// The decoder verdict rides with the client facts, and says which one it
    /// is loudly enough to notice in a wall of log lines. `null` (a browser
    /// without mediaCapabilities) prints nothing rather than guessing.
    #[test]
    fn a_software_decode_is_named_in_the_beacon() {
        let mut ev = beacon("stall", 900);
        assert!(
            !client_log_line(&ev, 0).contains("decode="),
            "silent when unknown"
        );

        ev.decode_hw = Some(true);
        assert!(client_log_line(&ev, 0).contains(" decode=hw"));

        ev.decode_hw = Some(false);
        ev.decode_smooth = Some(false);
        let line = client_log_line(&ev, 0);
        assert!(line.contains(" decode=SOFTWARE/not-smooth"), "{line}");
    }

    #[test]
    fn session_identity_does_not_change_the_human_log_line() {
        let mut event = beacon("ttff", 684);
        let before = client_log_line(&event, 0);
        event.session_id = Some("session-a".into());
        assert_eq!(client_log_line(&event, 0), before);
    }

    #[test]
    fn telemetry_retention_does_not_change_the_human_log_surface() {
        let mut event = beacon("disabled_telemetry_proof", 0);
        event.message = "still logs".into();
        assert_eq!(
            client_log_line(&event, 0),
            "client disabled_telemetry_proof method=remux: still logs ms=0"
        );
    }

    /// A measurement is named for what it measured.
    ///
    /// `ms` carries a start time on a `ttff` event and a stall's *duration* on
    /// a `stall` one, and both were printed as `ttff_ms`. So a grep for
    /// `ttff_ms` over the log ring returned a series that was part start times
    /// and part stall lengths — with the stalls, being longer, owning the whole
    /// tail. That is the one number M0 exists to produce, wrong in the
    /// direction that makes the server look worse than it is: on nynuc a real
    /// p90 of 1.5 s read as 4.6 s.
    #[test]
    fn a_stalls_duration_is_never_reported_as_a_start_time() {
        let start = client_log_line(&beacon("ttff", 684), 0);
        assert!(start.contains("ttff_ms=684"), "{start}");

        let stall = client_log_line(&beacon("stall", 4584), 0);
        assert!(stall.contains("stall_ms=4584"), "{stall}");
        assert!(
            !stall.contains("ttff_ms"),
            "a stall's duration entered the start-time series: {stall}"
        );

        // An event nobody has taught this about says the neutral thing rather
        // than borrowing whichever name is nearest.
        let other = client_log_line(&beacon("hls_fatal", 12), 0);
        assert!(other.contains(" ms=12"), "{other}");
        assert!(
            !other.contains("ttff_ms") && !other.contains("stall_ms"),
            "{other}"
        );
    }

    /// The log ring must survive a client stuck in an error loop: the burst gets
    /// through, the flood does not, and the gap is accounted for rather than
    /// silently dropped.
    #[test]
    fn client_log_bucket_bounds_a_flood_and_counts_the_gap() {
        let t0 = Instant::now();
        let mut b = LogBucket::new(30);

        // The full burst is admitted, nothing suppressed yet.
        for _ in 0..30 {
            assert_eq!(b.admit(t0), Some(0));
        }
        // The next 500 in the same instant are dropped.
        for _ in 0..500 {
            assert_eq!(b.admit(t0), None);
        }

        // Two seconds later one token has refilled (30/min), and the report that
        // spends it carries the count of everything dropped meanwhile.
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(b.admit(t1), Some(500));
        // That count resets once reported.
        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(b.admit(t2), Some(0));

        // A long quiet period refills to the burst cap and no further: an idle
        // client can't bank hours of credit and then dump it.
        let t3 = t2 + Duration::from_secs(3600);
        for _ in 0..30 {
            assert_eq!(b.admit(t3), Some(0));
        }
        assert_eq!(b.admit(t3), None);
    }

    /// A steady trickle below the limit is never throttled — normal playback
    /// errors keep flowing.
    #[test]
    fn client_log_bucket_passes_a_normal_trickle() {
        let mut now = Instant::now();
        let mut b = LogBucket::new(30);
        // One report every 5s for an hour: well under 30/min.
        for _ in 0..720 {
            assert_eq!(b.admit(now), Some(0));
            now += Duration::from_secs(5);
        }
    }

    #[test]
    fn client_log_limits_are_isolated_per_user() {
        let now = Instant::now();
        let mut limiter = ClientLogLimiter::new();
        for _ in 0..CLIENT_LOG_USER_PER_MIN {
            assert_eq!(limiter.admit(1, now), Some(0));
        }
        assert_eq!(limiter.admit(1, now), None);
        assert_eq!(
            limiter.admit(2, now),
            Some(0),
            "one user's flood must not silence another user"
        );
    }

    #[test]
    fn client_log_user_bucket_registry_is_bounded() {
        let now = Instant::now();
        let mut limiter = ClientLogLimiter::new();
        for user_id in 0..=CLIENT_LOG_USER_BUCKETS_MAX as i64 {
            let _ = limiter.admit(user_id, now + Duration::from_nanos(user_id as u64));
        }
        assert_eq!(limiter.users.len(), CLIENT_LOG_USER_BUCKETS_MAX);
        assert!(limiter
            .users
            .contains_key(&(CLIENT_LOG_USER_BUCKETS_MAX as i64)));
        assert!(
            !limiter.users.contains_key(&0),
            "the oldest idle bucket was evicted"
        );
    }

    #[test]
    fn client_log_suppression_counts_survive_the_global_ceiling() {
        let now = Instant::now();
        let mut limiter = ClientLogLimiter {
            global: LogBucket::new(1),
            users: HashMap::from([(1, LogBucket::new(1)), (2, LogBucket::new(1))]),
        };
        assert_eq!(limiter.admit(1, now), Some(0));
        assert_eq!(limiter.admit(1, now), None, "user bucket counts one drop");

        let minute = now + Duration::from_secs(60);
        assert_eq!(
            limiter.admit(2, minute),
            Some(0),
            "another user consumes the refilled global token"
        );
        assert_eq!(
            limiter.admit(1, minute),
            None,
            "user one's recovered token meets the global ceiling"
        );

        assert_eq!(
            limiter.admit(1, minute + Duration::from_secs(60)),
            Some(2),
            "one per-user and one global drop are both reported"
        );
    }

    #[tokio::test]
    async fn client_event_joins_and_persists_every_live_session_measurement() {
        let store: Arc<dyn Store> = Arc::new(
            plurx_core::store::SqliteStore::open_in_memory().expect("telemetry test store"),
        );
        let mut beacon = beacon("stall", 900);
        beacon.session_id = Some("session-a".into());
        let event = client_playback_event(&beacon, 7);
        let info = crate::transcode::SessionInfo {
            id: "session-a".into(),
            file_id: 42,
            item_id: 4,
            item_title: "not persisted".into(),
            user_name: "not persisted".into(),
            target_height: 1080,
            encoder: "qsv",
            started_unix: 0,
            idle_seconds: 0,
            speed: Some(2.0),
            recent_speed: Some(1.7),
            out_time_ms: Some(10_000),
            ahead_seconds: Some(34),
            hold_reason: Some(crate::transcode::AheadHoldReason::Time),
            resume_below_seconds: Some(30),
            resume_below_bytes: None,
            ahead_bytes: Some(123),
            delivered_bytes: 456,
            delivered_bps: Some(8_000_000),
            delivered_idle_ms: 25,
            readrate: 2.0,
            suspended: true,
            suspend_count: 1,
        };
        emit_client_playback_event(Arc::clone(&store), event, Some(&info), None);
        let row = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(row) = store
                    .playback_events(&PlaybackEventQuery {
                        event: Some("stall".into()),
                        limit: 10,
                        ..PlaybackEventQuery::default()
                    })
                    .await
                    .expect("query joined client event")
                    .into_iter()
                    .next()
                {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("joined client event persisted");
        assert_eq!(row.session_id.as_deref(), Some("session-a"));
        assert_eq!(row.file_id, Some(42));
        assert_eq!(row.speed_recent, Some(1.7));
        assert_eq!(row.ahead_seconds, Some(34));
        assert_eq!(row.suspended, Some(true));
        assert_eq!(row.hold_reason.as_deref(), Some("time"));
        assert_eq!(row.delivered_bps, Some(8_000_000));
        assert_eq!(row.readrate, Some(2.0));
    }

    #[test]
    fn playback_event_reader_caps_the_requested_row_count() {
        assert_eq!(
            bounded_playback_query(PlaybackEventsQuery {
                since: None,
                event: None,
                limit: i64::MAX,
            })
            .limit,
            2_000
        );
    }
}

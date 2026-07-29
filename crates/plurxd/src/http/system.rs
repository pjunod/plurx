//! Server identity, first-run setup, settings, and scan status.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use plurx_core::auth;
use plurx_core::store::keys;
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
    /// Short machine tag: "playback_failed" | "stream_rejected" | "hls_fatal" | "stall".
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
}

/// Sustained rate and burst allowance for `/client-log`, in reports per minute.
///
/// The log ring holds 2000 lines, so a browser that hits an error in a loop can
/// erase every other line in it in seconds — destroying precisely the history an
/// operator opened the page to read. That is not hypothetical: a stranded
/// hls.js instance polling a playlist that never ends fires a fatal error on a
/// timer, and there was no bound on how many of those could pile up.
const CLIENT_LOG_PER_MIN: u32 = 30;

/// Token bucket guarding the log ring. Global rather than per-user: the ring is
/// global, so it's the total rate that has to be bounded.
struct LogBucket {
    tokens: f64,
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
        self.tokens = (self.tokens + elapsed * (CLIENT_LOG_PER_MIN as f64 / 60.0))
            .min(CLIENT_LOG_PER_MIN as f64);
        if self.tokens < 1.0 {
            self.suppressed += 1;
            return None;
        }
        self.tokens -= 1.0;
        Some(std::mem::take(&mut self.suppressed))
    }
}

static CLIENT_LOG_BUCKET: std::sync::Mutex<LogBucket> = std::sync::Mutex::new(LogBucket {
    tokens: CLIENT_LOG_PER_MIN as f64,
    last: None,
    suppressed: 0,
});

/// POST /api/v1/client-log — any signed-in user. Records one browser playback
/// error into the server log ring so it surfaces in `Settings → Logs`. Bounded
/// by per-field clipping and by a global rate limit (this is diagnostics, not an
/// audit trail), and tagged with the `plurxd::client` target so it's visibly a
/// client report.
pub async fn client_log(_user: AuthUser, Json(ev): Json<ClientLog>) -> StatusCode {
    let suppressed = match CLIENT_LOG_BUCKET.lock() {
        Ok(mut b) => b.admit(std::time::Instant::now()),
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
    StatusCode::NO_CONTENT
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
    /// Scan every library once, ~30s after the server starts.
    pub scan_on_startup: bool,
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
    let scan_on_startup = state
        .store
        .get_setting(keys::JOB_SCAN_ON_STARTUP)
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
        hls_readrate,
        hls_burst_secs,
        hls_ahead_max_secs,
        hls_ahead_max_bytes,
        hls_scratch_max_bytes,
        probe_retry_mins,
        artwork_retry_mins,
        transcode_cleanup_mins,
        cache_produce_mins,
        cache_max_gb,
        cache_used_bytes,
        scan_on_startup,
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
    /// HLS session pacing: rate (x real time, "0" unpaced), opening burst in
    /// seconds, and the ahead-of-playhead window in seconds ("0" unbounded).
    pub hls_readrate: Option<String>,
    pub hls_burst_secs: Option<String>,
    pub hls_ahead_max_secs: Option<String>,
    pub hls_ahead_max_bytes: Option<String>,
    pub hls_scratch_max_bytes: Option<String>,
    /// Server-wide job intervals in minutes; 0 turns one off.
    pub probe_retry_mins: Option<i64>,
    pub artwork_retry_mins: Option<i64>,
    pub transcode_cleanup_mins: Option<i64>,
    pub cache_produce_mins: Option<i64>,
    pub cache_max_gb: Option<i64>,
    pub scan_on_startup: Option<bool>,
}

/// PUT /api/v1/settings (admin)
pub async fn update_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsDto>, ApiError> {
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
    if let Some(on) = req.scan_on_startup {
        state
            .store
            .put_setting(keys::JOB_SCAN_ON_STARTUP, if on { "1" } else { "0" })
            .await?;
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
    /// Machine-readable kind: "scan" | "enrich" | "stream" (more later).
    pub kind: &'static str,
    /// Short human label, e.g. "Scanning Movies".
    pub label: String,
    /// Optional detail, e.g. "412 of 3801 files".
    pub detail: Option<String>,
    /// 0–100 when a meaningful percentage exists.
    pub percent: Option<u8>,
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

/// GET /api/v1/activity/detail — the activity page: live playback sessions,
/// per-library scan state, and the Trakt sync story, all in one shape. Any
/// authenticated user may look (it's their household server); the stop action
/// below is admin-only.
pub async fn activity_detail(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state.transcode.list_sessions().await;
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
         {scans}",
        version = crate::version::SEMVER,
        build = crate::version::BUILD,
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
        let mut b = LogBucket {
            tokens: CLIENT_LOG_PER_MIN as f64,
            last: None,
            suppressed: 0,
        };

        // The full burst is admitted, nothing suppressed yet.
        for _ in 0..CLIENT_LOG_PER_MIN {
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
        for _ in 0..CLIENT_LOG_PER_MIN {
            assert_eq!(b.admit(t3), Some(0));
        }
        assert_eq!(b.admit(t3), None);
    }

    /// A steady trickle below the limit is never throttled — normal playback
    /// errors keep flowing.
    #[test]
    fn client_log_bucket_passes_a_normal_trickle() {
        let mut now = Instant::now();
        let mut b = LogBucket {
            tokens: CLIENT_LOG_PER_MIN as f64,
            last: None,
            suppressed: 0,
        };
        // One report every 5s for an hour: well under 30/min.
        for _ in 0..720 {
            assert_eq!(b.admit(now), Some(0));
            now += Duration::from_secs(5);
        }
    }
}

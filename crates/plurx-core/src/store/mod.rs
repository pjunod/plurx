//! The storage boundary.
//!
//! Everything that must survive a node (and later, replicate across the
//! cluster) goes through the [`Store`] trait family. Phase 0–2:
//! [`SqliteStore`] on local disk. Phase 3 spike / Phase 4: a raft-replicated
//! backend implements these same traits, and single-node mode becomes a
//! 1-voter cluster — same code path. See `docs/ARCHITECTURE.md` §2.
//!
//! The trait is split by domain area purely for readability; consumers hold
//! one `Arc<dyn Store>`. Contract notes for future backends:
//! - Operations are linearizable from the caller's perspective.
//! - A successful write method is durable (on a cluster: quorum-acked).
//!   The HTTP progress coalescer sits above this boundary: an intermediate
//!   active-playback beat may receive a soft acknowledgement while its newest
//!   value is pending, and the response exposes only the durable state.
//! - Implementations are shared via `Arc`, never cloned per-request.

mod sqlite;
mod telemetry;

#[cfg(feature = "hiqlite-store")]
mod hiqlite;
#[cfg(feature = "hiqlite-store")]
mod hiqlite_catalog;
#[cfg(feature = "hiqlite-store")]
mod hiqlite_durable;
#[cfg(feature = "hiqlite-store")]
mod hiqlite_import;
#[cfg(feature = "hiqlite-store")]
mod hiqlite_media;
#[cfg(feature = "hiqlite-store")]
mod hiqlite_reading;

pub mod replicated;

use std::path::PathBuf;

#[cfg(feature = "hiqlite-store")]
pub use self::hiqlite::{
    ClusterCompatibility, HiqliteAuthStore, AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION,
};
#[cfg(feature = "hiqlite-store")]
pub use self::hiqlite_import::{SqliteImportReport, SqliteImportTableDigest};
pub use sqlite::{SqliteStore, SQLITE_SCHEMA_VERSION};

use async_trait::async_trait;

use crate::domain::{
    CachedTranscode, InProgressItem, Item, ItemEdit, ItemKind, ItemPage, ItemSort, Library,
    MediaFile, MediaShape, MetadataPatch, NetworkPrior, NetworkPriorObservation, NewItem,
    NewLibrary, NewOfflinePackage, OfflineActivityPackage, OfflineCreateOutcome,
    OfflineLeaseOutcome, OfflinePackage, OfflinePackageStats, OfflineRemovalPlanEntry,
    OfflineRemovalReport, PlaybackEvent, PlaybackEventQuery, ProbeResult, ReadingState,
    ReadingStateWrite, RecentItem, TraktAuth, User, WatchRollup, WatchState,
};
// RecentItem is reused for next-up (episode + show title).
use crate::error::StoreError;
use crate::mediafacts::MediaFacts;
use crate::secrets::SealedSecret;

/// The text a backend may write into a durable Trakt bearer column.
///
/// Every store implementation funnels its bearer writes through here, so the
/// promise in `CLUSTERING-PLAN.md` §3.2 — a durable, and once M2 activates a
/// replicated, row carries only wrapped material — is enforced at the write
/// rather than assumed of every caller. Refusing costs one linked account a
/// loud error; accepting would put a cleartext bearer token in every voter's
/// raft log, where deleting the row cannot reach it.
pub(crate) fn persistable_credential(value: &SealedSecret) -> Result<String, StoreError> {
    value.to_persist().map(str::to_owned).map_err(|error| {
        StoreError::Credential(format!(
            "refusing to persist a Trakt bearer credential that is not sealed: {error}"
        ))
    })
}

/// Well-known settings keys. Keys are dotted, lowercase, and owned by the
/// module that writes them.
pub mod keys {
    /// Stable unique id for this logical server. Generated on first startup,
    /// immutable thereafter; in a cluster it identifies the *cluster*, not a
    /// node (REQ-HA-5: one logical identity).
    pub const INSTANCE_ID: &str = "instance.id";
    /// TMDB API key (set by the admin; empty/absent disables the agent).
    pub const TMDB_API_KEY: &str = "tmdb.api_key";
    /// OMDb API key — powers review-site ratings (Rotten Tomatoes / Metacritic /
    /// IMDb), which TMDB doesn't carry. Free key from omdbapi.com.
    pub const OMDB_API_KEY: &str = "omdb.api_key";
    /// Hardware-encoder preference for transcoding: "nvenc" | "qsv" | "vaapi"
    /// | "videotoolbox" | "software" | "" (automatic).
    pub const HWACCEL: &str = "transcode.hwaccel";
    /// Requested rate-control family. Missing/`bitrate` preserves the legacy
    /// VBR path exactly; `quality` is validated against every usable encoder
    /// before an effective snapshot is published.
    pub const TRANSCODE_RATE_MODE: &str = "transcode.rate_mode";
    /// Optional integer quality override. Empty/absent means the calibrated
    /// per-family default; the value matters only when rate mode is quality.
    pub const TRANSCODE_QUALITY: &str = "transcode.quality";
    /// Trakt API application credentials (the admin creates the app at
    /// trakt.tv/oauth/applications; empty/absent disables the integration).
    pub const TRAKT_CLIENT_ID: &str = "trakt.client_id";
    pub const TRAKT_CLIENT_SECRET: &str = "trakt.client_secret";
    /// Where monarr lives, and the key to read its calendar with. Both are
    /// server-side only: plurxd proxies the call so the key never reaches a
    /// browser (plan §11.2). Unset = no coming-soon rail, which is the
    /// default and changes nothing.
    pub const MONARR_URL: &str = "monarr.url";
    pub const MONARR_API_KEY: &str = "monarr.api_key";
    /// Push watch state to monarr ("1" = on). Off by default and separate
    /// from the URL/key pair, because reading monarr's calendar and sending
    /// it your household's viewing history are very different consents.
    pub const MONARR_WATCHED_SYNC: &str = "monarr.watched_sync";
    /// Preferred default audio language (ISO 639 code, e.g. "eng").
    pub const AUDIO_LANG: &str = "playback.audio_lang";
    /// Preferred default subtitle language (ISO 639 code, e.g. "eng").
    pub const SUB_LANG: &str = "playback.sub_lang";
    /// When subtitles auto-select: "auto" (only when the audio isn't the
    /// preferred language) | "always" | "off".
    pub const SUB_MODE: &str = "playback.sub_mode";
    /// Scheduled-job intervals, in minutes; "0" or absent means off, which is
    /// the default for every one of them — upgrading a server must not
    /// silently give it new nightly habits. Per-library scan and refresh
    /// intervals live on the library row instead, since they differ per
    /// library; only the server-wide jobs are settings.
    pub const JOB_PROBE_RETRY_MINS: &str = "jobs.probe_retry_mins";
    pub const JOB_TRANSCODE_CLEANUP_MINS: &str = "jobs.transcode_cleanup_mins";
    /// Re-fetch artwork for enriched items whose poster or hero art is still
    /// incomplete.
    ///
    /// The one job that is **on by default** ([`ARTWORK_RETRY_DEFAULT_MINS`]),
    /// against the rule above and deliberately. Every other job is optional
    /// housekeeping; this one repairs a hole the server itself left, and a
    /// default of 0 would ship the bug it exists to fix — the backlog would
    /// sit there until someone found a button, which is exactly the state
    /// that made this necessary. Set it to 0 to turn it off.
    pub const JOB_ARTWORK_RETRY_MINS: &str = "jobs.artwork_retry_mins";
    /// When those last ran, unix seconds. Persisted rather than kept in memory
    /// so restarting the server doesn't restart the clock.
    pub const JOB_LAST_PROBE_RETRY: &str = "jobs.last_probe_retry";
    pub const JOB_LAST_TRANSCODE_CLEANUP: &str = "jobs.last_transcode_cleanup";
    pub const JOB_LAST_ARTWORK_RETRY: &str = "jobs.last_artwork_retry";
    /// Node-local playback telemetry retention, in days. Missing means
    /// [`TELEMETRY_RETAIN_DEFAULT_DAYS`]; `0` disables both writes and pruning.
    /// This is deliberately on by default: it is bounded local bookkeeping and
    /// the measurement referee for every Performance II milestone.
    pub const TELEMETRY_RETAIN_DAYS: &str = "telemetry.retain_days";
    /// Opt-in node-local network history used to seed Auto quality. Missing
    /// and every value other than `"1"` are off.
    pub const PLAYBACK_NETWORK_PRIORS: &str = "playback.network_priors";
    /// Last successful bounded telemetry-prune pass, in unix seconds.
    pub const JOB_LAST_TELEMETRY_PRUNE: &str = "jobs.last_telemetry_prune";
    pub const TELEMETRY_RETAIN_DEFAULT_DAYS: i64 = 30;
    /// Fixed retention for the aggregate network-prior rows. The feature is
    /// opt-in and cardinality-bounded as well; this age bound prevents old
    /// networks from surviving indefinitely while it remains enabled.
    pub const NETWORK_PRIOR_RETAIN_DAYS: i64 = 30;
    /// Default artwork-retry interval, in minutes. Half-hourly: often enough
    /// that a scan interrupted by a TMDB blip repairs itself while the user is
    /// still watching that evening, rare enough to be invisible to TMDB.
    pub const ARTWORK_RETRY_DEFAULT_MINS: i64 = 30;
    /// How long an item waits between artwork attempts, in seconds. Longer
    /// than the sweep interval on purpose: the sweep decides how often to
    /// *look*, this decides how often any one item is *tried*, and a
    /// permanently art-less item should cost one request a day, not 48.
    pub const ARTWORK_RETRY_BACKOFF_SECS: i64 = 24 * 60 * 60;
    /// Arm the one-off genre backfill: "1" runs it, "done" is what the pass
    /// writes when it finishes, anything else (including absent, the default)
    /// is off.
    ///
    /// Opt-in, unlike the artwork retry, and the difference is what the two
    /// jobs cost. The artwork sweep repairs a hole the server left, from data
    /// it already has. This one re-hits the provider once per title, because
    /// nothing is stored to recompute genres from — and v9 records what
    /// happens when an upgrade decides on its own to re-fetch every library
    /// in the world. So an operator arms it, sees it finish, and it disarms
    /// itself.
    pub const GENRE_BACKFILL: &str = "genres.backfill";
    /// The highest item id the backfill has finished with, stamped after
    /// every single title.
    ///
    /// This IS the resumability. A crash, a restart or a 429 resumes at the
    /// next id instead of paying for the whole catalogue a second time, and
    /// the stamp is durable because the failure being designed against is the
    /// host rebooting mid-run — an in-memory cursor dies with the process
    /// that owns it, exactly as v10 says of retry backoff.
    pub const GENRE_BACKFILL_CURSOR: &str = "genres.backfill_cursor";
    /// Scan every library once, shortly after the server starts. "1" enables
    /// it; absent or anything else is off. For a server that was powered down
    /// while files landed — otherwise it waits out a whole interval (or, with
    /// no interval set, until someone presses a button) before noticing them.
    pub const JOB_SCAN_ON_STARTUP: &str = "jobs.scan_on_startup";
    /// How fast a remux may run, as a multiple of real time; "0" disables the
    /// limit. An unpaced remux delivers at line rate and can starve everything
    /// else sharing the link — including, over Wi-Fi, the client's own DHCP.
    /// Absent means the built-in default.
    pub const STREAM_READRATE: &str = "playback.stream_readrate";
    /// How fast an HLS session (transcode or copy-video) may read its input,
    /// as a multiple of real time; "0" disables pacing. Separate from
    /// [`STREAM_READRATE`] on purpose — the progressive remux is consumed by
    /// the browser's own back-pressure, while an HLS session writes to disk
    /// and needs its own answer.
    pub const HLS_READRATE: &str = "playback.hls_readrate";
    /// Seconds of content an HLS session may deliver flat-out before
    /// [`HLS_READRATE`] engages. This is the buffer the viewer starts with, so
    /// it is also what a marginal link gets to spend before it stalls.
    pub const HLS_BURST_SECS: &str = "playback.hls_burst_secs";
    /// Seconds of content an HLS session may write beyond the client's
    /// playhead before it is suspended; "0" lets it run unbounded. This is the
    /// disk bound that realtime pacing used to provide, minus the part where
    /// realtime pacing also prevented the viewer from ever building a buffer.
    pub const HLS_AHEAD_MAX_SECS: &str = "playback.hls_ahead_max_secs";
    /// The same window in bytes, per session. Time alone is not a disk
    /// contract: 180 seconds is a few hundred megabytes at a transcode rung
    /// and over a gigabyte of 4K copy, so a stream with an unexpectedly high
    /// bitrate would blow through any time-only limit.
    pub const HLS_AHEAD_MAX_BYTES: &str = "playback.hls_ahead_max_bytes";
    /// Ceiling on scratch across every live session. A per-session cap bounds
    /// one runaway session; it says nothing about several healthy ones filling
    /// the disk between them.
    pub const HLS_SCRATCH_MAX_BYTES: &str = "playback.hls_scratch_max_bytes";
    /// Experimental Apple-facing playlist envelope. When enabled, new live
    /// HLS sessions serve a typeless sliding playlist from their first
    /// response instead of changing from EVENT only after retention begins.
    /// Off by default until the physical-iPad control run is conclusive.
    pub const HLS_TYPELESS_SLIDING: &str = "playback.hls_typeless_sliding";
    /// How many transcodes may run on the hardware encoder at once.
    ///
    /// An iGPU has one video-processing block, and two 4K sessions on it do not
    /// run at half speed each — they contend, and both can fall under realtime.
    /// One person's stream becomes two people's stutter. Settable because the
    /// right number is a property of the silicon: a discrete card with two
    /// NVENC chips is not a NUC.
    pub const MAX_HW_SESSIONS: &str = "transcode.max_hw_sessions";
    /// Threads the software-encoder CPU pool may hand out at once. Defaults
    /// to every core but one (plurxd derives it from the machine); an admin
    /// sets it lower on a box whose CPU has other jobs, or higher at their
    /// own risk. API-settable, deliberately no UI dropdown — the same policy
    /// as the scratch limits.
    pub const SW_POOL_THREADS: &str = "transcode.software_pool_threads";
    /// Disk the pre-transcode cache may occupy, in gigabytes. `0` turns the
    /// cache off: nothing is produced, and what is already there is evicted.
    ///
    /// A budget rather than a target. The cache is worth exactly what it
    /// saves, and a server that filled its disk to spare a few seconds has
    /// made the trade backwards. Eviction is LRU, so what survives is what
    /// people actually come back to.
    pub const CACHE_MAX_GB: &str = "cache.max_gb";
    /// Master kill switch for app-managed offline packages. Missing means on;
    /// an operator can stop new admission without invalidating local copies.
    pub const OFFLINE_ENABLED: &str = "offline.enabled";
    /// Global and per-user reservations for pinned offline artifacts. These
    /// are separate from the playback cache so a flight queue cannot evict
    /// the bytes an active viewer is reading.
    pub const OFFLINE_MAX_GB: &str = "offline.max_gb";
    pub const OFFLINE_MAX_GB_PER_USER: &str = "offline.max_gb_per_user";
    /// Registry hygiene, not the primary quota (bytes are). Includes failed
    /// rows until their bounded diagnostic retention sweep removes them.
    pub const OFFLINE_MAX_ROWS_PER_USER: &str = "offline.max_rows_per_user";
    /// How often the producer looks for something worth pre-transcoding, in
    /// minutes; "0" is off, and off is the default like every other job — an
    /// upgraded server must not start encoding overnight on its own.
    pub const JOB_CACHE_PRODUCE_MINS: &str = "jobs.cache_produce_mins";
    pub const JOB_LAST_CACHE_PRODUCE: &str = "jobs.last_cache_produce";
}

#[async_trait]
pub trait SettingsStore: Send + Sync + 'static {
    /// Cheap liveness probe of the backing storage (drives `/readyz`).
    async fn ping(&self) -> Result<(), StoreError>;
    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError>;
    /// Read two related settings from one database snapshot.
    async fn get_setting_pair(
        &self,
        first: &str,
        second: &str,
    ) -> Result<(Option<String>, Option<String>), StoreError>;
    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError>;
    /// Atomically publish a related group of settings.
    ///
    /// Callers use this when one key activates the meaning of another. A
    /// partial write must never leave a durable configuration that no request
    /// actually submitted.
    async fn put_settings(&self, values: &[(&str, &str)]) -> Result<(), StoreError>;
    /// The stable unique id of this logical server.
    async fn instance_id(&self) -> Result<String, StoreError>;
}

#[async_trait]
pub trait UserStore: Send + Sync + 'static {
    async fn count_users(&self) -> Result<i64, StoreError>;
    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<User, StoreError>;
    async fn get_user(&self, id: i64) -> Result<Option<User>, StoreError>;
    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, StoreError>;
    async fn list_users(&self) -> Result<Vec<User>, StoreError>;
    async fn delete_user(&self, id: i64) -> Result<bool, StoreError>;
    async fn count_admins(&self) -> Result<i64, StoreError>;
    /// Replace a user's password hash. Callers should also revoke the user's
    /// tokens so old sessions die with the old password.
    async fn set_password(&self, id: i64, password_hash: &str) -> Result<bool, StoreError>;
    async fn set_admin(&self, id: i64, is_admin: bool) -> Result<bool, StoreError>;
    /// Revoke every login token for one user; returns how many were dropped.
    async fn delete_tokens_for_user(&self, user_id: i64) -> Result<u64, StoreError>;

    /// Register a login token. Only the SHA-256 hash of the token is stored.
    async fn create_token(
        &self,
        token_hash: &str,
        user_id: i64,
        device: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Resolve a token hash to its user (touching `last_seen_at`).
    async fn user_for_token(&self, token_hash: &str) -> Result<Option<User>, StoreError>;
    async fn delete_token(&self, token_hash: &str) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait LibraryStore: Send + Sync + 'static {
    async fn create_library(&self, library: &NewLibrary) -> Result<Library, StoreError>;
    async fn update_library(
        &self,
        id: i64,
        library: &NewLibrary,
    ) -> Result<Option<Library>, StoreError>;
    async fn delete_library(&self, id: i64) -> Result<bool, StoreError>;
    /// Set the automatic scan/refresh intervals (minutes; `0` = off). Separate
    /// from `update_library` because the schedule is not part of a library's
    /// identity — the settings UI edits one without touching the other, and a
    /// path edit must never silently reset a schedule.
    async fn set_library_schedule(
        &self,
        id: i64,
        scan_interval_mins: i64,
        refresh_interval_mins: i64,
    ) -> Result<Option<Library>, StoreError>;
    /// Stamp a completed run. `refreshed` also stamps the refresh clock, since
    /// a refresh does everything a scan does.
    async fn mark_library_scanned(&self, id: i64, refreshed: bool) -> Result<(), StoreError>;
    async fn get_library(&self, id: i64) -> Result<Option<Library>, StoreError>;
    async fn list_libraries(&self) -> Result<Vec<Library>, StoreError>;
}

#[async_trait]
pub trait MediaStore: Send + Sync + 'static {
    /// The item carrying these external ids, across every library.
    ///
    /// For resolving something another application named. Matching on ids and
    /// never on titles is the rule this whole integration is built on — an
    /// application guessing which item you meant is the failure it exists to
    /// remove, and a title match here would reintroduce it from the other
    /// side.
    ///
    /// `kind` is required rather than inferred: a film and a show can hold the
    /// same TMDB id, because the two id spaces are separate.
    async fn item_by_external_id(
        &self,
        kind: ItemKind,
        tmdb_id: Option<i64>,
        imdb_id: Option<&str>,
    ) -> Result<Option<Item>, StoreError>;

    // --- item placement (scanner) ---
    async fn find_movie(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_book(
        &self,
        library_id: i64,
        kind: ItemKind,
        title: &str,
        year: Option<i32>,
        identity_path: Option<&str>,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_show(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_episode(
        &self,
        season_id: i64,
        episode_number: i32,
    ) -> Result<Option<Item>, StoreError>;
    /// Find a child by (library, parent, kind, title) — how the home
    /// scanner's mirrored folder tree keeps its identity. `parent_id: None`
    /// matches items directly under a library root.
    async fn find_child_item(
        &self,
        library_id: i64,
        parent_id: Option<i64>,
        kind: ItemKind,
        title: &str,
    ) -> Result<Option<Item>, StoreError>;
    async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError>;

    // --- browse ---
    async fn get_item(&self, id: i64) -> Result<Option<Item>, StoreError>;
    async fn get_item_children(&self, parent_id: i64) -> Result<Vec<Item>, StoreError>;
    /// One page of a library's grid, optionally narrowed to a single genre.
    ///
    /// The genre is matched against the item's stored list (migration v13),
    /// case-insensitively; `None` is the unfiltered query verbatim. It is a
    /// WHERE clause rather than a second code path on purpose — a filtered
    /// page that sorted or counted differently from an unfiltered one is the
    /// bug this shape cannot have.
    async fn list_top_items_in_genre(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
        genre: Option<&str>,
    ) -> Result<ItemPage, StoreError>;
    /// The unfiltered grid: every caller that had no opinion about genres
    /// before this existed and still has none. Provided rather than
    /// implemented so adding the parameter could not quietly change what any
    /// of them asks for.
    async fn list_top_items(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
    ) -> Result<ItemPage, StoreError> {
        self.list_top_items_in_genre(library_id, sort, offset, limit, None)
            .await
    }
    async fn recently_added(
        &self,
        library_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RecentItem>, StoreError>;
    async fn search_items(&self, query: &str, limit: i64) -> Result<Vec<RecentItem>, StoreError>;

    // --- metadata enrichment ---
    async fn apply_metadata(&self, item_id: i64, patch: &MetadataPatch) -> Result<(), StoreError>;
    /// Movies and shows to enrich: normally those no provider has answered
    /// for yet, which is *not* the same as "those with no TMDB id" — an item
    /// can arrive carrying an id from another application (a monarr scan
    /// request) and still need every other field. `force` includes
    /// already-enriched items too (a metadata refresh, e.g. to backfill
    /// season posters onto shows enriched before that existed).
    ///
    /// `only` narrows the result to specific item ids — what a targeted scan
    /// enriches, so that a request another application is *waiting on* costs
    /// the handful of items it just delivered rather than a whole library.
    /// `None` means "no id filter" and is the unnarrowed query verbatim;
    /// `Some(&[])` means "these zero items", i.e. nothing.
    async fn items_needing_metadata(
        &self,
        library_id: Option<i64>,
        force: bool,
        only: Option<&[i64]>,
    ) -> Result<Vec<Item>, StoreError>;
    /// All episodes of a show (across seasons), for bulk episode enrichment.
    async fn episodes_for_show(&self, show_id: i64) -> Result<Vec<Item>, StoreError>;
    /// Home-library items whose artwork the local enricher should generate:
    /// folders, videos, and photos with no poster yet (`force` = all of them).
    /// Folders come last so they can inherit a child's finished poster.
    /// `only` narrows to specific ids, as on
    /// [`items_needing_metadata`](Self::items_needing_metadata).
    async fn items_needing_artwork(
        &self,
        library_id: i64,
        force: bool,
        only: Option<&[i64]>,
    ) -> Result<Vec<Item>, StoreError>;
    /// Provider-enriched items still carrying incomplete artwork, oldest
    /// attempt first — the retry sweep's input, and the mirror of
    /// [`files_missing_probe`](Self::files_missing_probe).
    ///
    /// `retry_after_secs` is the backoff: an item attempted more recently than
    /// that is skipped. Without it an item TMDB genuinely has no art for would
    /// be re-fetched on every single cycle, forever, which is how a
    /// self-healing job turns into a rate-limit generator.
    /// `limit` bounds one pass so a large upgrade backlog cannot monopolize
    /// the scheduler or provider connection for hours. A non-positive limit
    /// requests no work; callers must opt into an explicit positive bound.
    async fn items_missing_artwork(
        &self,
        library_id: Option<i64>,
        retry_after_secs: i64,
        limit: i64,
    ) -> Result<Vec<Item>, StoreError>;
    /// Movies and shows with no genres yet, in ascending id order, starting
    /// strictly after `after_id` — the genre backfill's input.
    ///
    /// It cannot reuse [`items_needing_metadata`](Self::items_needing_metadata):
    /// every item this is for is already `metadata_at`-stamped and therefore
    /// invisible to that query, which is the whole reason a backfill exists.
    ///
    /// Ordered by id and cut at `after_id` because that pair IS the
    /// resumability: the caller stamps the last id it finished, and a crash,
    /// a restart or a 429 resumes from there instead of paying for the
    /// catalogue twice. A `LIMIT` rather than the whole list so one pass is
    /// bounded work against a rate-limited API.
    async fn items_missing_genres(
        &self,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<Item>, StoreError>;
    /// Apply a hand edit — distinct from [`apply_metadata`](Self::apply_metadata)
    /// because an edit must be able to *clear* a field. Returns the updated
    /// item, or `None` if the id doesn't exist.
    async fn update_item_fields(
        &self,
        item_id: i64,
        edit: &ItemEdit,
    ) -> Result<Option<Item>, StoreError>;
    /// Record that an NFO sidecar has been consumed for this item. Seeding
    /// happens at most once, ever (docs/HOMEVIDEO-PLAN.md §4.3) — after this
    /// the sidecar is dead to plurx, so a user's edits can never be clobbered.
    async fn set_nfo_seeded(&self, item_id: i64) -> Result<(), StoreError>;

    // --- files ---
    async fn get_file_by_path(&self, path: &str) -> Result<Option<MediaFile>, StoreError>;
    async fn upsert_file(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
    ) -> Result<i64, StoreError>;
    async fn get_file(&self, id: i64) -> Result<Option<MediaFile>, StoreError>;
    /// A census of what the libraries actually hold, in transcoder terms.
    ///
    /// Aggregated in SQL rather than by walking files: a library of a few
    /// hundred thousand rows should cost one scan, not one round trip per
    /// title, or nobody will run it twice.
    async fn media_shape(&self) -> Result<MediaShape, StoreError>;
    async fn files_for_item(&self, item_id: i64) -> Result<Vec<MediaFile>, StoreError>;
    /// How many children each of the given items has. Folder cards say "12
    /// items"; doing that with one query per card would be an N+1 on every
    /// grid render.
    async fn child_counts(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, i64>, StoreError>;
    /// Best (max) probed file height per item, for the given item ids. Used to
    /// badge/section the library grid by resolution without loading every file.
    async fn item_max_heights(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, i64>, StoreError>;
    /// Aggregated media facts per item, for the given item ids — the `media`
    /// block on library and season listings.
    ///
    /// One statement for the whole page, exactly like `item_max_heights`: this
    /// feeds a list, so a per-item lookup is a 200-round-trip page render, and
    /// the obvious alternative (load every file of every item and reduce in
    /// Rust) is that same fan-out wearing a different hat. Items with no files
    /// are absent from the map rather than present-and-empty — the caller
    /// decorates what it got and leaves the rest bare.
    async fn item_media_facts(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, MediaFacts>, StoreError>;
    /// Persist a manual A/V sync correction for one file (0 clears it).
    async fn set_file_audio_offset(&self, file_id: i64, offset_ms: i64) -> Result<(), StoreError>;
    /// The raw ffprobe JSON captured at scan time (for the declared per-stream
    /// start-time readout in the player's sync menu, and the chapter markers
    /// the player shows as Skip Intro / Skip Credits).
    async fn get_file_probe_json(&self, file_id: i64) -> Result<Option<String>, StoreError>;
    /// Graft a `chapters` array onto a file's stored probe JSON.
    ///
    /// Only for files probed before chapters were captured at scan time: the
    /// play path probes such a file once, then writes the answer here so the
    /// next play reads it like any other. `chapters_json` is a JSON array.
    /// A file with no stored probe is left alone — there is nothing to graft
    /// onto, and inventing a document would fake a successful probe.
    async fn merge_file_probe_chapters(
        &self,
        file_id: i64,
        chapters_json: &str,
    ) -> Result<(), StoreError>;
    /// Files whose probe never succeeded (`probe_json IS NULL`), oldest scan
    /// first. `library_id` narrows to one library; `None` is server-wide. These
    /// are the records the retry job and the scan's repair pass exist for —
    /// nothing about them changes on disk when the reason they failed is fixed.
    async fn files_missing_probe(
        &self,
        library_id: Option<i64>,
    ) -> Result<Vec<MediaFile>, StoreError>;
    /// All known file paths in a library (for vanished-file detection).
    async fn library_file_paths(&self, library_id: i64) -> Result<Vec<(i64, PathBuf)>, StoreError>;
    /// Establish the stable identity of the mounted roots for this library,
    /// or compare a later scanner's observation with cluster truth.
    async fn ensure_library_root_fingerprint(
        &self,
        library_id: i64,
        fingerprint: &str,
        allow_establish: bool,
    ) -> Result<RootFingerprintStatus, StoreError>;
    /// Forget a library's recorded root identity after an operator has
    /// verified a deliberate mount/path replacement.
    async fn reset_library_root_fingerprint(&self, library_id: i64) -> Result<bool, StoreError>;
    /// Recreate the derived full-text index from authoritative item rows.
    async fn rebuild_search_index(&self) -> Result<u64, StoreError>;
    /// Atomically remove vanished files and their empty item hierarchy only
    /// when the scanner sees the expected roots and stays within its deletion
    /// budget. This is the M1c publication boundary; M4 adds its lease fence.
    async fn reconcile_library(
        &self,
        library_id: i64,
        root_fingerprint: &str,
        gone_file_ids: &[i64],
        prune_limit: u64,
    ) -> Result<ReconcileOutcome, StoreError>;
    async fn delete_files(&self, ids: &[i64]) -> Result<u64, StoreError>;
    /// Remove items left childless/file-less after a scan. Returns rows removed.
    async fn prune_empty_items(&self, library_id: i64) -> Result<u64, StoreError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootFingerprintStatus {
    Established,
    Matched,
    Unestablished,
    Mismatch { expected: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Applied {
        deleted_files: u64,
        pruned_items: u64,
    },
    RefusedRoot {
        expected: String,
    },
    RefusedPrune {
        requested: u64,
        limit: u64,
    },
}

#[async_trait]
pub trait WatchStore: Send + Sync + 'static {
    async fn watch_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<WatchState>, StoreError>;
    /// Batch lookup for annotating item lists.
    async fn watch_map(
        &self,
        user_id: i64,
        item_ids: &[i64],
    ) -> Result<Vec<(i64, WatchState)>, StoreError>;
    /// Record playback progress; crossing 95% marks watched automatically.
    async fn put_progress(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<WatchState, StoreError> {
        self.put_progress_at(user_id, item_id, position_ms, duration_ms, None)
            .await
    }
    /// Commit a coalesced playback beat only if the durable row still matches
    /// the state observed by the coalescer's leading write.
    ///
    /// A manual watched/unwatched action, an offline replay, or a Trakt merge
    /// may update the same row while a trailing beat is waiting. Returning
    /// `None` tells the coalescer that another writer won; it must discard the
    /// stale beat rather than overwrite that newer intent.
    async fn put_progress_if_current(
        &self,
        user_id: i64,
        item_id: i64,
        expected: &WatchState,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<Option<WatchState>, StoreError>;
    /// Record playback progress with an optional client observation time.
    ///
    /// `recorded_at` is used by offline clients replaying their durable final
    /// state after reconnecting. A write older than the stored watch state is
    /// ignored, which prevents a late phone sync from rewinding playback that
    /// already continued on another device. Omitting it uses server time and
    /// preserves the ordinary online-heartbeat behavior.
    async fn put_progress_at(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
        recorded_at: Option<i64>,
    ) -> Result<WatchState, StoreError>;
    /// Flip one item's flag and nothing else. Callers acting on something a
    /// person clicked want [`WatchStore::set_watched_tree`] instead — this is
    /// the single-row primitive it is built from.
    async fn set_watched(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<(), StoreError>;
    /// Mark everything playable under `item_id` — and the item itself when it
    /// is playable — watched or unwatched. Returns the ids that actually
    /// changed, so a caller can notify on those and stay quiet about the rest.
    ///
    /// Containers are the point. A show is not something you watch; its
    /// episodes are, and "I've seen this series" is a statement about all of
    /// them. Marking only the show row would leave every episode unwatched
    /// underneath it, which is worse than not offering the button: the badge
    /// would say watched while Next Up went on offering episode one.
    async fn set_watched_tree(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<Vec<i64>, StoreError>;
    /// Count the playable leaves under `item_id` and how many of them are
    /// watched. A playable item is its own leaf, so this answers for movies
    /// too — a movie is 1/1 or 0/1.
    async fn watch_rollup(&self, user_id: i64, item_id: i64) -> Result<WatchRollup, StoreError>;
    /// [`WatchStore::watch_rollup`] for a whole page of containers, in one
    /// query.
    ///
    /// A library grid needs this for every show and season it paints, and a
    /// rollup is a recursive walk: doing it per card is an N+1 that grows
    /// with the season count, not the page size. Same contract as the single
    /// version, including the answer for a container holding nothing playable
    /// — every requested id gets an entry, `0/0` if its subtree has no
    /// leaves, so a caller never has to guess whether a missing key means
    /// "empty" or "not asked".
    async fn watch_rollups(
        &self,
        user_id: i64,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, WatchRollup>, StoreError>;
    async fn continue_watching(
        &self,
        user_id: i64,
        limit: i64,
    ) -> Result<Vec<InProgressItem>, StoreError>;
    /// Next-up episodes: for each show the user has watched into, the first
    /// unwatched, not-in-progress episode after the last watched one. Pairs
    /// with continue-watching (resume) — this is "start the next episode".
    async fn next_up(&self, user_id: i64, limit: i64) -> Result<Vec<RecentItem>, StoreError>;
    /// Write a watch fact that arrived from an external source (Trakt sync):
    /// unlike [`put_progress`] the caller controls `updated_at`, so remote
    /// timestamps land verbatim and later merges compare correctly.
    async fn apply_remote_watch(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
        position_ms: i64,
        duration_ms: Option<i64>,
        updated_at: i64,
    ) -> Result<(), StoreError>;
}

/// Per-user text-publication state. It is separate from [`WatchStore`]
/// because a reflowable locator is not timed playback and completion is an
/// explicit reader action rather than a 95% threshold.
#[async_trait]
pub trait ReadingStore: Send + Sync + 'static {
    /// Read the row for one edition, including a stale revision retained for
    /// diagnosis. The HTTP boundary compares its revision to the current
    /// file before offering a resume action.
    async fn reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<Option<ReadingState>, StoreError>;
    /// Newest state whose snapshotted revision still matches the current file.
    /// Item detail uses this one query instead of looking up every edition.
    async fn current_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<ReadingState>, StoreError>;
    /// Persist a locator. A dated offline write older than the durable row
    /// returns the winner without rewinding it; an undated online write uses
    /// the server clock and is authoritative now. A different file revision
    /// begins a fresh ordering epoch because the previous locator is stale.
    async fn put_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        state: &ReadingStateWrite,
    ) -> Result<ReadingState, StoreError>;
    /// Clear one edition's progress. Idempotent so Start over can be retried.
    async fn delete_reading_state(
        &self,
        user_id: i64,
        item_id: i64,
        file_id: i64,
    ) -> Result<(), StoreError>;
}

/// Trakt account links and the identity join sync needs.
#[async_trait]
pub trait TraktStore: Send + Sync + 'static {
    async fn get_trakt_auth(&self, user_id: i64) -> Result<Option<TraktAuth>, StoreError>;
    async fn list_trakt_auth(&self) -> Result<Vec<TraktAuth>, StoreError>;
    async fn put_trakt_auth(&self, auth: &TraktAuth) -> Result<(), StoreError>;
    async fn delete_trakt_auth(&self, user_id: i64) -> Result<(), StoreError>;
    /// Delete a rejected OAuth link only if no other voter has already
    /// rotated the refresh token.
    ///
    /// The compare-and-set operand is the stored envelope, not the cleartext
    /// token: every voter sees the same ciphertext for a given row, so this
    /// stays an exact equality check without any node unwrapping anything.
    async fn delete_trakt_auth_if_current(
        &self,
        user_id: i64,
        expected_refresh_token: &SealedSecret,
    ) -> Result<bool, StoreError>;
    /// Refresh bookkeeping after a token rotation. The caller seals the new
    /// tokens; the store never sees a bearer credential in the clear.
    async fn update_trakt_tokens(
        &self,
        user_id: i64,
        expected_refresh_token: &SealedSecret,
        access_token: &SealedSecret,
        refresh_token: &SealedSecret,
        expires_at: i64,
    ) -> Result<bool, StoreError>;
    /// Stamp a completed sync run (and the last_activities gate JSON).
    async fn set_trakt_sync(
        &self,
        user_id: i64,
        last_sync_at: i64,
        last_activities: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Every movie/episode with a Trakt-matchable identity, plus the user's
    /// watch row and a fallback duration — the sync planner's input.
    async fn trakt_sync_candidates(
        &self,
        user_id: i64,
    ) -> Result<Vec<crate::trakt::SyncCandidate>, StoreError>;
}

/// Scoped API keys — the machine credential, kept deliberately separate
/// from [`UserStore`] because a key is not a user and must never be able to
/// become one.
#[async_trait]
pub trait ApiKeyStore: Send + Sync + 'static {
    /// Store a key. Only the SHA-256 hash of the secret is persisted; the
    /// caller shows the plaintext once and then forgets it.
    async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        scopes: &[String],
    ) -> Result<crate::domain::ApiKey, StoreError>;
    async fn list_api_keys(&self) -> Result<Vec<crate::domain::ApiKey>, StoreError>;
    async fn api_key_for_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<crate::domain::ApiKey>, StoreError>;
    /// Record that a key was just used — the only way an operator can tell a
    /// forgotten key from a working one.
    async fn touch_api_key(&self, id: i64) -> Result<(), StoreError>;
    async fn delete_api_key(&self, id: i64) -> Result<bool, StoreError>;
    async fn set_api_key_disabled(&self, id: i64, disabled: bool) -> Result<bool, StoreError>;
}

/// One queued outbound notification.
#[derive(Clone, Debug)]
pub struct OutboxEntry {
    pub id: i64,
    pub payload: String,
    pub attempts: i64,
    pub last_error: String,
    /// pending | ok | failed
    pub status: String,
    pub next_at: i64,
    /// Lease deadline for the worker that fetched this row. A stale worker's
    /// settlement is ignored after another voter reclaims the delivery.
    pub claim_until: i64,
}

/// The outbox for watched notifications (master plan §11.1).
///
/// A table rather than a channel, because this is plurx's first *outbound*
/// push. Answering a request needs no durability — the caller is still there
/// to be told. Pushing is the opposite: the moment that matters is one where
/// the far side may be restarting, and nobody is waiting to retry for us.
#[async_trait]
pub trait WatchedOutboxStore: Send + Sync + 'static {
    async fn enqueue_watched(&self, payload: &str) -> Result<i64, StoreError>;
    async fn due_watched(&self, limit: i64) -> Result<Vec<OutboxEntry>, StoreError>;
    async fn settle_watched(&self, entry: &OutboxEntry) -> Result<(), StoreError>;
    /// `(pending, ok, failed)` — for the settings page and `/metrics`.
    async fn watched_outbox_counts(&self) -> Result<(i64, i64, i64), StoreError>;
}

/// The pre-transcode cache: what has been produced, and where a copy is.
///
/// Split into recipes and locations at the schema level because a cluster
/// needs to say "node A has this, node B had it and evicted it" (PERF-PLAN
/// §6.1). Nothing here exposes that split, because no caller has ever wanted
/// it: the question at every call site is "is there a usable copy" or "make a
/// note that there is one", and a two-table join is an implementation detail
/// of answering it.
#[async_trait]
pub trait TranscodeCacheStore: Send + Sync + 'static {
    /// A complete local copy of `recipe_hash`, if one exists. `None` covers
    /// both "never produced" and "produced but still being written", which are
    /// the same answer to the only question a player asks.
    async fn cache_hit(
        &self,
        recipe_hash: &str,
        node_id: &str,
    ) -> Result<Option<CachedTranscode>, StoreError>;

    /// Claim a directory for a recipe about to be produced.
    ///
    /// Returns whether THIS caller took the claim. `false` means somebody got
    /// there first and is either producing it now or has already finished, and
    /// the answer matters: without it a second producer cannot tell "I own
    /// this" from "somebody else does", so it either duplicates hours of encode
    /// or — worse — publishes over a directory another process is writing.
    /// Idempotent in the sense that matters: repeated claims never move the
    /// first one's directory.
    async fn claim_cache_entry(
        &self,
        recipe_hash: &str,
        file_id: i64,
        recipe_version: i64,
        node_id: &str,
        relative_dir: &str,
    ) -> Result<bool, StoreError>;

    /// Say that a claim is still being worked on.
    ///
    /// Distinct from [`TranscodeCacheStore::touch_cache_entry`], which records
    /// that somebody *watched* an entry and drives eviction. This records that
    /// a producer is still making one, and drives the opposite decision — the
    /// crash sweep. Sharing a timestamp would mean a producer's progress made
    /// its output look freshly watched, and eviction would start protecting
    /// something nobody has ever played.
    async fn touch_cache_claim(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError>;

    /// Mark a claim finished and serveable, with its measured size. Until this
    /// runs the entry is invisible to [`TranscodeCacheStore::cache_hit`].
    async fn complete_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        bytes: i64,
    ) -> Result<(), StoreError>;

    /// Note that somebody watched it — the LRU clock.
    async fn touch_cache_entry(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError>;

    /// Complete local entries, coldest first. What eviction walks.
    async fn cache_by_age(
        &self,
        node_id: &str,
        limit: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError>;

    /// Claims older than `older_than_unix` that never completed — a producer
    /// that died. Their directories are garbage and their rows are lies.
    async fn stale_cache_claims(
        &self,
        node_id: &str,
        older_than_unix: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError>;

    /// Every local cache row, complete or claimed, without eviction policy.
    /// The filesystem orphan pass needs storage ownership facts; filtered LRU
    /// and stale-candidate queries are deliberately the wrong truth for it.
    async fn all_cache_rows(&self, node_id: &str) -> Result<Vec<CachedTranscode>, StoreError>;

    /// Forget one storage copy. The recipe row goes too when its last copy
    /// does: a recipe nobody has is not a fact worth keeping on a single node,
    /// and on a cluster the other nodes' rows keep it alive.
    async fn forget_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        storage_class: &str,
    ) -> Result<(), StoreError>;

    /// Complete local bytes charged to the ordinary playback-cache budget.
    /// Offline-pinned recipes have their own admission budget and are excluded
    /// so a flight queue cannot evict the playback cache to make room.
    async fn cache_bytes(&self, node_id: &str) -> Result<i64, StoreError>;
}

/// Durable app-managed offline packages and their one renewable capability.
#[async_trait]
pub trait OfflinePackageStore: Send + Sync + 'static {
    /// Idempotency lookup, normalized-choice comparison, quota checks, and the
    /// insert happen under one transaction. Splitting them admits a tap-loop
    /// race even though SQLite access is mutexed per individual store call.
    async fn create_offline_package(
        &self,
        package: &NewOfflinePackage,
        max_rows_per_user: i64,
        max_bytes_per_user: i64,
        max_bytes_global: i64,
    ) -> Result<OfflineCreateOutcome, StoreError>;

    async fn offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<Option<OfflinePackage>, StoreError>;

    /// Authenticated status polling renews package interest and the same lease
    /// URL in place; it never rotates a platform downloader's cache identity.
    async fn renew_offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
        expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError>;

    /// Active preparation plus recently touched ready leases for the
    /// authenticated operator page. The limit is clamped by the store so an
    /// accidentally large caller value cannot turn polling into an unbounded
    /// read.
    async fn offline_activity_packages(
        &self,
        node_id: &str,
        now: i64,
        active_since: i64,
        limit: i64,
    ) -> Result<Vec<OfflineActivityPackage>, StoreError>;

    /// Current state and quota gauges. Aggregated in SQL so `/metrics` never
    /// has to load package rows or introduce labels from package data.
    async fn offline_package_stats(
        &self,
        node_id: &str,
        now: i64,
    ) -> Result<OfflinePackageStats, StoreError>;

    async fn reset_interrupted_offline_packages(&self, node_id: &str) -> Result<u64, StoreError>;

    async fn claim_next_offline_package(
        &self,
        node_id: &str,
    ) -> Result<Option<OfflinePackage>, StoreError>;

    /// `node_id` fences the yield back to the queue to the current owner, for
    /// the same reason [`Self::fail_offline_package`] is fenced: a re-homed
    /// package the survivor has already claimed must not be knocked back to
    /// `queued` by the departing node's producer finishing its last part.
    async fn requeue_offline_package(
        &self,
        package_id: &str,
        node_id: &str,
    ) -> Result<bool, StoreError>;

    /// Bind the content-addressed recipe as soon as production starts. The
    /// completed cache entry must become offline-owned before subtitle
    /// extraction and final publication can leave a gap for budget eviction.
    async fn set_offline_package_recipe(
        &self,
        package_id: &str,
        recipe_hash: &str,
    ) -> Result<bool, StoreError>;

    /// `node_id` fences progress to the current owner so a doomed producer on
    /// a departing node cannot flap the phase and percentage a survivor is
    /// reporting for the same package.
    async fn update_offline_progress(
        &self,
        package_id: &str,
        node_id: &str,
        phase: &str,
        progress_millis: i64,
    ) -> Result<bool, StoreError>;

    /// `node_id` fences the write to the current owner. Re-homing a package
    /// during node removal changes its owner while the old node's producer may
    /// still be running; without this guard that doomed producer's late
    /// failure would terminate work a survivor has already taken over.
    async fn fail_offline_package(
        &self,
        package_id: &str,
        node_id: &str,
        phase: &str,
        code: &str,
        message: &str,
    ) -> Result<bool, StoreError>;

    async fn put_offline_lease(
        &self,
        package_id: &str,
        user_id: i64,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<OfflineLeaseOutcome, StoreError>;

    /// A media read both authorizes and renews the stable URL. `None` covers
    /// wrong, revoked, expired, not-ready, and missing packages deliberately.
    async fn offline_package_for_lease(
        &self,
        token_hash: &str,
        now: i64,
        renewed_expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError>;

    /// `node_id` fences publication to the current owner for the same reason
    /// [`OfflinePackageStore::fail_offline_package`] does. A package re-homed
    /// mid-production must not be advertised ready by bytes that live on the
    /// node that just left.
    async fn mark_offline_package_ready(
        &self,
        package_id: &str,
        node_id: &str,
        recipe_hash: &str,
        actual_bytes: i64,
        duration_ms: i64,
    ) -> Result<bool, StoreError>;

    async fn delete_offline_package(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<bool, StoreError>;

    async fn expire_offline_packages(&self, now: i64) -> Result<u64, StoreError>;

    // --- Node removal (`CLUSTERING-PLAN.md` §6.7) -------------------------
    //
    // Removing a node must resolve the offline work it owns before the
    // membership change commits. A package's `source_path` replicates, but a
    // mount does not, so re-homing is only allowed against a survivor that
    // answered a probe by actually reading the snapshotted source. Everything
    // below exists to make that proof durable rather than assumed.
    //
    // The single-node SQLite backend has no removal path at all. Its
    // implementations are deliberately inert, and no SQLite table backs them.

    /// Every package the node still owns that removal has to resolve —
    /// `queued`, `preparing`, and `ready`. `failed` rows are terminal, hold no
    /// reservation, and are left for the ordinary expiry sweep.
    async fn unresolved_offline_packages(
        &self,
        node_id: &str,
    ) -> Result<Vec<OfflinePackage>, StoreError>;

    /// How many of the node's `ready` packages a client is fetching right now,
    /// measured the way the activity surface measures it. Removal refuses
    /// while a transfer is in flight rather than cutting a download off.
    async fn offline_transfers_in_flight(
        &self,
        node_id: &str,
        now: i64,
        active_since: i64,
    ) -> Result<i64, StoreError>;

    /// Ask each candidate node to prove it can read each package's source.
    /// Re-asking resets any previous answer: a mount that worked last week is
    /// not evidence about this removal.
    async fn request_offline_source_probes(
        &self,
        package_ids: &[String],
        node_ids: &[String],
        now: i64,
    ) -> Result<u64, StoreError>;

    /// Packages this node has been asked about and has not answered yet. The
    /// full package row travels because the answer requires its snapshotted
    /// path, size, and mtime — the probe row itself stores no media path.
    async fn pending_offline_source_probes(
        &self,
        node_id: &str,
        requested_since: i64,
    ) -> Result<Vec<OfflinePackage>, StoreError>;

    async fn answer_offline_source_probe(
        &self,
        package_id: &str,
        node_id: &str,
        readable: bool,
        now: i64,
    ) -> Result<bool, StoreError>;

    /// Probes asked at or after `requested_at` that nobody has answered yet.
    /// Removal waits on this reaching zero rather than on a particular node
    /// saying yes, so a unanimous "no" ends the wait immediately instead of
    /// burning the whole timeout on a package no survivor can take.
    async fn outstanding_offline_source_probes(&self, requested_at: i64)
        -> Result<i64, StoreError>;

    /// Nodes that answered "yes" to a probe asked at or after
    /// `requested_since`. Scoped by when the question was asked rather than
    /// when it was answered, because the asking node and the answering node
    /// keep different clocks and only the former's is consistent here.
    async fn verified_offline_source_nodes(
        &self,
        package_id: &str,
        requested_since: i64,
    ) -> Result<Vec<String>, StoreError>;

    /// Apply one whole removal plan in a single transaction, and report an
    /// error if any entry did not apply.
    ///
    /// The error is not a rollback, and this deliberately does not claim to be
    /// one: the transaction rolls back on a statement *error*, but an UPDATE
    /// that matches no row succeeds, so a package that moved underneath the
    /// plan leaves the rest of the plan applied. What keeps that safe is the
    /// caller, not the transaction — the membership change never commits on an
    /// error, the entries that did apply are individually correct resolutions
    /// (requeued work is claimable, failed work has released its reservation),
    /// and the operator's retry re-reads and finishes the job.
    ///
    /// Failing a package clears its reservation and completed size in the same
    /// statement: the bytes it accounted for lived on the departing node and
    /// reporting them as held would be a lie the operator cannot act on.
    async fn resolve_offline_packages_for_removal(
        &self,
        node_id: &str,
        plan: &[OfflineRemovalPlanEntry],
        now: i64,
    ) -> Result<OfflineRemovalReport, StoreError>;
}

/// Node-local playback telemetry.
///
/// Unlike catalogue and watch state, these rows describe what happened on one
/// machine and must never be submitted to Raft. The hiqlite implementation
/// therefore owns a separate versioned SQLite sidecar per voter.
#[async_trait]
pub trait PlaybackTelemetryStore: Send + Sync + 'static {
    async fn record_playback_event(&self, event: &PlaybackEvent) -> Result<i64, StoreError>;
    async fn prune_playback_events(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError>;
    async fn playback_events(
        &self,
        query: &PlaybackEventQuery,
    ) -> Result<Vec<PlaybackEvent>, StoreError>;
}

/// Bounded node-local network history derived from playback telemetry.
///
/// Like [`PlaybackTelemetryStore`], this data must not pass through Raft: a
/// prior describes the network observed by one server node.
#[async_trait]
pub trait NetworkPriorStore: Send + Sync + 'static {
    async fn observe_network_prior(
        &self,
        observation: &NetworkPriorObservation,
    ) -> Result<NetworkPrior, StoreError>;
    async fn network_prior(
        &self,
        credential_generation: &str,
        client_class: &str,
        network_fingerprint: &str,
    ) -> Result<Option<NetworkPrior>, StoreError>;
    async fn prune_network_priors(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError>;
}

/// The full storage boundary — what plurxd holds as `Arc<dyn Store>`.
pub trait Store:
    SettingsStore
    + UserStore
    + ApiKeyStore
    + LibraryStore
    + MediaStore
    + WatchStore
    + ReadingStore
    + TraktStore
    + WatchedOutboxStore
    + TranscodeCacheStore
    + OfflinePackageStore
    + PlaybackTelemetryStore
    + NetworkPriorStore
    + Send
    + Sync
    + 'static
{
}

impl<T> Store for T where
    T: SettingsStore
        + UserStore
        + ApiKeyStore
        + LibraryStore
        + MediaStore
        + WatchStore
        + ReadingStore
        + TraktStore
        + WatchedOutboxStore
        + TranscodeCacheStore
        + OfflinePackageStore
        + PlaybackTelemetryStore
        + NetworkPriorStore
        + Send
        + Sync
        + 'static
{
}

// Compile-time proof that the composed trait remains object-safe and that the
// production ownership shape can be constructed. If a future method makes
// `Store` non-object-safe, this function stops compiling before runtime tests.
#[allow(dead_code)]
fn assert_arc_dyn_store(store: std::sync::Arc<SqliteStore>) -> std::sync::Arc<dyn Store> {
    store
}

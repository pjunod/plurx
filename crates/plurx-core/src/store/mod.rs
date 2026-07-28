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
//! - A write acknowledged ⇒ durable (on a cluster: quorum-acked).
//! - Implementations are shared via `Arc`, never cloned per-request.

mod sqlite;

use std::path::PathBuf;

pub use sqlite::SqliteStore;

use async_trait::async_trait;

use crate::domain::{
    InProgressItem, Item, ItemEdit, ItemKind, ItemPage, ItemSort, Library, MediaFile,
    MetadataPatch, NewItem, NewLibrary, ProbeResult, RecentItem, TraktAuth, User, WatchRollup,
    WatchState,
};
// RecentItem is reused for next-up (episode + show title).
use crate::error::StoreError;

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
    /// When those last ran, unix seconds. Persisted rather than kept in memory
    /// so restarting the server doesn't restart the clock.
    pub const JOB_LAST_PROBE_RETRY: &str = "jobs.last_probe_retry";
    pub const JOB_LAST_TRANSCODE_CLEANUP: &str = "jobs.last_transcode_cleanup";
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
}

#[async_trait]
pub trait SettingsStore: Send + Sync + 'static {
    /// Cheap liveness probe of the backing storage (drives `/readyz`).
    async fn ping(&self) -> Result<(), StoreError>;
    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError>;
    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError>;
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
    async fn list_top_items(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
    ) -> Result<ItemPage, StoreError>;
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
    async fn items_needing_metadata(
        &self,
        library_id: Option<i64>,
        force: bool,
    ) -> Result<Vec<Item>, StoreError>;
    /// All episodes of a show (across seasons), for bulk episode enrichment.
    async fn episodes_for_show(&self, show_id: i64) -> Result<Vec<Item>, StoreError>;
    /// Home-library items whose artwork the local enricher should generate:
    /// folders, videos, and photos with no poster yet (`force` = all of them).
    /// Folders come last so they can inherit a child's finished poster.
    async fn items_needing_artwork(
        &self,
        library_id: i64,
        force: bool,
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
    async fn delete_files(&self, ids: &[i64]) -> Result<u64, StoreError>;
    /// Remove items left childless/file-less after a scan. Returns rows removed.
    async fn prune_empty_items(&self, library_id: i64) -> Result<u64, StoreError>;
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

/// Trakt account links and the identity join sync needs.
#[async_trait]
pub trait TraktStore: Send + Sync + 'static {
    async fn get_trakt_auth(&self, user_id: i64) -> Result<Option<TraktAuth>, StoreError>;
    async fn list_trakt_auth(&self) -> Result<Vec<TraktAuth>, StoreError>;
    async fn put_trakt_auth(&self, auth: &TraktAuth) -> Result<(), StoreError>;
    async fn delete_trakt_auth(&self, user_id: i64) -> Result<(), StoreError>;
    /// Refresh bookkeeping after a token rotation.
    async fn update_trakt_tokens(
        &self,
        user_id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: i64,
    ) -> Result<(), StoreError>;
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

/// The full storage boundary — what plurxd holds as `Arc<dyn Store>`.
pub trait Store:
    SettingsStore
    + UserStore
    + ApiKeyStore
    + LibraryStore
    + MediaStore
    + WatchStore
    + TraktStore
    + WatchedOutboxStore
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
        + TraktStore
        + WatchedOutboxStore
        + Send
        + Sync
        + 'static
{
}

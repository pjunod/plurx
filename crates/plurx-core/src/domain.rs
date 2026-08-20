//! Shared domain types. These are the server's internal shapes — API DTOs in
//! plurxd map from them, and no HTTP/serde-facing concern leaks in here except
//! serde derives for convenience.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::secrets::{CredentialKey, SealedSecret, Secret, SecretError};

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
    /// Text books and audiobooks. The scanner classifies each file by format,
    /// so one shelf can hold both without forcing the user to maintain two
    /// copies of the same directory tree.
    Books,
    /// Home video & photos: a folder tree of camera files. No metadata
    /// provider — the source of truth is the disk (folder layout, optional
    /// Kodi-style `.nfo` sidecars, embedded dates). See docs/HOMEVIDEO-PLAN.md.
    Home,
}

impl LibraryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Movies => "movies",
            LibraryKind::Shows => "shows",
            LibraryKind::Books => "books",
            LibraryKind::Home => "home",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movies" => Some(LibraryKind::Movies),
            "shows" => Some(LibraryKind::Shows),
            "books" => Some(LibraryKind::Books),
            "home" => Some(LibraryKind::Home),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Library {
    pub id: i64,
    pub name: String,
    pub kind: LibraryKind,
    pub paths: Vec<PathBuf>,
    /// A shows library flagged as anime: absolute episode numbering + AniList
    /// metadata (REQ-META-3). Always false for movie libraries.
    pub anime: bool,
    pub created_at: i64,
    /// Minutes between automatic scans; `0` is off, which is the default. Kept
    /// per library because a download folder and a finished archive deserve
    /// wildly different cadences.
    pub scan_interval_mins: i64,
    /// Minutes between automatic *full metadata refreshes* (the heavy job that
    /// re-fetches even matched items). `0` is off. Measured in the same unit as
    /// the scan interval, but sane values are days, not hours — it hits the
    /// provider for every item in the library.
    pub refresh_interval_mins: i64,
    /// When the last scan/refresh *finished*, unix seconds. Persisted rather
    /// than held in memory so a restart doesn't reset the schedule.
    pub last_scan_at: Option<i64>,
    pub last_refresh_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewLibrary {
    pub name: String,
    pub kind: LibraryKind,
    pub paths: Vec<PathBuf>,
    pub anime: bool,
}

// ---------------------------------------------------------------------------
// Items (movie | show | season | episode | book | audiobook | folder | video | photo)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Movie,
    Show,
    Season,
    Episode,
    /// A readable text/illustrated book file (EPUB, PDF, MOBI, comic archive,
    /// and related formats). Served as the original file; it has no playback
    /// progress timeline.
    Book,
    /// One audiobook work. It may own one long file (commonly M4B) or several
    /// chapter/part files; every file uses the normal audio probe and playback
    /// pipeline.
    Audiobook,
    /// A mirrored directory in a `home` library ("2019", "Beach Trip").
    /// Folders have no files of their own — they group other items.
    Folder,
    /// A home-video clip: plays exactly like a movie, but is titled from the
    /// filename and never matched against a provider.
    Video,
    /// A still image in a `home` library. Served as bytes, never transcoded.
    Photo,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Movie => "movie",
            ItemKind::Show => "show",
            ItemKind::Season => "season",
            ItemKind::Episode => "episode",
            ItemKind::Book => "book",
            ItemKind::Audiobook => "audiobook",
            ItemKind::Folder => "folder",
            ItemKind::Video => "video",
            ItemKind::Photo => "photo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(ItemKind::Movie),
            "show" => Some(ItemKind::Show),
            "season" => Some(ItemKind::Season),
            "episode" => Some(ItemKind::Episode),
            "book" => Some(ItemKind::Book),
            "audiobook" => Some(ItemKind::Audiobook),
            "folder" => Some(ItemKind::Folder),
            "video" => Some(ItemKind::Video),
            "photo" => Some(ItemKind::Photo),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub library_id: i64,
    pub kind: ItemKind,
    pub parent_id: Option<i64>,
    pub title: String,
    pub sort_title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub air_date: Option<String>,
    pub runtime_ms: Option<i64>,
    /// Relative paths under the artwork cache dir (never absolute).
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub added_at: i64,
    pub updated_at: i64,
    /// ISO-8601 date or datetime ("2019-06-14" / "2019-06-14T18:22:03") of
    /// when the footage/photo was captured. TEXT so it sorts
    /// lexicographically; filled by the home scanner's precedence ladder
    /// (NFO date → container creation_time / EXIF → filename → mtime).
    pub recorded_at: Option<String>,
    /// Free-form labels ("beach", "kids"). JSON array in SQLite, like
    /// `audio_streams`. Seeded from NFO `<tag>`/`<genre>`, edited in the UI.
    pub tags: Vec<String>,
    /// Unix seconds when an NFO sidecar was consumed for this item.
    /// `None` = never seeded (and eligible for seeding if a sidecar appears).
    /// Once set, the sidecar is dead to plurx — see docs/HOMEVIDEO-PLAN.md §4.3.
    pub nfo_seeded_at: Option<i64>,
    /// Unix seconds of the last artwork *download attempt*, and why it failed
    /// if it did (`None` = it didn't, or none has been made).
    ///
    /// These exist because `poster_path IS NULL` alone cannot tell "TMDB has
    /// no poster for this" from "the download 429'd once and nobody ever
    /// looked again". Without them a transient blip is indistinguishable
    /// from never-tried, and the retry sweep has nothing to key on.
    pub artwork_attempted_at: Option<i64>,
    pub artwork_error: Option<String>,
    /// Provider genres ("Action", "Science Fiction"), in the provider's own
    /// order. A JSON array in SQLite, exactly like `tags` — migration v13
    /// records why this is a column and not a join table.
    ///
    /// Empty is not "unknown". An item nobody has enriched and a film the
    /// provider files under nothing both read as `[]`; `metadata_at` is what
    /// says whether a provider has answered at all.
    pub genres: Vec<String>,
}

/// What the scanner knows when it first sees a file — enough to place the
/// item in the hierarchy. Metadata enrichment comes later.
#[derive(Debug, Clone)]
pub struct NewItem {
    pub library_id: i64,
    pub kind: ItemKind,
    pub parent_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
}

/// Metadata enrichment written by an agent (TMDB now; AniDB/AniList in
/// Phase 2). `None` means "leave as is"; `Some(None)` semantics are not
/// needed yet — agents only ever add or replace.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub air_date: Option<String>,
    pub runtime_ms: Option<i64>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    /// Capture date (home libraries). Same add-or-replace semantics as the
    /// rest: `None` leaves the stored value alone.
    pub recorded_at: Option<String>,
    /// Labels to set (home libraries). `None` leaves them alone; an empty
    /// vector is not a clear — use [`ItemEdit`] for that.
    pub tags: Option<Vec<String>>,
    /// Genres from the provider. `None` leaves the stored list alone, which
    /// is what every patch that is not an enrichment must do: a Trakt
    /// backfill or a caller-supplied id has no opinion about genres, and
    /// `Some(vec![])` from one of those would erase a good list.
    ///
    /// A provider that answers with no genres therefore sends `None`, not
    /// `Some(vec![])` — same rule `tags` follows, for the same reason.
    pub genres: Option<Vec<String>>,
    /// "A provider answered for this item" — stamps `metadata_at`, which is
    /// what takes the item out of the enrichment queue.
    ///
    /// Deliberately explicit rather than inferred from the patch's contents.
    /// A caller-supplied id (a monarr scan request) is a patch carrying only
    /// `tmdb_id`, and inferring "enriched" from that would mark the item done
    /// *because* it named an id — the exact opposite of what the id is for.
    pub enriched: bool,
    /// What happened when this patch's artwork was fetched, when the provider
    /// offered any. `None` = nothing was attempted, and the stored attempt
    /// columns are left alone.
    pub artwork: Option<ArtworkAttempt>,
}

/// The outcome of one artwork download.
///
/// The distinction that matters is *offered and failed* versus *never
/// offered*: only the first is worth coming back for. `poster_path` cannot
/// carry it — both are `None` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtworkAttempt {
    /// The provider offered an image and it is now in the artwork cache.
    Stored,
    /// The provider offered an image and it did not land. The string is why,
    /// kept for the operator: "the poster is missing" is a symptom, "429 from
    /// TMDB" is the cause.
    Failed(String),
}

impl MetadataPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.year.is_none()
            && self.overview.is_none()
            && self.tmdb_id.is_none()
            && self.imdb_id.is_none()
            && self.air_date.is_none()
            && self.runtime_ms.is_none()
            && self.poster_path.is_none()
            && self.backdrop_path.is_none()
            && self.recorded_at.is_none()
            && self.tags.is_none()
            && self.genres.is_none()
            && !self.enriched
            && self.artwork.is_none()
    }
}

/// A hand edit of one item's metadata (home libraries only — see
/// docs/HOMEVIDEO-PLAN.md §2). Unlike [`MetadataPatch`], which agents use to
/// add or replace, an edit must be able to *clear* a field: the outer
/// `Option` is "present in the request", the inner one is the new value.
#[derive(Debug, Clone, Default)]
pub struct ItemEdit {
    /// Never empty when present — an empty title is rejected by the caller.
    pub title: Option<String>,
    pub overview: Option<Option<String>>,
    pub recorded_at: Option<Option<String>>,
    pub year: Option<Option<i32>>,
    pub tags: Option<Vec<String>>,
}

impl ItemEdit {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.overview.is_none()
            && self.recorded_at.is_none()
            && self.year.is_none()
            && self.tags.is_none()
    }
}

/// Compute the sort title: lowercase, leading articles stripped.
pub fn sort_title_for(title: &str) -> String {
    let lower = title.to_lowercase();
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = lower.strip_prefix(article) {
            if !rest.is_empty() {
                return rest.to_owned();
            }
        }
    }
    lower
}

// ---------------------------------------------------------------------------
// Media files & streams
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioStream {
    /// Index among audio streams (ffmpeg `a:{n}` ordering), not global index.
    pub index: i64,
    pub codec: String,
    pub channels: Option<i64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SubtitleStream {
    pub index: i64,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// The container's `hearing_impaired` disposition — SDH / closed
    /// captions, the track that also transcribes the door slam.
    ///
    /// `#[serde(default)]` because probe JSON stored before this field
    /// existed has no key for it, and a library is not re-probed just
    /// because plurx learned to read one more flag: an old row deserializes
    /// as `false` and falls back to the title sniff it always used.
    #[serde(default)]
    pub hearing_impaired: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaFile {
    pub id: i64,
    pub item_id: i64,
    pub path: PathBuf,
    pub size: i64,
    pub mtime: i64,
    pub duration_ms: Option<i64>,
    /// Container short name derived from the file extension ("mkv", "mp4").
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bit_depth: Option<i64>,
    /// "hdr10" | "hlg" | "dolby_vision" | None (SDR/unknown). Coarse type the
    /// decision engine keys on.
    pub hdr: Option<String>,
    /// Human HDR label with detail for display: "Dolby Vision · Profile 7
    /// (HDR10-compatible)", "HDR10+", "HLG". None when `hdr` is None.
    pub hdr_format: Option<String>,
    pub bitrate: Option<i64>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    pub scanned_at: i64,
    /// Manual A/V sync correction, milliseconds; positive delays audio.
    /// Applied server-side at stream time (forces remux for direct-play
    /// sources). Persisted per file — a bad mux stays fixed. 0 = none.
    pub audio_offset_ms: i64,
    /// Did ffprobe ever succeed on this file? `false` means the record is a
    /// placeholder — no codec, no duration, no tracks — and playback decisions
    /// for it are guesses. It is also the retry signal: a file that failed on
    /// permissions keeps its size and mtime when the permissions are fixed, so
    /// nothing else would ever mark it as worth looking at again.
    pub probed: bool,
}

/// Everything the prober learned about one file.
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bit_depth: Option<i64>,
    pub hdr: Option<String>,
    pub hdr_format: Option<String>,
    pub bitrate: Option<i64>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    /// Raw ffprobe JSON, kept verbatim for future decision-engine needs.
    pub raw_json: Option<String>,
    /// Container capture time (`format.tags.creation_time`), normalized to a
    /// local-naive ISO-8601 string. Phones and camcorders set it, which makes
    /// it the best home-video date short of an NFO — see
    /// docs/HOMEVIDEO-PLAN.md §4.4.
    pub creation_time: Option<String>,
}

// ---------------------------------------------------------------------------
// Users & auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub is_admin: bool,
    pub created_at: i64,
}

/// A machine credential: scopes, no user, no admin flag.
///
/// Deliberately not a `User`. A login token IS a user and carries that
/// user's privileges wholesale — which is how "let monarr trigger scans"
/// would otherwise become "let monarr read every secret plurx holds". A key
/// can do exactly what its scopes say and cannot widen itself.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    /// SHA-256 of the secret. Never serialized: the plaintext is shown once,
    /// at creation, and the hash is not something an API should hand back.
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub disabled: bool,
}

impl ApiKey {
    /// Whether this key may do `scope`. A disabled key may do nothing —
    /// checked here rather than at each call site, because a revocation that
    /// depends on every caller remembering to check is not a revocation.
    pub fn allows(&self, scope: &str) -> bool {
        !self.disabled && self.scopes.iter().any(|s| s == scope)
    }
}

/// Scopes a key can hold. Small and closed on purpose: every scope is a
/// promise about what a stolen key can do, so they are added one considered
/// grant at a time rather than invented at call sites.
pub mod scopes {
    /// Ask for a scan of a path. The whole point of the monarr integration.
    pub const SCAN_TRIGGER: &str = "scan:trigger";
    /// Read the progress/result of a scan this key asked for.
    pub const STATUS_READ: &str = "status:read";

    /// Every scope that exists, for validation at creation time — a key
    /// created with a typo'd scope would otherwise look fine and silently
    /// authorize nothing.
    pub const ALL: &[&str] = &[SCAN_TRIGGER, STATUS_READ];
}

// ---------------------------------------------------------------------------
// Watch state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct WatchState {
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
    pub watched: bool,
    pub updated_at: i64,
}

/// How much of a container has been seen: how many playable leaves sit under
/// an item, and how many of those are watched.
///
/// A show has no watch state of its own — "I've seen this series" is a claim
/// about its episodes. A rollup is the only honest way to label a container,
/// and it is what lets a button read *Mark unwatched* on a series you have
/// finished instead of forever offering to mark it watched again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct WatchRollup {
    /// Playable descendants — movies, episodes, audiobooks, home videos.
    /// Text books, photos, and containers don't count; they have no timed
    /// playback progress.
    pub leaves: i64,
    pub watched: i64,
}

impl WatchRollup {
    /// Every leaf seen, and there is at least one. An empty container is not
    /// "fully watched"; it is nothing at all.
    pub fn complete(self) -> bool {
        self.leaves > 0 && self.watched == self.leaves
    }
}

/// A cached transcode, as the server needs to know it.
///
/// One row of `transcode_cache_locations` joined to the recipe it belongs to —
/// which is the only shape a caller ever wants. Identity and location are
/// separate *tables* because a cluster needs them to be (PERF-PLAN §6.1); they
/// are not separate *questions* at the point of use, where the question is
/// always "is there a usable copy of this, and where".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedTranscode {
    pub recipe_hash: String,
    pub file_id: i64,
    /// Which byte store owns this copy. Deletion is scoped to this value so a
    /// stale local claim cannot remove a future shared-storage location for
    /// the same recipe and node.
    pub storage_class: String,
    /// Under the configured cache root — never absolute, because an absolute
    /// path is a fact about one machine's mounts and this row is meant to
    /// travel between machines that do not share them.
    pub relative_dir: String,
    pub bytes: i64,
    /// A partial entry is a producer that died. Nothing may serve one.
    pub complete: bool,
    pub last_used_at: i64,
}

/// A durable request for an app-managed offline HLS package.
///
/// This is storage-domain state, not an HTTP DTO. In particular, errors stay
/// as stable machine codes here and the API decides how much text to expose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OfflinePackage {
    pub id: String,
    pub request_id: String,
    pub user_id: i64,
    pub file_id: i64,
    pub node_id: String,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    pub recipe_hash: Option<String>,
    /// Effective encoder rate control captured when the request was created.
    /// A queued package may yield and resume after the global setting changes,
    /// so its recipe must never be rebuilt from mutable policy.
    pub effective_rate_control: String,
    pub target_height: i64,
    /// Exact even-sized output frame computed with the transcoder's scaler
    /// arithmetic. Optional only for legacy/unprobed sources.
    pub output_width: Option<i64>,
    pub output_height: Option<i64>,
    pub audio_index: Option<i64>,
    pub audio_offset_ms: i64,
    pub subtitle_index: Option<i64>,
    /// Snapshotted container language for the one native HLS rendition. The
    /// capability route cannot depend on a file row surviving a later rescan.
    pub subtitle_language: Option<String>,
    pub subtitle_mode: String,
    pub state: String,
    pub phase: String,
    pub progress_millis: i64,
    pub estimated_bytes: i64,
    pub reserved_bytes: i64,
    pub actual_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_access_at: i64,
    pub expires_at: i64,
}

/// One package that belongs on the operator activity surface.
///
/// `lease_active` is deliberately derived by the store from a recent,
/// unexpired lease touch. It is not persisted package state: a ready package
/// is only "sending" while a downloader is still fetching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineActivityPackage {
    pub package: OfflinePackage,
    pub lease_active: bool,
}

/// Fixed-cardinality offline gauges for one server node.
///
/// Bytes use the completed size when it exists and the conservative
/// reservation otherwise, matching quota accounting. Named fields keep the
/// only valid state vocabulary explicit all the way to Prometheus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OfflinePackageStats {
    pub queued: i64,
    pub preparing: i64,
    pub ready: i64,
    pub failed: i64,
    pub queued_bytes: i64,
    pub preparing_bytes: i64,
    pub ready_bytes: i64,
    pub failed_bytes: i64,
    pub active_leases: i64,
    pub pinned_bytes: i64,
}

/// The stable failure code a package carries when the node that owned it left
/// the cluster and no survivor could prove it reads the same source.
///
/// Clients treat this exactly like any other terminal failure: the package is
/// gone, its reservation is released, and creating a fresh request is the
/// retry. It is a distinct code so a client can say "the server that was
/// preparing this went away" rather than blaming the media.
pub const OFFLINE_NODE_REMOVED_CODE: &str = "node_removed";

/// What node removal decided about one package the departing node owned.
///
/// `CLUSTERING-PLAN.md` §6.7 allows exactly two outcomes, and this type makes
/// the caller name one of them. There is deliberately no "leave it alone"
/// variant: a package left owned by a node that no longer exists is the
/// stranded work the milestone exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineRemovalPlanEntry {
    pub package_id: String,
    /// `Some(node)` requeues on a survivor that positively verified it can
    /// read this package's exact snapshotted source. `None` fails the package
    /// with [`OFFLINE_NODE_REMOVED_CODE`] and releases its reservation.
    ///
    /// A replicated `source_path` is not that proof, so this field may only be
    /// filled from an answered source probe.
    pub requeue_to: Option<String>,
}

/// What one removal actually did to the departing node's offline work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OfflineRemovalReport {
    pub requeued: u64,
    pub failed: u64,
}

/// Validated values inserted as one transaction with idempotency and quota
/// checks. Source identity is copied so a rescan cannot silently retarget a
/// queued job at different bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOfflinePackage {
    pub id: String,
    pub request_id: String,
    pub user_id: i64,
    pub file_id: i64,
    pub node_id: String,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    /// Canonical [`crate::transcode::EffectiveRateControl::snapshot_value`].
    pub effective_rate_control: String,
    pub target_height: i64,
    pub output_width: Option<i64>,
    pub output_height: Option<i64>,
    pub audio_index: Option<i64>,
    pub audio_offset_ms: i64,
    pub subtitle_index: Option<i64>,
    pub subtitle_language: Option<String>,
    pub subtitle_mode: String,
    pub estimated_bytes: i64,
    pub reserved_bytes: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineCreateOutcome {
    Created(OfflinePackage),
    Existing(OfflinePackage),
    RequestConflict,
    RowLimit {
        limit: i64,
    },
    ByteLimit {
        used: i64,
        limit: i64,
    },
    GlobalByteLimit {
        used: i64,
        limit: i64,
    },
    /// The requesting node has been removed from the cluster; its `removed_at`
    /// tombstone is set in `cluster_nodes`. A removed-but-still-running node
    /// cannot create new offline packages. Single-node SQLite never returns
    /// this variant because it has no removal path and no `cluster_nodes`
    /// table.
    NodeIsTombstone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OfflineLease {
    pub package_id: String,
    pub token_hash: String,
    pub created_at: i64,
    pub last_access_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineLeaseOutcome {
    Created(OfflineLease),
    Renewed(OfflineLease),
    PackageNotReady,
    TokenConflict,
}

/// An in-progress item for the continue-watching row.
#[derive(Debug, Clone, Serialize)]
pub struct InProgressItem {
    pub item: Item,
    /// For episodes: the show title, so clients can label the card.
    pub show_title: Option<String>,
    /// For episodes: the season's poster, so rail cards can show the season
    /// artwork instead of a (often arbitrary) per-episode still.
    pub season_poster: Option<String>,
    pub state: WatchState,
}

/// A recently added row entry (episodes carry their show title).
#[derive(Debug, Clone, Serialize)]
pub struct RecentItem {
    pub item: Item,
    pub show_title: Option<String>,
    /// For episodes: the season's poster (see [`InProgressItem::season_poster`]).
    pub season_poster: Option<String>,
}

// ---------------------------------------------------------------------------
// Browse queries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemSort {
    #[default]
    Title,
    Added,
    Year,
    /// Highest video resolution first (by the item's best file); items with no
    /// probed height sort last.
    Resolution,
    /// Capture date, newest first; items with no `recorded_at` sort last.
    /// The natural default for home libraries.
    Recorded,
}

impl ItemSort {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "title" => Some(ItemSort::Title),
            "added" => Some(ItemSort::Added),
            "year" => Some(ItemSort::Year),
            "resolution" => Some(ItemSort::Resolution),
            "recorded" => Some(ItemSort::Recorded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ItemPage {
    pub items: Vec<Item>,
    pub total: i64,
}

/// What a library is actually made of, in the terms the transcoder cares about.
///
/// Exists to answer a question PERF-PLAN §5 could not. The GPU tone-map passed
/// its probe at ~5× the CPU chain, but it is declined for Dolby Vision with
/// no HDR10-compatible base layer — the vendor filter cannot read the dynamic
/// metadata, and only a compatible base gives it plain PQ it can map (see
/// `transcode::routing_hdr`). "The graph is accepted" and "the graph helps
/// *this* library" are different claims, and only a census of what is on disk
/// settles the second.
///
/// Counts, deliberately, not a list. Nobody needs to know which files; they
/// need to know whether the 4K HDR they own is mostly the kind the fast path
/// can reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaShape {
    /// Files with a successful probe. Everything below is a subset — an
    /// unprobed file has no codec, no height and no HDR flavour, and counting
    /// it in a denominator would understate every percentage.
    pub probed: i64,
    /// Files nothing has ever read. Reported because a large number here makes
    /// the rest of this a sample rather than a census.
    pub unprobed: i64,
    /// `sdr` | `hdr10` | `hlg` | `dolby_vision` → count, over probed files.
    pub hdr: Vec<(String, i64)>,
    /// The same split, restricted to 4K. This is the set M2's tone-map exists
    /// for: at 1080p the CPU chain is fast enough that the question does not
    /// arise.
    pub hdr_4k: Vec<(String, i64)>,
    /// Video codec → count.
    pub codecs: Vec<(String, i64)>,
    /// Files at or above the former 40 Mb/s progressive-remux floor
    /// (§4.3bis). This remains the high-bitrate stress population after the
    /// route widened to every probed remux.
    pub over_segmented_floor: i64,
    pub max_bitrate: Option<i64>,
}

/// One persisted playback observation, joined with node-local server truth at
/// ingest when the client names a live session.
///
/// `id = 0` means an unpersisted event; stores assign and return the durable
/// row id. Optional columns are intentional: client and server emitters know
/// different facts, and absence must remain distinguishable from zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlaybackEvent {
    pub id: i64,
    pub at_unix_ms: i64,
    pub user_id: Option<i64>,
    pub session_id: Option<String>,
    pub file_id: Option<i64>,
    pub event: String,
    pub level: Option<String>,
    pub method: Option<String>,
    pub encoder: Option<String>,
    pub height: Option<i64>,
    pub ms: Option<i64>,
    pub runway_ds: Option<i64>,
    pub bandwidth_kbps: Option<i64>,
    pub speed_recent: Option<f64>,
    pub ahead_seconds: Option<i64>,
    pub suspended: Option<bool>,
    pub hold_reason: Option<String>,
    pub delivered_bps: Option<i64>,
    /// Effective ffmpeg input pace, as a multiple of realtime. `0` means the
    /// session is unpaced; a legacy `-re` session records `1`.
    pub readrate: Option<f64>,
    pub detail: Option<String>,
    pub attempt: Option<String>,
    pub reason: Option<String>,
    pub ua: Option<String>,
    pub extra: Option<String>,
}

/// Bounded query for the operator telemetry reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackEventQuery {
    pub since_ms: Option<i64>,
    pub event: Option<String>,
    pub limit: i64,
}

/// Node-local playback history for one coarse client/network tuple.
///
/// The fingerprint is deliberately opaque outside the storage boundary. It is
/// derived from a coarse client class and an IPv4 /24, never a full address,
/// and must not be returned by an API or written to application logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPrior {
    pub user_id: i64,
    pub client_class: String,
    pub network_fingerprint: String,
    /// Conservative sustained throughput estimate, in decimal kilobits/s.
    pub sustained_kbps: Option<u32>,
    /// Lowest ladder height at which a supply/network stall was observed,
    /// within the verdict's lifetime. Always written together with
    /// [`NetworkPrior::starved_at_ms`].
    pub worst_rung_height: Option<i64>,
    /// When the most recent starvation was observed. The verdict ages out
    /// from this stamp, not from `updated_at_ms`, which every healthy
    /// observation refreshes.
    pub starved_at_ms: Option<i64>,
    pub sample_count: u32,
    pub updated_at_ms: i64,
}

/// How long a supply-starvation verdict is believed after the starvation that
/// produced it.
///
/// Without a horizon the verdict is a monotonic `min` that nothing can raise:
/// one transient stall — a roommate's download, a brief Wi-Fi dropout — caps
/// that tuple one rung down for as long as the row lives, and because every
/// observation refreshes `updated_at_ms`, an actively used row never reaches
/// retention either. A week of stall-free operation is enough evidence that
/// the network changed, so the verdict expires and the throughput estimate
/// decides alone. A link that is genuinely still starving re-arms it on the
/// next stall.
pub const NETWORK_PRIOR_STARVED_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

impl NetworkPrior {
    /// The starvation verdict if it is still recent enough to believe.
    ///
    /// A verdict with no stamp is treated as expired: the two are written
    /// together, so the only way to see one is a row this build did not write,
    /// and "forget it" is the recovering answer rather than the permanent one.
    pub fn active_starved_rung(&self, now_ms: i64) -> Option<i64> {
        let height = self.worst_rung_height.filter(|height| *height > 0)?;
        let starved_at_ms = self.starved_at_ms?;
        (now_ms.saturating_sub(starved_at_ms) <= NETWORK_PRIOR_STARVED_TTL_MS).then_some(height)
    }
}

/// One telemetry-derived update to a [`NetworkPrior`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPriorObservation {
    pub user_id: i64,
    pub client_class: String,
    pub network_fingerprint: String,
    pub throughput_kbps: Option<u32>,
    pub starved_rung_height: Option<i64>,
    pub observed_at_ms: i64,
}

/// A user's linked Trakt account (tokens + sync bookkeeping).
///
/// The two bearer credentials are [`SealedSecret`]s, not strings, because this
/// struct is exactly what crosses the `Store` boundary into a durable — and,
/// once M2 activates, replicated — row. A store implementation therefore has
/// no cleartext to write even by accident; recovering one costs a call to
/// [`reveal_access_token`](TraktAuth::reveal_access_token) with the node-local
/// key. See `secrets` and CLUSTERING-PLAN.md §3.2.
#[derive(Debug, Clone)]
pub struct TraktAuth {
    pub user_id: i64,
    pub access_token: SealedSecret,
    pub refresh_token: SealedSecret,
    /// Unix seconds when the access token expires (refresh happens earlier).
    pub expires_at: i64,
    pub trakt_username: Option<String>,
    pub connected_at: i64,
    /// Unix seconds of the last completed sync run (0 = never).
    pub last_sync_at: i64,
    /// Raw `/sync/last_activities` JSON from the last run — an opaque change
    /// gate: identical JSON and nothing local to push means the pull can skip.
    pub last_activities: Option<String>,
}

impl TraktAuth {
    /// Recover the access token for an outbound Trakt call.
    ///
    /// The user id is authenticated with the ciphertext, so this cannot be
    /// tricked into opening another user's credential by rewriting a row's
    /// `user_id`.
    pub fn reveal_access_token(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        key.open_trakt(self.user_id, &self.access_token)
    }

    /// Recover the refresh token for an outbound rotation.
    pub fn reveal_refresh_token(&self, key: &CredentialKey) -> Result<Secret, SecretError> {
        key.open_trakt(self.user_id, &self.refresh_token)
    }

    /// True once both bearer columns are envelopes rather than cleartext.
    pub fn is_wrapped(&self) -> bool {
        self.access_token.is_wrapped() && self.refresh_token.is_wrapped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_title_strips_articles() {
        assert_eq!(sort_title_for("The Matrix"), "matrix");
        assert_eq!(sort_title_for("A Quiet Place"), "quiet place");
        assert_eq!(sort_title_for("An American Tail"), "american tail");
        assert_eq!(sort_title_for("Heat"), "heat");
        // Degenerate: the whole title is an article-ish word.
        assert_eq!(sort_title_for("The "), "the ");
    }
}

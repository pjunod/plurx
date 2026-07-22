//! Shared domain types. These are the server's internal shapes — API DTOs in
//! plurxd map from them, and no HTTP/serde-facing concern leaks in here except
//! serde derives for convenience.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
}

impl LibraryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Movies => "movies",
            LibraryKind::Shows => "shows",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movies" => Some(LibraryKind::Movies),
            "shows" => Some(LibraryKind::Shows),
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
}

#[derive(Debug, Clone)]
pub struct NewLibrary {
    pub name: String,
    pub kind: LibraryKind,
    pub paths: Vec<PathBuf>,
    pub anime: bool,
}

// ---------------------------------------------------------------------------
// Items (movie | show | season | episode)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Movie,
    Show,
    Season,
    Episode,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Movie => "movie",
            ItemKind::Show => "show",
            ItemKind::Season => "season",
            ItemKind::Episode => "episode",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(ItemKind::Movie),
            "show" => Some(ItemKind::Show),
            "season" => Some(ItemKind::Season),
            "episode" => Some(ItemKind::Episode),
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

// ---------------------------------------------------------------------------
// Watch state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct WatchState {
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
    pub watched: bool,
    pub updated_at: i64,
}

/// An in-progress item for the continue-watching row.
#[derive(Debug, Clone, Serialize)]
pub struct InProgressItem {
    pub item: Item,
    /// For episodes: the show title, so clients can label the card.
    pub show_title: Option<String>,
    pub state: WatchState,
}

/// A recently added row entry (episodes carry their show title).
#[derive(Debug, Clone, Serialize)]
pub struct RecentItem {
    pub item: Item,
    pub show_title: Option<String>,
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
}

impl ItemSort {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "title" => Some(ItemSort::Title),
            "added" => Some(ItemSort::Added),
            "year" => Some(ItemSort::Year),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ItemPage {
    pub items: Vec<Item>,
    pub total: i64,
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

//! API data-transfer objects. Domain types map to these so the wire format is
//! stable independent of storage shape, and so image paths become URLs and
//! watch state can be attached per user.

use plurx_core::domain::{
    AudioStream, InProgressItem, Item, ItemKind, Library, MediaFile, RecentItem, SubtitleStream,
    User, WatchState,
};
use serde::Serialize;

/// Build the API URL for a cached artwork filename.
fn image_url(filename: &Option<String>) -> Option<String> {
    filename.as_ref().map(|f| format!("/api/v1/images/{f}"))
}

#[derive(Serialize)]
pub struct WatchDto {
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
    pub watched: bool,
    pub updated_at: i64,
}

impl From<WatchState> for WatchDto {
    fn from(w: WatchState) -> Self {
        WatchDto {
            position_ms: w.position_ms,
            duration_ms: w.duration_ms,
            watched: w.watched,
            updated_at: w.updated_at,
        }
    }
}

#[derive(Serialize)]
pub struct ItemDto {
    pub id: i64,
    pub library_id: i64,
    pub kind: ItemKind,
    pub parent_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub air_date: Option<String>,
    pub runtime_ms: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchDto>,
}

impl From<Item> for ItemDto {
    fn from(item: Item) -> Self {
        ItemDto {
            id: item.id,
            library_id: item.library_id,
            kind: item.kind,
            parent_id: item.parent_id,
            title: item.title,
            year: item.year,
            overview: item.overview,
            season_number: item.season_number,
            episode_number: item.episode_number,
            air_date: item.air_date,
            runtime_ms: item.runtime_ms,
            tmdb_id: item.tmdb_id,
            imdb_id: item.imdb_id,
            poster: image_url(&item.poster_path),
            backdrop: image_url(&item.backdrop_path),
            show_title: None,
            watch: None,
        }
    }
}

impl ItemDto {
    pub fn with_watch(mut self, watch: Option<WatchState>) -> Self {
        self.watch = watch.map(Into::into);
        self
    }

    pub fn with_show_title(mut self, show_title: Option<String>) -> Self {
        self.show_title = show_title;
        self
    }
}

pub fn recent_dto(recent: RecentItem, watch: Option<WatchState>) -> ItemDto {
    ItemDto::from(recent.item)
        .with_show_title(recent.show_title)
        .with_watch(watch)
}

pub fn in_progress_dto(item: InProgressItem) -> ItemDto {
    ItemDto::from(item.item)
        .with_show_title(item.show_title)
        .with_watch(Some(item.state))
}

#[derive(Serialize)]
pub struct FileDto {
    pub id: i64,
    pub filename: String,
    pub size: i64,
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bit_depth: Option<i64>,
    pub hdr: Option<String>,
    pub bitrate: Option<i64>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    /// Whether the file is actually readable on the server right now. `false`
    /// means the path no longer resolves (unmounted share, moved/deleted file,
    /// wrong container mount) — the client shows this and refuses to "play"
    /// something that isn't there. Set by the handler, not from the row.
    pub available: bool,
    /// Full server-side path, shown to admins when a file is missing so they
    /// can fix the mount. Only populated for missing files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_path: Option<String>,
}

impl From<MediaFile> for FileDto {
    fn from(f: MediaFile) -> Self {
        let filename = f
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        FileDto {
            id: f.id,
            filename,
            size: f.size,
            duration_ms: f.duration_ms,
            container: f.container,
            video_codec: f.video_codec,
            video_profile: f.video_profile,
            width: f.width,
            height: f.height,
            bit_depth: f.bit_depth,
            hdr: f.hdr,
            bitrate: f.bitrate,
            audio_streams: f.audio_streams,
            subtitle_streams: f.subtitle_streams,
            available: true,
            missing_path: None,
        }
    }
}

#[derive(Serialize)]
pub struct LibraryDto {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub paths: Vec<String>,
    pub anime: bool,
    pub created_at: i64,
}

impl From<Library> for LibraryDto {
    fn from(l: Library) -> Self {
        LibraryDto {
            id: l.id,
            name: l.name,
            kind: l.kind.as_str().to_owned(),
            paths: l
                .paths
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            anime: l.anime,
            created_at: l.created_at,
        }
    }
}

#[derive(Serialize)]
pub struct UserDto {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    pub created_at: i64,
}

impl From<User> for UserDto {
    fn from(u: User) -> Self {
        UserDto {
            id: u.id,
            username: u.username,
            is_admin: u.is_admin,
            created_at: u.created_at,
        }
    }
}

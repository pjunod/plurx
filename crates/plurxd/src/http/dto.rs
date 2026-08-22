//! API data-transfer objects. Domain types map to these so the wire format is
//! stable independent of storage shape, and so image paths become URLs and
//! watch state can be attached per user.

use plurx_core::domain::{
    AudioStream, InProgressItem, Item, ItemKind, Library, MediaFile, ReadingState, RecentItem,
    SubtitleStream, User, WatchRollup, WatchState,
};
use plurx_core::mediafacts::MediaFacts;
use plurx_core::tracks::{
    lang_matches, prefers_original_audio, select_tracks, LangPrefs, TrackSelection,
};
use serde::{Deserialize, Serialize};

/// Build the API URL for a cached artwork filename.
///
/// Artwork filenames historically contain only the item id, so importing a
/// catalog or refreshing a poster can put different bytes behind the same
/// path. Native image loaders and browsers are then entitled to keep the old
/// response for its full cache lifetime. The replicated item revision changes
/// with every metadata/artwork patch and makes that mutable filename a new
/// cache identity without exposing node-local filesystem state in the API.
fn image_url(filename: &Option<String>, revision: i64) -> Option<String> {
    filename
        .as_ref()
        .map(|f| format!("/api/v1/images/{f}?v={revision}"))
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct RevisionDto {
    pub size: i64,
    pub mtime: i64,
}

#[derive(Serialize)]
pub struct ReadingDto {
    pub file_id: i64,
    pub revision: RevisionDto,
    pub locator: serde_json::Value,
    pub progression: f64,
    pub completed: bool,
    pub updated_at: i64,
}

impl TryFrom<ReadingState> for ReadingDto {
    type Error = serde_json::Error;

    fn try_from(state: ReadingState) -> Result<Self, Self::Error> {
        Ok(Self {
            file_id: state.file_id,
            revision: RevisionDto {
                size: state.file_size,
                mtime: state.file_mtime,
            },
            locator: serde_json::from_str(&state.locator_json)?,
            progression: state.progression_millis as f64 / 1_000_000.0,
            completed: state.completed,
            updated_at: state.updated_at,
        })
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
    /// Stable ordering fields for native clients that merge several library
    /// shares into one Movies / TV category. Without these, each share is
    /// sorted correctly but the combined grid cannot be.
    pub added_at: i64,
    pub updated_at: i64,
    /// When the footage/photo was captured (home libraries). ISO-8601 date or
    /// datetime; sorts lexicographically.
    pub recorded_at: Option<String>,
    /// Free-form labels (home libraries).
    pub tags: Vec<String>,
    /// Provider genres ("Action", "Science Fiction"). Always present, empty
    /// when unknown — a client that predates this field ignores it, and one
    /// that knows about it never has to distinguish "absent" from "none".
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_work_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_edition_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_metadata_source: Option<String>,
    pub tmdb_id: Option<i64>,
    pub imdb_id: Option<String>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    /// Best (max) file height for the item, e.g. 2160 / 1080 / 720. Populated
    /// on library grids and playable season children so compact cards can show
    /// a resolution badge. `None` for shows and where unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<i64>,
    /// Aggregated facts about the files behind this item — codec, dynamic
    /// range, audio, size on disk. Populated on the library list when the
    /// caller asks (`?facts=1`) and on playable children in item detail, where
    /// a season needs to describe its episode rows without an N+1 of episode
    /// detail requests. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaDto>,
    /// How many items a folder holds — the "12 items" line on a folder card.
    /// Only populated on grids, and only for folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchDto>,
    /// How many playable things sit under this item and how many are watched.
    /// Only populated on the detail response, and only for containers — a
    /// show has no `watch` of its own, so this is the only thing a client can
    /// label a "mark watched / unwatched" control from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollup: Option<RollupDto>,
}

/// The `media` block: what a list row can say about an item's files without
/// asking for them one item at a time.
///
/// `files`/`bytes` cover every file; the rest describe the item's *best* file
/// and only that file — the rule, and the reason a union would be a lie, is
/// on [`plurx_core::mediafacts::MediaFacts`]. Null fields are dropped rather
/// than serialized: this is badge material, absent means "no badge", and a
/// grid of 200 items should not carry 200 rows of nulls.
#[derive(Serialize)]
pub struct MediaDto {
    pub files: i64,
    pub bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// Source dynamic range, spelled as `FileDto.hdr` spells it:
    /// `dolby_vision` | `hdr10` | `hlg`, absent for SDR. What is on disk,
    /// never a promise about what will be delivered — the play path decides
    /// that per client, per display, and reports it as
    /// `delivered_dynamic_range`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
    /// The rich label for the same file, verbatim from the probe. Paired with
    /// the token exactly as `FileDto` pairs them: code compares the token,
    /// each client words the label however that platform words it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

impl From<MediaFacts> for MediaDto {
    fn from(f: MediaFacts) -> Self {
        MediaDto {
            files: f.files,
            bytes: f.bytes,
            video: f.video,
            height: f.height,
            hdr: f.hdr,
            hdr_format: f.hdr_format,
            audio: f.audio,
            container: f.container,
        }
    }
}

#[derive(Serialize)]
pub struct RollupDto {
    pub leaves: i64,
    pub watched: i64,
}

impl From<WatchRollup> for RollupDto {
    fn from(r: WatchRollup) -> Self {
        RollupDto {
            leaves: r.leaves,
            watched: r.watched,
        }
    }
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
            added_at: item.added_at,
            updated_at: item.updated_at,
            recorded_at: item.recorded_at,
            tags: item.tags,
            genres: item.genres,
            author: item.author,
            book_work_id: item.book_work_id,
            book_edition_id: item.book_edition_id,
            book_metadata_source: item.book_metadata_source,
            tmdb_id: item.tmdb_id,
            imdb_id: item.imdb_id,
            poster: image_url(&item.poster_path, item.updated_at),
            backdrop: image_url(&item.backdrop_path, item.updated_at),
            resolution: None,
            media: None,
            child_count: None,
            show_title: None,
            watch: None,
            rollup: None,
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

    /// On rail cards, an episode should wear its season's poster rather than a
    /// per-episode still (which is often an arbitrary frame). Only overrides
    /// when a season poster exists; the per-episode still stays the fallback
    /// and is still used on the season page (which builds DTOs without this).
    pub fn with_season_poster(mut self, season_poster: Option<String>) -> Self {
        if season_poster.is_some() {
            self.poster = image_url(&season_poster, self.updated_at);
        }
        self
    }

    pub fn with_resolution(mut self, resolution: Option<i64>) -> Self {
        self.resolution = resolution;
        self
    }

    /// List decoration, like `with_resolution`: callers opt into the one
    /// page-wide query when they are rendering playable rows or cards.
    pub fn with_media(mut self, facts: Option<MediaFacts>) -> Self {
        self.media = facts.map(Into::into);
        self
    }

    pub fn with_child_count(mut self, count: Option<i64>) -> Self {
        self.child_count = count;
        self
    }

    pub fn with_rollup(mut self, rollup: Option<WatchRollup>) -> Self {
        self.rollup = rollup.map(Into::into);
        self
    }
}

pub fn recent_dto(recent: RecentItem, watch: Option<WatchState>) -> ItemDto {
    ItemDto::from(recent.item)
        .with_show_title(recent.show_title)
        .with_season_poster(recent.season_poster)
        .with_watch(watch)
}

pub fn in_progress_dto(item: InProgressItem) -> ItemDto {
    ItemDto::from(item.item)
        .with_show_title(item.show_title)
        .with_season_poster(item.season_poster)
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
    /// Rich HDR label for display ("Dolby Vision · Profile 7 (HDR10-compatible)").
    pub hdr_format: Option<String>,
    pub bitrate: Option<i64>,
    pub audio_streams: Vec<AudioStream>,
    pub subtitle_streams: Vec<SubtitleStream>,
    /// The server's cold-start track outcome for this file. This is computed
    /// from stored stream facts and the same pure policy playback uses; clients
    /// do not need the underlying admin preference values to reproduce it.
    pub playback_defaults: PlaybackDefaultsDto,
    /// Start of this file within a multi-file audiobook. Zero for ordinary
    /// media and for the first part. Clients add this to local player time
    /// before posting item progress, so resume remains one continuous book.
    #[serde(skip_serializing_if = "is_zero")]
    pub part_offset_ms: i64,
    /// Chapter table embedded in the container, captured by ffprobe at scan
    /// time. Empty for containers with no authored chapters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chapters: Vec<ChapterDto>,
    /// Whether the file is actually readable on the server right now. `false`
    /// means the path no longer resolves (unmounted share, moved/deleted file,
    /// wrong container mount) — the client shows this and refuses to "play"
    /// something that isn't there. Set by the handler, not from the row.
    pub available: bool,
    /// Server-owned reader actions for this exact detected format.  Clients
    /// consume the surface entry instead of inferring Read from an extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reader: Option<crate::reader_formats::ReaderCapability>,
    /// Exact file identity a native document reader must echo when it saves a
    /// locator.  It is present only for recognized ebook formats, alongside
    /// `reader`, so clients never have to infer a revision from HTTP dates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reader_revision: Option<RevisionDto>,
    /// Did ffprobe ever succeed on this file? `false` means every media field
    /// above is empty because nothing was ever read — not because the file has
    /// no video. The item page says so and offers a re-analyze, since the usual
    /// cause (permissions) is fixed outside plurx and leaves no other trace.
    pub probed: bool,
    /// Full server-side path, shown to admins when a file is missing so they
    /// can fix the mount. Only populated for missing files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_path: Option<String>,
}

/// How the configured preferred language relates to one file's tracks and the
/// policy-selected track.
///
/// `available` is intentionally distinct from `selected`: the dual-audio anime
/// rule can select Japanese original audio even when the configured audio
/// language is also present. `missing` means every track is tagged and none
/// matches; `unknown` means an untagged track prevents that claim; `no_tracks`
/// lets a detail screen say "no subtitles" instead.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreferredLanguageStatus {
    Selected,
    Available,
    Missing,
    Unknown,
    NoTracks,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PlaybackTrackDefaultDto {
    pub selected_index: Option<i64>,
    pub preferred_language: String,
    pub preferred_language_status: PreferredLanguageStatus,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PlaybackDefaultsDto {
    pub audio: PlaybackTrackDefaultDto,
    pub subtitle: PlaybackTrackDefaultDto,
}

fn preference_status(
    tracks_present: bool,
    preferred_present: bool,
    selected_matches_preference: bool,
    unknown_language_present: bool,
) -> PreferredLanguageStatus {
    if !tracks_present {
        PreferredLanguageStatus::NoTracks
    } else if selected_matches_preference {
        PreferredLanguageStatus::Selected
    } else if preferred_present {
        PreferredLanguageStatus::Available
    } else if unknown_language_present {
        PreferredLanguageStatus::Unknown
    } else {
        PreferredLanguageStatus::Missing
    }
}

fn playback_defaults(
    audio: &[AudioStream],
    subtitles: &[SubtitleStream],
    prefs: &LangPrefs,
) -> PlaybackDefaultsDto {
    let selected = select_tracks(audio, subtitles, prefers_original_audio(audio), prefs);
    defaults_from_selection(audio, subtitles, prefs, selected)
}

fn defaults_from_selection(
    audio: &[AudioStream],
    subtitles: &[SubtitleStream],
    prefs: &LangPrefs,
    selected: TrackSelection,
) -> PlaybackDefaultsDto {
    let audio_preferred = audio
        .iter()
        .any(|track| lang_matches(&track.language, &prefs.audio_lang));
    let audio_unknown = audio.iter().any(|track| {
        track
            .language
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    });
    let audio_selected_preferred = selected
        .audio_index
        .and_then(|index| audio.iter().find(|track| track.index == index))
        .is_some_and(|track| lang_matches(&track.language, &prefs.audio_lang));
    let subtitle_preferred = subtitles
        .iter()
        .any(|track| lang_matches(&track.language, &prefs.sub_lang));
    let subtitle_unknown = subtitles.iter().any(|track| {
        track
            .language
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    });
    let subtitle_selected_preferred = selected
        .subtitle_index
        .and_then(|index| subtitles.iter().find(|track| track.index == index))
        .is_some_and(|track| lang_matches(&track.language, &prefs.sub_lang));

    PlaybackDefaultsDto {
        audio: PlaybackTrackDefaultDto {
            selected_index: selected.audio_index,
            preferred_language: prefs.audio_lang.clone(),
            preferred_language_status: preference_status(
                !audio.is_empty(),
                audio_preferred,
                audio_selected_preferred,
                audio_unknown,
            ),
        },
        subtitle: PlaybackTrackDefaultDto {
            selected_index: selected.subtitle_index,
            preferred_language: prefs.sub_lang.clone(),
            preferred_language_status: preference_status(
                !subtitles.is_empty(),
                subtitle_preferred,
                subtitle_selected_preferred,
                subtitle_unknown,
            ),
        },
    }
}

impl FileDto {
    pub fn from_media_file(f: MediaFile, prefs: &LangPrefs) -> Self {
        let playback_defaults = playback_defaults(&f.audio_streams, &f.subtitle_streams, prefs);
        let reader = crate::reader_formats::capability(&f.path, f.container.as_deref());
        let reader_revision = reader.map(|_| RevisionDto {
            size: f.size,
            mtime: f.mtime,
        });
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
            hdr_format: f.hdr_format,
            bitrate: f.bitrate,
            audio_streams: f.audio_streams,
            subtitle_streams: f.subtitle_streams,
            playback_defaults,
            part_offset_ms: 0,
            chapters: Vec::new(),
            available: true,
            reader,
            reader_revision,
            probed: f.probed,
            missing_path: None,
        }
    }
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ChapterDto {
    pub index: i64,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Read the general chapter table out of stored ffprobe JSON. Playback's
/// marker classifier consumes the same source but intentionally returns only
/// intro/credits; audiobook detail needs every authored chapter.
pub fn chapters_from_probe_json(raw: Option<&str>) -> Vec<ChapterDto> {
    let Some(probe) = raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    else {
        return Vec::new();
    };
    let Some(chapters) = probe.get("chapters").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ChapterDto> = chapters
        .iter()
        .enumerate()
        .filter_map(|(position, chapter)| {
            let seconds = |key: &str| {
                chapter
                    .get(key)
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse::<f64>().ok())
                    .map(|value| (value * 1000.0).round() as i64)
            };
            let start_ms = seconds("start_time")?;
            let end_ms = seconds("end_time")?;
            if end_ms <= start_ms {
                return None;
            }
            let index = chapter
                .get("id")
                .and_then(|value| value.as_i64())
                .unwrap_or(position as i64);
            let title = chapter
                .get("tags")
                .and_then(|tags| tags.get("title"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Chapter {}", position + 1));
            Some(ChapterDto {
                index,
                title,
                start_ms,
                end_ms,
            })
        })
        .collect();
    out.sort_by_key(|chapter| chapter.start_ms);
    out
}

#[cfg(test)]
mod audiobook_tests {
    use super::*;

    #[test]
    fn chapter_table_is_named_sorted_and_converted_to_milliseconds() {
        let raw = r#"{"chapters":[
            {"id":8,"start_time":"12.500","end_time":"20.000","tags":{"title":"Second"}},
            {"id":7,"start_time":"0.000","end_time":"12.500","tags":{}},
            {"id":9,"start_time":"30.000","end_time":"29.000","tags":{"title":"invalid"}}
        ]}"#;
        let chapters = chapters_from_probe_json(Some(raw));
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].title, "Chapter 2");
        assert_eq!((chapters[0].start_ms, chapters[0].end_ms), (0, 12_500));
        assert_eq!(chapters[1].title, "Second");
        assert_eq!(chapters[1].index, 8);
    }

    #[test]
    fn malformed_probe_has_no_chapters() {
        assert!(chapters_from_probe_json(Some("not json")).is_empty());
        assert!(chapters_from_probe_json(None).is_empty());
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
    /// Automatic job intervals in minutes; `0` is off, which is the default.
    pub scan_interval_mins: i64,
    pub refresh_interval_mins: i64,
    /// When each last finished (unix seconds), so the UI can say "last scanned
    /// 40 minutes ago" rather than only promising a future.
    pub last_scan_at: Option<i64>,
    pub last_refresh_at: Option<i64>,
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
            scan_interval_mins: l.scan_interval_mins,
            refresh_interval_mins: l.refresh_interval_mins,
            last_scan_at: l.last_scan_at,
            last_refresh_at: l.last_refresh_at,
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

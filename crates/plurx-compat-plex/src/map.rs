//! Map plurx domain types onto Plex `MediaContainer` XML elements.
//!
//! Rating keys are plurx item ids; library section keys are plurx library ids.
//! Only the attributes real direct-connect clients (Kodi Composite/PKC,
//! python-plexapi, Home Assistant) rely on are emitted.

use plurx_core::domain::{Item, ItemKind, Library, LibraryKind, MediaFile, WatchState};

use crate::xml::Element;

/// Per-item playback annotation for the requesting user.
#[derive(Default, Clone, Copy)]
pub struct View {
    pub offset_ms: Option<i64>,
    pub watched: bool,
}

impl From<Option<WatchState>> for View {
    fn from(w: Option<WatchState>) -> Self {
        match w {
            Some(w) => View {
                offset_ms: (w.position_ms > 0 && !w.watched).then_some(w.position_ms),
                watched: w.watched,
            },
            None => View::default(),
        }
    }
}

/// The Plex section type for a library, or `None` for library kinds the
/// façade doesn't expose. Home libraries are deliberately absent: Plex has no
/// section type for a folder tree of camera files, and a half-mapped section
/// breaks Kodi clients harder than an absent one.
fn section_type(kind: LibraryKind) -> Option<&'static str> {
    match kind {
        LibraryKind::Movies => Some("movie"),
        LibraryKind::Shows => Some("show"),
        LibraryKind::Home => None,
    }
}

/// The Plex item type, or `None` for kinds the façade doesn't expose (the
/// home-library kinds — see [`section_type`]).
fn item_type(kind: ItemKind) -> Option<&'static str> {
    match kind {
        ItemKind::Movie => Some("movie"),
        ItemKind::Show => Some("show"),
        ItemKind::Season => Some("season"),
        ItemKind::Episode => Some("episode"),
        ItemKind::Folder | ItemKind::Video | ItemKind::Photo => None,
    }
}

/// Whether the Plex façade exposes this library at all.
pub fn is_plex_visible(kind: LibraryKind) -> bool {
    section_type(kind).is_some()
}

fn resolution(height: Option<i64>) -> Option<&'static str> {
    height.map(|h| match h {
        h if h >= 2160 => "4k",
        h if h >= 1080 => "1080",
        h if h >= 720 => "720",
        h if h >= 480 => "480",
        _ => "sd",
    })
}

/// A `<Directory>` for `/library/sections`, or `None` for a library the
/// façade doesn't expose (see [`section_type`]).
pub fn section_directory(lib: &Library) -> Option<Element> {
    Some(
        Element::new("Directory")
            .attr_i("key", lib.id)
            .attr("title", lib.name.clone())
            .attr("type", section_type(lib.kind)?)
            .attr("agent", "com.plexapp.agents.none")
            .attr("scanner", "plurx")
            .attr("language", "en")
            .attr("uuid", format!("plurx-section-{}", lib.id))
            .attr_i("updatedAt", lib.created_at),
    )
}

/// Common metadata attributes shared by Video and Directory items.
fn base_meta(mut el: Element, item: &Item, view: View) -> Element {
    el = el
        .attr_i("ratingKey", item.id)
        .attr("key", format!("/library/metadata/{}", item.id))
        // Unexposed kinds never reach the façade (the handlers 404 them), so
        // the fallback only guards against a future caller forgetting.
        .attr("type", item_type(item.kind).unwrap_or("unknown"))
        .attr("title", item.title.clone())
        .attr("titleSort", item.sort_title.clone())
        .attr_opt("summary", item.overview.clone())
        .attr_i_opt("year", item.year.map(|y| y as i64))
        .attr("thumb", format!("/library/metadata/{}/thumb", item.id))
        .attr("art", format!("/library/metadata/{}/art", item.id))
        .attr_i("addedAt", item.added_at)
        .attr_i("updatedAt", item.updated_at);
    // Plex indexing: a season's `index` is its number; an episode's `index`
    // is the episode number and `parentIndex` is the season. Setting `index`
    // twice (as season *and* episode) produces duplicate XML attributes.
    match item.kind {
        ItemKind::Season => {
            el = el.attr_i_opt("index", item.season_number.map(|n| n as i64));
        }
        ItemKind::Episode => {
            el = el
                .attr_i_opt("index", item.episode_number.map(|n| n as i64))
                .attr_i_opt("parentIndex", item.season_number.map(|n| n as i64));
        }
        _ => {}
    }
    if view.watched {
        el = el.attr_i("viewCount", 1);
    }
    if let Some(offset) = view.offset_ms {
        el = el.attr_i("viewOffset", offset);
    }
    el
}

/// A `<Media>`+`<Part>` subtree for a file.
pub fn media_element(file: &MediaFile) -> Element {
    let ext = file
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv");
    let audio_codec = file.audio_streams.first().map(|a| a.codec.clone());
    let part = Element::new("Part")
        .attr_i("id", file.id)
        .attr(
            "key",
            format!("/library/parts/{}/{}/file.{}", file.id, file.mtime, ext),
        )
        .attr_i_opt("duration", file.duration_ms)
        .attr("file", file.path.to_string_lossy().into_owned())
        .attr_i("size", file.size)
        .attr_opt("container", file.container.clone());

    Element::new("Media")
        .attr_i("id", file.id)
        .attr_i_opt("duration", file.duration_ms)
        .attr_i_opt("bitrate", file.bitrate.map(|b| b / 1000))
        .attr_i_opt("width", file.width)
        .attr_i_opt("height", file.height)
        .attr_opt("videoResolution", resolution(file.height))
        .attr_opt("container", file.container.clone())
        .attr_opt("videoCodec", file.video_codec.clone())
        .attr_opt("audioCodec", audio_codec)
        .attr_i_opt(
            "audioChannels",
            file.audio_streams.first().and_then(|a| a.channels),
        )
        .child(part)
}

/// A `<Video>` element (movie or episode) with its media.
pub fn video_element(item: &Item, files: &[MediaFile], view: View) -> Element {
    let mut el = base_meta(Element::new("Video"), item, view);
    el = el.attr_i_opt("duration", item.runtime_ms);
    for file in files {
        el = el.child(media_element(file));
    }
    el
}

/// A `<Directory>` element (show or season).
pub fn directory_element(item: &Item, child_count: Option<i64>, view: View) -> Element {
    let mut el = base_meta(Element::new("Directory"), item, view);
    if let Some(n) = child_count {
        el = el.attr_i(
            match item.kind {
                ItemKind::Show => "childCount",
                _ => "leafCount",
            },
            n,
        );
    }
    el
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::domain::{AudioStream, MediaFile};

    fn movie_item() -> Item {
        Item {
            id: 42,
            library_id: 1,
            kind: ItemKind::Movie,
            parent_id: None,
            title: "The Matrix".into(),
            sort_title: "matrix".into(),
            year: Some(1999),
            overview: Some("A hacker learns the truth.".into()),
            tmdb_id: Some(603),
            imdb_id: None,
            season_number: None,
            episode_number: None,
            air_date: None,
            runtime_ms: Some(8_160_000),
            poster_path: Some("42-poster.jpg".into()),
            backdrop_path: None,
            added_at: 100,
            updated_at: 200,
            recorded_at: None,
            tags: Vec::new(),
            nfo_seeded_at: None,
            artwork_attempted_at: None,
            artwork_error: None,
        }
    }

    fn movie_file() -> MediaFile {
        MediaFile {
            id: 7,
            item_id: 42,
            path: "/media/The Matrix (1999).mkv".into(),
            size: 8_000_000_000,
            mtime: 12345,
            duration_ms: Some(8_160_000),
            container: Some("mkv".into()),
            video_codec: Some("h264".into()),
            video_profile: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(8_000_000),
            audio_streams: vec![AudioStream {
                index: 0,
                codec: "aac".into(),
                channels: Some(6),
                default: true,
                ..Default::default()
            }],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    #[test]
    fn video_element_has_plex_shape() {
        let view = View {
            offset_ms: Some(30_000),
            watched: false,
        };
        let doc = video_element(&movie_item(), &[movie_file()], view).to_document();
        assert!(doc.contains("<Video ratingKey=\"42\""));
        assert!(doc.contains("key=\"/library/metadata/42\""));
        assert!(doc.contains("type=\"movie\""));
        assert!(doc.contains("title=\"The Matrix\""));
        assert!(doc.contains("year=\"1999\""));
        assert!(doc.contains("viewOffset=\"30000\""));
        // Media + Part nested.
        assert!(doc.contains("<Media id=\"7\""));
        assert!(doc.contains("videoResolution=\"1080\""));
        assert!(doc.contains("videoCodec=\"h264\""));
        assert!(doc.contains("audioCodec=\"aac\""));
        assert!(doc.contains("<Part id=\"7\""));
        assert!(doc.contains("key=\"/library/parts/7/12345/file.mkv\""));
    }

    #[test]
    fn watched_movie_sets_viewcount_not_offset() {
        let view = View {
            offset_ms: None,
            watched: true,
        };
        let doc = video_element(&movie_item(), &[], view).to_document();
        assert!(doc.contains("viewCount=\"1\""));
        assert!(!doc.contains("viewOffset"));
    }

    #[test]
    fn section_directory_shape() {
        let lib = Library {
            id: 3,
            name: "Movies".into(),
            kind: LibraryKind::Movies,
            paths: vec![],
            anime: false,
            created_at: 5,
            scan_interval_mins: 0,
            refresh_interval_mins: 0,
            last_scan_at: None,
            last_refresh_at: None,
        };
        let doc = section_directory(&lib)
            .expect("movies are exposed")
            .to_document();
        assert!(doc.contains("key=\"3\""));
        assert!(doc.contains("type=\"movie\""));
        assert!(doc.contains("title=\"Movies\""));
    }

    #[test]
    fn home_libraries_are_not_exposed() {
        // A half-mapped section type breaks Kodi clients harder than an absent
        // one, so the façade skips home libraries outright.
        let lib = Library {
            id: 4,
            name: "Home videos".into(),
            kind: LibraryKind::Home,
            paths: vec![],
            anime: false,
            created_at: 5,
            scan_interval_mins: 0,
            refresh_interval_mins: 0,
            last_scan_at: None,
            last_refresh_at: None,
        };
        assert!(section_directory(&lib).is_none());
        assert!(!is_plex_visible(LibraryKind::Home));
        assert!(is_plex_visible(LibraryKind::Movies));
        assert!(item_type(ItemKind::Video).is_none());
        assert!(item_type(ItemKind::Photo).is_none());
        assert!(item_type(ItemKind::Folder).is_none());
    }
}

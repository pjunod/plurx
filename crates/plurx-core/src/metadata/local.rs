//! Local artwork for `home` libraries.
//!
//! Home libraries never call a provider (docs/HOMEVIDEO-PLAN.md §2) — there
//! is nothing to match "Christmas 2019.mp4" against, and a false match would
//! be worse than nothing. Their "enrichment" is entirely local: art already
//! sitting beside the file wins, otherwise ffmpeg grabs a frame.
//!
//! Generated artwork goes in the artwork cache, exactly like a TMDB poster —
//! never next to the media. ARCHITECTURE §8's "plurx never writes to media
//! storage" is why local art found on disk is *copied into* the cache rather
//! than referenced in place: the image server still touches only one
//! directory.

use std::path::{Path, PathBuf};

use crate::domain::{ItemKind, MetadataPatch};
use crate::store::Store;

/// Thumbnail width. Matches the TMDB poster bucket (`w500`) so the grid mixes
/// home and movie cards without a visible quality step.
const THUMB_WIDTH: i64 = 500;

/// The ffmpeg binary; overridable via `PLURX_FFMPEG` for jellyfin-ffmpeg or a
/// pinned path — same convention as the transcoder and prober.
fn ffmpeg_bin() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct LocalArtReport {
    /// Posters copied from art found next to the media.
    pub adopted: usize,
    /// Posters generated with ffmpeg (frame grabs and photo thumbnails).
    pub generated: usize,
    /// Folders that inherited a child's poster.
    pub inherited: usize,
    pub errors: usize,
}

/// Give every item in a home library a poster. Failures are non-fatal and
/// counted — the same posture as artwork downloads: one unreadable file must
/// not stop the rest of a 2,000-photo import from getting thumbnails.
///
/// `only` restricts the pass to specific item ids — what a targeted scan just
/// placed, plus the folders above them, so importing one evening's clips does
/// not re-walk the whole library. `None` is every item, exactly as before.
pub async fn enrich_home_library(
    store: &dyn Store,
    artwork_dir: &Path,
    library_id: i64,
    force: bool,
    only: Option<&[i64]>,
) -> LocalArtReport {
    let mut report = LocalArtReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        return report;
    }
    let items = match store.items_needing_artwork(library_id, force, only).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "listing home items needing artwork");
            report.errors += 1;
            return report;
        }
    };

    // Folders come last from the store query, so by the time one is handled
    // its children already have the poster it inherits.
    for item in items {
        let poster = match item.kind {
            ItemKind::Folder => match folder_poster(store, artwork_dir, item.id).await {
                Some(name) => {
                    report.inherited += 1;
                    Some(name)
                }
                None => None,
            },
            ItemKind::Video | ItemKind::Photo => {
                let Some(path) = first_file_path(store, item.id).await else {
                    continue;
                };
                match adopt_local_art(artwork_dir, item.id, &path).await {
                    Some(name) => {
                        report.adopted += 1;
                        Some(name)
                    }
                    None => {
                        match generate_thumb(
                            artwork_dir,
                            item.id,
                            &path,
                            item.kind,
                            duration(store, item.id).await,
                        )
                        .await
                        {
                            Some(name) => {
                                report.generated += 1;
                                Some(name)
                            }
                            None => {
                                report.errors += 1;
                                None
                            }
                        }
                    }
                }
            }
            _ => None,
        };
        let Some(poster) = poster else { continue };
        if let Err(e) = store
            .apply_metadata(
                item.id,
                &MetadataPatch {
                    poster_path: Some(poster),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(item = item.id, error = %e, "recording local poster");
            report.errors += 1;
        }
    }

    tracing::info!(
        adopted = report.adopted,
        generated = report.generated,
        inherited = report.inherited,
        errors = report.errors,
        "local artwork complete"
    );
    report
}

async fn first_file_path(store: &dyn Store, item_id: i64) -> Option<PathBuf> {
    store
        .files_for_item(item_id)
        .await
        .ok()?
        .into_iter()
        .next()
        .map(|f| f.path)
}

async fn duration(store: &dyn Store, item_id: i64) -> Option<i64> {
    store
        .files_for_item(item_id)
        .await
        .ok()?
        .into_iter()
        .next()
        .and_then(|f| f.duration_ms)
}

/// Art the owner already put next to the media wins over anything we could
/// generate (REQ-META-4's spirit): `<stem>-thumb.jpg`/`-poster.jpg` for a
/// file. Copied into the cache under the usual `{item_id}-poster.*` name.
async fn adopt_local_art(artwork_dir: &Path, item_id: i64, media: &Path) -> Option<String> {
    let stem = media.file_stem()?.to_string_lossy().into_owned();
    let dir = media.parent()?;
    for suffix in ["-thumb", "-poster"] {
        for ext in ["jpg", "jpeg", "png"] {
            let candidate = dir.join(format!("{stem}{suffix}.{ext}"));
            if candidate.is_file() {
                return copy_into_cache(artwork_dir, item_id, &candidate).await;
            }
        }
    }
    None
}

/// Local art for a folder: `poster.jpg`/`folder.jpg` inside the directory.
async fn adopt_folder_art(artwork_dir: &Path, item_id: i64, dir: &Path) -> Option<String> {
    for name in ["poster", "folder"] {
        for ext in ["jpg", "jpeg", "png"] {
            let candidate = dir.join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return copy_into_cache(artwork_dir, item_id, &candidate).await;
            }
        }
    }
    None
}

async fn copy_into_cache(artwork_dir: &Path, item_id: i64, source: &Path) -> Option<String> {
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_lowercase();
    let filename = format!("{item_id}-poster.{ext}");
    let dest = artwork_dir.join(&filename);
    match tokio::fs::copy(source, &dest).await {
        Ok(_) => Some(filename),
        Err(e) => {
            tracing::warn!(source = %source.display(), error = %e, "copying local artwork");
            None
        }
    }
}

/// Grab a frame (video) or scale the still (photo) into the artwork cache.
///
/// `-ss` goes *before* `-i` so ffmpeg fast-seeks the input: decoding a 2 GB
/// clip from zero to reach the 20% mark would make a first scan glacial.
///
/// HDR phone footage grabs washed out — there is no tone-map in this
/// pipeline. Accepted for v1: it's a thumbnail. The transcode path's
/// tone-map filter can be reused here later if it grates.
async fn generate_thumb(
    artwork_dir: &Path,
    item_id: i64,
    media: &Path,
    kind: ItemKind,
    duration_ms: Option<i64>,
) -> Option<String> {
    let filename = format!("{item_id}-poster.jpg");
    let dest = artwork_dir.join(&filename);

    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.arg("-nostdin").args(["-v", "error", "-y"]);
    if kind == ItemKind::Video {
        cmd.arg("-ss")
            .arg(format!("{:.3}", seek_seconds(duration_ms)));
    }
    cmd.arg("-i")
        .arg(media)
        .args(["-frames:v", "1"])
        .args(["-vf", &format!("scale=w={THUMB_WIDTH}:h=-2")])
        .args(["-q:v", "4"])
        .arg(&dest);

    match cmd.output().await {
        Ok(out) if out.status.success() && dest.is_file() => Some(filename),
        Ok(out) => {
            tracing::warn!(
                path = %media.display(),
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "thumbnail generation failed"
            );
            None
        }
        Err(e) => {
            tracing::warn!(path = %media.display(), error = %e, "spawning ffmpeg for a thumbnail");
            None
        }
    }
}

/// Where to grab the frame: 20% in, clamped to [1 s, 300 s]. The opening
/// second of home video is usually a lens cap or a floor.
fn seek_seconds(duration_ms: Option<i64>) -> f64 {
    match duration_ms {
        Some(ms) if ms > 0 => ((ms as f64 / 1000.0) * 0.2).clamp(1.0, 300.0),
        // Unknown duration (probe failed): 5 s is far enough in to be a real
        // frame and near enough to exist in a short clip.
        _ => 5.0,
    }
}

/// A folder wears its own `poster.jpg` if it has one, else the poster of its
/// first child by recorded date. Cheap, deterministic, good enough.
async fn folder_poster(store: &dyn Store, artwork_dir: &Path, folder_id: i64) -> Option<String> {
    let children = store.get_item_children(folder_id).await.ok()?;
    // Any child's file tells us which directory this folder mirrors.
    for child in &children {
        if let Some(path) = first_file_path(store, child.id).await {
            if let Some(dir) = path.parent() {
                if let Some(name) = adopt_folder_art(artwork_dir, folder_id, dir).await {
                    return Some(name);
                }
            }
            break;
        }
    }
    // get_item_children orders folders first, then media by recorded date, so
    // this is "the first child by date" — and its poster was written earlier
    // in this same pass. A folder holding only subfolders stays posterless in
    // v1; the grid falls back to a plain card.
    children
        .into_iter()
        .filter(|c| c.kind != ItemKind::Folder)
        .find_map(|c| c.poster_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::{LibraryKind, NewLibrary};
    use crate::scan::scan_library;
    use crate::store::{LibraryStore, MediaStore, SqliteStore};

    /// Make a real (tiny) clip with ffmpeg. Returns false when ffmpeg isn't
    /// installed, so the suite degrades to "not run here" rather than "broken".
    async fn make_clip(path: &Path, seconds: &str) -> bool {
        let ffmpeg = ffmpeg_bin();
        let status = tokio::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size=320x240:rate=10:duration={seconds}"))
            .args(["-pix_fmt", "yuv420p"])
            .arg(path)
            .status()
            .await;
        match status {
            Ok(s) if s.success() => true,
            Ok(_) => false,
            Err(e) => {
                eprintln!(
                    "skipping: cannot run `{ffmpeg}` ({e}) — install ffmpeg to run this test"
                );
                false
            }
        }
    }

    #[tokio::test]
    async fn home_artwork_adopts_generates_and_inherits() {
        let dir = tempfile::tempdir().expect("tmp");
        let media = dir.path().join("media/2019");
        std::fs::create_dir_all(&media).expect("mkdir");
        if !make_clip(&media.join("Beach.mp4"), "2").await {
            return;
        }
        // A second clip with art already sitting next to it — that wins.
        if !make_clip(&media.join("Sandcastle.mp4"), "2").await {
            return;
        }
        let sidecar_art = media.join("Sandcastle-thumb.jpg");
        std::fs::write(&sidecar_art, b"pretend-jpeg-bytes").expect("art");

        let artwork = dir.path().join("artwork");
        let store = SqliteStore::open_in_memory().expect("store");
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().join("media")],
                anime: false,
            })
            .await
            .expect("lib");
        scan_library(&store, &lib).await.expect("scan");

        let report = enrich_home_library(&store, &artwork, lib.id, false, None).await;
        assert_eq!(report.errors, 0, "report: {report:?}");
        assert_eq!(report.generated, 1, "one frame grab");
        assert_eq!(report.adopted, 1, "one adopted sidecar image");
        assert_eq!(report.inherited, 1, "the folder took a child's poster");

        let page = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        let folder = &page.items[0];
        assert_eq!(folder.kind, ItemKind::Folder);
        let children = store.get_item_children(folder.id).await.expect("children");
        for child in &children {
            let poster = child.poster_path.clone().expect("poster");
            assert!(
                artwork.join(&poster).is_file(),
                "poster {poster} must live in the artwork cache, never beside the media"
            );
        }
        // Art found on disk is copied in, byte for byte — not linked.
        let adopted = children
            .iter()
            .find(|c| c.title == "Sandcastle")
            .and_then(|c| c.poster_path.clone())
            .expect("adopted poster");
        assert_eq!(
            std::fs::read(artwork.join(&adopted)).expect("read"),
            std::fs::read(&sidecar_art).expect("read source"),
            "the owner's own art wins over a frame grab"
        );
        // The folder inherited its first child's poster.
        assert!(folder
            .poster_path
            .as_ref()
            .is_none_or(|p| artwork.join(p).is_file()));
        let folder_after = store
            .get_item(folder.id)
            .await
            .expect("get")
            .expect("present");
        assert!(folder_after.poster_path.is_some());

        // Nothing is left needing artwork, so a second pass is a no-op.
        let again = enrich_home_library(&store, &artwork, lib.id, false, None).await;
        assert_eq!(
            (
                again.generated,
                again.adopted,
                again.inherited,
                again.errors
            ),
            (0, 0, 0, 0),
            "a rescan must not re-encode every thumbnail"
        );
    }

    use crate::domain::ItemSort;

    #[test]
    fn seek_is_a_fifth_in_and_clamped() {
        // 100 s clip → 20 s.
        assert!((seek_seconds(Some(100_000)) - 20.0).abs() < f64::EPSILON);
        // Very short clip → the 1 s floor, not 0.2 s.
        assert!((seek_seconds(Some(2_000)) - 1.0).abs() < f64::EPSILON);
        // Very long clip → the 300 s ceiling.
        assert!((seek_seconds(Some(10_000_000)) - 300.0).abs() < f64::EPSILON);
        // Unknown duration → 5 s.
        assert!((seek_seconds(None) - 5.0).abs() < f64::EPSILON);
        assert!((seek_seconds(Some(0)) - 5.0).abs() < f64::EPSILON);
    }
}

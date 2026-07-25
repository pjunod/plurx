//! Library scanning: walk the tree, place items in the hierarchy, record
//! files with their probe metadata, and reconcile away vanished files.
//!
//! Incremental by design (REQ-LIB-3): a file whose size and mtime are
//! unchanged is skipped without re-probing, so a rescan of a large library is
//! cheap and easy on shared storage. Probing runs sequentially for the same
//! reason — a scan should not thrash a NAS.

pub mod exif;
pub mod home;
pub mod nfo;
pub mod parse;
pub mod probe;

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use walkdir::WalkDir;

use crate::domain::{Item, ItemKind, Library, LibraryKind, NewItem};
use crate::error::StoreError;
use crate::store::Store;

/// Container extensions we treat as playable video.
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "webm", "wmv", "flv", "mpg", "mpeg", "vob",
    "ogv", "3gp",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed_files: usize,
    pub pruned_items: usize,
    pub skipped: usize,
    pub errors: usize,
    /// Home libraries only: items whose metadata was seeded from an `.nfo`
    /// sidecar on this run. Seeding happens at most once per item, ever.
    pub seeded: usize,
    /// Human-readable problems worth showing the operator: missing roots,
    /// unreadable directories, a scan that found no video files at all. A
    /// non-empty list means the scan's counts don't tell the whole story.
    pub problems: Vec<String>,
}

/// Cap on individual walk-error messages recorded per scan (the counts still
/// include everything; this just keeps the report readable).
const MAX_WALK_PROBLEMS: usize = 10;

/// Live counters a running scan updates as it goes. Shared with whoever wants
/// to display progress (the HTTP status endpoint samples these atomics without
/// touching the scan itself). Probing a big library over a NAS takes minutes;
/// "processing 412 of 3801" is the difference between progress and a hang.
#[derive(Debug, Default)]
pub struct ScanProgress {
    /// Candidate video files discovered by the directory walk.
    pub found: AtomicUsize,
    /// Files handled so far (unchanged, added, updated, skipped, or errored).
    pub processed: AtomicUsize,
    /// Files added or updated so far.
    pub changed: AtomicUsize,
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// File size and mtime (unix seconds), or `None` if unreadable.
fn file_stat(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((size, mtime))
}

/// Scan one library end to end. `store` is the full store; only media methods
/// are used. Returns a tally of what changed.
pub async fn scan_library(store: &dyn Store, library: &Library) -> Result<ScanReport, StoreError> {
    scan_library_with_progress(store, library, None).await
}

/// Like [`scan_library`], updating `progress` (when given) as the scan runs.
pub async fn scan_library_with_progress(
    store: &dyn Store,
    library: &Library,
    progress: Option<&ScanProgress>,
) -> Result<ScanReport, StoreError> {
    let mut report = ScanReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    // Collect candidate files first (cheap, synchronous), then process each.
    // A root that is missing or unreadable is a loud, actionable problem — the
    // most common cause is a container path mix-up (the library was configured
    // with a host path that isn't mounted inside the container) or an
    // unmounted NAS. Either way, silently scanning nothing is the worst
    // possible answer.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let mut walk_errors = 0usize;
    for root in &library.paths {
        if !root.is_dir() {
            report.errors += 1;
            walk_errors += 1;
            report.problems.push(format!(
                "library path `{}` does not exist on the server — if plurxd runs in a \
                 container, use the path as mounted inside the container (e.g. `/media/…`), \
                 and check the mount is present",
                root.display()
            ));
            continue;
        }
        for entry in WalkDir::new(root).follow_links(true) {
            match entry {
                Ok(entry) => {
                    // Photos count only in home libraries: a poster JPEG next
                    // to a movie must stay as invisible as it is today.
                    let wanted = is_video(entry.path())
                        || (library.kind == LibraryKind::Home && home::is_photo(entry.path()));
                    if entry.file_type().is_file() && wanted {
                        candidates.push(entry.into_path());
                    }
                }
                Err(e) => {
                    report.errors += 1;
                    walk_errors += 1;
                    if report.problems.len() < MAX_WALK_PROBLEMS {
                        let at = e
                            .path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| root.display().to_string());
                        report.problems.push(format!("cannot read `{at}`: {e}"));
                    }
                }
            }
        }
    }
    candidates.sort();
    if let Some(p) = progress {
        p.found.store(candidates.len(), Ordering::Relaxed);
    }

    if candidates.is_empty() && walk_errors == 0 {
        let (what, exts) = match library.kind {
            LibraryKind::Home => (
                "video or photo files",
                format!("{}, {}", VIDEO_EXTS.join(", "), home::PHOTO_EXTS.join(", ")),
            ),
            _ => ("video files", VIDEO_EXTS.join(", ")),
        };
        report.problems.push(format!(
            "no {what} found under {} — the path exists but contains no recognized \
             media files ({exts})",
            library
                .paths
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    for path in candidates {
        if let Some(p) = progress {
            p.processed.fetch_add(1, Ordering::Relaxed);
        }
        let path_str = path.to_string_lossy().into_owned();
        seen.insert(path_str.clone());

        let Some((size, mtime)) = file_stat(&path) else {
            report.errors += 1;
            continue;
        };

        // Incremental: unchanged file → skip probe entirely.
        let existing = store.get_file_by_path(&path_str).await?;
        if let Some(ref ex) = existing {
            if ex.size == size && ex.mtime == mtime {
                report.unchanged += 1;
                // An NFO written *after* the file was scanned still seeds: the
                // check is on the item, not on the file's size+mtime. Costs
                // one stat per unseeded home video, and no probe.
                if library.kind == LibraryKind::Home {
                    if let Some(item) = store.get_item(ex.item_id).await? {
                        if item.nfo_seeded_at.is_none()
                            && home::seed_from_nfo(store, &item, &path, &mut report.problems)
                                .await?
                        {
                            report.seeded += 1;
                        }
                    }
                }
                continue;
            }
        }
        let is_new = existing.is_none();

        let Some(placed) = place_item(store, library, &path).await? else {
            // Couldn't place it (e.g. a Shows file with no S/E marker).
            report.skipped += 1;
            continue;
        };
        let item_id = placed.id;

        // Probe is best-effort — a weird file still records with null media
        // details rather than failing the whole scan.
        let probe = match probe::probe(&path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path_str, error = %e, "probe failed; recording without media detail");
                report.errors += 1;
                Default::default()
            }
        };

        store
            .upsert_file(item_id, &path_str, size, mtime, &probe)
            .await?;
        if library.kind == LibraryKind::Home
            && home::after_record(store, placed, &path, &probe, mtime, &mut report.problems).await?
        {
            report.seeded += 1;
        }
        if is_new {
            report.added += 1;
        } else {
            report.updated += 1;
        }
        if let Some(p) = progress {
            p.changed.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Reconcile: anything in the DB for this library but not seen on disk is
    // gone. NEVER reconcile after a partial walk — if a root was missing or a
    // directory unreadable (NAS unmounted, permissions), the files under it
    // are invisible, not deleted, and removing them here would wipe the
    // library's records over a transient mount problem.
    if walk_errors == 0 {
        let known = store.library_file_paths(library.id).await?;
        let gone: Vec<i64> = known
            .into_iter()
            .filter(|(_, p)| !seen.contains(&p.to_string_lossy().into_owned()))
            .map(|(id, _)| id)
            .collect();
        if !gone.is_empty() {
            report.removed_files = store.delete_files(&gone).await? as usize;
        }
        report.pruned_items = store.prune_empty_items(library.id).await? as usize;
    } else {
        report.problems.push(
            "vanished-file cleanup skipped: some library paths were missing or unreadable, \
             so absent files were kept rather than removed"
                .to_owned(),
        );
    }

    tracing::info!(
        library = %library.name,
        added = report.added,
        unchanged = report.unchanged,
        removed = report.removed_files,
        pruned = report.pruned_items,
        seeded = report.seeded,
        skipped = report.skipped,
        errors = report.errors,
        "scan complete"
    );
    for problem in &report.problems {
        tracing::warn!(library = %library.name, "scan problem: {problem}");
    }
    Ok(report)
}

/// Find-or-create the item a file belongs to. `None` means the file couldn't
/// be identified for this library kind.
async fn place_item(
    store: &dyn Store,
    library: &Library,
    path: &Path,
) -> Result<Option<home::Placed>, StoreError> {
    match library.kind {
        LibraryKind::Movies => {
            let parsed = parse::parse_movie(path);
            if let Some(existing) = store
                .find_movie(library.id, &parsed.title, parsed.year)
                .await?
            {
                return Ok(Some(home::Placed {
                    id: existing.id,
                    created: false,
                }));
            }
            let id = store
                .insert_item(&NewItem {
                    library_id: library.id,
                    kind: ItemKind::Movie,
                    parent_id: None,
                    title: parsed.title,
                    year: parsed.year,
                    season_number: None,
                    episode_number: None,
                })
                .await?;
            Ok(Some(home::Placed { id, created: true }))
        }
        // Folders are the organization: the directory tree is mirrored as
        // browsable folder items, and the file itself becomes a video or a
        // photo under it.
        LibraryKind::Home => home::place(store, library, path).await,
        LibraryKind::Shows => {
            // Anime libraries use absolute numbering; regular shows use S/E.
            let parsed = if library.anime {
                parse::parse_anime_episode(path)
            } else {
                parse::parse_episode(path)
            };
            let Some(parsed) = parsed else {
                return Ok(None);
            };
            let show = find_or_create_show(store, library, &parsed).await?;
            let season = find_or_create_season(store, library, show.id, parsed.season).await?;
            if let Some(existing) = store.find_episode(season, parsed.episode).await? {
                return Ok(Some(home::Placed {
                    id: existing.id,
                    created: false,
                }));
            }
            let title = parsed
                .episode_title
                .clone()
                .unwrap_or_else(|| format!("Episode {}", parsed.episode));
            let id = store
                .insert_item(&NewItem {
                    library_id: library.id,
                    kind: ItemKind::Episode,
                    parent_id: Some(season),
                    title,
                    year: None,
                    season_number: Some(parsed.season),
                    episode_number: Some(parsed.episode),
                })
                .await?;
            Ok(Some(home::Placed { id, created: true }))
        }
    }
}

async fn find_or_create_show(
    store: &dyn Store,
    library: &Library,
    parsed: &parse::ParsedEpisode,
) -> Result<Item, StoreError> {
    if let Some(show) = store
        .find_show(library.id, &parsed.show_title, parsed.show_year)
        .await?
    {
        return Ok(show);
    }
    let id = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Show,
            parent_id: None,
            title: parsed.show_title.clone(),
            year: parsed.show_year,
            season_number: None,
            episode_number: None,
        })
        .await?;
    store
        .get_item(id)
        .await?
        .ok_or_else(|| StoreError::Database("show vanished after insert".to_owned()))
}

async fn find_or_create_season(
    store: &dyn Store,
    library: &Library,
    show_id: i64,
    season_number: i32,
) -> Result<i64, StoreError> {
    if let Some(season) = store.find_season(show_id, season_number).await? {
        return Ok(season.id);
    }
    store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Season,
            parent_id: Some(show_id),
            title: format!("Season {season_number}"),
            year: None,
            season_number: Some(season_number),
            episode_number: None,
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::{ItemEdit, ItemSort, NewLibrary};
    use crate::store::{LibraryStore, MediaStore, SqliteStore};

    async fn write_fake_video(dir: &Path, rel: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        // Content is irrelevant — probe will fail gracefully and the scanner
        // still records the file and builds the hierarchy.
        std::fs::write(&path, b"not really video").expect("write");
        path
    }

    #[tokio::test]
    async fn scans_movies_incrementally_and_reconciles() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "The Matrix (1999)/The Matrix (1999).mkv").await;
        write_fake_video(dir.path(), "Heat (1995).mkv").await;

        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");

        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 2);
        let page = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(page.total, 2);

        // Second scan: nothing changed.
        let r = scan_library(&store, &lib).await.expect("rescan");
        assert_eq!(r.added, 0);
        assert_eq!(r.unchanged, 2);

        // Delete one file, rescan: file removed and its movie pruned.
        std::fs::remove_file(dir.path().join("Heat (1995).mkv")).expect("rm");
        let r = scan_library(&store, &lib).await.expect("rescan2");
        assert_eq!(r.removed_files, 1);
        assert_eq!(r.pruned_items, 1);
        let page = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].title, "The Matrix");
    }

    #[tokio::test]
    async fn missing_root_is_reported_and_preserves_files() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "Heat (1995).mkv").await;

        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 1);

        // The root vanishes (unmounted NAS / wrong container path). The scan
        // must say so loudly — and must NOT delete the known files.
        drop(dir);
        let r = scan_library(&store, &lib).await.expect("rescan");
        assert_eq!(r.errors, 1);
        assert!(
            r.problems.iter().any(|p| p.contains("does not exist")),
            "problems: {:?}",
            r.problems
        );
        assert_eq!(r.removed_files, 0);
        assert_eq!(r.pruned_items, 0);
        let known = store.library_file_paths(lib.id).await.expect("paths");
        assert_eq!(known.len(), 1, "files must survive a missing root");
    }

    #[tokio::test]
    async fn empty_root_reports_no_videos_found() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");

        // Path exists but holds no video files (the classic empty-volume /
        // wrong-subfolder misconfiguration): counts are all zero, so the
        // report must carry an explicit problem instead.
        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 0);
        assert_eq!(r.errors, 0);
        assert!(
            r.problems
                .iter()
                .any(|p| p.contains("no video files found")),
            "problems: {:?}",
            r.problems
        );
    }

    /// The home arm end to end: folder mirroring, titles kept verbatim, the
    /// date ladder, seed-once NFO handling, and a junk sidecar that complains
    /// exactly once.
    #[tokio::test]
    async fn scans_home_tree_with_folders_dates_and_nfos() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "2019/Beach Trip/Christmas 2019.mp4").await;
        write_fake_video(dir.path(), "2019/Beach Trip/2019-06-14 - Sandcastle.mp4").await;
        write_fake_video(dir.path(), "Loose clip.mov").await;
        // A photo — invisible in movie/show libraries, a first-class item here.
        std::fs::write(dir.path().join("2019/IMG_4021.jpg"), b"jpeg-ish").expect("photo");
        // One good sidecar, one that is just a URL someone saved.
        std::fs::write(
            dir.path().join("2019/Beach Trip/Christmas 2019.nfo"),
            "<movie><title>Christmas morning</title><premiered>2019-12-25</premiered>\
             <tag>kids</tag></movie>",
        )
        .expect("nfo");
        std::fs::write(
            dir.path().join("Loose clip.nfo"),
            "https://www.imdb.com/title/tt0133093/",
        )
        .expect("junk nfo");

        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");

        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 4, "three videos and one photo");
        assert_eq!(r.seeded, 2, "both sidecars are consumed exactly once");
        assert!(
            r.problems.iter().any(|p| p.contains("Loose clip.nfo")),
            "a junk sidecar complains by name: {:?}",
            r.problems
        );

        // Top level: the mirrored "2019" folder and the loose clip.
        let top = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(top.total, 2);
        let folder = top
            .items
            .iter()
            .find(|i| i.kind == ItemKind::Folder)
            .expect("2019 folder");
        assert_eq!(folder.title, "2019");
        let loose = top
            .items
            .iter()
            .find(|i| i.kind == ItemKind::Video)
            .expect("loose video");
        assert_eq!(loose.title, "Loose clip");

        // "2019" holds the photo and the nested "Beach Trip" folder —
        // subfolders sort first.
        let kids = store.get_item_children(folder.id).await.expect("children");
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].kind, ItemKind::Folder);
        assert_eq!(kids[0].title, "Beach Trip");
        assert_eq!(kids[1].kind, ItemKind::Photo);
        assert_eq!(kids[1].title, "IMG_4021", "camera junk names stay honest");

        let clips = store.get_item_children(kids[0].id).await.expect("clips");
        assert_eq!(clips.len(), 2);
        let by_title = |t: &str| {
            clips
                .iter()
                .find(|i| i.title == t)
                .unwrap_or_else(|| panic!("missing {t}: {clips:?}"))
                .clone()
        };
        // NFO seeding renamed this one and set its date and tags.
        let christmas = by_title("Christmas morning");
        assert_eq!(christmas.recorded_at.as_deref(), Some("2019-12-25"));
        assert_eq!(christmas.tags, vec!["kids".to_owned()]);
        assert!(christmas.nfo_seeded_at.is_some());
        // No sidecar: the title survives the year (the movie parser would have
        // eaten it) and the date came off the filename.
        let sandcastle = by_title("Sandcastle");
        assert_eq!(sandcastle.recorded_at.as_deref(), Some("2019-06-14"));
        assert!(sandcastle.nfo_seeded_at.is_none());

        // Rescan: nothing changed, and nothing re-seeds.
        let r = scan_library(&store, &lib).await.expect("rescan");
        assert_eq!(r.added, 0);
        assert_eq!(r.unchanged, 4);
        assert_eq!(r.seeded, 0);

        // (a) A hand edit survives a rewritten NFO — the sidecar is dead to
        // plurx once consumed. This is the whole seed-once contract.
        store
            .update_item_fields(
                christmas.id,
                &ItemEdit {
                    title: Some("Christmas at the beach".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("edit");
        std::fs::write(
            dir.path().join("2019/Beach Trip/Christmas 2019.nfo"),
            "<movie><title>OVERWRITTEN</title></movie>",
        )
        .expect("rewrite nfo");
        let r = scan_library(&store, &lib).await.expect("rescan2");
        assert_eq!(r.seeded, 0);
        assert_eq!(
            store
                .get_item(christmas.id)
                .await
                .expect("get")
                .expect("present")
                .title,
            "Christmas at the beach"
        );

        // (b) A sidecar written *after* the file was scanned seeds on the next
        // rescan — the file is unchanged, but the item is unseeded.
        std::fs::write(
            dir.path()
                .join("2019/Beach Trip/2019-06-14 - Sandcastle.nfo"),
            "<movie><title>Sandcastle contest</title></movie>",
        )
        .expect("late nfo");
        let r = scan_library(&store, &lib).await.expect("rescan3");
        assert_eq!(r.seeded, 1, "a late sidecar seeds exactly once");
        let seeded = store
            .get_item(sandcastle.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(seeded.title, "Sandcastle contest");
        assert_eq!(
            seeded.recorded_at.as_deref(),
            Some("2019-06-14"),
            "an NFO without a date leaves the ladder's date alone"
        );
        let r = scan_library(&store, &lib).await.expect("rescan4");
        assert_eq!(r.seeded, 0, "and never again");
    }

    /// A minimal JPEG carrying one EXIF field: `DateTimeOriginal`. Hand-built
    /// because the point is the metadata, not the pixels — a real camera file
    /// would be megabytes of test fixture for the same assertion.
    fn write_exif_jpeg(path: &Path, taken: &str) {
        fn le16(v: u16) -> [u8; 2] {
            v.to_le_bytes()
        }
        fn le32(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }
        let mut ascii = taken.as_bytes().to_vec();
        ascii.push(0);

        // TIFF: header → IFD0 (one pointer to the Exif IFD) → Exif IFD (one
        // DateTimeOriginal) → the string itself. Offsets are from the header.
        const IFD0: u32 = 8;
        const EXIF_IFD: u32 = 26;
        const ASCII_AT: u32 = 44;
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&le32(IFD0));
        tiff.extend_from_slice(&le16(1));
        tiff.extend_from_slice(&le16(0x8769)); // ExifIFDPointer
        tiff.extend_from_slice(&le16(4)); // LONG
        tiff.extend_from_slice(&le32(1));
        tiff.extend_from_slice(&le32(EXIF_IFD));
        tiff.extend_from_slice(&le32(0));
        tiff.extend_from_slice(&le16(1));
        tiff.extend_from_slice(&le16(0x9003)); // DateTimeOriginal
        tiff.extend_from_slice(&le16(2)); // ASCII
        tiff.extend_from_slice(&le32(ascii.len() as u32));
        tiff.extend_from_slice(&le32(ASCII_AT));
        tiff.extend_from_slice(&le32(0));
        tiff.extend_from_slice(&ascii);

        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, jpeg).expect("write jpeg");
    }

    #[tokio::test]
    async fn photos_take_their_date_from_exif() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_exif_jpeg(&dir.path().join("IMG_4021.jpg"), "2019:06:14 12:34:56");
        // No EXIF: the ladder falls through to the filename.
        std::fs::write(dir.path().join("2011-08-09 Picnic.jpg"), b"not a jpeg").expect("write");

        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        assert_eq!(scan_library(&store, &lib).await.expect("scan").added, 2);

        let page = store
            .list_top_items(lib.id, ItemSort::Recorded, 0, 10)
            .await
            .expect("list");
        let dates: Vec<(&str, Option<&str>)> = page
            .items
            .iter()
            .map(|i| (i.title.as_str(), i.recorded_at.as_deref()))
            .collect();
        assert_eq!(
            dates,
            vec![
                ("IMG_4021", Some("2019-06-14T12:34:56")),
                ("Picnic", Some("2011-08-09")),
            ],
            "EXIF beats the filename, and newest sorts first"
        );
        assert!(page.items.iter().all(|i| i.kind == ItemKind::Photo));
    }

    #[tokio::test]
    async fn photos_stay_out_of_recently_added() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "Trip/clip.mp4").await;
        for n in 0..5 {
            std::fs::write(dir.path().join(format!("Trip/IMG_{n}.jpg")), b"x").expect("photo");
        }
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        scan_library(&store, &lib).await.expect("scan");

        // A photo import must not flood the home screen; its video and folder
        // still surface that something arrived.
        let recent = store
            .recently_added(Some(lib.id), 20)
            .await
            .expect("recent");
        let kinds: Vec<ItemKind> = recent.iter().map(|r| r.item.kind).collect();
        assert!(!kinds.contains(&ItemKind::Photo), "kinds: {kinds:?}");
        assert!(kinds.contains(&ItemKind::Video));
        assert!(kinds.contains(&ItemKind::Folder));
    }

    #[tokio::test]
    async fn empty_home_root_names_photos_too() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        let r = scan_library(&store, &lib).await.expect("scan");
        assert!(
            r.problems
                .iter()
                .any(|p| p.contains("no video or photo files found")),
            "problems: {:?}",
            r.problems
        );
    }

    #[tokio::test]
    async fn photos_are_invisible_outside_home_libraries() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "Heat (1995).mkv").await;
        std::fs::write(dir.path().join("poster.jpg"), b"jpeg-ish").expect("poster");

        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 1, "the poster next to a movie stays invisible");
    }

    #[tokio::test]
    async fn deleted_home_files_prune_their_folder_chain() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(dir.path(), "2019/Summer/Beach Trip/clip.mp4").await;
        let lib = store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");
        assert_eq!(scan_library(&store, &lib).await.expect("scan").added, 1);

        std::fs::remove_file(dir.path().join("2019/Summer/Beach Trip/clip.mp4")).expect("rm");
        let r = scan_library(&store, &lib).await.expect("rescan");
        assert_eq!(r.removed_files, 1);
        assert_eq!(
            r.pruned_items, 4,
            "the video and all three empty folders above it"
        );
        let top = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(top.total, 0);
    }

    #[tokio::test]
    async fn scans_show_hierarchy() {
        let store = SqliteStore::open_in_memory().expect("store");
        let dir = tempfile::tempdir().expect("tmp");
        write_fake_video(
            dir.path(),
            "Severance (2022)/Season 01/Severance - S01E01 - Good News.mkv",
        )
        .await;
        write_fake_video(
            dir.path(),
            "Severance (2022)/Season 01/Severance - S01E02 - Half Loop.mkv",
        )
        .await;
        // A stray non-episode file is skipped, not errored.
        write_fake_video(dir.path(), "Severance (2022)/trailer.mkv").await;

        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("lib");

        let r = scan_library(&store, &lib).await.expect("scan");
        assert_eq!(r.added, 2);
        assert_eq!(r.skipped, 1);

        // One show → one season → two episodes.
        let page = store
            .list_top_items(lib.id, ItemSort::Title, 0, 10)
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        let show = &page.items[0];
        assert_eq!(show.title, "Severance");
        let seasons = store.get_item_children(show.id).await.expect("seasons");
        assert_eq!(seasons.len(), 1);
        let eps = store.get_item_children(seasons[0].id).await.expect("eps");
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].episode_number, Some(1));
    }
}

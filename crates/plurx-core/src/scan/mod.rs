//! Library scanning: walk the tree, place items in the hierarchy, record
//! files with their probe metadata, and reconcile away vanished files.
//!
//! Incremental by design (REQ-LIB-3): a file whose size and mtime are
//! unchanged is skipped without re-probing, so a rescan of a large library is
//! cheap and easy on shared storage. Probing runs sequentially for the same
//! reason — a scan should not thrash a NAS.

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
                    if entry.file_type().is_file() && is_video(entry.path()) {
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
        report.problems.push(format!(
            "no video files found under {} — the path exists but contains no recognized \
             video containers ({})",
            library
                .paths
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", "),
            VIDEO_EXTS.join(", ")
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
                continue;
            }
        }
        let is_new = existing.is_none();

        let item_id = match place_item(store, library, &path).await? {
            Some(id) => id,
            None => {
                // Couldn't place it (e.g. a Shows file with no S/E marker).
                report.skipped += 1;
                continue;
            }
        };

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
        skipped = report.skipped,
        errors = report.errors,
        "scan complete"
    );
    for problem in &report.problems {
        tracing::warn!(library = %library.name, "scan problem: {problem}");
    }
    Ok(report)
}

/// Find-or-create the item a file belongs to, returning its id. `None` means
/// the file couldn't be identified for this library kind.
async fn place_item(
    store: &dyn Store,
    library: &Library,
    path: &Path,
) -> Result<Option<i64>, StoreError> {
    match library.kind {
        LibraryKind::Movies => {
            let parsed = parse::parse_movie(path);
            if let Some(existing) = store
                .find_movie(library.id, &parsed.title, parsed.year)
                .await?
            {
                return Ok(Some(existing.id));
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
            Ok(Some(id))
        }
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
                return Ok(Some(existing.id));
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
            Ok(Some(id))
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
    use crate::domain::{ItemSort, NewLibrary};
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
            r.problems.iter().any(|p| p.contains("no video files found")),
            "problems: {:?}",
            r.problems
        );
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

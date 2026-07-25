//! The `home` library arm of the scanner: folder mirroring, one-time NFO
//! seeding, and the recorded-date ladder.
//!
//! Home video has no metadata provider — the source of truth is what's on
//! disk. The directory tree *is* the organization (docs/HOMEVIDEO-PLAN.md
//! §1.2): `2019/Beach Trip/clip.mp4` becomes a Folder "2019" holding a Folder
//! "Beach Trip" holding a Video. Loose files at a root stand alone.
//!
//! Nothing in here writes to a library path. Thumbnails go in the artwork
//! cache; the NFO is read once and never written.

use std::path::{Path, PathBuf};

use super::{nfo, parse};
use crate::domain::{Item, ItemKind, Library, MetadataPatch, NewItem, ProbeResult};
use crate::error::StoreError;
use crate::store::Store;

/// Still-image extensions, recognized **only** in home libraries — a poster
/// JPEG sitting next to a movie must stay invisible, exactly as today.
pub const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif"];

pub fn is_photo(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| PHOTO_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Where a scanned file landed, and whether this scan is what created it.
/// Only a freshly created item gets the date ladder applied — a rescan must
/// never overwrite a date the owner edited by hand.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    pub id: i64,
    pub created: bool,
}

/// Mirror the directory chain above `path` and find-or-create the item for
/// the file itself.
pub async fn place(
    store: &dyn Store,
    library: &Library,
    path: &Path,
) -> Result<Option<Placed>, StoreError> {
    let Some(dirs) = relative_dirs(library, path) else {
        // Outside every configured root — can't happen from the walk, but a
        // silent mis-parent would be much worse than a skip.
        return Ok(None);
    };

    let mut parent: Option<i64> = None;
    for dir in dirs {
        parent = Some(find_or_create_folder(store, library, parent, &dir).await?);
    }

    let parsed = parse::parse_home_media(path);
    let kind = if is_photo(path) {
        ItemKind::Photo
    } else {
        ItemKind::Video
    };
    if let Some(existing) = store
        .find_child_item(library.id, parent, kind, &parsed.title)
        .await?
    {
        return Ok(Some(Placed {
            id: existing.id,
            created: false,
        }));
    }
    let id = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind,
            parent_id: parent,
            title: parsed.title,
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await?;
    Ok(Some(Placed { id, created: true }))
}

/// Everything that happens after the file's row is written: seed from an NFO
/// sidecar if the item has never been seeded, then fill `recorded_at` if it's
/// still empty. Returns whether a sidecar was consumed; problems are appended
/// to `problems`.
pub async fn after_record(
    store: &dyn Store,
    placed: Placed,
    path: &Path,
    probe: &ProbeResult,
    mtime: i64,
    problems: &mut Vec<String>,
) -> Result<bool, StoreError> {
    let Some(item) = store.get_item(placed.id).await? else {
        return Ok(false);
    };
    let seeded = seed_from_nfo(store, &item, path, problems).await?;

    // The ladder only fills a hole. An NFO date (applied just above) wins,
    // and so does anything the owner typed in the edit UI.
    if !placed.created && !seeded {
        return Ok(seeded);
    }
    let current = store.get_item(placed.id).await?;
    if current.and_then(|i| i.recorded_at).is_some() {
        return Ok(seeded);
    }
    if let Some(date) = ladder_date(path, probe, mtime) {
        store
            .apply_metadata(
                placed.id,
                &MetadataPatch {
                    recorded_at: Some(date),
                    ..Default::default()
                },
            )
            .await?;
    }
    Ok(seeded)
}

/// Seed an item's metadata from `<basename>.nfo` — **at most once, ever**
/// (docs/HOMEVIDEO-PLAN.md §4.3). Returns whether a sidecar was consumed.
///
/// Photos never seed: Kodi has no photo NFO convention, so their metadata
/// comes from EXIF and the filename.
///
/// The check runs on the *item*, not inside the size+mtime skip, which is
/// what makes "write the sidecar next week, rescan, it seeds then" work. Once
/// `nfo_seeded_at` is set the sidecar is dead to plurx: a rewritten NFO
/// changes nothing, so a hand edit can never be clobbered by one.
pub async fn seed_from_nfo(
    store: &dyn Store,
    item: &Item,
    path: &Path,
    problems: &mut Vec<String>,
) -> Result<bool, StoreError> {
    if item.kind != ItemKind::Video || item.nfo_seeded_at.is_some() {
        return Ok(false);
    }
    let Some(sidecar) = nfo::sidecar_for(path) else {
        return Ok(false);
    };
    match nfo::read(&sidecar) {
        Some(parsed) => {
            let patch = MetadataPatch {
                title: parsed.title,
                overview: parsed.overview,
                year: parsed.year,
                recorded_at: parsed.recorded_at,
                tags: (!parsed.tags.is_empty()).then_some(parsed.tags),
                ..Default::default()
            };
            if !patch.is_empty() {
                store.apply_metadata(item.id, &patch).await?;
            }
        }
        None => {
            // A broken sidecar should complain once, not on every scan — so
            // it still counts as consumed.
            problems.push(format!(
                "`{}` is not a readable Kodi <movie> NFO — ignoring it (the item keeps \
                 its filename metadata, and plurx will not look at this file again)",
                sidecar.display()
            ));
        }
    }
    store.set_nfo_seeded(item.id).await?;
    Ok(true)
}

/// The recorded-date ladder below the NFO (which is applied separately):
/// container `creation_time` → a date lifted off the filename → file mtime.
/// mtime lies after a copy, but it beats nothing, and the edit UI is the fix.
fn ladder_date(path: &Path, probe: &ProbeResult, mtime: i64) -> Option<String> {
    if let Some(created) = probe.creation_time.clone() {
        return Some(created);
    }
    if let Some(date) = parse::parse_home_media(path).date {
        return Some(date);
    }
    (mtime > 0).then(|| date_from_unix(mtime))
}

/// `YYYY-MM-DD` (UTC) for a unix timestamp. Hand-rolled because the date is
/// only ever sorted and displayed — pulling in a calendar crate for one
/// conversion isn't worth the dependency.
pub fn date_from_unix(secs: i64) -> String {
    // days_from_civil, inverted (Howard Hinnant's civil_from_days).
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The directory components between the library root containing `path` and
/// the file itself. `None` when the path is under no configured root.
fn relative_dirs(library: &Library, path: &Path) -> Option<Vec<String>> {
    let parent = path.parent()?;
    let root = library
        .paths
        .iter()
        // Longest root first: nested roots would otherwise mirror the deeper
        // one's folders under the shallower one.
        .filter(|root| parent.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())?;
    let rest: PathBuf = parent.strip_prefix(root).ok()?.to_path_buf();
    Some(
        rest.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect(),
    )
}

/// Folder identity is (library, parent, kind, name). Multiple roots therefore
/// merge at the top level by folder name — the same rule shows already use.
async fn find_or_create_folder(
    store: &dyn Store,
    library: &Library,
    parent: Option<i64>,
    name: &str,
) -> Result<i64, StoreError> {
    if let Some(existing) = store
        .find_child_item(library.id, parent, ItemKind::Folder, name)
        .await?
    {
        return Ok(existing.id);
    }
    store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Folder,
            parent_id: parent,
            title: name.to_owned(),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_extensions_are_case_insensitive() {
        assert!(is_photo(Path::new("/h/IMG_1.JPG")));
        assert!(is_photo(Path::new("/h/a.heic")));
        assert!(!is_photo(Path::new("/h/clip.mp4")));
        assert!(!is_photo(Path::new("/h/notes.txt")));
    }

    #[test]
    fn dates_convert_without_a_calendar_crate() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(1_560_500_523), "2019-06-14");
        // Leap day, and the day after.
        assert_eq!(date_from_unix(1_583_020_800), "2020-03-01");
        assert_eq!(date_from_unix(1_582_934_400), "2020-02-29");
    }

    #[test]
    fn the_ladder_prefers_the_container_date() {
        let probe = ProbeResult {
            creation_time: Some("2019-06-14T18:22:03".into()),
            ..Default::default()
        };
        let path = Path::new("/h/2001-01-01 - Beach.mp4");
        assert_eq!(
            ladder_date(path, &probe, 1_600_000_000).as_deref(),
            Some("2019-06-14T18:22:03")
        );
        // No container date → the filename's.
        assert_eq!(
            ladder_date(path, &ProbeResult::default(), 1_600_000_000).as_deref(),
            Some("2001-01-01")
        );
        // Neither → mtime, date only.
        assert_eq!(
            ladder_date(
                Path::new("/h/IMG_4021.mp4"),
                &ProbeResult::default(),
                1_560_500_523
            )
            .as_deref(),
            Some("2019-06-14")
        );
    }
}

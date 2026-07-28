//! Keeping the pre-transcode cache inside its budget, and cleaning up after
//! producers that died.
//!
//! Separate from the producer because it has to run whether or not anything is
//! being produced. A cache that only sweeps when it is also filling is a cache
//! that holds its high-water mark forever once you turn production off — and
//! turning production off is exactly what somebody does when their disk is
//! full.
//!
//! Three jobs, in the order they must happen:
//!
//! 1. **Crash leftovers.** A `complete = 0` row is either a producer at work or
//!    a producer that died; only age tells them apart. Old ones are swept
//!    first because their bytes are on the disk the budget is about.
//! 2. **Budget.** Evict coldest-first until the total fits. LRU by
//!    `last_used_at`, which the serving path touches on every hit — so what
//!    survives is what people come back to, not what happened to be made last.
//! 3. **Orphan directories.** Bytes on disk with no row are invisible to every
//!    query above, which makes them the one kind of leak a budget cannot
//!    correct: the sum says there is room and the filesystem disagrees.
//!
//! Deleting the directory before the row, everywhere. The other order leaves a
//! row pointing at nothing when the process dies between the two, and that row
//! is a *hit* — a viewer gets a playlist for a directory that no longer exists.
//! This way the failure is an orphan directory, which step 3 collects.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use plurx_core::domain::CachedTranscode;
use plurx_core::store::{keys, Store};
use std::sync::Arc;

/// How old an unfinished claim has to be before it counts as a crash rather
/// than as work in progress.
///
/// A day, deliberately generously. The cost of waiting is one stale directory;
/// the cost of being wrong in the other direction is deleting the output of a
/// producer that is still writing it — a 4K film can take hours, and a job
/// paused behind live playback for an afternoon is normal, not dead.
pub const STALE_CLAIM_SECS: i64 = 24 * 3600;

/// Default budget when nothing is set. Fifty gigabytes is a few 4K films: big
/// enough that Next Up is nearly always warm, small enough to be an obviously
/// safe default on a NAS.
pub const DEFAULT_MAX_GB: i64 = 50;

const GB: i64 = 1024 * 1024 * 1024;

/// What one sweep did, for the log and for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Unfinished claims old enough to be crash leftovers.
    pub stale: usize,
    /// Complete entries evicted to get under budget.
    pub evicted: usize,
    /// Directories on disk that no row claimed.
    pub orphans: usize,
    pub bytes_freed: i64,
    /// What the cache occupies now, by the rows.
    pub bytes_after: i64,
}

/// The cache's disk budget in bytes, or `None` when the cache is switched off.
///
/// Off is a real answer rather than a budget of zero, and the difference shows
/// up one line later: a zero budget evicts everything on every sweep, which is
/// what somebody who set `max_gb = 0` to stop the cache actually wants.
pub async fn budget_bytes(store: &Arc<dyn Store>) -> Option<i64> {
    let gb = match store.get_setting(keys::CACHE_MAX_GB).await {
        Ok(Some(v)) => v.trim().parse::<i64>().ok().filter(|n| *n >= 0),
        _ => None,
    }
    .unwrap_or(DEFAULT_MAX_GB);
    (gb > 0).then(|| gb * GB)
}

/// Run the whole sweep. Safe to call at any time; does nothing surprising when
/// the cache is empty, unconfigured, or its root does not exist.
///
/// `now` is passed in rather than read here, for the same reason
/// [`crate::schedule::due_jobs`] takes one: the interesting cases are all about
/// elapsed time, and a function that reads its own clock can only be tested by
/// waiting. Here that would mean waiting a day.
pub async fn sweep(store: &Arc<dyn Store>, root: &Path, node_id: &str, now: i64) -> Swept {
    let mut out = Swept::default();

    // 1. Crash leftovers, before the budget: their bytes are on the same disk.
    match store
        .stale_cache_claims(node_id, now - STALE_CLAIM_SECS)
        .await
    {
        Ok(stale) => {
            for entry in stale {
                if forget(store, root, node_id, &entry).await {
                    out.stale += 1;
                    // Not added to `bytes_freed`: an incomplete row's `bytes`
                    // is whatever it was when it was claimed, which is zero.
                    // Reporting it would make the figure a fiction.
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cache: could not list stale claims"),
    }

    // 2. Budget.
    let budget = budget_bytes(store).await;
    let mut used = store.cache_bytes(node_id).await.unwrap_or(0);
    let ceiling = budget.unwrap_or(0);
    if used > ceiling {
        // Coldest first. Asking for them in one page rather than one at a time
        // because the alternative re-queries per eviction, and the answer only
        // changes in the direction we are already walking.
        match store.cache_by_age(node_id, 512).await {
            Ok(cold) => {
                for entry in cold {
                    if used <= ceiling {
                        break;
                    }
                    let bytes = entry.bytes.max(0);
                    if forget(store, root, node_id, &entry).await {
                        out.evicted += 1;
                        out.bytes_freed += bytes;
                        used -= bytes;
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "cache: could not list entries by age"),
        }
        if used > ceiling {
            // Said out loud rather than left to be inferred from a disk that
            // keeps growing. One page of evictions was not enough, which on a
            // 512-entry page means something is wrong with the sizes, not with
            // the walk.
            tracing::warn!(
                used,
                ceiling,
                "cache: still over budget after a full eviction pass"
            );
        }
    }
    out.bytes_after = used;

    // 3. Directories nothing claims.
    out.orphans = sweep_orphan_dirs(store, root, node_id).await;
    if out.stale + out.evicted + out.orphans > 0 {
        tracing::info!(
            stale = out.stale,
            evicted = out.evicted,
            orphans = out.orphans,
            freed = out.bytes_freed,
            used = out.bytes_after,
            "cache: swept"
        );
    }
    out
}

/// Delete an entry's bytes, then its row. Returns whether the row went.
///
/// Directory first, always. If the process dies between the two the row is left
/// pointing at nothing — and that row is a *hit*, which hands a viewer a
/// playlist for a directory that is gone. The other order leaves an orphan
/// directory instead, which the sweep below collects and which nothing serves.
async fn forget(
    store: &Arc<dyn Store>,
    root: &Path,
    node_id: &str,
    entry: &CachedTranscode,
) -> bool {
    let Some(dir) = entry_dir(root, &entry.relative_dir) else {
        // A row whose path escapes the root cannot be trusted to name what to
        // delete. Drop the row and leave the bytes, wherever they are.
        tracing::warn!(
            recipe = %entry.recipe_hash, dir = %entry.relative_dir,
            "cache: refusing to delete a path outside the cache root; dropping the row only"
        );
        return store
            .forget_cache_entry(&entry.recipe_hash, node_id)
            .await
            .is_ok();
    };
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            // The row stays. Dropping it would strand the bytes permanently:
            // nothing else knows they exist, and the orphan sweep below only
            // reaches directories directly under the root's two levels.
            tracing::warn!(
                recipe = %entry.recipe_hash, dir = %dir.display(), error = %e,
                "cache: could not remove entry directory; keeping the row so it is retried"
            );
            return false;
        }
    }
    match store.forget_cache_entry(&entry.recipe_hash, node_id).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(recipe = %entry.recipe_hash, error = %e, "cache: could not forget entry");
            false
        }
    }
}

/// Resolve a row's relative directory under the root, refusing anything that
/// escapes it.
///
/// The rows are ours, so this should never fire — which is the reason to check
/// rather than to trust. What it guards is a `remove_dir_all`, and the distance
/// between "a corrupt row" and "the media library" is one `..`.
fn entry_dir(root: &Path, relative: &str) -> Option<PathBuf> {
    let rel = Path::new(relative);
    if rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        return None;
    }
    if relative.trim().is_empty() {
        return None;
    }
    Some(root.join(rel))
}

/// Delete cache directories no row claims.
///
/// The cache is two levels deep (`ab/abcdef…`), the first level being a fanout
/// so no single directory holds thousands of entries. Both levels are walked
/// because a leftover can be either: a half-published entry under a live
/// prefix, or a whole prefix left by a producer that died before its first
/// rename.
async fn sweep_orphan_dirs(store: &Arc<dyn Store>, root: &Path, node_id: &str) -> usize {
    // Everything a row claims, as absolute paths. Both complete and claimed:
    // a producer's temp destination is claimed and must not be swept out from
    // under it.
    let mut known: HashSet<PathBuf> = HashSet::new();
    let rows = store
        .cache_by_age(node_id, i64::MAX)
        .await
        .unwrap_or_default();
    let claims = store
        .stale_cache_claims(node_id, i64::MAX)
        .await
        .unwrap_or_default();
    for e in rows.iter().chain(claims.iter()) {
        if let Some(dir) = entry_dir(root, &e.relative_dir) {
            known.insert(dir);
        }
    }

    let Ok(mut prefixes) = tokio::fs::read_dir(root).await else {
        return 0; // no cache root yet just means nothing has been produced
    };
    let mut removed = 0usize;
    while let Ok(Some(prefix)) = prefixes.next_entry().await {
        let ppath = prefix.path();
        if !prefix
            .file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&ppath).await else {
            continue;
        };
        let mut kept = 0usize;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if known.contains(&path) {
                kept += 1;
                continue;
            }
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(dir = %path.display(), "cache: removed an unclaimed directory");
                }
                Err(e) => {
                    tracing::warn!(dir = %path.display(), error = %e, "cache: orphan sweep failed")
                }
            }
        }
        // An empty prefix is not worth keeping, but it is also not worth
        // forcing: `remove_dir` fails harmlessly if something appeared.
        if kept == 0 {
            let _ = tokio::fs::remove_dir(&ppath).await;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};
    use plurx_core::store::{LibraryStore, MediaStore, SqliteStore};

    /// The real clock. These tests are about elapsed time, so `sweep` takes
    /// `now` as an argument; this is only the starting point they measure from.
    fn unix_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    const NODE: &str = "node-a";

    async fn store() -> (Arc<dyn Store>, i64) {
        let store = SqliteStore::open_in_memory().expect("store");
        let lib = store
            .create_library(&NewLibrary {
                name: "M".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let file = store
            .upsert_file(movie, "/m/Heat.mkv", 1, 1, &ProbeResult::default())
            .await
            .expect("file");
        (Arc::new(store) as Arc<dyn Store>, file)
    }

    /// A claimed directory with bytes in it, on disk and in the store.
    async fn claim(store: &Arc<dyn Store>, root: &Path, file: i64, hash: &str) -> PathBuf {
        let rel = format!("{}/{hash}", &hash[..2]);
        let dir = root.join(&rel);
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        tokio::fs::write(dir.join("index.m3u8"), vec![b'x'; 16])
            .await
            .expect("playlist");
        store
            .claim_cache_entry(hash, file, 1, NODE, &rel)
            .await
            .expect("claim");
        dir
    }

    /// …and finished, so it can be served and evicted.
    async fn entry(
        store: &Arc<dyn Store>,
        root: &Path,
        file: i64,
        hash: &str,
        bytes: i64,
    ) -> PathBuf {
        let dir = claim(store, root, file, hash).await;
        store
            .complete_cache_entry(hash, NODE, bytes)
            .await
            .expect("complete");
        dir
    }

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("root")
    }

    /// The budget is a ceiling and eviction stops the moment it is met — the
    /// cache is not supposed to empty itself over one entry too many. What goes
    /// is decided coldest-first by the store (tested there); what this owns is
    /// stopping at the right point, and taking the *bytes* rather than just the
    /// row. A budget that deletes rows and leaves directories is not a budget.
    #[tokio::test]
    async fn eviction_stops_as_soon_as_it_fits_and_takes_the_bytes_with_it() {
        let (store, file) = store().await;
        let root = root();
        store
            .put_setting(keys::CACHE_MAX_GB, "1")
            .await
            .expect("budget");
        // Three entries at 0.4 GB: 1.2 GB against a 1 GB ceiling. One has to go
        // and only one — 0.8 GB fits.
        let size = (0.4 * GB as f64) as i64;
        let mut dirs = Vec::new();
        for hash in ["aafirst", "bbsecond", "ccthird"] {
            dirs.push(entry(&store, root.path(), file, hash, size).await);
        }
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), size * 3);

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.evicted, 1, "one is enough, so one is all that goes");
        assert_eq!(out.bytes_freed, size);
        assert!(out.bytes_after <= GB, "still over: {}", out.bytes_after);

        let gone: Vec<bool> = dirs.iter().map(|d| !d.exists()).collect();
        assert_eq!(
            gone.iter().filter(|g| **g).count(),
            1,
            "exactly one directory should have gone, got {gone:?}"
        );
        // Whichever it was, its row went with its bytes and vice versa.
        for (hash, dir) in ["aafirst", "bbsecond", "ccthird"].iter().zip(&dirs) {
            let row = store.cache_hit(hash, NODE).await.expect("hit").is_some();
            assert_eq!(
                row,
                dir.exists(),
                "{hash}: row and bytes disagree — one outlived the other"
            );
        }
    }

    /// `max_gb = 0` is how somebody turns the cache off, and turning it off has
    /// to reclaim the disk — that is the whole reason they touched the setting.
    #[tokio::test]
    async fn a_zero_budget_empties_the_cache() {
        let (store, file) = store().await;
        let root = root();
        let dir = entry(&store, root.path(), file, "aakeep", 100).await;
        assert!(
            budget_bytes(&store).await.is_some(),
            "the default is a budget"
        );

        store
            .put_setting(keys::CACHE_MAX_GB, "0")
            .await
            .expect("off");
        assert!(budget_bytes(&store).await.is_none(), "zero means off");
        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.evicted, 1);
        assert_eq!(out.bytes_after, 0);
        assert!(!dir.exists());
    }

    /// A claim is either a producer at work or a producer that died, and only
    /// elapsed time tells them apart. Nothing about the claim below changes
    /// between the two sweeps — only the clock does, which is exactly the
    /// distinction being drawn.
    #[tokio::test]
    async fn a_claim_becomes_a_crash_leftover_only_with_age() {
        let (store, file) = store().await;
        let root = root();
        let now = unix_now();
        let dir = claim(&store, root.path(), file, "aaworking").await;

        let out = sweep(&store, root.path(), NODE, now).await;
        assert_eq!(out.stale, 0, "a fresh claim is a producer at work");
        assert!(
            dir.exists(),
            "a producer's output was deleted underneath it"
        );

        let out = sweep(&store, root.path(), NODE, now + STALE_CLAIM_SECS + 60).await;
        assert_eq!(out.stale, 1, "a day-old claim is a producer that died");
        assert!(!dir.exists());
    }

    /// A live producer's destination is claimed but not complete, and the
    /// orphan sweep must not read that as a leftover: it would delete the
    /// directory being written into, and the failure would surface much later
    /// as a corrupt entry, somewhere else entirely.
    #[tokio::test]
    async fn a_producer_at_work_is_not_an_orphan() {
        let (store, file) = store().await;
        let root = root();
        let dir = claim(&store, root.path(), file, "cdworking").await;
        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!((out.orphans, out.stale, out.evicted), (0, 0, 0));
        assert!(dir.exists());
    }

    /// Bytes with no row are the one leak a budget cannot see: every query says
    /// there is room and the filesystem disagrees. They arrive the ordinary
    /// way — a kill between writing the directory and committing the row.
    #[tokio::test]
    async fn directories_nothing_claims_are_reclaimed() {
        let (store, file) = store().await;
        let root = root();
        let kept = entry(&store, root.path(), file, "aakeep", 10).await;
        // A leftover beside a live entry, and a whole prefix of leftovers.
        tokio::fs::create_dir_all(root.path().join("aa/aaorphan"))
            .await
            .expect("mkdir");
        tokio::fs::write(root.path().join("aa/aaorphan/seg00000.ts"), b"junk")
            .await
            .expect("write");
        tokio::fs::create_dir_all(root.path().join("zz/zzorphan"))
            .await
            .expect("mkdir");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.orphans, 2);
        assert!(kept.join("index.m3u8").exists(), "a live entry was swept");
        assert!(!root.path().join("aa/aaorphan").exists());
        assert!(
            !root.path().join("zz").exists(),
            "an emptied prefix goes with its last entry"
        );
    }

    /// The rows are ours, so a `..` in one should be impossible — which is
    /// exactly why it is checked rather than trusted. What it guards is a
    /// recursive delete, and the distance between a corrupt row and somebody's
    /// media library is one path component.
    #[test]
    fn a_row_cannot_name_a_directory_outside_the_cache() {
        let root = Path::new("/var/lib/plurx/cache/transcode");
        assert_eq!(
            entry_dir(root, "ab/abcdef"),
            Some(root.join("ab/abcdef")),
            "an ordinary entry still resolves"
        );
        for bad in ["../../../media", "ab/../../media", "/etc", "", "   "] {
            assert!(entry_dir(root, bad).is_none(), "{bad:?} should be refused");
        }
    }

    /// Bytes that will not delete keep their row. Dropping it would strand them
    /// permanently: nothing else records that they exist, so no later sweep
    /// could find them, and the budget would go on counting disk it can never
    /// reclaim.
    ///
    /// The failure is injected by making the entry's path a *file*, which
    /// `remove_dir_all` refuses for every user. Permissions would be the more
    /// natural injection and were the first attempt; they are useless here,
    /// because root ignores them and containers run as root.
    #[tokio::test]
    async fn bytes_that_will_not_delete_keep_their_row() {
        let (store, file) = store().await;
        let root = root();
        tokio::fs::create_dir_all(root.path().join("aa"))
            .await
            .expect("prefix");
        tokio::fs::write(root.path().join("aa/aastuck"), b"not a directory")
            .await
            .expect("write");
        store
            .claim_cache_entry("aastuck", file, 1, NODE, "aa/aastuck")
            .await
            .expect("claim");
        store
            .complete_cache_entry("aastuck", NODE, 100)
            .await
            .expect("complete");

        store
            .put_setting(keys::CACHE_MAX_GB, "0")
            .await
            .expect("off");
        let out = sweep(&store, root.path(), NODE, unix_now()).await;

        assert_eq!(out.evicted, 0, "nothing was actually freed");
        assert!(
            store
                .cache_hit("aastuck", NODE)
                .await
                .expect("hit")
                .is_some(),
            "the row went but the bytes did not — they are unreachable forever now"
        );
        assert!(
            root.path().join("aa/aastuck").exists(),
            "and the bytes are still there, so the next sweep gets another go"
        );
    }

    /// The sweep runs against a server that has never produced anything far
    /// more often than against one that has. None of it may fail, log alarming
    /// things, or create the root as a side effect of looking at it.
    #[tokio::test]
    async fn an_empty_cache_sweeps_to_nothing() {
        let (store, _file) = store().await;
        let missing = root().path().join("never-made");
        assert_eq!(
            sweep(&store, &missing, NODE, unix_now()).await,
            Swept::default()
        );
        assert!(!missing.exists(), "looking is not creating");
    }
}

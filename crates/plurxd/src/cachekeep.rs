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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use plurx_core::domain::CachedTranscode;
use plurx_core::store::{keys, Store};

/// Process-local ownership for finished cache entries that are being served.
///
/// The store records whether bytes exist; it cannot record whether an HTTP
/// response on this node is reading them right now. Readers claim a recipe
/// before looking it up and keep that claim for the session's lifetime.
/// Eviction claims the same recipe exclusively before deleting anything, so
/// lookup and removal cannot pass each other between the row check and the
/// filesystem operation.
///
/// The key is deliberately the recipe hash rather than
/// `(recipe_hash, node_id, storage_class)`: one manager owns one node process,
/// and a reader of any copy must conservatively block removal of every copy
/// that process could resolve. The safe cost is temporary over-protection;
/// including a class here would let a future routing mismatch under-protect
/// bytes that are actually in use.
#[derive(Clone, Default)]
pub struct ActiveCacheReaders {
    states: Arc<Mutex<HashMap<String, CacheActivity>>>,
}

#[derive(Clone, Copy)]
enum CacheActivity {
    Readers(usize),
    Evicting,
    /// A recipe this process previously served. It is not protected, but it
    /// is a positive ownership fact when the store returns an empty inventory
    /// after the row disappeared under that reader.
    IdleReader,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CacheBusy {
    Readers,
    Evicting,
}

pub struct CacheReadGuard {
    readers: ActiveCacheReaders,
    recipe: String,
}

pub(crate) struct CacheEvictionGuard {
    readers: ActiveCacheReaders,
    recipe: String,
}

impl ActiveCacheReaders {
    fn lock_states(&self) -> MutexGuard<'_, HashMap<String, CacheActivity>> {
        self.states.lock().unwrap_or_else(|poisoned| {
            tracing::error!("cache ownership mutex was poisoned; recovering its state");
            poisoned.into_inner()
        })
    }

    /// Start serving one recipe, unless eviction already owns it.
    pub fn begin_read(&self, recipe: &str) -> Option<CacheReadGuard> {
        let mut states = self.lock_states();
        match states.get_mut(recipe) {
            Some(CacheActivity::Readers(count)) => *count += 1,
            Some(CacheActivity::Evicting) => return None,
            Some(state @ CacheActivity::IdleReader) => *state = CacheActivity::Readers(1),
            None => {
                states.insert(recipe.to_owned(), CacheActivity::Readers(1));
            }
        }
        Some(CacheReadGuard {
            readers: self.clone(),
            recipe: recipe.to_owned(),
        })
    }

    /// Claim a recipe for removal. Active readers make eviction skip it; the
    /// next sweep will retry after their sessions end.
    pub(crate) fn begin_eviction(&self, recipe: &str) -> Result<CacheEvictionGuard, CacheBusy> {
        let mut states = self.lock_states();
        match states.get(recipe) {
            Some(CacheActivity::Readers(_)) => return Err(CacheBusy::Readers),
            Some(CacheActivity::Evicting) => return Err(CacheBusy::Evicting),
            Some(CacheActivity::IdleReader) => {}
            None => {}
        }
        states.insert(recipe.to_owned(), CacheActivity::Evicting);
        Ok(CacheEvictionGuard {
            readers: self.clone(),
            recipe: recipe.to_owned(),
        })
    }

    /// Number of recipes protected by at least one active playback session.
    /// Reader multiplicity stays internal; the operational question is how
    /// many cache entries housekeeping is presently forbidden to remove.
    pub fn active_entries(&self) -> usize {
        self.lock_states()
            .values()
            .filter(|state| matches!(state, CacheActivity::Readers(_)))
            .count()
    }

    /// Whether this process has positive evidence that a recipe was real.
    /// Used only to narrow an orphan pass when the durable owner inventory is
    /// empty; it never authorizes deletion of unrelated directories.
    fn knows_recipe(&self, recipe: &str) -> bool {
        self.lock_states().contains_key(recipe)
    }
}

impl Drop for CacheReadGuard {
    fn drop(&mut self) {
        let mismatch = {
            let mut states = self.readers.lock_states();
            match states.get_mut(&self.recipe) {
                Some(CacheActivity::Readers(count)) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(state @ CacheActivity::Readers(_)) => {
                    *state = CacheActivity::IdleReader;
                    false
                }
                Some(CacheActivity::Evicting | CacheActivity::IdleReader) | None => true,
            }
        };
        if mismatch {
            tracing::error!(
                recipe = %self.recipe,
                "cache reader ownership changed while its guard was held"
            );
        }
    }
}

impl Drop for CacheEvictionGuard {
    fn drop(&mut self) {
        let mismatch = {
            let mut states = self.readers.lock_states();
            if matches!(states.get(&self.recipe), Some(CacheActivity::Evicting)) {
                states.remove(&self.recipe);
                false
            } else {
                true
            }
        };
        if mismatch {
            tracing::error!(
                recipe = %self.recipe,
                "cache eviction ownership changed while its guard was held"
            );
        }
    }
}

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

/// Where a producer assembles an entry before publishing it: one directory per
/// recipe, under the cache root so the publish is a rename on one filesystem.
///
/// Named here rather than in the producer because *this* file is the one that
/// deletes things. It sits at the same depth as a fanout prefix and looks
/// exactly like one, so the orphan walk would otherwise treat a producer's
/// half-built asset as a leftover and remove it mid-encode — a producer losing
/// hours of work to the housekeeping job that runs beside it.
pub const STAGING: &str = "tmp";

/// The staging directory for one recipe. Deterministic, so a later pass can
/// find what an earlier one left and resume from it.
pub fn staging_dir(root: &Path, recipe_hash: &str) -> PathBuf {
    root.join(STAGING).join(recipe_hash)
}

const GB: i64 = 1024 * 1024 * 1024;

/// What one sweep did, for the log and for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Unfinished claims old enough to be crash leftovers.
    pub stale: usize,
    /// Complete entries evicted to get under budget.
    pub evicted: usize,
    /// Complete entries skipped because an active session is reading them.
    pub protected: usize,
    /// Entries already owned by another sweep in this process.
    pub in_flight: usize,
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
pub async fn sweep_with_readers(
    store: &Arc<dyn Store>,
    root: &Path,
    node_id: &str,
    readers: &ActiveCacheReaders,
    now: i64,
) -> Swept {
    let mut out = Swept::default();
    let mut removed_claims = HashSet::new();

    // 1. Crash leftovers, before the budget: their bytes are on the same disk.
    match store
        .stale_cache_claims(node_id, now - STALE_CLAIM_SECS)
        .await
    {
        Ok(stale) => {
            for entry in stale {
                let _eviction = match readers.begin_eviction(&entry.recipe_hash) {
                    Ok(guard) => guard,
                    Err(CacheBusy::Readers) => {
                        out.protected += 1;
                        continue;
                    }
                    Err(CacheBusy::Evicting) => {
                        out.in_flight += 1;
                        continue;
                    }
                };
                if forget(store, root, node_id, &entry).await {
                    removed_claims.insert(entry.recipe_hash.clone());
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
                    let _eviction = match readers.begin_eviction(&entry.recipe_hash) {
                        Ok(guard) => guard,
                        Err(CacheBusy::Readers) => {
                            out.protected += 1;
                            continue;
                        }
                        Err(CacheBusy::Evicting) => {
                            // A peer sweep is already changing the inventory
                            // this pass used to compute `used`. Continuing down
                            // the same snapshot would make both passes satisfy
                            // the full deficit independently and over-evict.
                            // End this pass; the peer owns the deletion and the
                            // next scheduled sweep will reconcile any failure.
                            out.in_flight += 1;
                            break;
                        }
                    };
                    if forget(store, root, node_id, &entry).await {
                        out.evicted += 1;
                        out.bytes_freed += bytes;
                        used -= bytes;
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "cache: could not list entries by age"),
        }
        if used > ceiling && out.in_flight > 0 {
            tracing::debug!(
                used,
                ceiling,
                in_flight = out.in_flight,
                "cache: peer sweep owns an eviction; ending this budget pass"
            );
        } else if used > ceiling && out.protected > 0 {
            tracing::warn!(
                used,
                ceiling,
                protected = out.protected,
                "cache: active playback keeps cache over budget; retrying on the next sweep"
            );
        } else if used > ceiling {
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
    let (orphans, protected, in_flight) =
        sweep_orphan_dirs(store, root, node_id, readers, &removed_claims).await;
    out.orphans = orphans;
    out.protected += protected;
    out.in_flight += in_flight;
    if out.stale + out.evicted + out.protected + out.in_flight + out.orphans > 0 {
        tracing::info!(
            stale = out.stale,
            evicted = out.evicted,
            protected = out.protected,
            in_flight = out.in_flight,
            orphans = out.orphans,
            freed = out.bytes_freed,
            used = out.bytes_after,
            "cache: swept"
        );
    }
    out
}

/// Tests that do not model an active HTTP reader use an empty registry. The
/// production entry points call [`sweep_with_readers`] with the transcode
/// manager's shared registry, so there is no unprotected production sweep.
#[cfg(test)]
async fn sweep(store: &Arc<dyn Store>, root: &Path, node_id: &str, now: i64) -> Swept {
    sweep_with_readers(store, root, node_id, &ActiveCacheReaders::default(), now).await
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
            .forget_cache_entry(&entry.recipe_hash, node_id, &entry.storage_class)
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
    match store
        .forget_cache_entry(&entry.recipe_hash, node_id, &entry.storage_class)
        .await
    {
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
///
/// [`STAGING`] sits at the same depth as a prefix and is handled by its own
/// rule below: what keeps a staging directory alive is the *claim* on its
/// recipe, not a published location, because a producer part-way through a
/// two-hour film has the former and cannot have the latter.
async fn sweep_orphan_dirs(
    store: &Arc<dyn Store>,
    root: &Path,
    node_id: &str,
    readers: &ActiveCacheReaders,
    removed_claims: &HashSet<String>,
) -> (usize, usize, usize) {
    // The common empty-install case should not ask the store for ownership or
    // emit an uncertainty warning. A missing root cannot contain bytes.
    if tokio::fs::metadata(root).await.is_err() {
        return (0, 0, 0);
    }
    // Everything a row names, as absolute paths. Both complete and claimed: a
    // producer's publish destination is claimed and must not be swept out from
    // under it.
    let mut known: HashSet<PathBuf> = HashSet::new();
    let mut claimed_recipes: HashSet<String> = HashSet::new();
    // Filesystem ownership is not eviction policy. The filtered LRU and stale
    // queries intentionally hide offline-owned rows; using either as this
    // keep-list turns that protection inside out and deletes the protected
    // directories as "orphans".
    let rows = match store.all_cache_rows(node_id).await {
        Ok(rows) => rows,
        Err(error) => {
            // Ownership uncertainty must fail closed. Treating a store error
            // as an empty keep-list would recursively delete valid media.
            tracing::warn!(%error, "cache: could not enumerate filesystem owners; skipping orphan sweep");
            return (0, 0, 0);
        }
    };
    let local_rows: Vec<_> = rows
        .iter()
        .filter(|entry| entry.storage_class == "local")
        .collect();
    let inventory_complete = !local_rows.is_empty();
    if !inventory_complete {
        // An empty inventory is indistinguishable from a backend that returned
        // an incomplete ownership view. Deleting the whole cache tree on that
        // answer is not cleanup; it is data loss. Fail closed and retry after
        // a later pass has a positive ownership fact.
        tracing::warn!("cache: filesystem owner inventory is empty; skipping orphan sweep");
    }
    for e in local_rows {
        if let Some(dir) = entry_dir(root, &e.relative_dir) {
            known.insert(dir);
        }
        if !e.complete {
            claimed_recipes.insert(e.recipe_hash.clone());
        }
    }

    let Ok(mut prefixes) = tokio::fs::read_dir(root).await else {
        return (0, 0, 0); // no cache root yet just means nothing has been produced
    };
    let mut removed = 0usize;
    let mut protected = 0usize;
    let mut in_flight = 0usize;
    let mut staging: Option<PathBuf> = None;
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
        if prefix.file_name() == STAGING {
            staging = Some(ppath);
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
            let recipe = entry.file_name().to_string_lossy().into_owned();
            if !inventory_complete
                && !readers.knows_recipe(&recipe)
                && !removed_claims.contains(&recipe)
            {
                kept += 1;
                continue;
            }
            let _eviction = match readers.begin_eviction(&recipe) {
                Ok(guard) => guard,
                Err(CacheBusy::Readers) => {
                    protected += 1;
                    kept += 1;
                    continue;
                }
                Err(CacheBusy::Evicting) => {
                    in_flight += 1;
                    kept += 1;
                    continue;
                }
            };
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

    // The staging area, by its own rule: a directory here is a producer's
    // work-in-progress, and what says it is still wanted is a claim on its
    // recipe. One with no claim is what a killed process leaves — and, since
    // the claim is what a later pass resumes from, one without a claim is also
    // work nothing will ever pick up again.
    if let Some(staging) = staging {
        if let Ok(mut entries) = tokio::fs::read_dir(&staging).await {
            let mut kept = 0usize;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if claimed_recipes.contains(&name) {
                    kept += 1;
                    continue;
                }
                if !inventory_complete
                    && !readers.knows_recipe(&name)
                    && !removed_claims.contains(&name)
                {
                    kept += 1;
                    continue;
                }
                let _eviction = match readers.begin_eviction(&name) {
                    Ok(guard) => guard,
                    Err(CacheBusy::Readers) => {
                        protected += 1;
                        kept += 1;
                        continue;
                    }
                    Err(CacheBusy::Evicting) => {
                        in_flight += 1;
                        kept += 1;
                        continue;
                    }
                };
                let path = entry.path();
                match tokio::fs::remove_dir_all(&path).await {
                    Ok(()) => {
                        removed += 1;
                        tracing::info!(
                            dir = %path.display(),
                            "cache: removed staging for a recipe nothing claims"
                        );
                    }
                    Err(e) => tracing::warn!(
                        dir = %path.display(), error = %e, "cache: staging sweep failed"
                    ),
                }
            }
            if kept == 0 {
                let _ = tokio::fs::remove_dir(&staging).await;
            }
        }
    }
    (removed, protected, in_flight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::cluster::{open_store, StoreHandle};
    use plurx_core::config::Config;
    use plurx_core::domain::{
        ItemKind, LibraryKind, NewItem, NewLibrary, NewOfflinePackage, OfflineCreateOutcome,
        ProbeResult,
    };
    use plurx_core::store::{
        LibraryStore, MediaStore, OfflinePackageStore, SettingsStore, SqliteStore,
        TranscodeCacheStore, UserStore,
    };

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

    async fn preparing_package(store: &Arc<dyn Store>, file: i64, recipe: &str) -> String {
        let user = store
            .create_user(&format!("user-{recipe}"), "hash", false)
            .await
            .expect("user");
        let package = NewOfflinePackage {
            id: format!("package-{recipe}"),
            request_id: format!("request-{recipe}"),
            user_id: user.id,
            file_id: file,
            node_id: NODE.into(),
            source_path: "/m/Heat.mkv".into(),
            source_size: 1,
            source_mtime: 1,
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: None,
            audio_offset_ms: 0,
            subtitle_index: None,
            subtitle_language: None,
            subtitle_mode: "none".into(),
            estimated_bytes: 10,
            reserved_bytes: 20,
            expires_at: i64::MAX,
        };
        assert!(matches!(
            store
                .create_offline_package(&package, 10, 1_000, 2_000)
                .await
                .expect("package"),
            OfflineCreateOutcome::Created(_)
        ));
        let claimed = store
            .claim_next_offline_package(NODE)
            .await
            .expect("claim package")
            .expect("queued package");
        assert_eq!(claimed.id, package.id);
        assert!(store
            .set_offline_package_recipe(&package.id, recipe)
            .await
            .expect("bind recipe"));
        package.id
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

    #[tokio::test]
    async fn a_published_offline_package_is_neither_evicted_nor_orphaned() {
        let (store, file) = store().await;
        let root = root();
        let package_id = preparing_package(&store, file, "efready").await;
        let dir = entry(&store, root.path(), file, "efready", 100).await;
        assert!(store
            .mark_offline_package_ready(&package_id, "efready", 100, 90_000)
            .await
            .expect("ready"));
        store
            .put_setting(keys::CACHE_MAX_GB, "0")
            .await
            .expect("zero budget");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!((out.evicted, out.orphans), (0, 0));
        assert!(dir.join("index.m3u8").exists());
        assert!(store
            .cache_hit("efready", NODE)
            .await
            .expect("hit")
            .is_some());
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

    /// A cache row can disappear while its bytes are still being served: scan
    /// reconciliation cascades a vanished source file through the recipe and
    /// location tables. The orphan pass must honor the same reader ownership
    /// as LRU eviction, then collect the bytes after the last viewer leaves.
    #[tokio::test]
    async fn an_active_reader_survives_row_loss_and_the_orphan_pass() {
        let (store, file) = store().await;
        let root = root();
        let dir = entry(&store, root.path(), file, "aawatching", 100).await;
        let readers = ActiveCacheReaders::default();
        let reader = readers.begin_read("aawatching").expect("reader claim");
        store
            .forget_cache_entry("aawatching", NODE, "local")
            .await
            .expect("remove row out from under reader");

        let active = sweep_with_readers(&store, root.path(), NODE, &readers, unix_now()).await;
        assert_eq!((active.orphans, active.protected), (0, 1));
        assert!(
            dir.join("index.m3u8").exists(),
            "the orphan pass deleted bytes held by an active session"
        );

        drop(reader);
        let idle = sweep_with_readers(&store, root.path(), NODE, &readers, unix_now()).await;
        assert_eq!((idle.orphans, idle.protected), (1, 0));
        assert!(
            !dir.exists(),
            "unowned bytes survived after the reader left"
        );
    }

    #[test]
    fn every_reader_must_leave_before_eviction_can_claim_a_recipe() {
        let readers = ActiveCacheReaders::default();
        let first = readers.begin_read("recipe").expect("first reader");
        let second = readers.begin_read("recipe").expect("second reader");
        assert_eq!(readers.active_entries(), 1);

        drop(first);
        assert!(matches!(
            readers.begin_eviction("recipe"),
            Err(CacheBusy::Readers)
        ));
        assert_eq!(readers.active_entries(), 1);

        drop(second);
        assert_eq!(readers.active_entries(), 0);
        assert!(readers.begin_eviction("recipe").is_ok());
    }

    #[test]
    fn a_lookup_during_eviction_is_an_ordinary_cache_miss() {
        let readers = ActiveCacheReaders::default();
        let eviction = readers.begin_eviction("recipe").expect("eviction claim");
        assert!(
            readers.begin_read("recipe").is_none(),
            "a lookup entered while deletion owned the recipe"
        );
        drop(eviction);
        assert!(readers.begin_read("recipe").is_some());
    }

    /// The staging area is not a fanout prefix, and the difference is hours of
    /// somebody's GPU.
    ///
    /// A producer part-way through a two-hour film has a claim and no published
    /// location — so judging its half-built asset by the rule that governs
    /// published ones deletes it, mid-encode, from the housekeeping job running
    /// beside it. The producer then finds its own files gone and fails, and the
    /// log blames ffmpeg.
    #[tokio::test]
    async fn the_sweep_does_not_delete_what_a_producer_is_building() {
        let (store, file) = store().await;
        let root = root();
        // A producer at work: a claim on the final path, bytes in staging.
        let staging = staging_dir(root.path(), "aaworking");
        tokio::fs::create_dir_all(staging.join("part-000"))
            .await
            .expect("mkdir");
        tokio::fs::write(staging.join("part-000/seg00000.ts"), b"half a film")
            .await
            .expect("write");
        store
            .claim_cache_entry("aaworking", file, 1, NODE, "aa/aaworking")
            .await
            .expect("claim");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.orphans, 0, "the sweep took a producer's work");
        assert!(
            staging.join("part-000/seg00000.ts").exists(),
            "an encode in progress was deleted by the housekeeping job beside it"
        );
    }

    #[tokio::test]
    async fn offline_preparation_keeps_its_staging_directory() {
        let (store, file) = store().await;
        let root = root();
        let _package_id = preparing_package(&store, file, "ghoffline").await;
        store
            .claim_cache_entry("ghoffline", file, 1, NODE, "gh/ghoffline")
            .await
            .expect("cache claim");
        let staging = staging_dir(root.path(), "ghoffline");
        tokio::fs::create_dir_all(staging.join("part-000"))
            .await
            .expect("mkdir");
        tokio::fs::write(staging.join("part-000/seg00000.ts"), b"half a film")
            .await
            .expect("write");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!((out.stale, out.orphans), (0, 0));
        assert!(staging.join("part-000/seg00000.ts").exists());
    }

    /// …but staging that nothing claims is a producer that died. Nothing will
    /// pick it up — a later pass resumes from the *claim* — so it is bytes
    /// costing disk for no possible benefit.
    #[tokio::test]
    async fn staging_with_no_claim_behind_it_is_reclaimed() {
        let (store, file) = store().await;
        let root = root();
        // One positive local owner makes this a complete inventory rather than
        // the ambiguous empty-Ok case pinned separately below.
        entry(&store, root.path(), file, "aakeep", 1).await;
        let abandoned = staging_dir(root.path(), "bbabandoned");
        tokio::fs::create_dir_all(&abandoned).await.expect("mkdir");
        tokio::fs::write(abandoned.join("seg00000.ts"), b"junk")
            .await
            .expect("write");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.orphans, 1);
        assert!(!abandoned.exists());
        assert!(
            !root.path().join(STAGING).exists(),
            "an emptied staging area goes with its last directory"
        );
    }

    /// A crashed producer is cleaned up in one pass, not two: the claim ages
    /// out, and the staging walk — which runs afterwards and re-reads the
    /// claims — no longer finds anything keeping its bytes.
    ///
    /// The ordering inside `sweep` is what makes that true, and it is the only
    /// reason these are separate steps rather than one.
    #[tokio::test]
    async fn a_crashed_producers_claim_and_bytes_go_together() {
        let (store, file) = store().await;
        let root = root();
        let now = unix_now();
        let staging = staging_dir(root.path(), "ccdead");
        tokio::fs::create_dir_all(&staging).await.expect("mkdir");
        tokio::fs::write(staging.join("seg00000.ts"), b"orphan")
            .await
            .expect("write");
        store
            .claim_cache_entry("ccdead", file, 1, NODE, "cc/ccdead")
            .await
            .expect("claim");

        let out = sweep(&store, root.path(), NODE, now + STALE_CLAIM_SECS + 60).await;
        assert_eq!((out.stale, out.orphans), (1, 1));
        assert!(
            store
                .stale_cache_claims(NODE, i64::MAX)
                .await
                .expect("claims")
                .is_empty(),
            "the stale claim survived"
        );
        assert!(
            !staging.exists(),
            "the bytes outlived every reference to them"
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

    /// Two sweeps use the same cold snapshot. Once one owns an entry, the
    /// other must not walk farther and satisfy the same deficit against a
    /// disjoint set — doing so lets two correct-looking passes empty a cache.
    #[tokio::test]
    async fn a_peer_eviction_ends_the_budget_pass_before_it_over_evicts() {
        let (store, file) = store().await;
        let root = root();
        store
            .put_setting(keys::CACHE_MAX_GB, "1")
            .await
            .expect("budget");
        let size = (0.4 * GB as f64) as i64;
        for hash in ["aafirst", "bbsecond", "ccthird"] {
            entry(&store, root.path(), file, hash, size).await;
        }

        let peer_entry = store
            .cache_by_age(NODE, 10)
            .await
            .expect("cold inventory")
            .into_iter()
            .next()
            .expect("first cold entry");
        let readers = ActiveCacheReaders::default();
        let peer = readers
            .begin_eviction(&peer_entry.recipe_hash)
            .expect("peer sweep owns the first eviction");

        let out = sweep_with_readers(&store, root.path(), NODE, &readers, unix_now()).await;
        assert_eq!(out.evicted, 0, "the losing pass evicted a second entry");
        assert_eq!(out.in_flight, 1);
        assert_eq!(
            store.cache_bytes(NODE).await.expect("bytes before peer"),
            size * 3,
            "the losing pass changed the peer's inventory"
        );

        drop(peer);
        assert!(forget(&store, root.path(), NODE, &peer_entry).await);
        assert_eq!(
            store.cache_bytes(NODE).await.expect("bytes after peer"),
            size * 2,
            "one peer eviction should be exactly enough to meet the ceiling"
        );
    }

    /// `Ok([])` is not proof that nobody owns a filesystem tree. A backend
    /// returning an incomplete empty inventory must not turn one orphan pass
    /// into a recursive deletion of every cached byte.
    #[tokio::test]
    async fn an_empty_ownership_inventory_fails_closed() {
        let (store, _file) = store().await;
        let root = root();
        let orphan = root.path().join("aa/aaunverified");
        tokio::fs::create_dir_all(&orphan).await.expect("orphan");
        tokio::fs::write(orphan.join("seg00000.ts"), b"unverified bytes")
            .await
            .expect("bytes");

        let out = sweep(&store, root.path(), NODE, unix_now()).await;
        assert_eq!(out.orphans, 0);
        assert!(
            orphan.exists(),
            "an empty ownership answer deleted the whole cache inventory"
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

    /// M0's compatibility fixture. Before clustering, both cache locations and
    /// offline packages were owned by `instance.id`. Initializing `node.id`
    /// must seed that exact value before either cleanup path runs, or the cache
    /// directory becomes an orphan and the offline worker loses its queue.
    #[tokio::test]
    async fn node_identity_initialization_preserves_populated_v14_ownership_and_bytes() {
        let data = tempfile::tempdir().expect("data dir");
        let mut config = Config::default();
        config.storage.data_dir = data.path().to_owned();
        let cache_root = data.path().join("cache/transcode");
        let entry_dir = cache_root.join("aa/aakeep");
        let ready_id = "offline-ready";
        let interrupted_id = "offline-interrupted";
        let (cluster_id, user_id, file_id) = {
            // Populate the v14 store before any M0 identity code has run.
            let store = SqliteStore::open(&data.path().join("plurx.db")).expect("v14 store");
            let cluster_id = store.instance_id().await.expect("instance id");

            let library = store
                .create_library(&NewLibrary {
                    name: "Movies".into(),
                    kind: LibraryKind::Movies,
                    paths: vec![],
                    anime: false,
                })
                .await
                .expect("library");
            let movie = store
                .insert_item(&NewItem {
                    library_id: library.id,
                    kind: ItemKind::Movie,
                    parent_id: None,
                    title: "Heat".into(),
                    year: Some(1995),
                    season_number: None,
                    episode_number: None,
                })
                .await
                .expect("movie");
            let file_id = store
                .upsert_file(movie, "/m/Heat.mkv", 1, 1, &ProbeResult::default())
                .await
                .expect("file");
            let user = store
                .create_user("traveller", "hash", false)
                .await
                .expect("user");

            tokio::fs::create_dir_all(&entry_dir)
                .await
                .expect("cache entry dir");
            tokio::fs::write(entry_dir.join("index.m3u8"), b"durable cached bytes")
                .await
                .expect("cache bytes");
            store
                .claim_cache_entry("aakeep", file_id, 1, &cluster_id, "aa/aakeep")
                .await
                .expect("cache claim");
            store
                .complete_cache_entry("aakeep", &cluster_id, 20)
                .await
                .expect("complete cache");

            let package = |id: &str, request: &str| NewOfflinePackage {
                id: id.into(),
                request_id: request.into(),
                user_id: user.id,
                file_id,
                node_id: cluster_id.clone(),
                source_path: "/m/Heat.mkv".into(),
                source_size: 1,
                source_mtime: 1,
                target_height: 720,
                output_width: Some(1280),
                output_height: Some(720),
                audio_index: None,
                audio_offset_ms: 0,
                subtitle_index: None,
                subtitle_language: None,
                subtitle_mode: "none".into(),
                estimated_bytes: 10,
                reserved_bytes: 20,
                expires_at: i64::MAX,
            };

            assert!(matches!(
                store
                    .create_offline_package(&package(ready_id, "request-ready"), 10, 1_000, 2_000)
                    .await
                    .expect("ready package"),
                OfflineCreateOutcome::Created(_)
            ));
            assert_eq!(
                store
                    .claim_next_offline_package(&cluster_id)
                    .await
                    .expect("claim ready")
                    .expect("ready package exists")
                    .id,
                ready_id
            );
            store
                .set_offline_package_recipe(ready_id, "aakeep")
                .await
                .expect("bind recipe");
            assert!(store
                .mark_offline_package_ready(ready_id, "aakeep", 20, 90_000)
                .await
                .expect("publish ready package"));

            assert!(matches!(
                store
                    .create_offline_package(
                        &package(interrupted_id, "request-interrupted"),
                        10,
                        1_000,
                        2_000,
                    )
                    .await
                    .expect("interrupted package"),
                OfflineCreateOutcome::Created(_)
            ));
            assert_eq!(
                store
                    .claim_next_offline_package(&cluster_id)
                    .await
                    .expect("claim interrupted")
                    .expect("interrupted package exists")
                    .id,
                interrupted_id
            );
            (cluster_id, user.id, file_id)
        };

        assert!(
            !data.path().join("node.id").exists(),
            "the pre-M0 fixture must not initialize M0 identity"
        );

        // Upgrade through the real M0 path, then run both startup cleanup
        // behaviors against the newly initialized node-local id.
        let StoreHandle { store, identity } = open_store(&config).await.expect("M0 open");
        assert_eq!(identity.cluster_id, cluster_id);
        assert_eq!(identity.node_id, cluster_id);
        assert_eq!(
            std::fs::read_to_string(data.path().join("node.id")).expect("node id file"),
            format!("{cluster_id}\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(data.path().join("node.id"))
                    .expect("node id metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        // Arm orphan collection with a row created for the current identity.
        // If the upgrade picked a different id, the pre-M0 entry is now a
        // visible orphan and this same sweep deletes it.
        let current_entry = cache_root.join("bb/bbcurrent");
        tokio::fs::create_dir_all(&current_entry)
            .await
            .expect("current cache entry dir");
        tokio::fs::write(current_entry.join("index.m3u8"), b"current cached bytes")
            .await
            .expect("current cache bytes");
        store
            .claim_cache_entry("bbcurrent", file_id, 1, &identity.node_id, "bb/bbcurrent")
            .await
            .expect("current cache claim");
        store
            .complete_cache_entry("bbcurrent", &identity.node_id, 20)
            .await
            .expect("complete current cache");
        assert_eq!(
            store
                .reset_interrupted_offline_packages(&identity.node_id)
                .await
                .expect("recover interrupted package"),
            1
        );

        let swept = sweep_with_readers(
            &store,
            &cache_root,
            &identity.node_id,
            &ActiveCacheReaders::default(),
            unix_now(),
        )
        .await;
        assert_eq!((swept.stale, swept.evicted, swept.orphans), (0, 0, 0));
        assert!(
            entry_dir.join("index.m3u8").exists(),
            "identity initialization let cache cleanup delete owned bytes"
        );
        assert!(store
            .cache_hit("aakeep", &identity.node_id)
            .await
            .expect("cache lookup")
            .is_some());
        assert_eq!(
            store
                .offline_package_for_user(ready_id, user_id)
                .await
                .expect("offline lookup")
                .expect("ready package retained")
                .state,
            "ready"
        );
        assert_eq!(
            store
                .claim_next_offline_package(&identity.node_id)
                .await
                .expect("reclaimed queue")
                .expect("interrupted package was requeued")
                .id,
            interrupted_id
        );
    }

    #[tokio::test]
    async fn a_distinct_node_id_cannot_strand_existing_local_ownership() {
        let data = tempfile::tempdir().expect("data dir");
        let mut config = Config::default();
        config.storage.data_dir = data.path().to_owned();
        let store = SqliteStore::open(&data.path().join("plurx.db")).expect("store");
        let cluster_id = store.instance_id().await.expect("instance id");
        let library = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("library");
        let movie = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let file_id = store
            .upsert_file(movie, "/m/Heat.mkv", 1, 1, &ProbeResult::default())
            .await
            .expect("file");
        store
            .claim_cache_entry("aakeep", file_id, 1, &cluster_id, "aa/aakeep")
            .await
            .expect("legacy cache claim");
        drop(store);

        let distinct = uuid::Uuid::new_v4().to_string();
        std::fs::write(data.path().join("node.id"), format!("{distinct}\n"))
            .expect("distinct node id");
        let error = match open_store(&config).await {
            Ok(_) => panic!("a distinct node id must not strand legacy ownership"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("refusing to strand owned bytes"));
    }
}

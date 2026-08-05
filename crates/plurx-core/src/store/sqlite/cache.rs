//! The pre-transcode cache's storage: which transcodes exist, and where.
//!
//! Two tables behind one trait. The split is for the cluster (PERF-PLAN §6.1) —
//! "node A has this copy, node B had one and evicted it" needs a row per copy —
//! and every query here joins them straight back, because no caller has ever
//! wanted the halves separately.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::SqliteStore;
use crate::domain::CachedTranscode;
use crate::error::StoreError;
use crate::store::TranscodeCacheStore;

/// The columns a [`CachedTranscode`] is built from, in order. One list so a
/// query and its row-reader cannot drift.
const CACHE_COLS: &str = "l.recipe_hash, r.file_id, l.relative_dir, l.bytes, \
                          l.complete, l.last_used_at";

fn cache_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedTranscode> {
    Ok(CachedTranscode {
        recipe_hash: row.get(0)?,
        file_id: row.get(1)?,
        relative_dir: row.get(2)?,
        bytes: row.get(3)?,
        complete: row.get::<_, i64>(4)? != 0,
        last_used_at: row.get(5)?,
    })
}

#[async_trait]
impl TranscodeCacheStore for SqliteStore {
    async fn cache_hit(
        &self,
        recipe_hash: &str,
        node_id: &str,
    ) -> Result<Option<CachedTranscode>, StoreError> {
        let (hash, node) = (recipe_hash.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            // `complete = 1` is the whole safety of this query. A claimed row
            // points at a directory a producer is still writing into, and
            // serving that is a playlist that ends in the middle of the film.
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {CACHE_COLS}
                         FROM transcode_cache_locations l
                         JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash
                         WHERE l.recipe_hash = ?1 AND l.node_id = ?2 AND l.complete = 1"
                    ),
                    params![hash, node],
                    cache_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn claim_cache_entry(
        &self,
        recipe_hash: &str,
        file_id: i64,
        recipe_version: i64,
        node_id: &str,
        relative_dir: &str,
    ) -> Result<bool, StoreError> {
        let (hash, node, dir) = (
            recipe_hash.to_owned(),
            node_id.to_owned(),
            relative_dir.to_owned(),
        );
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO transcode_cache_recipes (recipe_hash, file_id, recipe_version)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(recipe_hash) DO NOTHING",
                params![hash, file_id, recipe_version],
            )?;
            // DO NOTHING, not DO UPDATE: a claim that already exists belongs to
            // a producer that may be mid-write, and overwriting its directory
            // would leave that producer filling a path nobody will look in.
            // The second producer's job is to notice and stand down — which is
            // what the row count reports. Zero rows means somebody else owns
            // this recipe, and that is the whole signal.
            let taken = tx.execute(
                "INSERT INTO transcode_cache_locations
                     (recipe_hash, node_id, storage_class, relative_dir, complete)
                 VALUES (?1, ?2, 'local', ?3, 0)
                 ON CONFLICT(recipe_hash, node_id, storage_class) DO NOTHING",
                params![hash, node, dir],
            )?;
            tx.commit()?;
            Ok(taken > 0)
        })
        .await
    }

    async fn touch_cache_claim(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError> {
        let (hash, node) = (recipe_hash.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            // Only while incomplete: a finished entry's `last_seen_at` is the
            // moment it was published, and there is nothing left to be seen
            // working on it.
            conn.execute(
                "UPDATE transcode_cache_locations SET last_seen_at = unixepoch()
                 WHERE recipe_hash = ?1 AND node_id = ?2 AND complete = 0",
                params![hash, node],
            )?;
            Ok(())
        })
        .await
    }

    async fn complete_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        bytes: i64,
    ) -> Result<(), StoreError> {
        let (hash, node) = (recipe_hash.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE transcode_cache_locations
                 SET complete = 1, bytes = ?3,
                     last_used_at = unixepoch(), last_seen_at = unixepoch()
                 WHERE recipe_hash = ?1 AND node_id = ?2 AND storage_class = 'local'",
                params![hash, node, bytes],
            )?;
            Ok(())
        })
        .await
    }

    async fn touch_cache_entry(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError> {
        let (hash, node) = (recipe_hash.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE transcode_cache_locations SET last_used_at = unixepoch()
                 WHERE recipe_hash = ?1 AND node_id = ?2",
                params![hash, node],
            )?;
            Ok(())
        })
        .await
    }

    async fn cache_by_age(
        &self,
        node_id: &str,
        limit: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CACHE_COLS}
                 FROM transcode_cache_locations l
                 JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash
                 WHERE l.node_id = ?1 AND l.complete = 1
                   AND NOT EXISTS (
                       SELECT 1 FROM offline_packages p
                       WHERE p.recipe_hash = l.recipe_hash
                         AND p.node_id = l.node_id
                         AND p.state IN ('preparing', 'ready')
                   )
                 -- The rowid tiebreak makes eviction deterministic. Without it
                 -- a batch of entries finished in the same second sorts
                 -- arbitrarily, so which one an over-budget sweep deletes
                 -- depends on the query plan — reproducible right up until it
                 -- isn't. Oldest row first is the honest tiebreak: among
                 -- equally cold entries, the one that has been here longest.
                 ORDER BY l.last_used_at ASC, l.rowid ASC
                 LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![node, limit], cache_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn stale_cache_claims(
        &self,
        node_id: &str,
        older_than_unix: i64,
    ) -> Result<Vec<CachedTranscode>, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            // Incomplete and old. A live producer's claim is incomplete too,
            // which is why the age bound matters: it is the difference between
            // cleaning up after a crash and deleting the work in progress.
            let mut stmt = conn.prepare(&format!(
                "SELECT {CACHE_COLS}
                 FROM transcode_cache_locations l
                 JOIN transcode_cache_recipes r ON r.recipe_hash = l.recipe_hash
                 WHERE l.node_id = ?1 AND l.complete = 0 AND l.last_seen_at < ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM offline_packages p
                       WHERE p.file_id = r.file_id
                         AND p.node_id = l.node_id
                         AND p.state IN ('queued', 'preparing')
                   )"
            ))?;
            let rows = stmt
                .query_map(params![node, older_than_unix], cache_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn forget_cache_entry(&self, recipe_hash: &str, node_id: &str) -> Result<(), StoreError> {
        let (hash, node) = (recipe_hash.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM transcode_cache_locations
                 WHERE recipe_hash = ?1 AND node_id = ?2",
                params![hash, node],
            )?;
            // A recipe whose last copy is gone is not a fact worth keeping. On
            // a cluster the other nodes' rows hold it alive, which is exactly
            // the behaviour the two-table split exists to give.
            tx.execute(
                "DELETE FROM transcode_cache_recipes
                 WHERE recipe_hash = ?1
                   AND NOT EXISTS (SELECT 1 FROM transcode_cache_locations
                                   WHERE recipe_hash = ?1)",
                params![hash],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn cache_bytes(&self, node_id: &str) -> Result<i64, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(l.bytes), 0)
                 FROM transcode_cache_locations l
                 WHERE l.node_id = ?1 AND l.complete = 1
                   AND NOT EXISTS (
                       SELECT 1 FROM offline_packages p
                       WHERE p.recipe_hash = l.recipe_hash
                         AND p.node_id = l.node_id
                         AND p.state IN ('preparing', 'ready')
                   )",
                params![node],
                |row| row.get(0),
            )?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        ItemKind, LibraryKind, NewItem, NewLibrary, NewOfflinePackage, OfflineCreateOutcome,
        ProbeResult,
    };
    use crate::store::{
        LibraryStore, MediaStore, OfflinePackageStore, SqliteStore, TranscodeCacheStore, UserStore,
    };

    const NODE: &str = "node-a";

    async fn seed_file(store: &SqliteStore) -> i64 {
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
        store
            .upsert_file(movie, "/m/Heat.mkv", 1, 1, &ProbeResult::default())
            .await
            .expect("file")
    }

    /// A claim is not a hit. The directory a producer is still writing into
    /// holds a playlist that stops in the middle of the film, and serving it
    /// is worse than never having cached anything.
    #[tokio::test]
    async fn an_entry_is_invisible_until_it_is_finished() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;

        store
            .claim_cache_entry("abc123", file, 1, NODE, "ab/abc123")
            .await
            .expect("claim");
        assert!(store
            .cache_hit("abc123", NODE)
            .await
            .expect("hit")
            .is_none());
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 0);

        store
            .complete_cache_entry("abc123", NODE, 4_000_000)
            .await
            .expect("complete");
        let hit = store.cache_hit("abc123", NODE).await.expect("hit");
        let hit = hit.expect("a finished entry is serveable");
        assert_eq!(hit.relative_dir, "ab/abc123");
        assert_eq!(hit.bytes, 4_000_000);
        assert!(hit.complete);
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 4_000_000);

        // Another node's copy is another node's business.
        assert!(store
            .cache_hit("abc123", "node-b")
            .await
            .expect("hit")
            .is_none());
    }

    /// Two producers racing for the same recipe: the first claim stands. The
    /// alternative — last writer wins — leaves the first one filling a
    /// directory nothing will ever look in, and it would still be doing it
    /// when the second finished.
    #[tokio::test]
    async fn a_second_claim_does_not_move_the_first_ones_directory() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        assert!(
            store
                .claim_cache_entry("abc", file, 1, NODE, "first/dir")
                .await
                .expect("claim"),
            "an unclaimed recipe is taken"
        );
        assert!(
            !store
                .claim_cache_entry("abc", file, 1, NODE, "second/dir")
                .await
                .expect("claim again"),
            "the second producer has to be told it lost, or it publishes over the first"
        );
        store
            .complete_cache_entry("abc", NODE, 10)
            .await
            .expect("complete");
        let hit = store
            .cache_hit("abc", NODE)
            .await
            .expect("hit")
            .expect("a completed entry");
        assert_eq!(hit.relative_dir, "first/dir");
    }

    /// Eviction order, and what eviction leaves behind. A recipe whose last
    /// copy is gone goes with it; on a cluster the other nodes' rows are what
    /// keep it alive, which is the point of the split.
    #[tokio::test]
    async fn eviction_walks_coldest_first_and_takes_the_recipe_with_the_last_copy() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        for (hash, used) in [("cold", 1_000), ("warm", 5_000), ("hot", 9_000)] {
            store
                .claim_cache_entry(hash, file, 1, NODE, hash)
                .await
                .expect("claim");
            store
                .complete_cache_entry(hash, NODE, 100)
                .await
                .expect("complete");
            store
                .with_conn({
                    let hash = hash.to_owned();
                    move |conn| {
                        conn.execute(
                            "UPDATE transcode_cache_locations SET last_used_at = ?2
                             WHERE recipe_hash = ?1",
                            rusqlite::params![hash, used],
                        )?;
                        Ok(())
                    }
                })
                .await
                .expect("age it");
        }

        let order: Vec<String> = store
            .cache_by_age(NODE, 10)
            .await
            .expect("by age")
            .into_iter()
            .map(|c| c.recipe_hash)
            .collect();
        assert_eq!(order, vec!["cold", "warm", "hot"]);
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 300);

        // Watching the coldest one makes it the warmest — the LRU clock is the
        // only thing that keeps a rewatched title from being evicted forever.
        store.touch_cache_entry("cold", NODE).await.expect("touch");
        let first = store.cache_by_age(NODE, 1).await.expect("by age");
        assert_eq!(first[0].recipe_hash, "warm", "cold is no longer coldest");

        store
            .forget_cache_entry("cold", NODE)
            .await
            .expect("forget");
        assert!(store.cache_hit("cold", NODE).await.expect("hit").is_none());
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 200);
        let recipes: i64 = store
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM transcode_cache_recipes", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .expect("count");
        assert_eq!(recipes, 2, "the recipe went with its last copy");
    }

    /// A claim that never completed is a producer that died. Cleaning those up
    /// has to be bounded by age, or it deletes the work in progress — a live
    /// producer's claim looks exactly the same.
    #[tokio::test]
    async fn only_old_claims_count_as_crash_leftovers() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        store
            .claim_cache_entry("fresh", file, 1, NODE, "fresh")
            .await
            .expect("claim");

        let now: i64 = store
            .with_conn(|conn| Ok(conn.query_row("SELECT unixepoch()", [], |r| r.get(0))?))
            .await
            .expect("now");
        assert!(
            store
                .stale_cache_claims(NODE, now - 60)
                .await
                .expect("stale")
                .is_empty(),
            "a claim made a moment ago is a producer at work, not a crash"
        );
        let stale = store
            .stale_cache_claims(NODE, now + 60)
            .await
            .expect("stale");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].recipe_hash, "fresh");

        // …and a completed entry is never a leftover, however old.
        store
            .complete_cache_entry("fresh", NODE, 1)
            .await
            .expect("complete");
        assert!(store
            .stale_cache_claims(NODE, now + 60)
            .await
            .expect("stale")
            .is_empty());
    }

    #[tokio::test]
    async fn an_active_offline_request_protects_its_resumable_claim() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        let user = store.create_user("paul", "hash", true).await.expect("user");
        store
            .claim_cache_entry("flight", file, 1, NODE, "flight")
            .await
            .expect("claim");
        let package = NewOfflinePackage {
            id: "package".into(),
            request_id: "request".into(),
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
        assert!(store
            .stale_cache_claims(NODE, i64::MAX)
            .await
            .expect("stale")
            .is_empty());

        store
            .delete_offline_package("package", user.id)
            .await
            .expect("delete");
        assert_eq!(
            store
                .stale_cache_claims(NODE, i64::MAX)
                .await
                .expect("stale")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn ready_offline_bytes_are_not_charged_to_playback_cache() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        let user = store.create_user("paul", "hash", true).await.expect("user");
        store
            .claim_cache_entry("flight", file, 1, NODE, "flight")
            .await
            .expect("claim");
        store
            .complete_cache_entry("flight", NODE, 900)
            .await
            .expect("complete");
        let package = NewOfflinePackage {
            id: "package".into(),
            request_id: "request".into(),
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
            subtitle_mode: "none".into(),
            estimated_bytes: 800,
            reserved_bytes: 1_000,
            expires_at: i64::MAX,
        };
        store
            .create_offline_package(&package, 10, 2_000, 3_000)
            .await
            .expect("package");
        store
            .mark_offline_package_ready("package", "flight", 900, 1_000)
            .await
            .expect("ready");
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 0);
        assert!(store.cache_by_age(NODE, 10).await.expect("lru").is_empty());

        store
            .delete_offline_package("package", user.id)
            .await
            .expect("delete");
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 900);
        assert_eq!(store.cache_by_age(NODE, 10).await.expect("lru").len(), 1);
    }

    /// The source file going away takes its cache entries with it — the
    /// foreign key does the work, so nothing has to remember to.
    #[tokio::test]
    async fn deleting_a_file_deletes_what_was_cached_from_it() {
        let store = SqliteStore::open_in_memory().expect("open");
        let file = seed_file(&store).await;
        store
            .claim_cache_entry("gone", file, 1, NODE, "gone")
            .await
            .expect("claim");
        store
            .complete_cache_entry("gone", NODE, 1)
            .await
            .expect("complete");
        store.delete_files(&[file]).await.expect("delete");
        assert!(store.cache_hit("gone", NODE).await.expect("hit").is_none());
        assert_eq!(store.cache_bytes(NODE).await.expect("bytes"), 0);
    }
}

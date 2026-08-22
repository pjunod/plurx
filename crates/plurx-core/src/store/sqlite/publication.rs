//! SQLite lease-fenced job publications.

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::SqliteStore;
use crate::cluster::coordination::Lease;
use crate::domain::{
    sort_title_for, ArtworkAttempt, BookMetadataPatch, Item, ItemKind, MetadataPatch, NewItem,
    ProbeResult,
};
use crate::error::StoreError;
use crate::store::{
    ArtworkRepairFence, FencedPublicationStore, ReconcileOutcome, RootFingerprintStatus,
};

#[async_trait]
impl FencedPublicationStore for SqliteStore {
    async fn put_setting_fenced(
        &self,
        key: &str,
        value: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            put_setting(conn, &key, &value)
        })
        .await
    }

    async fn put_setting_if_absent_fenced(
        &self,
        key: &str,
        value: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            Ok(conn.execute(
                "INSERT INTO settings (key, value, updated_at)
                 VALUES (?1, ?2, unixepoch()) ON CONFLICT(key) DO NOTHING",
                params![key, value],
            )? == 1)
        })
        .await
    }

    async fn put_setting_if_absent_if_artwork_repair_current_fenced(
        &self,
        key: &str,
        value: &str,
        expected_item_id: i64,
        repair_fence: &ArtworkRepairFence,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        let key = key.to_owned();
        let value = value.to_owned();
        let repair_fence = repair_fence.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            Ok(conn.execute(
                "INSERT INTO settings (key, value, updated_at)
                 SELECT ?1, ?2, unixepoch() WHERE ?3 = ?4 AND EXISTS (
                   SELECT 1 FROM cluster_artwork_repairs
                   WHERE item_id = ?4 AND owner_node_id = ?5 AND leader_term = ?6
                     AND generation = ?7)
                 ON CONFLICT(key) DO NOTHING",
                params![
                    key,
                    value,
                    expected_item_id,
                    repair_fence.item_id,
                    repair_fence.owner_node_id,
                    repair_fence.leader_term,
                    repair_fence.generation,
                ],
            )? == 1)
        })
        .await
    }

    async fn mark_library_scanned_fenced(
        &self,
        id: i64,
        refreshed: bool,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "UPDATE libraries
                 SET last_scan_at = unixepoch(),
                     last_refresh_at = CASE WHEN ?2 THEN unixepoch() ELSE last_refresh_at END
                 WHERE id = ?1",
                params![id, refreshed],
            )?;
            Ok(())
        })
        .await
    }

    async fn insert_item_fenced(
        &self,
        item: &NewItem,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<i64, StoreError> {
        let item = item.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            let sort_title = if item.kind == ItemKind::Folder {
                item.title.to_lowercase()
            } else {
                sort_title_for(&item.title)
            };
            conn.execute(
                "INSERT INTO items
                   (library_id, kind, parent_id, title, sort_title, year,
                    season_number, episode_number)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    item.library_id,
                    item.kind.as_str(),
                    item.parent_id,
                    item.title,
                    sort_title,
                    item.year,
                    item.season_number,
                    item.episode_number,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    async fn apply_metadata_fenced(
        &self,
        item_id: i64,
        patch: &MetadataPatch,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let patch = patch.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            apply_metadata(conn, item_id, &patch)
        })
        .await
    }

    async fn apply_metadata_if_artwork_repair_current_fenced(
        &self,
        item_id: i64,
        patch: &MetadataPatch,
        repair_fence: &ArtworkRepairFence,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        let patch = patch.clone();
        let repair_fence = repair_fence.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            apply_metadata_if_artwork_current(conn, item_id, &patch, &repair_fence)
        })
        .await
    }

    async fn apply_book_metadata_fenced(
        &self,
        item_id: i64,
        patch: &BookMetadataPatch,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let patch = patch.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            apply_book_metadata(conn, item_id, &patch)
        })
        .await
    }

    async fn apply_book_metadata_if_current_fenced(
        &self,
        expected: &Item,
        patch: &BookMetadataPatch,
        repair_fence: Option<&ArtworkRepairFence>,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        let expected = expected.clone();
        let patch = patch.clone();
        let repair_fence = repair_fence.cloned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            apply_book_metadata_if_current(conn, &expected, &patch, repair_fence.as_ref())
        })
        .await
    }

    async fn set_nfo_seeded_fenced(
        &self,
        item_id: i64,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "UPDATE items SET nfo_seeded_at = unixepoch() WHERE id = ?1",
                params![item_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn upsert_file_fenced(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<i64, StoreError> {
        let path = path.to_owned();
        let probe = probe.clone();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            upsert_file(conn, item_id, &path, size, mtime, &probe)
        })
        .await
    }

    async fn ensure_library_root_fingerprint_fenced(
        &self,
        library_id: i64,
        fingerprint: &str,
        allow_establish: bool,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<RootFingerprintStatus, StoreError> {
        let fingerprint = fingerprint.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            ensure_library_root_fingerprint(conn, library_id, &fingerprint, allow_establish)
        })
        .await
    }

    async fn reconcile_library_fenced(
        &self,
        library_id: i64,
        root_fingerprint: &str,
        gone_file_ids: &[i64],
        prune_limit: u64,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<ReconcileOutcome, StoreError> {
        let root_fingerprint = root_fingerprint.to_owned();
        let ids = gone_file_ids.to_vec();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            reconcile_library(conn, library_id, &root_fingerprint, &ids, prune_limit)
        })
        .await
    }

    async fn claim_cache_entry_fenced(
        &self,
        recipe_hash: &str,
        file_id: i64,
        recipe_version: i64,
        node_id: &str,
        relative_dir: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<bool, StoreError> {
        let hash = recipe_hash.to_owned();
        let node = node_id.to_owned();
        let relative_dir = relative_dir.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "INSERT INTO transcode_cache_recipes (recipe_hash, file_id, recipe_version)
                 VALUES (?1, ?2, ?3) ON CONFLICT(recipe_hash) DO NOTHING",
                params![hash, file_id, recipe_version],
            )?;
            let claimed = conn.execute(
                "INSERT INTO transcode_cache_locations
                   (recipe_hash, node_id, storage_class, relative_dir, complete)
                 VALUES (?1, ?2, 'local', ?3, 0)
                 ON CONFLICT(recipe_hash, node_id, storage_class) DO UPDATE SET
                   relative_dir = excluded.relative_dir,
                   last_seen_at = unixepoch()
                 WHERE transcode_cache_locations.complete = 0",
                params![hash, node, relative_dir],
            )?;
            Ok(claimed > 0)
        })
        .await
    }

    async fn touch_cache_claim_fenced(
        &self,
        recipe_hash: &str,
        node_id: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let hash = recipe_hash.to_owned();
        let node = node_id.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "UPDATE transcode_cache_locations SET last_seen_at = unixepoch()
                 WHERE recipe_hash = ?1 AND node_id = ?2 AND complete = 0",
                params![hash, node],
            )?;
            Ok(())
        })
        .await
    }

    async fn complete_cache_entry_fenced(
        &self,
        recipe_hash: &str,
        node_id: &str,
        relative_dir: &str,
        bytes: i64,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let hash = recipe_hash.to_owned();
        let node = node_id.to_owned();
        let relative_dir = relative_dir.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "UPDATE transcode_cache_locations
                 SET relative_dir = ?3, complete = 1, bytes = ?4,
                     last_used_at = unixepoch(), last_seen_at = unixepoch()
                 WHERE recipe_hash = ?1 AND node_id = ?2 AND storage_class = 'local'",
                params![hash, node, relative_dir, bytes],
            )?;
            Ok(())
        })
        .await
    }

    async fn forget_cache_entry_fenced(
        &self,
        recipe_hash: &str,
        node_id: &str,
        storage_class: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let hash = recipe_hash.to_owned();
        let node = node_id.to_owned();
        let storage_class = storage_class.to_owned();
        self.with_fenced_conn(lease, observed_at_unix_ms, move |conn| {
            conn.execute(
                "DELETE FROM transcode_cache_locations
                 WHERE recipe_hash = ?1 AND node_id = ?2 AND storage_class = ?3",
                params![hash, node, storage_class],
            )?;
            conn.execute(
                "DELETE FROM transcode_cache_recipes WHERE recipe_hash = ?1
                 AND NOT EXISTS (SELECT 1 FROM transcode_cache_locations
                                 WHERE recipe_hash = ?1)",
                params![hash],
            )?;
            Ok(())
        })
        .await
    }
}

fn put_setting(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at)
         VALUES (?1, ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE
            SET value = excluded.value, updated_at = unixepoch()",
        params![key, value],
    )?;
    Ok(())
}

fn apply_metadata_if_artwork_current(
    conn: &Connection,
    item_id: i64,
    patch: &MetadataPatch,
    fence: &ArtworkRepairFence,
) -> Result<bool, StoreError> {
    let sort_title = patch.title.as_deref().map(sort_title_for);
    let tags = patch
        .tags
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Database(error.to_string()))?;
    let genres = patch
        .genres
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Database(error.to_string()))?;
    Ok(conn.execute(
        "UPDATE items SET
             title = COALESCE(?2, title), sort_title = COALESCE(?3, sort_title),
             year = COALESCE(?4, year), overview = COALESCE(?5, overview),
             tmdb_id = COALESCE(?6, tmdb_id), imdb_id = COALESCE(?7, imdb_id),
             air_date = COALESCE(?8, air_date), runtime_ms = COALESCE(?9, runtime_ms),
             poster_path = COALESCE(?10, poster_path),
             backdrop_path = COALESCE(?11, backdrop_path),
             recorded_at = COALESCE(?12, recorded_at), tags = COALESCE(?13, tags),
             metadata_at = CASE WHEN ?14 = 1 THEN unixepoch() ELSE metadata_at END,
             artwork_attempted_at =
                 CASE WHEN ?15 = 1 THEN unixepoch() ELSE artwork_attempted_at END,
             artwork_error = CASE WHEN ?15 = 1 THEN ?16 ELSE artwork_error END,
             genres = COALESCE(?17, genres), updated_at = unixepoch()
         WHERE id = ?1 AND ?18 = ?1 AND EXISTS (
           SELECT 1 FROM cluster_artwork_repairs
           WHERE item_id = ?18 AND owner_node_id = ?19 AND leader_term = ?20
             AND generation = ?21)",
        params![
            item_id,
            patch.title,
            sort_title,
            patch.year,
            patch.overview,
            patch.tmdb_id,
            patch.imdb_id,
            patch.air_date,
            patch.runtime_ms,
            patch.poster_path,
            patch.backdrop_path,
            patch.recorded_at,
            tags,
            patch.enriched as i64,
            patch.artwork.is_some() as i64,
            match &patch.artwork {
                Some(ArtworkAttempt::Failed(why)) => Some(why.as_str()),
                _ => None,
            },
            genres,
            fence.item_id,
            fence.owner_node_id,
            fence.leader_term,
            fence.generation,
        ],
    )? == 1)
}

fn apply_book_metadata_if_current(
    conn: &Connection,
    expected: &Item,
    patch: &BookMetadataPatch,
    repair_fence: Option<&ArtworkRepairFence>,
) -> Result<bool, StoreError> {
    let sort_title = patch.title.as_deref().map(sort_title_for);
    let source = patch.source.as_str();
    let base_sql = "UPDATE items SET
             title = COALESCE(?2, title), sort_title = COALESCE(?3, sort_title),
             author = COALESCE(?4, author), book_work_id = COALESCE(?5, book_work_id),
             book_edition_id = COALESCE(?6, book_edition_id),
             poster_path = COALESCE(?7, poster_path), book_metadata_source = ?8,
             updated_at = unixepoch()
         WHERE id = ?1 AND kind IN ('book', 'audiobook')
           AND title = ?9 AND author IS ?10 AND book_work_id IS ?11
           AND book_metadata_source IS ?12 AND book_edition_id IS ?13
           AND poster_path IS ?14";
    let changed = if let Some(fence) = repair_fence {
        conn.execute(
            &format!(
                "{base_sql} AND ?15 = ?1 AND EXISTS (
                   SELECT 1 FROM cluster_artwork_repairs
                   WHERE item_id = ?15 AND owner_node_id = ?16 AND leader_term = ?17
                     AND generation = ?18)"
            ),
            params![
                expected.id,
                patch.title,
                sort_title,
                patch.author,
                patch.work_id,
                patch.edition_id,
                patch.poster_path,
                source,
                expected.title,
                expected.author,
                expected.book_work_id,
                expected.book_metadata_source,
                expected.book_edition_id,
                expected.poster_path,
                fence.item_id,
                fence.owner_node_id,
                fence.leader_term,
                fence.generation,
            ],
        )?
    } else {
        conn.execute(
            base_sql,
            params![
                expected.id,
                patch.title,
                sort_title,
                patch.author,
                patch.work_id,
                patch.edition_id,
                patch.poster_path,
                source,
                expected.title,
                expected.author,
                expected.book_work_id,
                expected.book_metadata_source,
                expected.book_edition_id,
                expected.poster_path,
            ],
        )?
    };
    Ok(changed == 1)
}

fn apply_metadata(
    conn: &Connection,
    item_id: i64,
    patch: &MetadataPatch,
) -> Result<(), StoreError> {
    let sort_title = patch.title.as_deref().map(sort_title_for);
    let tags = patch
        .tags
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Database(error.to_string()))?;
    let genres = patch
        .genres
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| StoreError::Database(error.to_string()))?;
    conn.execute(
        "UPDATE items SET
             title = COALESCE(?2, title), sort_title = COALESCE(?3, sort_title),
             year = COALESCE(?4, year), overview = COALESCE(?5, overview),
             tmdb_id = COALESCE(?6, tmdb_id), imdb_id = COALESCE(?7, imdb_id),
             air_date = COALESCE(?8, air_date), runtime_ms = COALESCE(?9, runtime_ms),
             poster_path = COALESCE(?10, poster_path),
             backdrop_path = COALESCE(?11, backdrop_path),
             recorded_at = COALESCE(?12, recorded_at), tags = COALESCE(?13, tags),
             metadata_at = CASE WHEN ?14 = 1 THEN unixepoch() ELSE metadata_at END,
             artwork_attempted_at =
                 CASE WHEN ?15 = 1 THEN unixepoch() ELSE artwork_attempted_at END,
             artwork_error = CASE WHEN ?15 = 1 THEN ?16 ELSE artwork_error END,
             genres = COALESCE(?17, genres), updated_at = unixepoch()
         WHERE id = ?1",
        params![
            item_id,
            patch.title,
            sort_title,
            patch.year,
            patch.overview,
            patch.tmdb_id,
            patch.imdb_id,
            patch.air_date,
            patch.runtime_ms,
            patch.poster_path,
            patch.backdrop_path,
            patch.recorded_at,
            tags,
            patch.enriched as i64,
            patch.artwork.is_some() as i64,
            match &patch.artwork {
                Some(ArtworkAttempt::Failed(why)) => Some(why.as_str()),
                _ => None,
            },
            genres,
        ],
    )?;
    Ok(())
}

fn apply_book_metadata(
    conn: &Connection,
    item_id: i64,
    patch: &BookMetadataPatch,
) -> Result<(), StoreError> {
    let sort_title = patch.title.as_deref().map(sort_title_for);
    let source = patch.source.as_str();
    conn.execute(
        "UPDATE items SET
             title = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                      OR book_metadata_source = 'epub')
                               THEN COALESCE(?2, title) ELSE title END,
             sort_title = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                           OR book_metadata_source = 'epub')
                                    THEN COALESCE(?3, sort_title) ELSE sort_title END,
             author = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                       OR book_metadata_source = 'epub')
                                THEN COALESCE(?4, author) ELSE author END,
             book_work_id = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                             OR book_metadata_source = 'epub')
                                      THEN COALESCE(?5, book_work_id) ELSE book_work_id END,
             book_edition_id = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                                OR book_metadata_source = 'epub')
                                         THEN COALESCE(?6, book_edition_id)
                                         ELSE book_edition_id END,
             poster_path = CASE WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                            OR book_metadata_source = 'epub')
                                     THEN COALESCE(?7, poster_path) ELSE poster_path END,
             book_metadata_source = CASE
                 WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                       OR book_metadata_source = 'epub') THEN ?8
                 ELSE book_metadata_source END,
             updated_at = CASE
                 WHEN (?8 = 'curator' OR book_metadata_source IS NULL
                                       OR book_metadata_source = 'epub') THEN unixepoch()
                 ELSE updated_at END
         WHERE id = ?1 AND kind IN ('book', 'audiobook')",
        params![
            item_id,
            patch.title,
            sort_title,
            patch.author,
            patch.work_id,
            patch.edition_id,
            patch.poster_path,
            source,
        ],
    )?;
    Ok(())
}

fn upsert_file(
    conn: &Connection,
    item_id: i64,
    path: &str,
    size: i64,
    mtime: i64,
    probe: &ProbeResult,
) -> Result<i64, StoreError> {
    let audio = serde_json::to_string(&probe.audio_streams)
        .map_err(|error| StoreError::Database(error.to_string()))?;
    let subtitles = serde_json::to_string(&probe.subtitle_streams)
        .map_err(|error| StoreError::Database(error.to_string()))?;
    Ok(conn.query_row(
        "INSERT INTO files
           (item_id, path, size, mtime, duration_ms, container, video_codec,
            video_profile, width, height, bit_depth, hdr, bitrate,
            audio_streams, subtitle_streams, probe_json, hdr_format, scanned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, unixepoch())
         ON CONFLICT(path) DO UPDATE SET
             item_id = excluded.item_id, size = excluded.size, mtime = excluded.mtime,
             duration_ms = excluded.duration_ms, container = excluded.container,
             video_codec = excluded.video_codec, video_profile = excluded.video_profile,
             width = excluded.width, height = excluded.height, bit_depth = excluded.bit_depth,
             hdr = excluded.hdr, bitrate = excluded.bitrate,
             audio_streams = excluded.audio_streams,
             subtitle_streams = excluded.subtitle_streams,
             probe_json = excluded.probe_json, hdr_format = excluded.hdr_format,
             scanned_at = unixepoch()
         RETURNING id",
        params![
            item_id,
            path,
            size,
            mtime,
            probe.duration_ms,
            probe.container,
            probe.video_codec,
            probe.video_profile,
            probe.width,
            probe.height,
            probe.bit_depth,
            probe.hdr,
            probe.bitrate,
            audio,
            subtitles,
            probe.raw_json,
            probe.hdr_format,
        ],
        |row| row.get(0),
    )?)
}

fn ensure_library_root_fingerprint(
    conn: &Connection,
    library_id: i64,
    fingerprint: &str,
    allow_establish: bool,
) -> Result<RootFingerprintStatus, StoreError> {
    let inserted = conn.execute(
        "INSERT INTO library_roots (library_id, fingerprint) SELECT ?1, ?2 WHERE ?3
         ON CONFLICT(library_id) DO NOTHING",
        params![library_id, fingerprint, allow_establish],
    )?;
    if inserted == 1 {
        return Ok(RootFingerprintStatus::Established);
    }
    let expected: Option<String> = conn
        .query_row(
            "SELECT fingerprint FROM library_roots WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(expected) = expected else {
        return Ok(RootFingerprintStatus::Unestablished);
    };
    if expected == fingerprint {
        Ok(RootFingerprintStatus::Matched)
    } else {
        Ok(RootFingerprintStatus::Mismatch { expected })
    }
}

fn reconcile_library(
    conn: &Connection,
    library_id: i64,
    root_fingerprint: &str,
    ids: &[i64],
    prune_limit: u64,
) -> Result<ReconcileOutcome, StoreError> {
    let expected: Option<String> = conn
        .query_row(
            "SELECT fingerprint FROM library_roots WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .optional()?;
    if expected.as_deref() != Some(root_fingerprint) {
        return Ok(ReconcileOutcome::RefusedRoot {
            expected: expected.unwrap_or_else(|| "<unregistered>".to_owned()),
        });
    }
    let requested = ids.len() as u64;
    if requested > prune_limit {
        return Ok(ReconcileOutcome::RefusedPrune {
            requested,
            limit: prune_limit,
        });
    }

    let mut deleted_files = 0_u64;
    {
        let mut delete = conn.prepare_cached(
            "DELETE FROM files WHERE id = ?1 AND item_id IN
             (SELECT id FROM items WHERE library_id = ?2)",
        )?;
        for id in ids {
            deleted_files += delete.execute(params![id, library_id])? as u64;
        }
    }
    let mut pruned_items = 0_u64;
    pruned_items += conn.execute(
        "DELETE FROM items WHERE library_id = ?1
         AND kind IN ('movie','episode','video','photo','book','audiobook')
         AND id NOT IN (SELECT item_id FROM files)",
        params![library_id],
    )? as u64;
    pruned_items += conn.execute(
        "DELETE FROM items WHERE library_id = ?1 AND kind = 'season'
         AND id NOT IN (SELECT parent_id FROM items
                        WHERE kind = 'episode' AND parent_id IS NOT NULL)",
        params![library_id],
    )? as u64;
    pruned_items += conn.execute(
        "DELETE FROM items WHERE library_id = ?1 AND kind = 'show'
         AND id NOT IN (SELECT parent_id FROM items
                        WHERE kind = 'season' AND parent_id IS NOT NULL)",
        params![library_id],
    )? as u64;
    let empty_folders: i64 = conn.query_row(
        "WITH RECURSIVE descendants(root_id, id, kind) AS (
             SELECT root.id, child.id, child.kind FROM items root
             LEFT JOIN items child ON child.parent_id = root.id
             WHERE root.library_id = ?1 AND root.kind = 'folder'
             UNION
             SELECT descendants.root_id, child.id, child.kind
             FROM descendants JOIN items child ON child.parent_id = descendants.id
         )
         SELECT COUNT(*) FROM items WHERE library_id = ?1 AND kind = 'folder'
           AND NOT EXISTS (SELECT 1 FROM descendants
                           WHERE root_id = items.id AND kind != 'folder')",
        params![library_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "WITH RECURSIVE descendants(root_id, id, kind) AS (
             SELECT root.id, child.id, child.kind FROM items root
             LEFT JOIN items child ON child.parent_id = root.id
             WHERE root.library_id = ?1 AND root.kind = 'folder'
             UNION
             SELECT descendants.root_id, child.id, child.kind
             FROM descendants JOIN items child ON child.parent_id = descendants.id
         )
         DELETE FROM items WHERE library_id = ?1 AND kind = 'folder'
           AND NOT EXISTS (SELECT 1 FROM descendants
                           WHERE root_id = items.id AND kind != 'folder')",
        params![library_id],
    )?;
    pruned_items += empty_folders.max(0) as u64;
    Ok(ReconcileOutcome::Applied {
        deleted_files,
        pruned_items,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cluster::coordination::LeaseClaim;
    use crate::domain::{LibraryKind, NewLibrary};
    use crate::store::{CoordinationStore, LibraryStore, MediaStore};

    #[tokio::test]
    async fn conditional_book_publication_composes_both_fences_and_snapshot_cas() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .with_conn(|conn| {
                conn.execute_batch(
                    "CREATE TABLE cluster_artwork_repairs (
                       item_id INTEGER PRIMARY KEY,
                       owner_node_id TEXT NOT NULL,
                       leader_term INTEGER NOT NULL,
                       generation INTEGER NOT NULL
                     ) STRICT;",
                )?;
                Ok(())
            })
            .await
            .expect("artwork fence fixture table");
        let library = store
            .create_library(&NewLibrary {
                name: "Dual Fence Books".to_owned(),
                kind: LibraryKind::Books,
                paths: vec![PathBuf::from("/dual-fence")],
                anime: false,
            })
            .await
            .expect("library");
        let item_id = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Book,
                parent_id: None,
                title: "Original".to_owned(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("book");
        store
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO cluster_artwork_repairs
                       (item_id, owner_node_id, leader_term, generation)
                     VALUES (?1, 'node-a', 7, 1)",
                    params![item_id],
                )?;
                Ok(())
            })
            .await
            .expect("artwork fence fixture");
        let first = match store
            .acquire_lease("provider:artwork", "node-a", 100, 300)
            .await
            .expect("job lease")
        {
            LeaseClaim::Acquired(lease) => lease,
            held => panic!("job lease was unexpectedly held: {held:?}"),
        };
        let repair = ArtworkRepairFence {
            item_id,
            owner_node_id: "node-a".to_owned(),
            leader_term: 7,
            generation: 1,
        };
        let original = store
            .get_item(item_id)
            .await
            .expect("read original")
            .expect("book exists");
        assert!(store
            .apply_book_metadata_if_current_fenced(
                &original,
                &BookMetadataPatch {
                    title: Some("Current".to_owned()),
                    author: Some("Current Author".to_owned()),
                    work_id: Some("current-work".to_owned()),
                    edition_id: Some("current-edition".to_owned()),
                    poster_path: Some("current.jpg".to_owned()),
                    source: crate::domain::BookMetadataSource::Curator,
                },
                Some(&repair),
                &first,
                150,
            )
            .await
            .expect("current dual-fenced publication"));
        assert!(
            !store
                .apply_book_metadata_if_current_fenced(
                    &original,
                    &BookMetadataPatch {
                        title: Some("Stale Snapshot".to_owned()),
                        author: None,
                        work_id: None,
                        edition_id: None,
                        poster_path: None,
                        source: crate::domain::BookMetadataSource::Curator,
                    },
                    Some(&repair),
                    &first,
                    151,
                )
                .await
                .expect("stale snapshot result"),
            "a stale full-field snapshot must lose"
        );
        let current = store
            .get_item(item_id)
            .await
            .expect("read current")
            .expect("book exists");
        store
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE cluster_artwork_repairs SET generation = 2 WHERE item_id = ?1",
                    params![item_id],
                )?;
                Ok(())
            })
            .await
            .expect("advance artwork generation");
        assert!(
            !store
                .apply_book_metadata_if_current_fenced(
                    &current,
                    &BookMetadataPatch {
                        title: None,
                        author: Some("Stale Artwork Owner".to_owned()),
                        work_id: None,
                        edition_id: None,
                        poster_path: None,
                        source: crate::domain::BookMetadataSource::Curator,
                    },
                    Some(&repair),
                    &first,
                    152,
                )
                .await
                .expect("stale artwork result"),
            "an obsolete artwork generation must lose"
        );
        let renewed = store
            .renew_lease(&first, 160, 400)
            .await
            .expect("renew result")
            .expect("renewed lease");
        let current_repair = ArtworkRepairFence {
            generation: 2,
            ..repair
        };
        assert!(matches!(
            store
                .apply_book_metadata_if_current_fenced(
                    &current,
                    &BookMetadataPatch {
                        title: None,
                        author: Some("Stale Job Owner".to_owned()),
                        work_id: None,
                        edition_id: None,
                        poster_path: None,
                        source: crate::domain::BookMetadataSource::Curator,
                    },
                    Some(&current_repair),
                    &first,
                    170,
                )
                .await,
            Err(StoreError::FenceRejected { .. })
        ));
        assert!(store
            .apply_book_metadata_if_current_fenced(
                &current,
                &BookMetadataPatch {
                    title: None,
                    author: Some("Successor Author".to_owned()),
                    work_id: None,
                    edition_id: None,
                    poster_path: None,
                    source: crate::domain::BookMetadataSource::Curator,
                },
                Some(&current_repair),
                &renewed,
                171,
            )
            .await
            .expect("successor dual-fenced publication"));
    }
}

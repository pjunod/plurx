//! Replicated lease-fenced job publications.
//!
//! Every one-statement mutation carries the exact lease predicate in that
//! statement. Reconciliation prepends the same predicate to its existing Raft
//! transaction guard, so a stale owner cannot publish even after finishing
//! expensive work.

use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;

use super::hiqlite::{database_error, HiqliteAuthStore};
use crate::cluster::coordination::Lease;
use crate::domain::{
    sort_title_for, ArtworkAttempt, BookMetadataPatch, ItemKind, MetadataPatch, NewItem,
    ProbeResult,
};
use crate::error::StoreError;
use crate::store::{FencedPublicationStore, ReconcileOutcome, RootFingerprintStatus};

struct CurrentRow {
    current: i64,
}

impl From<&mut Row<'_>> for CurrentRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            current: row.get("current"),
        }
    }
}

struct IdRow {
    id: i64,
}

impl From<&mut Row<'_>> for IdRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self { id: row.get("id") }
    }
}

struct FingerprintRow {
    fingerprint: String,
}

struct CountRow {
    count: i64,
}

impl From<&mut Row<'_>> for CountRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            count: row.get("count"),
        }
    }
}

impl From<&mut Row<'_>> for FingerprintRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            fingerprint: row.get("fingerprint"),
        }
    }
}

impl HiqliteAuthStore {
    async fn require_publication_fence(
        &self,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<CurrentRow, _>(
                "SELECT EXISTS(SELECT 1 FROM job_leases
                   WHERE resource = $1 AND owner_node_id = $2
                     AND fence = $3 AND revision = $4
                     AND expires_at_ms = $5 AND expires_at_ms > $6) AS current",
                params!(
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        if rows.first().is_some_and(|row| row.current == 1) {
            Ok(())
        } else {
            Err(fence_rejected(lease))
        }
    }

    async fn accept_zero_if_current(
        &self,
        changed: usize,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        if changed > 0 {
            Ok(())
        } else {
            self.require_publication_fence(lease, observed_at_unix_ms)
                .await
        }
    }

    async fn map_returning_fence<T>(
        &self,
        result: Result<T, StoreError>,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<T, StoreError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => match self
                .require_publication_fence(lease, observed_at_unix_ms)
                .await
            {
                Err(StoreError::FenceRejected { .. }) => Err(fence_rejected(lease)),
                Err(validation_error) => Err(validation_error),
                Ok(()) => Err(error),
            },
        }
    }
}

#[async_trait]
impl FencedPublicationStore for HiqliteAuthStore {
    async fn put_setting_fenced(
        &self,
        key: &str,
        value: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        let changed = self
            .execute(
                "INSERT INTO settings (key, value, updated_at)
                 SELECT $1, $2, $3 WHERE EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $4 AND owner_node_id = $5
                     AND fence = $6 AND revision = $7 AND expires_at_ms = $8
                     AND expires_at_ms > $9)
                 ON CONFLICT(key) DO UPDATE SET
                   value = excluded.value, updated_at = excluded.updated_at",
                params!(
                    key,
                    value,
                    now,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
            .await
    }

    async fn mark_library_scanned_fenced(
        &self,
        id: i64,
        refreshed: bool,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        let changed = self
            .execute(
                "UPDATE libraries SET last_scan_at = $1,
                   last_refresh_at = CASE WHEN $2 THEN $1 ELSE last_refresh_at END
                 WHERE id = $3 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $4 AND owner_node_id = $5
                     AND fence = $6 AND revision = $7 AND expires_at_ms = $8
                     AND expires_at_ms > $9)",
                params!(
                    now,
                    refreshed,
                    id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
            .await
    }

    async fn insert_item_fenced(
        &self,
        item: &NewItem,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<i64, StoreError> {
        let sort_title = if item.kind == ItemKind::Folder {
            item.title.to_lowercase()
        } else {
            sort_title_for(&item.title)
        };
        let now = self.now()?;
        let result = self
            .client()
            .execute_returning_map_one::<_, IdRow>(
                "INSERT INTO items
                   (library_id, kind, parent_id, title, sort_title, year,
                    season_number, episode_number, added_at, updated_at)
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $9
                 WHERE EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $10 AND owner_node_id = $11
                     AND fence = $12 AND revision = $13 AND expires_at_ms = $14
                     AND expires_at_ms > $15)
                 RETURNING id",
                params!(
                    item.library_id,
                    item.kind.as_str(),
                    item.parent_id,
                    item.title.as_str(),
                    sort_title,
                    item.year,
                    item.season_number,
                    item.episode_number,
                    now,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await;
        Ok(self
            .map_returning_fence(result, lease, observed_at_unix_ms)
            .await?
            .id)
    }

    async fn apply_metadata_fenced(
        &self,
        item_id: i64,
        patch: &MetadataPatch,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let sort_title = patch.title.as_deref().map(sort_title_for);
        let tags = patch
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(database_error)?;
        let genres = patch
            .genres
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(database_error)?;
        let artwork_error = match &patch.artwork {
            Some(ArtworkAttempt::Failed(reason)) => Some(reason.as_str()),
            _ => None,
        };
        let now = self.now()?;
        let changed = self
            .execute(
                "UPDATE items SET
                   title = COALESCE($1, title), sort_title = COALESCE($2, sort_title),
                   year = COALESCE($3, year), overview = COALESCE($4, overview),
                   tmdb_id = COALESCE($5, tmdb_id), imdb_id = COALESCE($6, imdb_id),
                   air_date = COALESCE($7, air_date), runtime_ms = COALESCE($8, runtime_ms),
                   poster_path = COALESCE($9, poster_path),
                   backdrop_path = COALESCE($10, backdrop_path),
                   recorded_at = COALESCE($11, recorded_at), tags = COALESCE($12, tags),
                   genres = COALESCE($13, genres),
                   artwork_error = CASE WHEN $14 = 1 THEN $15 ELSE artwork_error END,
                   metadata_at = CASE WHEN $16 = 1 THEN $17 ELSE metadata_at END,
                   artwork_attempted_at = CASE WHEN $14 = 1 THEN $17 ELSE artwork_attempted_at END,
                   updated_at = $17
                 WHERE id = $18 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $19 AND owner_node_id = $20
                     AND fence = $21 AND revision = $22 AND expires_at_ms = $23
                     AND expires_at_ms > $24)",
                params!(
                    patch.title.as_deref(),
                    sort_title,
                    patch.year,
                    patch.overview.as_deref(),
                    patch.tmdb_id,
                    patch.imdb_id.as_deref(),
                    patch.air_date.as_deref(),
                    patch.runtime_ms,
                    patch.poster_path.as_deref(),
                    patch.backdrop_path.as_deref(),
                    patch.recorded_at.as_deref(),
                    tags,
                    genres,
                    patch.artwork.is_some(),
                    artwork_error,
                    patch.enriched,
                    now,
                    item_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
            .await
    }

    async fn apply_book_metadata_fenced(
        &self,
        item_id: i64,
        patch: &BookMetadataPatch,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let sort_title = patch.title.as_deref().map(sort_title_for);
        let source = patch.source.as_str();
        let now = self.now()?;
        let changed = self
            .execute(
                "UPDATE items SET
                   title = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                            OR book_metadata_source = 'epub')
                                  THEN COALESCE($2, title) ELSE title END,
                   sort_title = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                                 OR book_metadata_source = 'epub')
                                       THEN COALESCE($3, sort_title) ELSE sort_title END,
                   author = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                             OR book_metadata_source = 'epub')
                                  THEN COALESCE($4, author) ELSE author END,
                   book_work_id = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                                   OR book_metadata_source = 'epub')
                                      THEN COALESCE($5, book_work_id) ELSE book_work_id END,
                   book_edition_id = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                                      OR book_metadata_source = 'epub')
                                         THEN COALESCE($6, book_edition_id)
                                         ELSE book_edition_id END,
                   poster_path = CASE WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                                  OR book_metadata_source = 'epub')
                                      THEN COALESCE($7, poster_path) ELSE poster_path END,
                   book_metadata_source = CASE
                     WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                           OR book_metadata_source = 'epub') THEN $1
                     ELSE book_metadata_source END,
                   updated_at = CASE
                     WHEN ($1 = 'curator' OR book_metadata_source IS NULL
                                           OR book_metadata_source = 'epub') THEN $8
                     ELSE updated_at END
                 WHERE id = $9 AND kind IN ('book','audiobook') AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $10 AND owner_node_id = $11
                     AND fence = $12 AND revision = $13 AND expires_at_ms = $14
                     AND expires_at_ms > $15)",
                params!(
                    source,
                    patch.title.as_deref(),
                    sort_title,
                    patch.author.as_deref(),
                    patch.work_id.as_deref(),
                    patch.edition_id.as_deref(),
                    patch.poster_path.as_deref(),
                    now,
                    item_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
            .await
    }

    async fn set_nfo_seeded_fenced(
        &self,
        item_id: i64,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let now = self.now()?;
        let changed = self
            .execute(
                "UPDATE items SET nfo_seeded_at = $1 WHERE id = $2 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $3 AND owner_node_id = $4
                     AND fence = $5 AND revision = $6 AND expires_at_ms = $7
                     AND expires_at_ms > $8)",
                params!(
                    now,
                    item_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
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
        let audio = serde_json::to_string(&probe.audio_streams).map_err(database_error)?;
        let subtitles = serde_json::to_string(&probe.subtitle_streams).map_err(database_error)?;
        let now = self.now()?;
        let result = self
            .client()
            .execute_returning_map_one::<_, IdRow>(
                "INSERT INTO files
                   (item_id, path, size, mtime, duration_ms, container, video_codec,
                    video_profile, width, height, bit_depth, hdr, bitrate,
                    audio_streams, subtitle_streams, probe_json, hdr_format, scanned_at)
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                        $14, $15, $16, $17, $18 WHERE EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $19 AND owner_node_id = $20
                     AND fence = $21 AND revision = $22 AND expires_at_ms = $23
                     AND expires_at_ms > $24)
                 ON CONFLICT(path) DO UPDATE SET
                   item_id = excluded.item_id, size = excluded.size, mtime = excluded.mtime,
                   duration_ms = excluded.duration_ms, container = excluded.container,
                   video_codec = excluded.video_codec, video_profile = excluded.video_profile,
                   width = excluded.width, height = excluded.height,
                   bit_depth = excluded.bit_depth, hdr = excluded.hdr,
                   bitrate = excluded.bitrate, audio_streams = excluded.audio_streams,
                   subtitle_streams = excluded.subtitle_streams,
                   probe_json = excluded.probe_json, hdr_format = excluded.hdr_format,
                   scanned_at = excluded.scanned_at RETURNING id",
                params!(
                    item_id,
                    path,
                    size,
                    mtime,
                    probe.duration_ms,
                    probe.container.as_deref(),
                    probe.video_codec.as_deref(),
                    probe.video_profile.as_deref(),
                    probe.width,
                    probe.height,
                    probe.bit_depth,
                    probe.hdr.as_deref(),
                    probe.bitrate,
                    audio,
                    subtitles,
                    probe.raw_json.as_deref(),
                    probe.hdr_format.as_deref(),
                    now,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await;
        Ok(self
            .map_returning_fence(result, lease, observed_at_unix_ms)
            .await?
            .id)
    }

    async fn ensure_library_root_fingerprint_fenced(
        &self,
        library_id: i64,
        fingerprint: &str,
        allow_establish: bool,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<RootFingerprintStatus, StoreError> {
        let inserted = self
            .execute(
                "INSERT INTO library_roots (library_id, fingerprint)
                 SELECT $1, $2 WHERE $3 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $4 AND owner_node_id = $5
                     AND fence = $6 AND revision = $7 AND expires_at_ms = $8
                     AND expires_at_ms > $9)
                 ON CONFLICT(library_id) DO NOTHING",
                params!(
                    library_id,
                    fingerprint,
                    allow_establish,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        if inserted == 1 {
            return Ok(RootFingerprintStatus::Established);
        }
        let touched = self
            .execute(
                "UPDATE library_roots SET fingerprint = fingerprint
                 WHERE library_id = $1 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $2 AND owner_node_id = $3
                     AND fence = $4 AND revision = $5 AND expires_at_ms = $6
                     AND expires_at_ms > $7)",
                params!(
                    library_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        if touched == 0 {
            self.require_publication_fence(lease, observed_at_unix_ms)
                .await?;
        }
        let expected = self
            .client()
            .query_consistent_map::<FingerprintRow, _>(
                "SELECT fingerprint FROM library_roots WHERE library_id = $1",
                params!(library_id),
            )
            .await?
            .into_iter()
            .next()
            .map(|row| row.fingerprint);
        let Some(expected) = expected else {
            return Ok(RootFingerprintStatus::Unestablished);
        };
        if expected == fingerprint {
            Ok(RootFingerprintStatus::Matched)
        } else {
            Ok(RootFingerprintStatus::Mismatch { expected })
        }
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
        let ids = serde_json::to_string(gone_file_ids).map_err(database_error)?;
        let limit = i64::try_from(prune_limit).unwrap_or(i64::MAX);
        let statements = vec![
            (
                "INSERT INTO scan_reconcile_guards (library_id)
                 SELECT $1 WHERE EXISTS (SELECT 1 FROM library_roots
                   WHERE library_id = $1 AND fingerprint = $2)
                 AND (SELECT COUNT(*) FROM files f JOIN items i ON i.id = f.item_id
                   WHERE i.library_id = $1
                     AND f.id IN (SELECT value FROM json_each($3))) <= $4
                 AND EXISTS (SELECT 1 FROM job_leases
                   WHERE resource = $5 AND owner_node_id = $6
                     AND fence = $7 AND revision = $8 AND expires_at_ms = $9
                     AND expires_at_ms > $10)
                 ON CONFLICT(library_id) DO NOTHING"
                    .to_owned(),
                params!(
                    library_id,
                    root_fingerprint,
                    ids.as_str(),
                    limit,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            ),
            (
                "DELETE FROM files WHERE item_id IN
                   (SELECT id FROM items WHERE library_id = $1)
                 AND id IN (SELECT value FROM json_each($2))
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)"
                    .to_owned(),
                params!(library_id, ids.as_str()),
            ),
            (
                "DELETE FROM items WHERE library_id = $1
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)
                 AND kind IN ('movie','episode','video','photo','book','audiobook')
                 AND id NOT IN (SELECT item_id FROM files)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)
                 AND kind = 'season' AND id NOT IN (SELECT parent_id FROM items
                   WHERE kind = 'episode' AND parent_id IS NOT NULL)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1
                 AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)
                 AND kind = 'show' AND id NOT IN (SELECT parent_id FROM items
                   WHERE kind = 'season' AND parent_id IS NOT NULL)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "WITH RECURSIVE descendants(root_id, id, kind) AS (
                   SELECT root.id, child.id, child.kind FROM items root
                   LEFT JOIN items child ON child.parent_id = root.id
                   WHERE root.library_id = $1 AND root.kind = 'folder'
                   UNION SELECT descendants.root_id, child.id, child.kind
                   FROM descendants JOIN items child ON child.parent_id = descendants.id
                 ) INSERT INTO scan_reconcile_items (library_id, item_id)
                   SELECT $1, items.id FROM items
                   WHERE items.library_id = $1 AND items.kind = 'folder'
                     AND EXISTS (SELECT 1 FROM scan_reconcile_guards WHERE library_id = $1)
                     AND NOT EXISTS (SELECT 1 FROM descendants
                       WHERE root_id = items.id AND kind != 'folder')
                   ON CONFLICT(library_id, item_id) DO NOTHING"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM items WHERE library_id = $1 AND id IN
                   (SELECT item_id FROM scan_reconcile_items WHERE library_id = $1)"
                    .to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM scan_reconcile_items WHERE library_id = $1".to_owned(),
                params!(library_id),
            ),
            (
                "DELETE FROM scan_reconcile_guards WHERE library_id = $1".to_owned(),
                params!(library_id),
            ),
        ];
        let results = self
            .client()
            .txn(statements)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.first().copied() != Some(1) {
            self.require_publication_fence(lease, observed_at_unix_ms)
                .await?;
            let expected = self
                .client()
                .query_consistent_map::<FingerprintRow, _>(
                    "SELECT fingerprint FROM library_roots WHERE library_id = $1",
                    params!(library_id),
                )
                .await?
                .into_iter()
                .next()
                .map(|row| row.fingerprint);
            if expected.as_deref() != Some(root_fingerprint) {
                return Ok(ReconcileOutcome::RefusedRoot {
                    expected: expected.unwrap_or_else(|| "<unregistered>".to_owned()),
                });
            }
            let requested = self
                .client()
                .query_consistent_map::<CountRow, _>(
                    "SELECT COUNT(*) AS count FROM files f JOIN items i ON i.id = f.item_id
                     WHERE i.library_id = $1
                       AND f.id IN (SELECT value FROM json_each($2))",
                    params!(library_id, ids.as_str()),
                )
                .await?
                .into_iter()
                .next()
                .map_or(0, |row| row.count.max(0) as u64);
            if requested > prune_limit {
                return Ok(ReconcileOutcome::RefusedPrune {
                    requested,
                    limit: prune_limit,
                });
            }
            return Err(StoreError::Database(
                "scan reconciliation guard was already held by another operation".to_owned(),
            ));
        }
        Ok(ReconcileOutcome::Applied {
            deleted_files: results[1] as u64,
            pruned_items: results[2..=5].iter().map(|rows| *rows as u64).sum(),
        })
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
        let now = self.now()?;
        let fence = lease_i64("fence", lease.fence)?;
        let revision = lease_i64("revision", lease.revision)?;
        let results = self
            .client()
            .txn(vec![
                (
                    "INSERT INTO transcode_cache_recipes
                   (recipe_hash, file_id, recipe_version, created_at)
                 SELECT $1, $2, $3, $4 WHERE EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $5 AND owner_node_id = $6
                     AND fence = $7 AND revision = $8 AND expires_at_ms = $9
                     AND expires_at_ms > $10)
                 ON CONFLICT(recipe_hash) DO NOTHING"
                        .to_owned(),
                    params!(
                        recipe_hash,
                        file_id,
                        recipe_version,
                        now,
                        lease.resource.as_str(),
                        lease.owner_node_id.as_str(),
                        fence,
                        revision,
                        lease.expires_at_unix_ms,
                        observed_at_unix_ms
                    ),
                ),
                (
                    "INSERT INTO transcode_cache_locations
                   (recipe_hash, node_id, storage_class, relative_dir, bytes, complete,
                    last_used_at, last_seen_at)
                 SELECT $1, $2, 'local', $3, 0, 0, $4, $4 WHERE EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $5 AND owner_node_id = $6
                     AND fence = $7 AND revision = $8 AND expires_at_ms = $9
                     AND expires_at_ms > $10)
                 ON CONFLICT(recipe_hash, node_id, storage_class) DO UPDATE SET
                   relative_dir = excluded.relative_dir,
                   last_seen_at = excluded.last_seen_at
                 WHERE transcode_cache_locations.complete = 0"
                        .to_owned(),
                    params!(
                        recipe_hash,
                        node_id,
                        relative_dir,
                        now,
                        lease.resource.as_str(),
                        lease.owner_node_id.as_str(),
                        fence,
                        revision,
                        lease.expires_at_unix_ms,
                        observed_at_unix_ms
                    ),
                ),
            ])
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        let claimed = results.get(1).copied().unwrap_or_default();
        if claimed == 0 {
            self.require_publication_fence(lease, observed_at_unix_ms)
                .await?;
        }
        Ok(claimed > 0)
    }

    async fn touch_cache_claim_fenced(
        &self,
        recipe_hash: &str,
        node_id: &str,
        lease: &Lease,
        observed_at_unix_ms: i64,
    ) -> Result<(), StoreError> {
        let changed = self
            .execute(
                "UPDATE transcode_cache_locations SET last_seen_at = $1
                 WHERE recipe_hash = $2 AND node_id = $3 AND complete = 0 AND EXISTS (
                   SELECT 1 FROM job_leases WHERE resource = $4 AND owner_node_id = $5
                     AND fence = $6 AND revision = $7 AND expires_at_ms = $8
                     AND expires_at_ms > $9)",
                params!(
                    self.now()?,
                    recipe_hash,
                    node_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
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
        let now = self.now()?;
        let changed = self
            .execute(
                "UPDATE transcode_cache_locations
                 SET relative_dir = $1, complete = 1, bytes = $2,
                     last_used_at = $3, last_seen_at = $3
                 WHERE recipe_hash = $4 AND node_id = $5 AND storage_class = 'local'
                   AND EXISTS (SELECT 1 FROM job_leases
                     WHERE resource = $6 AND owner_node_id = $7
                       AND fence = $8 AND revision = $9 AND expires_at_ms = $10
                       AND expires_at_ms > $11)",
                params!(
                    relative_dir,
                    bytes,
                    now,
                    recipe_hash,
                    node_id,
                    lease.resource.as_str(),
                    lease.owner_node_id.as_str(),
                    lease_i64("fence", lease.fence)?,
                    lease_i64("revision", lease.revision)?,
                    lease.expires_at_unix_ms,
                    observed_at_unix_ms
                ),
            )
            .await?;
        self.accept_zero_if_current(changed, lease, observed_at_unix_ms)
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
        let fence = lease_i64("fence", lease.fence)?;
        let revision = lease_i64("revision", lease.revision)?;
        let results = self
            .client()
            .txn(vec![
                (
                    "DELETE FROM transcode_cache_locations
                 WHERE recipe_hash = $1 AND node_id = $2 AND storage_class = $3
                   AND EXISTS (SELECT 1 FROM job_leases
                     WHERE resource = $4 AND owner_node_id = $5
                       AND fence = $6 AND revision = $7 AND expires_at_ms = $8
                       AND expires_at_ms > $9)"
                        .to_owned(),
                    params!(
                        recipe_hash,
                        node_id,
                        storage_class,
                        lease.resource.as_str(),
                        lease.owner_node_id.as_str(),
                        fence,
                        revision,
                        lease.expires_at_unix_ms,
                        observed_at_unix_ms
                    ),
                ),
                (
                    "DELETE FROM transcode_cache_recipes WHERE recipe_hash = $1
                     AND NOT EXISTS (SELECT 1 FROM transcode_cache_locations
                                     WHERE recipe_hash = $1)
                     AND EXISTS (SELECT 1 FROM job_leases
                       WHERE resource = $2 AND owner_node_id = $3
                         AND fence = $4 AND revision = $5 AND expires_at_ms = $6
                         AND expires_at_ms > $7)"
                        .to_owned(),
                    params!(
                        recipe_hash,
                        lease.resource.as_str(),
                        lease.owner_node_id.as_str(),
                        fence,
                        revision,
                        lease.expires_at_unix_ms,
                        observed_at_unix_ms
                    ),
                ),
            ])
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.first().copied().unwrap_or_default() == 0 {
            self.require_publication_fence(lease, observed_at_unix_ms)
                .await?;
        }
        Ok(())
    }
}

fn lease_i64(label: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|error| StoreError::Database(format!("lease {label} is out of range: {error}")))
}

fn fence_rejected(lease: &Lease) -> StoreError {
    StoreError::FenceRejected {
        resource: lease.resource.clone(),
        owner_node_id: lease.owner_node_id.clone(),
        fence: lease.fence,
    }
}

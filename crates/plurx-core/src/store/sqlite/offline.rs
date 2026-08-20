//! Durable app-managed offline package requests and transfer leases.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::SqliteStore;
use crate::domain::{
    NewOfflinePackage, OfflineActivityPackage, OfflineCreateOutcome, OfflineLease,
    OfflineLeaseOutcome, OfflinePackage, OfflinePackageStats, OfflineRemovalPlanEntry,
    OfflineRemovalReport,
};
use crate::error::StoreError;
use crate::store::OfflinePackageStore;

const PACKAGE_COLS: &str = "id, request_id, user_id, file_id, node_id, source_path, \
    source_size, source_mtime, recipe_hash, effective_rate_control, target_height, audio_index, audio_offset_ms, \
    output_width, output_height, subtitle_index, subtitle_language, subtitle_mode, state, phase, progress_millis, estimated_bytes, \
    reserved_bytes, actual_bytes, duration_ms, error_code, error_message, created_at, updated_at, \
    last_access_at, expires_at";
const PACKAGE_COL_COUNT: usize = 31;

fn package_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OfflinePackage> {
    Ok(OfflinePackage {
        id: row.get(0)?,
        request_id: row.get(1)?,
        user_id: row.get(2)?,
        file_id: row.get(3)?,
        node_id: row.get(4)?,
        source_path: row.get(5)?,
        source_size: row.get(6)?,
        source_mtime: row.get(7)?,
        recipe_hash: row.get(8)?,
        effective_rate_control: row.get(9)?,
        target_height: row.get(10)?,
        audio_index: row.get(11)?,
        audio_offset_ms: row.get(12)?,
        output_width: row.get(13)?,
        output_height: row.get(14)?,
        subtitle_index: row.get(15)?,
        subtitle_language: row.get(16)?,
        subtitle_mode: row.get(17)?,
        state: row.get(18)?,
        phase: row.get(19)?,
        progress_millis: row.get(20)?,
        estimated_bytes: row.get(21)?,
        reserved_bytes: row.get(22)?,
        actual_bytes: row.get(23)?,
        duration_ms: row.get(24)?,
        error_code: row.get(25)?,
        error_message: row.get(26)?,
        created_at: row.get(27)?,
        updated_at: row.get(28)?,
        last_access_at: row.get(29)?,
        expires_at: row.get(30)?,
    })
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OfflineLease> {
    Ok(OfflineLease {
        token_hash: row.get(0)?,
        package_id: row.get(1)?,
        created_at: row.get(2)?,
        last_access_at: row.get(3)?,
        expires_at: row.get(4)?,
    })
}

fn same_request(existing: &OfflinePackage, requested: &NewOfflinePackage) -> bool {
    existing.file_id == requested.file_id
        && existing.node_id == requested.node_id
        && existing.source_path == requested.source_path
        && existing.source_size == requested.source_size
        && existing.source_mtime == requested.source_mtime
        && existing.target_height == requested.target_height
        && existing.output_width == requested.output_width
        && existing.output_height == requested.output_height
        && existing.audio_index == requested.audio_index
        && existing.audio_offset_ms == requested.audio_offset_ms
        && existing.subtitle_index == requested.subtitle_index
        && existing.subtitle_language == requested.subtitle_language
        && existing.subtitle_mode == requested.subtitle_mode
        && existing.estimated_bytes == requested.estimated_bytes
        && existing.reserved_bytes == requested.reserved_bytes
}

fn exceeds_byte_limit(used: i64, reserved: i64, limit: i64) -> bool {
    limit <= 0 || reserved < 0 || used > limit || reserved > limit - used
}

#[async_trait]
impl OfflinePackageStore for SqliteStore {
    async fn create_offline_package(
        &self,
        package: &NewOfflinePackage,
        max_rows_per_user: i64,
        max_bytes_per_user: i64,
        max_bytes_global: i64,
    ) -> Result<OfflineCreateOutcome, StoreError> {
        if crate::transcode::EffectiveRateControl::parse_snapshot(&package.effective_rate_control)
            .is_none()
        {
            return Err(StoreError::Database(format!(
                "invalid offline effective rate control {:?}",
                package.effective_rate_control
            )));
        }
        let requested = package.clone();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;

            let existing = tx
                .query_row(
                    &format!(
                        "SELECT {PACKAGE_COLS} FROM offline_packages \
                         WHERE user_id = ?1 AND request_id = ?2"
                    ),
                    params![requested.user_id, requested.request_id],
                    package_from_row,
                )
                .optional()?;
            if let Some(existing) = existing {
                tx.commit()?;
                return Ok(if same_request(&existing, &requested) {
                    OfflineCreateOutcome::Existing(existing)
                } else {
                    OfflineCreateOutcome::RequestConflict
                });
            }

            let rows: i64 = tx.query_row(
                "SELECT COUNT(*) FROM offline_packages WHERE user_id = ?1",
                [requested.user_id],
                |row| row.get(0),
            )?;
            if max_rows_per_user <= 0 || rows >= max_rows_per_user {
                tx.commit()?;
                return Ok(OfflineCreateOutcome::RowLimit {
                    limit: max_rows_per_user.max(0),
                });
            }

            let used: i64 = tx.query_row(
                "SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                 FROM offline_packages \
                 WHERE user_id = ?1 AND state IN ('queued', 'preparing', 'ready')",
                [requested.user_id],
                |row| row.get(0),
            )?;
            if exceeds_byte_limit(used, requested.reserved_bytes, max_bytes_per_user) {
                tx.commit()?;
                return Ok(OfflineCreateOutcome::ByteLimit {
                    used,
                    limit: max_bytes_per_user.max(0),
                });
            }

            let global_used: i64 = tx.query_row(
                "SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                 FROM offline_packages WHERE node_id = ?1 \
                   AND state IN ('queued', 'preparing', 'ready')",
                [&requested.node_id],
                |row| row.get(0),
            )?;
            if exceeds_byte_limit(global_used, requested.reserved_bytes, max_bytes_global) {
                tx.commit()?;
                return Ok(OfflineCreateOutcome::GlobalByteLimit {
                    used: global_used,
                    limit: max_bytes_global.max(0),
                });
            }

            tx.execute(
                "INSERT INTO offline_packages (
                    id, request_id, user_id, file_id, node_id, source_path,
                    source_size, source_mtime, effective_rate_control, target_height, audio_index,
                    audio_offset_ms, output_width, output_height, subtitle_index,
                    subtitle_language, subtitle_mode, state, phase,
                    estimated_bytes, reserved_bytes, expires_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, 'queued', 'waiting_for_encoder', ?18, ?19, ?20
                 )",
                params![
                    requested.id,
                    requested.request_id,
                    requested.user_id,
                    requested.file_id,
                    requested.node_id,
                    requested.source_path,
                    requested.source_size,
                    requested.source_mtime,
                    requested.effective_rate_control,
                    requested.target_height,
                    requested.audio_index,
                    requested.audio_offset_ms,
                    requested.output_width,
                    requested.output_height,
                    requested.subtitle_index,
                    requested.subtitle_language,
                    requested.subtitle_mode,
                    requested.estimated_bytes,
                    requested.reserved_bytes,
                    requested.expires_at,
                ],
            )?;
            let created = tx.query_row(
                &format!("SELECT {PACKAGE_COLS} FROM offline_packages WHERE id = ?1"),
                [&requested.id],
                package_from_row,
            )?;
            tx.commit()?;
            Ok(OfflineCreateOutcome::Created(created))
        })
        .await
    }

    async fn offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let id = package_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {PACKAGE_COLS} FROM offline_packages \
                         WHERE id = ?1 AND user_id = ?2"
                    ),
                    params![id, user_id],
                    package_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn renew_offline_package_for_user(
        &self,
        package_id: &str,
        user_id: i64,
        expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let id = package_id.to_owned();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE offline_packages SET last_access_at = unixepoch(), \
                 expires_at = ?3, updated_at = unixepoch() \
                 WHERE id = ?1 AND user_id = ?2",
                params![id, user_id, expires_at],
            )?;
            if changed == 0 {
                tx.commit()?;
                return Ok(None);
            }
            tx.execute(
                "UPDATE offline_package_leases SET last_access_at = unixepoch(), \
                 expires_at = ?2 WHERE package_id = ?1",
                params![id, expires_at],
            )?;
            let package = tx.query_row(
                &format!("SELECT {PACKAGE_COLS} FROM offline_packages WHERE id = ?1"),
                [&id],
                package_from_row,
            )?;
            tx.commit()?;
            Ok(Some(package))
        })
        .await
    }

    async fn offline_activity_packages(
        &self,
        node_id: &str,
        now: i64,
        active_since: i64,
        limit: i64,
    ) -> Result<Vec<OfflineActivityPackage>, StoreError> {
        let node = node_id.to_owned();
        let limit = limit.clamp(1, 100);
        self.with_conn(move |conn| {
            let prefixed = PACKAGE_COLS
                .split(", ")
                .map(|column| format!("p.{column}"))
                .collect::<Vec<_>>()
                .join(", ");
            let active_lease = "EXISTS (
                SELECT 1 FROM offline_package_leases l
                WHERE l.package_id = p.id AND l.expires_at > ?2
                  AND l.last_access_at >= ?3
            )";
            let mut stmt = conn.prepare(&format!(
                "SELECT {prefixed}, {active_lease} FROM offline_packages p
                 WHERE p.node_id = ?1
                   AND (p.state IN ('queued', 'preparing') OR {active_lease})
                 ORDER BY CASE p.state
                    WHEN 'preparing' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END,
                    (SELECT COALESCE(MAX(recent.last_access_at), 0)
                     FROM offline_package_leases recent
                     WHERE recent.package_id = p.id) DESC,
                    p.created_at, p.id
                 LIMIT ?4"
            ))?;
            let rows = stmt
                .query_map(params![node, now, active_since, limit], |row| {
                    Ok(OfflineActivityPackage {
                        package: package_from_row(row)?,
                        lease_active: row.get(PACKAGE_COL_COUNT)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    async fn offline_package_stats(
        &self,
        node_id: &str,
        now: i64,
    ) -> Result<OfflinePackageStats, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT
                    COALESCE(SUM(state = 'queued'), 0),
                    COALESCE(SUM(state = 'preparing'), 0),
                    COALESCE(SUM(state = 'ready'), 0),
                    COALESCE(SUM(state = 'failed'), 0),
                    COALESCE(SUM(CASE WHEN state = 'queued'
                        THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN state = 'preparing'
                        THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN state = 'ready'
                        THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN state = 'failed'
                        THEN COALESCE(actual_bytes, reserved_bytes) ELSE 0 END), 0),
                    (SELECT COUNT(*) FROM offline_package_leases l
                     JOIN offline_packages active ON active.id = l.package_id
                     WHERE active.node_id = ?1 AND active.state = 'ready'
                       AND l.expires_at > ?2),
                    (SELECT COALESCE(SUM(location.bytes), 0)
                     FROM transcode_cache_locations location
                     WHERE location.node_id = ?1
                       AND location.storage_class = 'local'
                       AND location.complete = 1
                       AND EXISTS (
                           SELECT 1 FROM offline_packages pinned
                           WHERE pinned.node_id = location.node_id
                             AND pinned.recipe_hash = location.recipe_hash
                             AND pinned.state IN ('queued', 'preparing', 'ready')
                       ))
                 FROM offline_packages WHERE node_id = ?1",
                params![node, now],
                |row| {
                    Ok(OfflinePackageStats {
                        queued: row.get(0)?,
                        preparing: row.get(1)?,
                        ready: row.get(2)?,
                        failed: row.get(3)?,
                        queued_bytes: row.get(4)?,
                        preparing_bytes: row.get(5)?,
                        ready_bytes: row.get(6)?,
                        failed_bytes: row.get(7)?,
                        active_leases: row.get(8)?,
                        pinned_bytes: row.get(9)?,
                    })
                },
            )?)
        })
        .await
    }

    async fn reset_interrupted_offline_packages(&self, node_id: &str) -> Result<u64, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'queued', \
                 phase = 'waiting_for_encoder', updated_at = unixepoch() \
                 WHERE node_id = ?1 AND state = 'preparing'",
                [node],
            )? as u64)
        })
        .await
    }

    async fn claim_next_offline_package(
        &self,
        node_id: &str,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let node = node_id.to_owned();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let id = tx
                .query_row(
                    "SELECT candidate.id FROM offline_packages candidate \
                     WHERE candidate.node_id = ?1 AND candidate.state = 'queued' \
                     ORDER BY (SELECT COUNT(*) FROM offline_packages served \
                               WHERE served.node_id = candidate.node_id \
                                 AND served.user_id = candidate.user_id \
                                 AND served.state != 'queued'), \
                              candidate.created_at, candidate.id LIMIT 1",
                    [node],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(id) = id else {
                tx.commit()?;
                return Ok(None);
            };
            tx.execute(
                "UPDATE offline_packages SET state = 'preparing', \
                 phase = 'waiting_for_encoder', updated_at = unixepoch() \
                 WHERE id = ?1 AND state = 'queued'",
                [&id],
            )?;
            let package = tx.query_row(
                &format!("SELECT {PACKAGE_COLS} FROM offline_packages WHERE id = ?1"),
                [&id],
                package_from_row,
            )?;
            tx.commit()?;
            Ok(Some(package))
        })
        .await
    }

    async fn requeue_offline_package(
        &self,
        package_id: &str,
        node_id: &str,
    ) -> Result<bool, StoreError> {
        let (id, node) = (package_id.to_owned(), node_id.to_owned());
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'queued', \
                 phase = 'waiting_for_encoder', updated_at = unixepoch() \
                 WHERE id = ?1 AND node_id = ?2 AND state = 'preparing'",
                params![id, node],
            )? > 0)
        })
        .await
    }

    async fn set_offline_package_recipe(
        &self,
        package_id: &str,
        recipe_hash: &str,
    ) -> Result<bool, StoreError> {
        let (id, hash) = (package_id.to_owned(), recipe_hash.to_owned());
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET recipe_hash = ?2, updated_at = unixepoch() \
                 WHERE id = ?1 AND state = 'preparing' \
                   AND (recipe_hash IS NULL OR recipe_hash = ?2)",
                params![id, hash],
            )? > 0)
        })
        .await
    }

    async fn update_offline_progress(
        &self,
        package_id: &str,
        node_id: &str,
        phase: &str,
        progress_millis: i64,
    ) -> Result<bool, StoreError> {
        let (id, node, phase) = (package_id.to_owned(), node_id.to_owned(), phase.to_owned());
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET phase = ?3, \
                 progress_millis = MAX(progress_millis, ?4), \
                 updated_at = unixepoch() \
                 WHERE id = ?1 AND node_id = ?2 AND state = 'preparing'",
                params![id, node, phase, progress_millis.clamp(0, 999)],
            )? > 0)
        })
        .await
    }

    async fn fail_offline_package(
        &self,
        package_id: &str,
        node_id: &str,
        phase: &str,
        code: &str,
        message: &str,
    ) -> Result<bool, StoreError> {
        let (id, node, phase, code, message) = (
            package_id.to_owned(),
            node_id.to_owned(),
            phase.to_owned(),
            code.to_owned(),
            message.to_owned(),
        );
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'failed', phase = ?3, \
                 error_code = ?4, error_message = ?5, updated_at = unixepoch() \
                 WHERE id = ?1 AND node_id = ?2 AND state IN ('queued', 'preparing')",
                params![id, node, phase, code, message],
            )? > 0)
        })
        .await
    }

    async fn put_offline_lease(
        &self,
        package_id: &str,
        user_id: i64,
        token_hash: &str,
        expires_at: i64,
    ) -> Result<OfflineLeaseOutcome, StoreError> {
        let (id, hash) = (package_id.to_owned(), token_hash.to_owned());
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let state = tx
                .query_row(
                    "SELECT state FROM offline_packages WHERE id = ?1 AND user_id = ?2",
                    params![id, user_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if state.as_deref() != Some("ready") {
                tx.commit()?;
                return Ok(OfflineLeaseOutcome::PackageNotReady);
            }

            let existing = tx
                .query_row(
                    "SELECT token_hash, package_id, created_at, last_access_at, expires_at \
                     FROM offline_package_leases WHERE package_id = ?1",
                    [&id],
                    lease_from_row,
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing.token_hash != hash {
                    tx.commit()?;
                    return Ok(OfflineLeaseOutcome::TokenConflict);
                }
                tx.execute(
                    "UPDATE offline_package_leases \
                     SET last_access_at = unixepoch(), expires_at = ?2 \
                     WHERE package_id = ?1",
                    params![id, expires_at],
                )?;
                tx.execute(
                    "UPDATE offline_packages \
                     SET last_access_at = unixepoch(), expires_at = ?2, \
                         updated_at = unixepoch() WHERE id = ?1",
                    params![id, expires_at],
                )?;
                let renewed = tx.query_row(
                    "SELECT token_hash, package_id, created_at, last_access_at, expires_at \
                     FROM offline_package_leases WHERE package_id = ?1",
                    [&id],
                    lease_from_row,
                )?;
                tx.commit()?;
                return Ok(OfflineLeaseOutcome::Renewed(renewed));
            }

            tx.execute(
                "INSERT INTO offline_package_leases (token_hash, package_id, expires_at) \
                 VALUES (?1, ?2, ?3)",
                params![hash, id, expires_at],
            )?;
            tx.execute(
                "UPDATE offline_packages SET last_access_at = unixepoch(), \
                 expires_at = ?2, updated_at = unixepoch() WHERE id = ?1",
                params![id, expires_at],
            )?;
            let created = tx.query_row(
                "SELECT token_hash, package_id, created_at, last_access_at, expires_at \
                 FROM offline_package_leases WHERE package_id = ?1",
                [&id],
                lease_from_row,
            )?;
            tx.commit()?;
            Ok(OfflineLeaseOutcome::Created(created))
        })
        .await
    }

    async fn offline_package_for_lease(
        &self,
        token_hash: &str,
        now: i64,
        renewed_expires_at: i64,
    ) -> Result<Option<OfflinePackage>, StoreError> {
        let hash = token_hash.to_owned();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let package = tx
                .query_row(
                    &format!(
                        "SELECT {} FROM offline_packages p \
                         JOIN offline_package_leases l ON l.package_id = p.id \
                         WHERE l.token_hash = ?1 AND l.expires_at > ?2 \
                           AND p.state = 'ready'",
                        PACKAGE_COLS
                            .split(", ")
                            .map(|column| format!("p.{column}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    params![hash, now],
                    package_from_row,
                )
                .optional()?;
            let Some(mut package) = package else {
                tx.commit()?;
                return Ok(None);
            };
            // A segment storm must not become a SQLite write storm. One touch
            // per minute keeps a seven-day lease fresh while bounding writes
            // independently of title length and HLS segment size.
            if now.saturating_sub(package.last_access_at) >= 60 {
                tx.execute(
                    "UPDATE offline_package_leases \
                     SET last_access_at = ?2, expires_at = ?3 WHERE token_hash = ?1",
                    params![hash, now, renewed_expires_at],
                )?;
                tx.execute(
                    "UPDATE offline_packages SET last_access_at = ?2, expires_at = ?3, \
                     updated_at = ?2 WHERE id = ?1",
                    params![package.id, now, renewed_expires_at],
                )?;
                package.last_access_at = now;
                package.updated_at = now;
                package.expires_at = renewed_expires_at;
            }
            tx.commit()?;
            Ok(Some(package))
        })
        .await
    }

    async fn mark_offline_package_ready(
        &self,
        package_id: &str,
        node_id: &str,
        recipe_hash: &str,
        actual_bytes: i64,
        duration_ms: i64,
    ) -> Result<bool, StoreError> {
        let (id, node, hash) = (
            package_id.to_owned(),
            node_id.to_owned(),
            recipe_hash.to_owned(),
        );
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'ready', phase = 'ready', \
                 progress_millis = 1000, recipe_hash = ?3, actual_bytes = ?4, \
                 duration_ms = ?5, error_code = NULL, error_message = NULL, \
                 updated_at = unixepoch(), last_access_at = unixepoch() \
                 WHERE id = ?1 AND node_id = ?2 AND state IN ('queued', 'preparing')",
                params![id, node, hash, actual_bytes, duration_ms],
            )? > 0)
        })
        .await
    }

    async fn delete_offline_package(
        &self,
        package_id: &str,
        user_id: i64,
    ) -> Result<bool, StoreError> {
        let id = package_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "DELETE FROM offline_packages WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
            )? > 0)
        })
        .await
    }

    async fn expire_offline_packages(&self, now: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM offline_packages WHERE expires_at <= ?1", [now])? as u64)
        })
        .await
    }

    // --- Node removal (`CLUSTERING-PLAN.md` §6.7) -------------------------
    //
    // A single-node SQLite install has no membership, so it has no node to
    // remove and no survivor to re-home work onto. These answer honestly
    // rather than pretending: nothing is unresolved because nothing can be
    // removed, no probe can be asked because there is nobody to ask, and a
    // removal plan is a programming error here rather than a policy outcome.
    //
    // No SQLite table backs any of this. Adding one would change the durable
    // schema and the import parity plan for a path that cannot run, and §6.7
    // requires single-node offline behavior to stay exactly as it is.

    async fn unresolved_offline_packages(
        &self,
        _node_id: &str,
    ) -> Result<Vec<OfflinePackage>, StoreError> {
        Ok(Vec::new())
    }

    async fn offline_transfers_in_flight(
        &self,
        _node_id: &str,
        _now: i64,
        _active_since: i64,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }

    async fn request_offline_source_probes(
        &self,
        _package_ids: &[String],
        _node_ids: &[String],
        _now: i64,
    ) -> Result<u64, StoreError> {
        Ok(0)
    }

    async fn pending_offline_source_probes(
        &self,
        _node_id: &str,
        _requested_since: i64,
    ) -> Result<Vec<OfflinePackage>, StoreError> {
        Ok(Vec::new())
    }

    async fn answer_offline_source_probe(
        &self,
        _package_id: &str,
        _node_id: &str,
        _readable: bool,
        _now: i64,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn outstanding_offline_source_probes(
        &self,
        _requested_at: i64,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }

    async fn verified_offline_source_nodes(
        &self,
        _package_id: &str,
        _requested_since: i64,
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    async fn resolve_offline_packages_for_removal(
        &self,
        _node_id: &str,
        plan: &[OfflineRemovalPlanEntry],
        _now: i64,
    ) -> Result<OfflineRemovalReport, StoreError> {
        if plan.is_empty() {
            return Ok(OfflineRemovalReport::default());
        }
        Err(StoreError::Database(
            "a single-node SQLite install has no node-removal path to resolve offline work for"
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{PACKAGE_COLS, PACKAGE_COL_COUNT};
    use crate::domain::{NewOfflinePackage, OfflineCreateOutcome, OfflineLeaseOutcome};
    use crate::store::{OfflinePackageStore, SqliteStore, UserStore};

    #[test]
    fn package_column_count_tracks_the_positional_mapper() {
        assert_eq!(PACKAGE_COLS.split(", ").count(), PACKAGE_COL_COUNT);
    }

    fn request_for(request_id: &str, user_id: i64) -> NewOfflinePackage {
        NewOfflinePackage {
            id: format!("pkg-{request_id}"),
            request_id: request_id.to_owned(),
            user_id,
            file_id: 42,
            node_id: "node-a".to_owned(),
            source_path: "/media/movie.mkv".to_owned(),
            source_size: 1_000,
            source_mtime: 7,
            effective_rate_control: "vbr".to_owned(),
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: Some(1),
            audio_offset_ms: 125,
            subtitle_index: Some(2),
            subtitle_language: Some("en".to_owned()),
            subtitle_mode: "native".to_owned(),
            estimated_bytes: 400,
            reserved_bytes: 500,
            expires_at: 10_000,
        }
    }

    fn request(request_id: &str) -> NewOfflinePackage {
        request_for(request_id, 1)
    }

    async fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().expect("store");
        store.create_user("paul", "hash", true).await.expect("user");
        store
    }

    #[tokio::test]
    async fn create_is_idempotent_and_detects_choice_conflicts() {
        let store = store().await;
        let mut requested = request("one");
        requested.effective_rate_control = "qvbr:21".to_owned();
        let created = match store
            .create_offline_package(&requested, 10, 1_000, 2_000)
            .await
            .expect("create")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(created.effective_rate_control, "qvbr:21");
        assert!(matches!(
            store
                .create_offline_package(&requested, 10, 1_000, 2_000)
                .await
                .expect("retry"),
            OfflineCreateOutcome::Existing(_)
        ));

        let mut changed = requested.clone();
        changed.effective_rate_control = "vbr".to_owned();
        let OfflineCreateOutcome::Existing(existing) = store
            .create_offline_package(&changed, 10, 1_000, 2_000)
            .await
            .expect("server-policy retry")
        else {
            panic!("a server-derived rate-control change broke request idempotency");
        };
        assert_eq!(
            existing.effective_rate_control, "qvbr:21",
            "the first accepted request owns the immutable package snapshot"
        );

        let mut invalid = request("invalid");
        invalid.effective_rate_control = "qvbr:256".to_owned();
        let error = store
            .create_offline_package(&invalid, 10, 1_000, 2_000)
            .await
            .expect_err("out-of-range snapshots must fail before insertion");
        assert!(error.to_string().contains("invalid offline effective"));
    }

    #[tokio::test]
    async fn quota_check_and_insert_share_the_transaction() {
        let store = store().await;
        store
            .create_offline_package(&request("one"), 10, 700, 2_000)
            .await
            .expect("first");
        assert_eq!(
            store
                .create_offline_package(&request("two"), 10, 700, 2_000)
                .await
                .expect("second"),
            OfflineCreateOutcome::ByteLimit {
                used: 500,
                limit: 700
            }
        );
    }


    #[tokio::test]
    async fn single_node_does_not_reject_packages_as_node_removed() {
        // Single-node SQLite has no removal path and no `cluster_nodes`
        // table. The `NodeIsTombstone` variant must never be returned.
        let store = store().await;
        let package = match store
            .create_offline_package(&request("single-node"), 10, 1_000, 2_000)
            .await
            .expect("create")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("single-node create returned {other:?}"),
        };
        assert_eq!(package.request_id, "single-node");
    }

    #[tokio::test]
    async fn zero_quotas_disable_offline_admission() {
        let store = store().await;
        assert_eq!(
            store
                .create_offline_package(&request("rows-off"), 0, 1_000, 2_000)
                .await
                .expect("row limit"),
            OfflineCreateOutcome::RowLimit { limit: 0 }
        );
        assert_eq!(
            store
                .create_offline_package(&request("user-bytes-off"), 10, 0, 2_000)
                .await
                .expect("user byte limit"),
            OfflineCreateOutcome::ByteLimit { used: 0, limit: 0 }
        );
        assert_eq!(
            store
                .create_offline_package(&request("global-bytes-off"), 10, 1_000, 0)
                .await
                .expect("global byte limit"),
            OfflineCreateOutcome::GlobalByteLimit { used: 0, limit: 0 }
        );
    }

    #[tokio::test]
    async fn lease_is_hashed_stable_and_renewable() {
        let store = store().await;
        let package = match store
            .create_offline_package(&request("one"), 10, 1_000, 2_000)
            .await
            .expect("package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert!(store
            .mark_offline_package_ready(&package.id, "node-a", "recipe", 350, 90_000)
            .await
            .expect("ready"));

        let hash = "a".repeat(64);
        assert!(matches!(
            store
                .put_offline_lease(&package.id, 1, &hash, 200)
                .await
                .expect("create lease"),
            OfflineLeaseOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .put_offline_lease(&package.id, 1, &hash, 300)
                .await
                .expect("renew lease"),
            OfflineLeaseOutcome::Renewed(_)
        ));
        assert_eq!(
            store
                .put_offline_lease(&package.id, 1, &"b".repeat(64), 300)
                .await
                .expect("rotate"),
            OfflineLeaseOutcome::TokenConflict
        );
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE offline_package_leases SET last_access_at = 200, expires_at = 1000",
                    [],
                )?;
                conn.execute(
                    "UPDATE offline_packages SET last_access_at = 200, expires_at = 1000",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("fix test clock");

        let served = store
            .offline_package_for_lease(&hash, 250, 400)
            .await
            .expect("serve")
            .expect("authorized");
        assert_eq!(served.id, package.id);
        assert_eq!(
            served.expires_at, 1000,
            "a touch inside a minute is read-only"
        );
        let renewed = store
            .offline_package_for_lease(&hash, 261, 1500)
            .await
            .expect("serve after throttle window")
            .expect("authorized");
        assert_eq!(renewed.expires_at, 1500);
        assert!(store
            .offline_package_for_lease(&"c".repeat(64), 250, 400)
            .await
            .expect("wrong token")
            .is_none());
    }

    #[tokio::test]
    async fn ownership_is_part_of_every_package_lookup_and_delete() {
        let store = store().await;
        let package = match store
            .create_offline_package(&request("one"), 10, 1_000, 2_000)
            .await
            .expect("package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert!(store
            .offline_package_for_user(&package.id, 2)
            .await
            .expect("lookup")
            .is_none());
        assert!(!store
            .delete_offline_package(&package.id, 2)
            .await
            .expect("delete"));
        assert!(store
            .offline_package_for_user(&package.id, 1)
            .await
            .expect("owner lookup")
            .is_some());
    }

    #[tokio::test]
    async fn durable_queue_recovers_and_progress_never_moves_backward() {
        let store = store().await;
        let package = match store
            .create_offline_package(&request("one"), 10, 1_000, 2_000)
            .await
            .expect("package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        let claimed = store
            .claim_next_offline_package("node-a")
            .await
            .expect("claim")
            .expect("queued package");
        assert_eq!(claimed.id, package.id);
        assert_eq!(claimed.state, "preparing");
        assert_eq!(claimed.effective_rate_control, "vbr");
        store
            .update_offline_progress(&package.id, "node-a", "transcoding", 600)
            .await
            .expect("progress");
        store
            .update_offline_progress(&package.id, "node-a", "transcoding", 200)
            .await
            .expect("stale progress");
        let current = store
            .offline_package_for_user(&package.id, 1)
            .await
            .expect("lookup")
            .expect("package");
        assert_eq!(current.progress_millis, 600);

        assert_eq!(
            store
                .reset_interrupted_offline_packages("node-a")
                .await
                .expect("reset"),
            1
        );
        assert_eq!(
            store
                .offline_package_for_user(&package.id, 1)
                .await
                .expect("lookup")
                .expect("package")
                .state,
            "queued"
        );
    }

    #[tokio::test]
    async fn queued_users_take_turns_even_when_one_arrived_first() {
        let store = store().await;
        store
            .create_user("alex", "hash", false)
            .await
            .expect("user");
        for request in [
            request_for("a-one", 1),
            request_for("a-two", 1),
            request_for("b-one", 2),
            request_for("b-two", 2),
        ] {
            assert!(matches!(
                store
                    .create_offline_package(&request, 10, 10_000, 20_000)
                    .await
                    .expect("create"),
                OfflineCreateOutcome::Created(_)
            ));
        }

        let mut owners = Vec::new();
        for turn in 0..4 {
            let package = store
                .claim_next_offline_package("node-a")
                .await
                .expect("claim")
                .expect("queued package");
            owners.push(package.user_id);
            assert!(store
                .mark_offline_package_ready(
                    &package.id,
                    "node-a",
                    &format!("recipe-{turn}"),
                    400,
                    90_000,
                )
                .await
                .expect("ready"));
        }

        assert_eq!(owners, [1, 2, 1, 2]);
    }

    #[tokio::test]
    async fn activity_and_metrics_use_fixed_state_aggregates() {
        let store = store().await;
        let preparing = match store
            .create_offline_package(&request("preparing"), 10, 10_000, 20_000)
            .await
            .expect("preparing package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(
            store
                .claim_next_offline_package("node-a")
                .await
                .expect("claim")
                .expect("package")
                .id,
            preparing.id
        );

        let ready = match store
            .create_offline_package(&request("ready"), 10, 10_000, 20_000)
            .await
            .expect("ready package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert!(store
            .mark_offline_package_ready(&ready.id, "node-a", "recipe", 350, 90_000)
            .await
            .expect("ready"));
        assert!(matches!(
            store
                .put_offline_lease(&ready.id, 1, &"a".repeat(64), 1_000)
                .await
                .expect("lease"),
            OfflineLeaseOutcome::Created(_)
        ));

        let failed = match store
            .create_offline_package(&request("failed"), 10, 10_000, 20_000)
            .await
            .expect("failed package")
        {
            OfflineCreateOutcome::Created(package) => package,
            other => panic!("unexpected {other:?}"),
        };
        assert!(store
            .fail_offline_package(
                &failed.id,
                "node-a",
                "transcoding",
                "encoder_failed",
                "failed"
            )
            .await
            .expect("fail"));
        store
            .with_conn(|conn| {
                conn.execute("UPDATE offline_package_leases SET last_access_at = 480", [])?;
                Ok(())
            })
            .await
            .expect("test clock");

        let activity = store
            .offline_activity_packages("node-a", 500, 450, 50)
            .await
            .expect("activity");
        assert_eq!(activity.len(), 2, "failed packages are not live work");
        assert_eq!(activity[0].package.id, preparing.id);
        assert!(!activity[0].lease_active);
        assert_eq!(activity[1].package.id, ready.id);
        assert!(activity[1].lease_active);

        let stats = store
            .offline_package_stats("node-a", 500)
            .await
            .expect("stats");
        assert_eq!(stats.queued, 0);
        assert_eq!(stats.preparing, 1);
        assert_eq!(stats.ready, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.preparing_bytes, 500);
        assert_eq!(stats.ready_bytes, 350);
        assert_eq!(stats.failed_bytes, 500);
        assert_eq!(stats.active_leases, 1);
        assert_eq!(stats.pinned_bytes, 0, "no cache location was seeded");

        store
            .with_conn(|conn| {
                conn.execute("UPDATE offline_package_leases SET last_access_at = 400", [])?;
                Ok(())
            })
            .await
            .expect("age lease");
        let activity = store
            .offline_activity_packages("node-a", 500, 450, 50)
            .await
            .expect("aged activity");
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].package.id, preparing.id);
    }
}

//! Durable app-managed offline package requests and transfer leases.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::SqliteStore;
use crate::domain::{
    NewOfflinePackage, OfflineCreateOutcome, OfflineLease, OfflineLeaseOutcome, OfflinePackage,
};
use crate::error::StoreError;
use crate::store::OfflinePackageStore;

const PACKAGE_COLS: &str = "id, request_id, user_id, file_id, node_id, source_path, \
    source_size, source_mtime, recipe_hash, target_height, audio_index, audio_offset_ms, \
    output_width, output_height, subtitle_index, subtitle_mode, state, phase, progress_millis, estimated_bytes, \
    reserved_bytes, actual_bytes, duration_ms, error_code, error_message, created_at, updated_at, \
    last_access_at, expires_at";

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
        target_height: row.get(9)?,
        audio_index: row.get(10)?,
        audio_offset_ms: row.get(11)?,
        output_width: row.get(12)?,
        output_height: row.get(13)?,
        subtitle_index: row.get(14)?,
        subtitle_mode: row.get(15)?,
        state: row.get(16)?,
        phase: row.get(17)?,
        progress_millis: row.get(18)?,
        estimated_bytes: row.get(19)?,
        reserved_bytes: row.get(20)?,
        actual_bytes: row.get(21)?,
        duration_ms: row.get(22)?,
        error_code: row.get(23)?,
        error_message: row.get(24)?,
        created_at: row.get(25)?,
        updated_at: row.get(26)?,
        last_access_at: row.get(27)?,
        expires_at: row.get(28)?,
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
        && existing.subtitle_mode == requested.subtitle_mode
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
            if max_bytes_per_user <= 0
                || used.saturating_add(requested.reserved_bytes) > max_bytes_per_user
            {
                tx.commit()?;
                return Ok(OfflineCreateOutcome::ByteLimit {
                    used,
                    limit: max_bytes_per_user.max(0),
                });
            }

            let global_used: i64 = tx.query_row(
                "SELECT COALESCE(SUM(COALESCE(actual_bytes, reserved_bytes)), 0) \
                 FROM offline_packages WHERE state IN ('queued', 'preparing', 'ready')",
                [],
                |row| row.get(0),
            )?;
            if max_bytes_global <= 0
                || global_used.saturating_add(requested.reserved_bytes) > max_bytes_global
            {
                tx.commit()?;
                return Ok(OfflineCreateOutcome::GlobalByteLimit {
                    used: global_used,
                    limit: max_bytes_global.max(0),
                });
            }

            tx.execute(
                "INSERT INTO offline_packages (
                    id, request_id, user_id, file_id, node_id, source_path,
                    source_size, source_mtime, target_height, audio_index,
                    audio_offset_ms, output_width, output_height, subtitle_index,
                    subtitle_mode, state, phase, estimated_bytes, reserved_bytes,
                    expires_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, 'queued', 'waiting_for_encoder', ?16, ?17, ?18
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
                    requested.target_height,
                    requested.audio_index,
                    requested.audio_offset_ms,
                    requested.output_width,
                    requested.output_height,
                    requested.subtitle_index,
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
                    "SELECT id FROM offline_packages \
                     WHERE node_id = ?1 AND state = 'queued' \
                     ORDER BY created_at, id LIMIT 1",
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

    async fn requeue_offline_package(&self, package_id: &str) -> Result<bool, StoreError> {
        let id = package_id.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'queued', \
                 phase = 'waiting_for_encoder', updated_at = unixepoch() \
                 WHERE id = ?1 AND state = 'preparing'",
                [id],
            )? > 0)
        })
        .await
    }

    async fn update_offline_progress(
        &self,
        package_id: &str,
        phase: &str,
        progress_millis: i64,
    ) -> Result<bool, StoreError> {
        let (id, phase) = (package_id.to_owned(), phase.to_owned());
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET phase = ?2, \
                 progress_millis = MAX(progress_millis, ?3), \
                 updated_at = unixepoch() WHERE id = ?1 AND state = 'preparing'",
                params![id, phase, progress_millis.clamp(0, 999)],
            )? > 0)
        })
        .await
    }

    async fn fail_offline_package(
        &self,
        package_id: &str,
        phase: &str,
        code: &str,
        message: &str,
    ) -> Result<bool, StoreError> {
        let (id, phase, code, message) = (
            package_id.to_owned(),
            phase.to_owned(),
            code.to_owned(),
            message.to_owned(),
        );
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'failed', phase = ?2, \
                 error_code = ?3, error_message = ?4, updated_at = unixepoch() \
                 WHERE id = ?1 AND state IN ('queued', 'preparing')",
                params![id, phase, code, message],
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
            tx.commit()?;
            Ok(Some(package))
        })
        .await
    }

    async fn mark_offline_package_ready(
        &self,
        package_id: &str,
        recipe_hash: &str,
        actual_bytes: i64,
        duration_ms: i64,
    ) -> Result<bool, StoreError> {
        let (id, hash) = (package_id.to_owned(), recipe_hash.to_owned());
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE offline_packages SET state = 'ready', phase = 'ready', \
                 progress_millis = 1000, recipe_hash = ?2, actual_bytes = ?3, \
                 duration_ms = ?4, error_code = NULL, error_message = NULL, \
                 updated_at = unixepoch(), last_access_at = unixepoch() \
                 WHERE id = ?1 AND state IN ('queued', 'preparing')",
                params![id, hash, actual_bytes, duration_ms],
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
}

#[cfg(test)]
mod tests {
    use crate::domain::{NewOfflinePackage, OfflineCreateOutcome, OfflineLeaseOutcome};
    use crate::store::{OfflinePackageStore, SqliteStore, UserStore};

    fn request(request_id: &str) -> NewOfflinePackage {
        NewOfflinePackage {
            id: format!("pkg-{request_id}"),
            request_id: request_id.to_owned(),
            user_id: 1,
            file_id: 42,
            node_id: "node-a".to_owned(),
            source_path: "/media/movie.mkv".to_owned(),
            source_size: 1_000,
            source_mtime: 7,
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: Some(1),
            audio_offset_ms: 125,
            subtitle_index: Some(2),
            subtitle_mode: "native".to_owned(),
            estimated_bytes: 400,
            reserved_bytes: 500,
            expires_at: 10_000,
        }
    }

    async fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().expect("store");
        store.create_user("paul", "hash", true).await.expect("user");
        store
    }

    #[tokio::test]
    async fn create_is_idempotent_and_detects_choice_conflicts() {
        let store = store().await;
        let requested = request("one");
        assert!(matches!(
            store
                .create_offline_package(&requested, 10, 1_000, 2_000)
                .await
                .expect("create"),
            OfflineCreateOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .create_offline_package(&requested, 10, 1_000, 2_000)
                .await
                .expect("retry"),
            OfflineCreateOutcome::Existing(_)
        ));

        let mut changed = requested;
        changed.target_height = 1080;
        assert_eq!(
            store
                .create_offline_package(&changed, 10, 1_000, 2_000)
                .await
                .expect("conflict"),
            OfflineCreateOutcome::RequestConflict
        );
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
            .mark_offline_package_ready(&package.id, "recipe", 350, 90_000)
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

        let served = store
            .offline_package_for_lease(&hash, 250, 400)
            .await
            .expect("serve")
            .expect("authorized");
        assert_eq!(served.id, package.id);
        assert_eq!(served.expires_at, 400);
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
        store
            .update_offline_progress(&package.id, "transcoding", 600)
            .await
            .expect("progress");
        store
            .update_offline_progress(&package.id, "transcoding", 200)
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
}

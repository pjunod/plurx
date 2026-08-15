//! Deterministic import of a prepared legacy SQLite backup into fresh Hiqlite.
//!
//! The coordinator owns the crash boundary: this method may leave a partial
//! `hiqlite.incoming` cluster after an error, and the next attempt removes that
//! directory before starting again. The importer therefore refuses a target
//! with any application rows instead of trying to resume or merge state.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use hiqlite::macros::params;
use hiqlite::{Param, Row};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use super::hiqlite::{database_error, HiqliteAuthStore};
use super::{keys, MediaStore, SettingsStore, SQLITE_SCHEMA_VERSION};
use crate::error::StoreError;
use crate::secrets::SealedSecret;

const MINIMUM_IMPORT_SCHEMA_VERSION: i64 = 14;
/// Rows per import `txn`, bounded by the production WAL rather than throughput.
///
/// One chunk becomes one Raft transaction, and `hiqlite` panics when a single
/// transaction exceeds the WAL payload capacity: 2,097,118 bytes under the
/// production `wal_size: 2 * 1024 * 1024`
/// (`crates/plurx-core/src/cluster/migration.rs`). `files.probe_json` holds
/// whole ffprobe documents, so a row-count bound is not a byte bound — 64 rows
/// of a real library serialized to 3,137,236 bytes and crashed activation into
/// unreplicated SQLite recovery.
///
/// 16 keeps the worst observed library (~55 KB/row) well inside that cap, but
/// the bound is still only a row count: 16 adjacent rows averaging more than
/// ~131,069 bytes each still overflow. Raising this for import throughput
/// re-opens that crash; a byte-measured chunk builder is the real fix.
const IMPORT_CHUNK_ROWS: i64 = 16;
/// Rows per read-only parity page. Independent of [`IMPORT_CHUNK_ROWS`]: these
/// pages are consistent reads that never enter a Raft transaction, so the WAL
/// payload cap does not apply to them.
const PARITY_PAGE_ROWS: i64 = 64;

/// Ordered evidence that one durable SQLite table exactly matches Hiqlite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteImportTableDigest {
    pub table: String,
    pub row_count: u64,
    pub sha256: String,
}

/// Evidence emitted only after every imported table and derived index passes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteImportReport {
    pub source_schema_version: i64,
    pub backup_sha256: String,
    pub imported_rows: u64,
    pub search_rows: u64,
    pub tables: Vec<SqliteImportTableDigest>,
}

#[derive(Clone, Copy)]
struct TablePlan {
    name: &'static str,
    columns: &'static [&'static str],
    order_by: &'static str,
    minimum_schema: i64,
    import_filter: Option<ImportFilter>,
    /// Columns holding a sealed credential, which import refuses to carry in
    /// the clear.
    ///
    /// Declared per table rather than checked by name at the one call site so
    /// that a future table with a credential column cannot quietly skip the
    /// gate: adding a `TablePlan` does not compile until this is answered.
    /// Empty for every table whose columns are ordinary catalogue data.
    sealed_columns: &'static [&'static str],
    parent_first: bool,
}

#[derive(Clone, Copy)]
enum ImportFilter {
    ExcludeInstanceId,
}

#[derive(Debug, PartialEq, Eq)]
struct OrderedRowsDigest {
    row_count: u64,
    sha256: String,
}

struct OrderedRowsHasher {
    hasher: Sha256,
    row_count: u64,
}

impl OrderedRowsHasher {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"[");
        Self {
            hasher,
            row_count: 0,
        }
    }

    fn push(&mut self, row: &str) -> Result<(), StoreError> {
        if self.row_count > 0 {
            self.hasher.update(b",");
        }
        let encoded = serde_json::to_vec(row)
            .map_err(|error| import_error(format!("serializing table parity row: {error}")))?;
        self.hasher.update(encoded);
        self.row_count = self
            .row_count
            .checked_add(1)
            .ok_or_else(|| import_error("table parity row count overflow"))?;
        Ok(())
    }

    fn finish(mut self) -> OrderedRowsDigest {
        self.hasher.update(b"]");
        OrderedRowsDigest {
            row_count: self.row_count,
            sha256: hex::encode(self.hasher.finalize()),
        }
    }
}

struct SourceMetadata {
    backup_sha256: String,
    schema_version: i64,
    instance_id: String,
    instance_updated_at: i64,
}

enum SourceChunk {
    Offset(i64),
    ItemIds(Vec<i64>),
}

enum SourceRequest {
    Count {
        table: TablePlan,
        for_import: bool,
        reply: oneshot::Sender<Result<i64, StoreError>>,
    },
    ParentFirstItemIds {
        reply: oneshot::Sender<Result<Vec<i64>, StoreError>>,
    },
    ImportChunk {
        table: TablePlan,
        schema_version: i64,
        chunk: SourceChunk,
        reply: oneshot::Sender<Result<Vec<hiqlite::Params>, StoreError>>,
    },
    Digest {
        table: TablePlan,
        schema_version: i64,
        reply: oneshot::Sender<Result<OrderedRowsDigest, StoreError>>,
    },
    UnsealedCredentialRows {
        table: TablePlan,
        reply: oneshot::Sender<Result<u64, StoreError>>,
    },
    #[cfg(test)]
    Pause {
        duration: std::time::Duration,
        reply: oneshot::Sender<()>,
    },
}

/// One blocking worker owns the immutable SQLite connection for the import.
///
/// Reopening the backup for every [`IMPORT_CHUNK_ROWS`] chunk would avoid
/// blocking Tokio but turn a large catalogue into thousands of connection
/// setups. This actor
/// keeps one read-only connection and sends row data back in bounded chunks.
/// The one deliberate O(items) reply is the parent-first id ordering used to
/// avoid rebuilding and sorting the full item tree for every chunk.
struct SourceReader {
    requests: mpsc::Sender<SourceRequest>,
}

impl SourceReader {
    async fn open(
        backup_path: &Path,
        expected_sha256: &str,
        expected_schema_version: i64,
    ) -> Result<(Self, SourceMetadata), StoreError> {
        let backup_path = backup_path.to_owned();
        let expected_sha256 = expected_sha256.to_owned();
        let (requests, mut receiver) = mpsc::channel::<SourceRequest>(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let opened =
                open_validated_source(&backup_path, &expected_sha256, expected_schema_version);
            let (source, metadata) = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(metadata)).is_err() {
                return;
            }

            while let Some(request) = receiver.blocking_recv() {
                match request {
                    SourceRequest::Count {
                        table,
                        for_import,
                        reply,
                    } => {
                        let _ = reply.send(source_count(&source, table, for_import));
                    }
                    SourceRequest::ParentFirstItemIds { reply } => {
                        let _ = reply.send(parent_first_item_ids(&source));
                    }
                    SourceRequest::ImportChunk {
                        table,
                        schema_version,
                        chunk,
                        reply,
                    } => {
                        let result = match chunk {
                            SourceChunk::Offset(offset) => {
                                let sql = import_select_sql(table, schema_version);
                                read_import_chunk(&source, table, &sql, offset)
                            }
                            SourceChunk::ItemIds(ids) => {
                                read_item_import_chunk(&source, schema_version, table, &ids)
                            }
                        };
                        let _ = reply.send(result);
                    }
                    SourceRequest::Digest {
                        table,
                        schema_version,
                        reply,
                    } => {
                        let _ = reply.send(source_digest(&source, schema_version, table));
                    }
                    SourceRequest::UnsealedCredentialRows { table, reply } => {
                        let _ = reply.send(unsealed_credential_rows(&source, table));
                    }
                    #[cfg(test)]
                    SourceRequest::Pause { duration, reply } => {
                        std::thread::sleep(duration);
                        let _ = reply.send(());
                    }
                }
            }
        });

        let metadata = ready_rx.await.map_err(|_| {
            import_error("SQLite source worker stopped before reporting initialization")
        })??;
        Ok((Self { requests }, metadata))
    }

    async fn count(&self, table: TablePlan, for_import: bool) -> Result<i64, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::Count {
            table,
            for_import,
            reply,
        })
        .await?;
        receive_source(response).await
    }

    async fn parent_first_item_ids(&self) -> Result<Vec<i64>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::ParentFirstItemIds { reply })
            .await?;
        receive_source(response).await
    }

    async fn import_chunk(
        &self,
        table: TablePlan,
        schema_version: i64,
        chunk: SourceChunk,
    ) -> Result<Vec<hiqlite::Params>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::ImportChunk {
            table,
            schema_version,
            chunk,
            reply,
        })
        .await?;
        receive_source(response).await
    }

    async fn digest(
        &self,
        table: TablePlan,
        schema_version: i64,
    ) -> Result<OrderedRowsDigest, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::Digest {
            table,
            schema_version,
            reply,
        })
        .await?;
        receive_source(response).await
    }

    async fn unsealed_credential_rows(&self, table: TablePlan) -> Result<u64, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::UnsealedCredentialRows { table, reply })
            .await?;
        receive_source(response).await
    }

    async fn send(&self, request: SourceRequest) -> Result<(), StoreError> {
        self.requests
            .send(request)
            .await
            .map_err(|_| import_error("SQLite source worker stopped during import"))
    }

    #[cfg(test)]
    async fn pause(&self, duration: std::time::Duration) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(SourceRequest::Pause { duration, reply }).await?;
        response
            .await
            .map_err(|_| import_error("SQLite source worker stopped during validation pause"))
    }
}

async fn receive_source<T>(
    response: oneshot::Receiver<Result<T, StoreError>>,
) -> Result<T, StoreError> {
    response
        .await
        .map_err(|_| import_error("SQLite source worker stopped during import"))?
}

const TABLES: &[TablePlan] = &[
    TablePlan {
        name: "settings",
        columns: &["key", "value", "updated_at"],
        order_by: "key",
        minimum_schema: 1,
        import_filter: Some(ImportFilter::ExcludeInstanceId),
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "users",
        columns: &["id", "username", "password_hash", "is_admin", "created_at"],
        order_by: "id",
        minimum_schema: 2,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "tokens",
        columns: &[
            "token_hash",
            "user_id",
            "device",
            "created_at",
            "last_seen_at",
        ],
        order_by: "token_hash",
        minimum_schema: 2,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "api_keys",
        columns: &[
            "id",
            "name",
            "key_hash",
            "scopes",
            "created_at",
            "last_used_at",
            "disabled",
        ],
        order_by: "id",
        minimum_schema: 8,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "libraries",
        columns: &[
            "id",
            "name",
            "kind",
            "paths",
            "anime",
            "created_at",
            "scan_interval_mins",
            "refresh_interval_mins",
            "last_scan_at",
            "last_refresh_at",
        ],
        order_by: "id",
        minimum_schema: 7,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "items",
        columns: &[
            "id",
            "library_id",
            "kind",
            "parent_id",
            "title",
            "sort_title",
            "year",
            "overview",
            "tmdb_id",
            "imdb_id",
            "season_number",
            "episode_number",
            "air_date",
            "runtime_ms",
            "poster_path",
            "backdrop_path",
            "added_at",
            "updated_at",
            "recorded_at",
            "tags",
            "nfo_seeded_at",
            "metadata_at",
            "artwork_attempted_at",
            "artwork_error",
            "genres",
        ],
        order_by: "id",
        minimum_schema: 13,
        import_filter: None,
        sealed_columns: &[],
        parent_first: true,
    },
    TablePlan {
        name: "files",
        columns: &[
            "id",
            "item_id",
            "path",
            "size",
            "mtime",
            "duration_ms",
            "container",
            "video_codec",
            "video_profile",
            "width",
            "height",
            "bit_depth",
            "hdr",
            "bitrate",
            "audio_streams",
            "subtitle_streams",
            "probe_json",
            "scanned_at",
            "hdr_format",
            "audio_offset_ms",
        ],
        order_by: "id",
        minimum_schema: 5,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "watch_state",
        columns: &[
            "user_id",
            "item_id",
            "position_ms",
            "duration_ms",
            "watched",
            "updated_at",
        ],
        order_by: "user_id, item_id",
        minimum_schema: 2,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "trakt_auth",
        columns: &[
            "user_id",
            "access_token",
            "refresh_token",
            "expires_at",
            "trakt_username",
            "connected_at",
            "last_sync_at",
            "last_activities",
        ],
        order_by: "user_id",
        minimum_schema: 5,
        import_filter: None,
        sealed_columns: &["access_token", "refresh_token"],
        parent_first: false,
    },
    TablePlan {
        name: "watched_outbox",
        columns: &[
            "id",
            "payload",
            "attempts",
            "last_error",
            "status",
            "next_at",
            "created_at",
            "updated_at",
            "claim_until",
        ],
        order_by: "id",
        minimum_schema: 10,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "transcode_cache_recipes",
        columns: &["recipe_hash", "file_id", "recipe_version", "created_at"],
        order_by: "recipe_hash",
        minimum_schema: 11,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "transcode_cache_locations",
        columns: &[
            "recipe_hash",
            "node_id",
            "storage_class",
            "relative_dir",
            "bytes",
            "complete",
            "last_used_at",
            "last_seen_at",
        ],
        order_by: "recipe_hash, node_id, storage_class",
        minimum_schema: 11,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "offline_packages",
        columns: &[
            "id",
            "request_id",
            "user_id",
            "file_id",
            "node_id",
            "source_path",
            "source_size",
            "source_mtime",
            "recipe_hash",
            "effective_rate_control",
            "target_height",
            "output_width",
            "output_height",
            "audio_index",
            "audio_offset_ms",
            "subtitle_index",
            "subtitle_language",
            "subtitle_mode",
            "state",
            "phase",
            "progress_millis",
            "estimated_bytes",
            "reserved_bytes",
            "actual_bytes",
            "duration_ms",
            "error_code",
            "error_message",
            "created_at",
            "updated_at",
            "last_access_at",
            "expires_at",
        ],
        order_by: "id",
        minimum_schema: 14,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "offline_package_leases",
        columns: &[
            "token_hash",
            "package_id",
            "created_at",
            "last_access_at",
            "expires_at",
        ],
        order_by: "token_hash",
        minimum_schema: 14,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "library_roots",
        columns: &["library_id", "fingerprint"],
        order_by: "library_id",
        minimum_schema: 15,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "scan_reconcile_guards",
        columns: &["library_id"],
        order_by: "library_id",
        minimum_schema: 15,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
    TablePlan {
        name: "scan_reconcile_items",
        columns: &["library_id", "item_id"],
        order_by: "library_id, item_id",
        minimum_schema: 15,
        import_filter: None,
        sealed_columns: &[],
        parent_first: false,
    },
];

impl HiqliteAuthStore {
    /// Import one immutable, content-addressed SQLite backup into a fresh
    /// Hiqlite target and prove exact row parity before returning evidence.
    ///
    /// Schema v14 is the oldest supported clustering source. v14 lacks the
    /// scan-reconciliation tables and the outbox claim deadline; those facts
    /// are imported as empty tables and a zero deadline respectively. v17's
    /// playback events are node-local telemetry and intentionally absent from
    /// this mapping.
    pub async fn import_sqlite_backup(
        &self,
        backup_path: &Path,
        expected_sha256: &str,
        expected_schema_version: i64,
    ) -> Result<SqliteImportReport, StoreError> {
        self.import_sqlite_backup_inner(
            backup_path,
            expected_sha256,
            expected_schema_version,
            false,
        )
        .await
    }

    /// Validation-only fault seam proving the per-table parity comparison is
    /// an enforced gate rather than report decoration.
    #[doc(hidden)]
    pub async fn validation_import_sqlite_backup_with_parity_fault(
        &self,
        backup_path: &Path,
        expected_sha256: &str,
        expected_schema_version: i64,
    ) -> Result<SqliteImportReport, StoreError> {
        self.import_sqlite_backup_inner(backup_path, expected_sha256, expected_schema_version, true)
            .await
    }

    async fn import_sqlite_backup_inner(
        &self,
        backup_path: &Path,
        expected_sha256: &str,
        expected_schema_version: i64,
        inject_parity_fault: bool,
    ) -> Result<SqliteImportReport, StoreError> {
        let (source, metadata) =
            SourceReader::open(backup_path, expected_sha256, expected_schema_version).await?;
        let schema_version = metadata.schema_version;
        self.refuse_unsealed_source_credentials(&source, schema_version)
            .await?;
        self.verify_empty_import_target().await?;
        self.import_instance_setting(&metadata).await?;

        for table in TABLES {
            self.import_table(&source, schema_version, *table).await?;
        }

        let search_rows = self.rebuild_search_index().await?;
        let item_count = self.target_count("items", None).await?;
        if search_rows != item_count {
            return Err(import_error(format!(
                "rebuilt items_fts has {search_rows} rows, expected {item_count}; discard the incoming target"
            )));
        }
        self.verify_target_foreign_keys().await?;
        if inject_parity_fault {
            let changed = self
                .client()
                .execute(
                    "UPDATE settings SET value = $1 WHERE key = $2",
                    params!("corrupted-after-import", "migration.fixture"),
                )
                .await?;
            if changed != 1 {
                return Err(import_error(format!(
                    "validation parity fault changed {changed} rows"
                )));
            }
        }

        let mut tables = Vec::with_capacity(TABLES.len());
        let mut imported_rows = 0_u64;
        for table in TABLES {
            let source_digest = source.digest(*table, schema_version).await?;
            let target_digest = self.target_digest(*table).await?;
            if source_digest != target_digest {
                return Err(import_error(format!(
                    "table {} failed SQLite-to-Hiqlite parity (source rows {}, target rows {}); discard the incoming target",
                    table.name,
                    source_digest.row_count,
                    target_digest.row_count
                )));
            }
            let row_count = source_digest.row_count;
            imported_rows = imported_rows
                .checked_add(row_count)
                .ok_or_else(|| import_error("imported row count overflow"))?;
            tables.push(SqliteImportTableDigest {
                table: table.name.to_owned(),
                row_count,
                sha256: source_digest.sha256,
            });
        }

        Ok(SqliteImportReport {
            source_schema_version: schema_version,
            backup_sha256: metadata.backup_sha256,
            imported_rows,
            search_rows,
            tables,
        })
    }

    /// Refuse a backup carrying a credential this build would replicate in the
    /// clear, before a single row is submitted to Raft.
    ///
    /// The check is up front rather than per chunk because Raft is the thing
    /// being protected. Every durable backend write already refuses an unsealed
    /// credential, but import does not go through those writers — it copies
    /// source columns straight into the target — so without this the one path
    /// that bypasses the write-time gate is also the one that fans the value
    /// out to every voter. Discarding a half-imported target recovers the
    /// database; it does not recover a bearer token from committed log entries
    /// on three machines.
    ///
    /// The remedy named in the error is real: `open_store` seals cleartext rows
    /// on boot, so starting this build on the SQLite install and taking a fresh
    /// backup produces a source this accepts.
    async fn refuse_unsealed_source_credentials(
        &self,
        source: &SourceReader,
        schema_version: i64,
    ) -> Result<(), StoreError> {
        for table in TABLES {
            if table.sealed_columns.is_empty() || schema_version < table.minimum_schema {
                continue;
            }
            let unsealed = source.unsealed_credential_rows(*table).await?;
            if unsealed != 0 {
                return Err(import_error(format!(
                    "SQLite backup has {unsealed} {} row(s) whose credential columns are not \
                     sealed envelopes; importing them would replicate a usable credential to \
                     every voter. Start this build on the SQLite install so it seals those rows, \
                     then take a fresh backup",
                    table.name
                )));
            }
        }
        Ok(())
    }

    async fn verify_empty_import_target(&self) -> Result<(), StoreError> {
        for table in TABLES {
            let filter = import_filter_sql(*table);
            let rows = self.target_count(table.name, filter).await?;
            if rows != 0 {
                return Err(import_error(format!(
                    "Hiqlite import target is not fresh: table {} already has {rows} application row(s)",
                    table.name
                )));
            }
        }
        let guards = self.target_count("offline_lease_guards", None).await?;
        if guards != 0 {
            return Err(import_error(format!(
                "Hiqlite import target is not fresh: table offline_lease_guards already has {guards} row(s)"
            )));
        }
        Ok(())
    }

    async fn import_instance_setting(&self, source: &SourceMetadata) -> Result<(), StoreError> {
        let target_id = self.instance_id().await?;
        if source.instance_id != target_id {
            return Err(StoreError::Identity(format!(
                "SQLite instance.id {} does not match incoming cluster instance.id {target_id}",
                source.instance_id
            )));
        }
        let changed = self
            .client()
            .execute(
                "UPDATE settings SET updated_at = $1 WHERE key = $2 AND value = $3",
                params!(
                    source.instance_updated_at,
                    keys::INSTANCE_ID,
                    source.instance_id.clone()
                ),
            )
            .await?;
        if changed != 1 {
            return Err(import_error(format!(
                "updating imported instance.id timestamp changed {changed} rows"
            )));
        }
        Ok(())
    }

    async fn import_table(
        &self,
        source: &SourceReader,
        schema_version: i64,
        table: TablePlan,
    ) -> Result<(), StoreError> {
        if schema_version < table.minimum_schema {
            return Ok(());
        }
        let expected = source.count(table, true).await?;
        let insert_sql = insert_sql(table);
        if table.parent_first {
            let ids = source.parent_first_item_ids().await?;
            let selected =
                i64::try_from(ids.len()).map_err(|error| import_error(error.to_string()))?;
            if selected != expected {
                let unreachable = expected.saturating_sub(selected);
                return Err(import_error(format!(
                    "items parent graph contains {unreachable} row(s) unreachable from a root; a parent_id cycle is possible"
                )));
            }
            for ids in ids.chunks(IMPORT_CHUNK_ROWS as usize) {
                let chunk = source
                    .import_chunk(table, schema_version, SourceChunk::ItemIds(ids.to_vec()))
                    .await?;
                self.insert_import_chunk(table, &insert_sql, chunk).await?;
            }
            return Ok(());
        }

        let mut offset = 0_i64;
        while offset < expected {
            let chunk = source
                .import_chunk(table, schema_version, SourceChunk::Offset(offset))
                .await?;
            if chunk.is_empty() {
                break;
            }
            let chunk_len =
                i64::try_from(chunk.len()).map_err(|error| import_error(error.to_string()))?;
            self.insert_import_chunk(table, &insert_sql, chunk).await?;
            offset += chunk_len;
        }
        if offset != expected {
            return Err(import_error(format!(
                "table {} import selected {offset} of {expected} source rows",
                table.name
            )));
        }
        Ok(())
    }

    async fn insert_import_chunk(
        &self,
        table: TablePlan,
        insert_sql: &str,
        chunk: Vec<hiqlite::Params>,
    ) -> Result<(), StoreError> {
        let results = self
            .client()
            .txn(chunk.into_iter().map(|row| (insert_sql.to_owned(), row)))
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if results.iter().any(|changed| *changed != 1) {
            return Err(import_error(format!(
                "table {} import did not insert exactly one row per statement",
                table.name
            )));
        }
        Ok(())
    }

    async fn target_count(
        &self,
        table: &'static str,
        filter: Option<String>,
    ) -> Result<u64, StoreError> {
        let sql = match filter {
            Some(filter) => format!("SELECT COUNT(*) AS count FROM {table} WHERE {filter}"),
            None => format!("SELECT COUNT(*) AS count FROM {table}"),
        };
        let rows = self
            .client()
            .query_consistent_map::<CountRow, _>(sql, params!())
            .await?;
        one_count(rows, table)
    }

    async fn verify_target_foreign_keys(&self) -> Result<(), StoreError> {
        let rows = self
            .client()
            .query_consistent_map::<CountRow, _>(
                "SELECT COUNT(*) AS count FROM pragma_foreign_key_check",
                params!(),
            )
            .await?;
        let dangling = one_count(rows, "pragma_foreign_key_check")?;
        if dangling != 0 {
            return Err(import_error(format!(
                "Hiqlite import left {dangling} dangling foreign key reference(s); discard the incoming target"
            )));
        }
        Ok(())
    }

    async fn target_digest(&self, table: TablePlan) -> Result<OrderedRowsDigest, StoreError> {
        let mut cursor = Vec::new();
        let mut digest = OrderedRowsHasher::new();
        loop {
            let (sql, query_params) = target_parity_page(table, &cursor)?;
            let page = self
                .client()
                .query_consistent_map::<ParityPageRow, _>(sql, query_params)
                .await?;
            if page.is_empty() {
                break;
            }
            if page.len() > PARITY_PAGE_ROWS as usize {
                return Err(import_error(format!(
                    "table {} parity page returned {} rows above the {PARITY_PAGE_ROWS}-row bound",
                    table.name,
                    page.len()
                )));
            }
            for row in &page {
                digest.push(&row.value)?;
            }
            let next_cursor = parity_cursor_params(table, &page[page.len() - 1].cursor)?;
            if next_cursor == cursor {
                return Err(import_error(format!(
                    "table {} parity cursor did not advance",
                    table.name
                )));
            }
            cursor = next_cursor;
            if page.len() < PARITY_PAGE_ROWS as usize {
                break;
            }
        }
        Ok(digest.finish())
    }
}

fn target_parity_page(
    table: TablePlan,
    cursor: &[Param],
) -> Result<(String, hiqlite::Params), StoreError> {
    let projection = json_projection(table, SQLITE_SCHEMA_VERSION, false);
    let keys = parity_key_columns(table);
    let cursor_projection = keys.join(", ");
    let mut query_params = cursor.to_vec();
    let filter = if cursor.is_empty() {
        String::new()
    } else {
        if cursor.len() != keys.len() {
            return Err(import_error(format!(
                "table {} parity cursor has {} values for {} ordering columns",
                table.name,
                cursor.len(),
                keys.len()
            )));
        }
        let placeholders = (1..=cursor.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" WHERE ({cursor_projection}) > ({placeholders})")
    };
    query_params.push(Param::Integer(PARITY_PAGE_ROWS));
    let limit = query_params.len();
    Ok((
        format!(
            "SELECT json_array({projection}) AS value, \
             json_array({cursor_projection}) AS cursor \
             FROM {}{filter} ORDER BY {} LIMIT ${limit}",
            table.name, table.order_by
        ),
        query_params,
    ))
}

fn parity_key_columns(table: TablePlan) -> Vec<&'static str> {
    table.order_by.split(", ").collect()
}

fn parity_cursor_params(table: TablePlan, cursor: &str) -> Result<Vec<Param>, StoreError> {
    let values = serde_json::from_str::<Vec<serde_json::Value>>(cursor).map_err(|error| {
        import_error(format!(
            "decoding table {} parity cursor {cursor}: {error}",
            table.name
        ))
    })?;
    let keys = parity_key_columns(table);
    if values.len() != keys.len() {
        return Err(import_error(format!(
            "table {} parity cursor has {} values for {} ordering columns",
            table.name,
            values.len(),
            keys.len()
        )));
    }
    values
        .into_iter()
        .map(|value| match value {
            serde_json::Value::Number(value) => value
                .as_i64()
                .map(Param::Integer)
                .ok_or_else(|| import_error("parity cursor integer is outside i64")),
            serde_json::Value::String(value) => Ok(Param::Text(value)),
            other => Err(import_error(format!(
                "table {} parity cursor contains unsupported key {other}",
                table.name
            ))),
        })
        .collect()
}

fn open_source(path: &Path) -> Result<Connection, StoreError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| import_error(format!("opening SQLite backup {}: {error}", path.display())))
}

fn open_validated_source(
    path: &Path,
    expected_sha256: &str,
    expected_schema_version: i64,
) -> Result<(Connection, SourceMetadata), StoreError> {
    let actual_sha256 = sha256_file(path)?;
    if actual_sha256 != expected_sha256 {
        return Err(import_error(format!(
            "SQLite backup checksum changed: expected {expected_sha256}, got {actual_sha256}"
        )));
    }
    let source = open_source(path)?;
    let schema_version = source_schema_version(&source)?;
    if schema_version != expected_schema_version {
        return Err(import_error(format!(
            "SQLite backup schema changed: expected v{expected_schema_version}, got v{schema_version}"
        )));
    }
    if !(MINIMUM_IMPORT_SCHEMA_VERSION..=SQLITE_SCHEMA_VERSION).contains(&schema_version) {
        return Err(import_error(format!(
            "clustering import supports SQLite schemas v{MINIMUM_IMPORT_SCHEMA_VERSION}..=v{SQLITE_SCHEMA_VERSION}, got v{schema_version}"
        )));
    }
    verify_source(&source)?;
    let (instance_id, instance_updated_at) = source
        .query_row(
            "SELECT value, updated_at FROM settings WHERE key = ?1",
            [keys::INSTANCE_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| import_error(format!("reading source instance.id: {error}")))?;
    Ok((
        source,
        SourceMetadata {
            backup_sha256: actual_sha256,
            schema_version,
            instance_id,
            instance_updated_at,
        },
    ))
}

fn source_schema_version(source: &Connection) -> Result<i64, StoreError> {
    source
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| import_error(format!("reading SQLite backup schema: {error}")))
}

fn verify_source(source: &Connection) -> Result<(), StoreError> {
    let integrity: String = source
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| import_error(format!("checking SQLite backup: {error}")))?;
    if integrity != "ok" {
        return Err(import_error(format!(
            "SQLite backup failed quick_check: {integrity}"
        )));
    }
    let dangling: i64 = source
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|error| import_error(format!("checking SQLite backup foreign keys: {error}")))?;
    if dangling != 0 {
        return Err(import_error(format!(
            "SQLite backup contains {dangling} dangling foreign key reference(s)"
        )));
    }
    Ok(())
}

fn source_count(
    source: &Connection,
    table: TablePlan,
    for_import: bool,
) -> Result<i64, StoreError> {
    let filter = for_import.then(|| import_filter_sql(table)).flatten();
    let sql = match filter {
        Some(filter) => format!("SELECT COUNT(*) FROM {} WHERE {filter}", table.name),
        None => format!("SELECT COUNT(*) FROM {}", table.name),
    };
    source
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| import_error(format!("counting source table {}: {error}", table.name)))
}

/// Count the source rows whose sealed columns are not envelopes this build can
/// parse.
///
/// The test is [`SealedSecret::is_wrapped`], not the laxer
/// [`looks_wrapped`](SealedSecret::looks_wrapped) the SQLite upgrade pass uses.
/// The upgrade pass is deciding whether re-sealing would destroy a credential,
/// so a damaged envelope belongs on its refusal path. Import is deciding what
/// may enter a replicated log that never forgets, and a damaged envelope is a
/// credential nobody can open — copying it into every voter fixes nothing and
/// makes the source install's problem permanent.
///
/// Only the count leaves this function. The values are exactly what must not
/// reach an error string or a log line.
fn unsealed_credential_rows(source: &Connection, table: TablePlan) -> Result<u64, StoreError> {
    if table.sealed_columns.is_empty() {
        return Ok(0);
    }
    let projection = table.sealed_columns.join(", ");
    let mut statement = source
        .prepare(&format!("SELECT {projection} FROM {}", table.name))
        .map_err(|error| {
            import_error(format!(
                "reading sealed columns of source table {}: {error}",
                table.name
            ))
        })?;
    let mut rows = statement.query([]).map_err(|error| {
        import_error(format!(
            "reading sealed columns of source table {}: {error}",
            table.name
        ))
    })?;

    let mut unsealed = 0_u64;
    while let Some(row) = rows.next().map_err(|error| {
        import_error(format!(
            "reading sealed columns of source table {}: {error}",
            table.name
        ))
    })? {
        // A NULL or non-text value in a credential column is not an envelope
        // either, so it fails closed with everything else rather than reading
        // as "nothing to check here".
        let sealed = (0..table.sealed_columns.len()).all(|index| {
            row.get::<_, String>(index)
                .is_ok_and(|value| SealedSecret::from_stored(value).is_wrapped())
        });
        if !sealed {
            unsealed = unsealed
                .checked_add(1)
                .ok_or_else(|| import_error("unsealed credential row count overflow"))?;
        }
    }
    Ok(unsealed)
}

fn import_select_sql(table: TablePlan, schema_version: i64) -> String {
    let projection = value_projection(table, schema_version, false);
    let filter = import_filter_sql(table)
        .map(|filter| format!(" WHERE {filter}"))
        .unwrap_or_default();
    format!(
        "SELECT {projection} FROM {}{filter} ORDER BY {} LIMIT ?1 OFFSET ?2",
        table.name, table.order_by
    )
}

fn import_filter_sql(table: TablePlan) -> Option<String> {
    table.import_filter.map(|ImportFilter::ExcludeInstanceId| {
        format!("key <> '{}'", keys::INSTANCE_ID.replace('\'', "''"))
    })
}

fn parent_first_item_ids(source: &Connection) -> Result<Vec<i64>, StoreError> {
    let mut statement = source
        .prepare(
            "WITH RECURSIVE import_items(id, depth) AS (
                 SELECT id, 0 FROM items WHERE parent_id IS NULL
                 UNION ALL
                 SELECT child.id, parent.depth + 1 FROM items AS child
                 JOIN import_items AS parent ON child.parent_id = parent.id
             )
             SELECT id FROM import_items ORDER BY depth, id",
        )
        .map_err(|error| import_error(format!("ordering source items by parent: {error}")))?;
    let mapped = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|error| import_error(format!("ordering source items by parent: {error}")))?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| import_error(format!("ordering source items by parent: {error}")))
}

fn read_item_import_chunk(
    source: &Connection,
    schema_version: i64,
    table: TablePlan,
    ids: &[i64],
) -> Result<Vec<hiqlite::Params>, StoreError> {
    let projection = value_projection(table, schema_version, true);
    let sql = format!("SELECT {projection} FROM items AS source WHERE source.id = ?1");
    let mut statement = source
        .prepare(&sql)
        .map_err(|error| import_error(format!("preparing source items: {error}")))?;
    ids.iter()
        .map(|id| {
            statement
                .query_row([id], |row| {
                    (0..table.columns.len())
                        .map(|index| value_param(row.get_ref(index)?))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .map_err(|error| import_error(format!("reading source item {id}: {error}")))
        })
        .collect()
}

fn insert_sql(table: TablePlan) -> String {
    let columns = table.columns.join(", ");
    let placeholders = (1..=table.columns.len())
        .map(|index| format!("${index}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
        table.name
    )
}

fn read_import_chunk(
    source: &Connection,
    table: TablePlan,
    sql: &str,
    offset: i64,
) -> Result<Vec<hiqlite::Params>, StoreError> {
    let mut statement = source
        .prepare(sql)
        .map_err(|error| import_error(format!("preparing source table {}: {error}", table.name)))?;
    let mapped = statement
        .query_map([IMPORT_CHUNK_ROWS, offset], |row| {
            (0..table.columns.len())
                .map(|index| value_param(row.get_ref(index)?))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|error| import_error(format!("reading source table {}: {error}", table.name)))?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| import_error(format!("reading source table {}: {error}", table.name)))
}

fn value_param(value: ValueRef<'_>) -> rusqlite::Result<Param> {
    Ok(match value {
        ValueRef::Null => Param::Null,
        ValueRef::Integer(value) => Param::Integer(value),
        ValueRef::Real(value) => Param::Real(value),
        ValueRef::Text(value) => {
            Param::Text(String::from_utf8(value.to_vec()).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?)
        }
        ValueRef::Blob(_) => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "BLOB import needs a deterministic parity encoding",
                )),
            ));
        }
    })
}

fn source_digest(
    source: &Connection,
    schema_version: i64,
    table: TablePlan,
) -> Result<OrderedRowsDigest, StoreError> {
    if schema_version < table.minimum_schema {
        return Ok(OrderedRowsHasher::new().finish());
    }
    let projection = json_projection(table, schema_version, false);
    let sql = format!(
        "SELECT json_array({projection}) AS value FROM {} ORDER BY {}",
        table.name, table.order_by
    );
    let mut statement = source
        .prepare(&sql)
        .map_err(|error| import_error(format!("hashing source table {}: {error}", table.name)))?;
    let mapped = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| import_error(format!("hashing source table {}: {error}", table.name)))?;
    let mut digest = OrderedRowsHasher::new();
    for row in mapped {
        let row = row.map_err(|error| {
            import_error(format!("hashing source table {}: {error}", table.name))
        })?;
        digest.push(&row)?;
    }
    Ok(digest.finish())
}

fn json_projection(table: TablePlan, schema_version: i64, qualify: bool) -> String {
    value_projection(table, schema_version, qualify)
}

fn value_projection(table: TablePlan, schema_version: i64, qualify: bool) -> String {
    table
        .columns
        .iter()
        .map(|column| {
            if table.name == "watched_outbox" && *column == "claim_until" && schema_version < 16 {
                "0".to_owned()
            } else if table.name == "offline_packages"
                && *column == "effective_rate_control"
                && schema_version < 18
            {
                "'vbr'".to_owned()
            } else if qualify {
                format!("source.{column}")
            } else {
                (*column).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let mut file = File::open(path).map_err(|error| {
        import_error(format!("opening SQLite backup {}: {error}", path.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| {
            import_error(format!("reading SQLite backup {}: {error}", path.display()))
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn import_error(message: impl Into<String>) -> StoreError {
    StoreError::Migration(message.into())
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

fn one_count(rows: Vec<CountRow>, source: &str) -> Result<u64, StoreError> {
    let [row] = rows.as_slice() else {
        return Err(import_error(format!(
            "counting {source} returned {} rows",
            rows.len()
        )));
    };
    u64::try_from(row.count).map_err(|error| import_error(error.to_string()))
}

struct ParityPageRow {
    value: String,
    cursor: String,
}

impl From<&mut Row<'_>> for ParityPageRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
            cursor: row.get("cursor"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_digest_matches_the_original_ordered_json_contract() {
        let rows = vec![
            r#"[1,"alpha",null]"#.to_owned(),
            r#"[2,"quote: \"",17]"#.to_owned(),
            r#"[10,"unicode: ☃",0]"#.to_owned(),
        ];
        let expected = hex::encode(Sha256::digest(
            serde_json::to_vec(&rows).expect("serialize reference rows"),
        ));
        let mut digest = OrderedRowsHasher::new();
        for row in &rows {
            digest.push(row).expect("hash row");
        }
        assert_eq!(
            digest.finish(),
            OrderedRowsDigest {
                row_count: 3,
                sha256: expected,
            }
        );
    }

    #[test]
    fn target_parity_uses_bounded_multi_column_keyset_pages() {
        let table = TABLES
            .iter()
            .find(|table| table.name == "transcode_cache_locations")
            .copied()
            .expect("location table plan");
        let (first_sql, first_params) = target_parity_page(table, &[]).expect("first page");
        assert!(!first_sql.contains(" WHERE "));
        assert!(first_sql.ends_with("LIMIT $1"));
        assert_eq!(first_params, vec![Param::Integer(PARITY_PAGE_ROWS)]);

        let cursor =
            parity_cursor_params(table, r#"["recipe","node","ssd"]"#).expect("decode cursor");
        let (next_sql, next_params) = target_parity_page(table, &cursor).expect("next page");
        assert!(next_sql.contains("WHERE (recipe_hash, node_id, storage_class) > ($1, $2, $3)"));
        assert!(next_sql.ends_with("LIMIT $4"));
        assert_eq!(next_params.len(), 4);
        assert_eq!(next_params[3], Param::Integer(PARITY_PAGE_ROWS));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn source_reader_blocking_work_keeps_the_async_executor_responsive() {
        let data = tempfile::tempdir().expect("data dir");
        let path = data.path().join(crate::cluster::migration::SQLITE_FILENAME);
        crate::store::SqliteStore::open(&path).expect("source store");
        let prepared =
            crate::cluster::migration::prepare_sqlite_import(data.path()).expect("prepared backup");
        let (reader, _) = SourceReader::open(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect("source reader");

        let pause = reader.pause(std::time::Duration::from_millis(500));
        tokio::pin!(pause);
        tokio::select! {
            result = &mut pause => panic!("blocking source work returned early: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        pause.await.expect("source pause completes");
    }

    #[test]
    fn v14_outbox_projection_supplies_unleased_claim_deadline() {
        let table = TABLES
            .iter()
            .find(|table| table.name == "watched_outbox")
            .copied()
            .expect("watched_outbox plan");
        let v14 = value_projection(table, 14, false);
        let current = value_projection(table, SQLITE_SCHEMA_VERSION, false);
        assert!(v14.ends_with("updated_at, 0"));
        assert!(current.ends_with("updated_at, claim_until"));
    }

    #[test]
    fn pre_v18_offline_projection_supplies_legacy_vbr_snapshot() {
        let table = TABLES
            .iter()
            .find(|table| table.name == "offline_packages")
            .copied()
            .expect("offline package plan");
        let v17 = value_projection(table, 17, false);
        let current = value_projection(table, SQLITE_SCHEMA_VERSION, false);
        assert!(v17.contains("recipe_hash, 'vbr', target_height"));
        assert!(current.contains("recipe_hash, effective_rate_control, target_height"));
    }

    #[test]
    fn replicated_tables_exclude_node_local_and_derived_state() {
        let names = TABLES.iter().map(|table| table.name).collect::<Vec<_>>();
        assert!(!names.contains(&"playback_events"));
        assert!(!names.contains(&"items_fts"));
        assert!(!names.contains(&"offline_lease_guards"));
        assert_eq!(names.len(), 17, "review every imported durable table");
    }

    #[test]
    fn insert_parameters_follow_hiqlite_numeric_order() {
        for table in TABLES {
            super::super::hiqlite::validate_sql(&insert_sql(*table))
                .unwrap_or_else(|error| panic!("{} insert is invalid: {error}", table.name));
        }
    }
}

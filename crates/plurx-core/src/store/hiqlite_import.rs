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

use super::hiqlite::{database_error, HiqliteAuthStore};
use super::{keys, MediaStore, SettingsStore, SQLITE_SCHEMA_VERSION};
use crate::error::StoreError;

const MINIMUM_IMPORT_SCHEMA_VERSION: i64 = 14;
const IMPORT_CHUNK_ROWS: i64 = 64;

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
    import_filter: Option<&'static str>,
    parent_first: bool,
}

const TABLES: &[TablePlan] = &[
    TablePlan {
        name: "settings",
        columns: &["key", "value", "updated_at"],
        order_by: "key",
        minimum_schema: 1,
        import_filter: Some("key <> 'instance.id'"),
        parent_first: false,
    },
    TablePlan {
        name: "users",
        columns: &["id", "username", "password_hash", "is_admin", "created_at"],
        order_by: "id",
        minimum_schema: 2,
        import_filter: None,
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
        parent_first: false,
    },
    TablePlan {
        name: "transcode_cache_recipes",
        columns: &["recipe_hash", "file_id", "recipe_version", "created_at"],
        order_by: "recipe_hash",
        minimum_schema: 11,
        import_filter: None,
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
        parent_first: false,
    },
    TablePlan {
        name: "library_roots",
        columns: &["library_id", "fingerprint"],
        order_by: "library_id",
        minimum_schema: 15,
        import_filter: None,
        parent_first: false,
    },
    TablePlan {
        name: "scan_reconcile_guards",
        columns: &["library_id"],
        order_by: "library_id",
        minimum_schema: 15,
        import_filter: None,
        parent_first: false,
    },
    TablePlan {
        name: "scan_reconcile_items",
        columns: &["library_id", "item_id"],
        order_by: "library_id, item_id",
        minimum_schema: 15,
        import_filter: None,
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
        let actual_sha256 = sha256_file(backup_path)?;
        if actual_sha256 != expected_sha256 {
            return Err(import_error(format!(
                "SQLite backup checksum changed: expected {expected_sha256}, got {actual_sha256}"
            )));
        }

        let source = open_source(backup_path)?;
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
        self.verify_empty_import_target().await?;
        self.import_instance_setting(&source).await?;

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

        let mut tables = Vec::with_capacity(TABLES.len());
        let mut imported_rows = 0_u64;
        for table in TABLES {
            let source_rows = source_hash_rows(&source, schema_version, *table)?;
            let target_rows = self.target_hash_rows(*table).await?;
            if source_rows != target_rows {
                return Err(import_error(format!(
                    "table {} failed SQLite-to-Hiqlite parity (source rows {}, target rows {}); discard the incoming target",
                    table.name,
                    source_rows.len(),
                    target_rows.len()
                )));
            }
            let row_count = u64::try_from(source_rows.len())
                .map_err(|error| import_error(error.to_string()))?;
            imported_rows = imported_rows
                .checked_add(row_count)
                .ok_or_else(|| import_error("imported row count overflow"))?;
            tables.push(SqliteImportTableDigest {
                table: table.name.to_owned(),
                row_count,
                sha256: hash_rows(&source_rows)?,
            });
        }

        Ok(SqliteImportReport {
            source_schema_version: schema_version,
            backup_sha256: actual_sha256,
            imported_rows,
            search_rows,
            tables,
        })
    }

    async fn verify_empty_import_target(&self) -> Result<(), StoreError> {
        for table in TABLES {
            let filter = (table.name == "settings").then_some("key <> 'instance.id'");
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

    async fn import_instance_setting(&self, source: &Connection) -> Result<(), StoreError> {
        let (source_id, updated_at): (String, i64) = source
            .query_row(
                "SELECT value, updated_at FROM settings WHERE key = ?1",
                [keys::INSTANCE_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| import_error(format!("reading source instance.id: {error}")))?;
        let target_id = self.instance_id().await?;
        if source_id != target_id {
            return Err(StoreError::Identity(format!(
                "SQLite instance.id {source_id} does not match incoming cluster instance.id {target_id}"
            )));
        }
        let changed = self
            .client()
            .execute(
                "UPDATE settings SET updated_at = $1 WHERE key = $2 AND value = $3",
                params!(updated_at, keys::INSTANCE_ID, source_id),
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
        source: &Connection,
        schema_version: i64,
        table: TablePlan,
    ) -> Result<(), StoreError> {
        if schema_version < table.minimum_schema {
            return Ok(());
        }
        let expected = source_count(source, table, true)?;
        let insert_sql = insert_sql(table);
        let select_sql = import_select_sql(table, schema_version);
        let mut offset = 0_i64;
        while offset < expected {
            let chunk = read_import_chunk(source, table, &select_sql, offset)?;
            if chunk.is_empty() {
                break;
            }
            let chunk_len =
                i64::try_from(chunk.len()).map_err(|error| import_error(error.to_string()))?;
            let results = self
                .client()
                .txn(chunk.into_iter().map(|row| (insert_sql.clone(), row)))
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

    async fn target_count(
        &self,
        table: &'static str,
        filter: Option<&'static str>,
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

    async fn target_hash_rows(&self, table: TablePlan) -> Result<Vec<String>, StoreError> {
        let projection = json_projection(table, SQLITE_SCHEMA_VERSION, false);
        let sql = format!(
            "SELECT json_array({projection}) AS value FROM {} ORDER BY {}",
            table.name, table.order_by
        );
        Ok(self
            .client()
            .query_consistent_map::<JsonValueRow, _>(sql, params!())
            .await?
            .into_iter()
            .map(|row| row.value)
            .collect())
    }
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
    let filter = for_import.then_some(table.import_filter).flatten();
    let sql = match filter {
        Some(filter) => format!("SELECT COUNT(*) FROM {} WHERE {filter}", table.name),
        None => format!("SELECT COUNT(*) FROM {}", table.name),
    };
    source
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| import_error(format!("counting source table {}: {error}", table.name)))
}

fn import_select_sql(table: TablePlan, schema_version: i64) -> String {
    let projection = value_projection(table, schema_version, table.parent_first);
    if table.parent_first {
        return format!(
            "WITH RECURSIVE import_items(id, depth) AS (\
                 SELECT id, 0 FROM items WHERE parent_id IS NULL \
                 UNION ALL \
                 SELECT child.id, parent.depth + 1 FROM items AS child \
                 JOIN import_items AS parent ON child.parent_id = parent.id\
             ) \
             SELECT {projection} FROM items AS source \
             JOIN import_items AS import_order ON import_order.id = source.id \
             ORDER BY import_order.depth, source.id LIMIT ?1 OFFSET ?2"
        );
    }
    let filter = table
        .import_filter
        .map(|filter| format!(" WHERE {filter}"))
        .unwrap_or_default();
    format!(
        "SELECT {projection} FROM {}{filter} ORDER BY {} LIMIT ?1 OFFSET ?2",
        table.name, table.order_by
    )
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
        ValueRef::Blob(value) => Param::Blob(value.to_vec()),
    })
}

fn source_hash_rows(
    source: &Connection,
    schema_version: i64,
    table: TablePlan,
) -> Result<Vec<String>, StoreError> {
    if schema_version < table.minimum_schema {
        return Ok(Vec::new());
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
        .query_map([], |row| row.get(0))
        .map_err(|error| import_error(format!("hashing source table {}: {error}", table.name)))?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| import_error(format!("hashing source table {}: {error}", table.name)))
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
            } else if qualify {
                format!("source.{column}")
            } else {
                (*column).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn hash_rows(rows: &[String]) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(rows)
        .map_err(|error| import_error(format!("serializing table parity rows: {error}")))?;
    Ok(hex::encode(Sha256::digest(bytes)))
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

struct JsonValueRow {
    value: String,
}

impl From<&mut Row<'_>> for JsonValueRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            value: row.get("value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

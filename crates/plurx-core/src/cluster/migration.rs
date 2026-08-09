//! Crash-safe preparation for the one-time SQLite-to-Hiqlite import.
//!
//! This module deliberately stops before it creates or selects a Hiqlite
//! store. It establishes the source-side invariants M2's row importer relies
//! on: reject future schemas without changing the data directory, discard an
//! abandoned incoming target, and publish a durable, content-addressed SQLite
//! backup that includes committed WAL contents.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::error::StoreError;
use crate::store::SQLITE_SCHEMA_VERSION;

pub const SQLITE_FILENAME: &str = "plurx.db";
pub const MIGRATION_DIRNAME: &str = "migration";
pub const HIQLITE_INCOMING_DIRNAME: &str = "hiqlite.incoming";

/// Immutable source material for the row-import and parity phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSqliteImport {
    pub source_path: PathBuf,
    pub backup_path: PathBuf,
    pub source_sha256: String,
    pub schema_version: i64,
}

/// Validate and durably snapshot the legacy SQLite source.
///
/// The daemon will call this only from `plurxd run`, before any listener or
/// background task starts. A future-schema database is refused before any
/// cleanup or backup write. SQLite's online-backup API is used instead of a
/// filesystem copy so committed pages still resident in `plurx.db-wal` are
/// included in the snapshot.
pub fn prepare_sqlite_import(data_dir: &Path) -> Result<PreparedSqliteImport, StoreError> {
    let source_path = data_dir.join(SQLITE_FILENAME);
    let source = open_source(&source_path)?;
    let schema_version = read_schema_version(&source)?;
    if schema_version > SQLITE_SCHEMA_VERSION {
        return Err(StoreError::Migration(format!(
            "source database schema is v{schema_version}, but this binary only knows \
             v{SQLITE_SCHEMA_VERSION}; refusing clustering import without changing {}",
            data_dir.display()
        )));
    }

    remove_abandoned_incoming(data_dir)?;

    let migration_dir = data_dir.join(MIGRATION_DIRNAME);
    std::fs::create_dir_all(&migration_dir)
        .map_err(|error| migration_io("creating", &migration_dir, error))?;
    sync_directory(data_dir)?;
    remove_abandoned_backup_temps(&migration_dir)?;

    let temporary_path = migration_dir.join(format!(
        ".{SQLITE_FILENAME}.{}.incoming",
        uuid::Uuid::new_v4()
    ));
    let prepared = create_backup(
        &source,
        &source_path,
        schema_version,
        &migration_dir,
        &temporary_path,
    );
    if let Err(error) = prepared {
        return match remove_file_if_present(&temporary_path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(StoreError::Migration(format!(
                "{error}; additionally failed to clean temporary backup: {cleanup}"
            ))),
        };
    }
    prepared
}

fn open_source(path: &Path) -> Result<Connection, StoreError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        StoreError::Migration(format!(
            "opening SQLite import source {}: {error}",
            path.display()
        ))
    })
}

fn read_schema_version(connection: &Connection) -> Result<i64, StoreError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| StoreError::Migration(format!("reading source schema version: {error}")))
}

fn remove_abandoned_incoming(data_dir: &Path) -> Result<(), StoreError> {
    let incoming = data_dir.join(HIQLITE_INCOMING_DIRNAME);
    let metadata = match std::fs::symlink_metadata(&incoming) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(migration_io("inspecting", &incoming, error)),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(&incoming)
            .map_err(|error| migration_io("removing abandoned", &incoming, error))?;
    } else {
        std::fs::remove_file(&incoming)
            .map_err(|error| migration_io("removing abandoned", &incoming, error))?;
    }
    sync_directory(data_dir)
}

fn remove_abandoned_backup_temps(migration_dir: &Path) -> Result<(), StoreError> {
    let mut removed = false;
    let entries = std::fs::read_dir(migration_dir)
        .map_err(|error| migration_io("reading", migration_dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| migration_io("reading", migration_dir, error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!(".{SQLITE_FILENAME}.")) && name.ends_with(".incoming") {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| migration_io("inspecting", &path, error))?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                return Err(StoreError::Migration(format!(
                    "refusing to recursively remove unexpected backup staging directory {}",
                    path.display()
                )));
            }
            std::fs::remove_file(&path)
                .map_err(|error| migration_io("removing abandoned", &path, error))?;
            removed = true;
        }
    }
    if removed {
        sync_directory(migration_dir)?;
    }
    Ok(())
}

fn create_backup(
    source: &Connection,
    source_path: &Path,
    schema_version: i64,
    migration_dir: &Path,
    temporary_path: &Path,
) -> Result<PreparedSqliteImport, StoreError> {
    create_private_file(temporary_path)?;
    let mut destination = Connection::open(temporary_path).map_err(|error| {
        StoreError::Migration(format!(
            "opening temporary SQLite backup {}: {error}",
            temporary_path.display()
        ))
    })?;
    {
        let backup = Backup::new(source, &mut destination).map_err(|error| {
            StoreError::Migration(format!("starting SQLite online backup: {error}"))
        })?;
        backup
            .run_to_completion(256, Duration::from_millis(10), None)
            .map_err(|error| {
                StoreError::Migration(format!("copying SQLite import source: {error}"))
            })?;
    }
    let copied_version = read_schema_version(&destination)?;
    if copied_version != schema_version {
        return Err(StoreError::Migration(format!(
            "SQLite backup schema changed during import preparation: \
             source v{schema_version}, backup v{copied_version}"
        )));
    }
    let integrity: String = destination
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| StoreError::Migration(format!("checking SQLite backup: {error}")))?;
    if integrity != "ok" {
        return Err(StoreError::Migration(format!(
            "SQLite backup failed quick_check: {integrity}"
        )));
    }
    drop(destination);

    File::open(temporary_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| migration_io("syncing", temporary_path, error))?;
    let source_sha256 = sha256_file(temporary_path)?;
    let backup_path = migration_dir.join(format!("plurx-v{schema_version}-{source_sha256}.db"));

    if backup_path.exists() {
        let existing_sha256 = sha256_file(&backup_path)?;
        if existing_sha256 != source_sha256 {
            return Err(StoreError::Migration(format!(
                "existing migration backup {} does not match its content-addressed name",
                backup_path.display()
            )));
        }
        remove_file_if_present(temporary_path)?;
    } else {
        match std::fs::rename(temporary_path, &backup_path) {
            Ok(()) => {}
            Err(error) if backup_path.exists() => {
                let existing_sha256 = sha256_file(&backup_path)?;
                if existing_sha256 != source_sha256 {
                    return Err(migration_io("publishing", &backup_path, error));
                }
                remove_file_if_present(temporary_path)?;
            }
            Err(error) => return Err(migration_io("publishing", &backup_path, error)),
        }
        sync_directory(migration_dir)?;
    }

    Ok(PreparedSqliteImport {
        source_path: source_path.to_owned(),
        backup_path,
        source_sha256,
        schema_version,
    })
}

fn create_private_file(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|error| migration_io("creating", path, error))?;
    file.write_all(&[])
        .map_err(|error| migration_io("initializing", path, error))
}

fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let mut file = File::open(path).map_err(|error| migration_io("opening", path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| migration_io("hashing", path, error))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn remove_file_if_present(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(migration_io("removing temporary", path, error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| migration_io("syncing directory", path, error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn migration_io(action: &str, path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Migration(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SettingsStore, SqliteStore};

    #[tokio::test]
    async fn backup_includes_committed_wal_state_and_removes_abandoned_incoming() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let store = SqliteStore::open(&source_path).expect("source store");
        store
            .put_setting("migration.wal", "committed")
            .await
            .expect("write source");

        let incoming = data.path().join(HIQLITE_INCOMING_DIRNAME);
        std::fs::create_dir_all(incoming.join("partial")).expect("incoming tree");
        std::fs::write(incoming.join("partial/state"), b"not complete").expect("incoming state");

        let prepared = prepare_sqlite_import(data.path()).expect("prepare import");
        assert!(!incoming.exists(), "abandoned target was removed");
        assert_eq!(prepared.schema_version, SQLITE_SCHEMA_VERSION);
        assert_eq!(
            prepared.source_sha256,
            sha256_file(&prepared.backup_path).expect("hash published backup")
        );

        let backup = Connection::open_with_flags(
            &prepared.backup_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("open backup");
        let value: String = backup
            .query_row(
                "SELECT value FROM settings WHERE key = 'migration.wal'",
                [],
                |row| row.get(0),
            )
            .expect("backed-up setting");
        assert_eq!(value, "committed");
    }

    #[test]
    fn future_schema_refusal_changes_nothing() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let source = Connection::open(&source_path).expect("source");
        source
            .pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION + 1)
            .expect("future version");
        drop(source);
        let incoming = data.path().join(HIQLITE_INCOMING_DIRNAME);
        std::fs::create_dir(&incoming).expect("incoming");
        std::fs::write(incoming.join("state"), b"preserve").expect("state");

        let error = prepare_sqlite_import(data.path()).expect_err("future schema refused");
        assert!(error.to_string().contains("refusing clustering import"));
        assert!(incoming.join("state").exists());
        assert!(!data.path().join(MIGRATION_DIRNAME).exists());
    }

    #[tokio::test]
    async fn content_addressed_backups_are_reused_and_old_backups_are_kept() {
        let data = tempfile::tempdir().expect("data dir");
        let source_path = data.path().join(SQLITE_FILENAME);
        let store = SqliteStore::open(&source_path).expect("source store");
        let migration_dir = data.path().join(MIGRATION_DIRNAME);
        std::fs::create_dir(&migration_dir).expect("migration dir");
        let abandoned = migration_dir.join(format!(
            ".{SQLITE_FILENAME}.{}.incoming",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&abandoned, b"partial backup").expect("abandoned backup");
        let first = prepare_sqlite_import(data.path()).expect("first backup");
        assert!(!abandoned.exists(), "abandoned backup temp was removed");
        let repeated = prepare_sqlite_import(data.path()).expect("repeated backup");
        assert_eq!(first, repeated);

        store
            .put_setting("migration.changed", "yes")
            .await
            .expect("change source");
        let changed = prepare_sqlite_import(data.path()).expect("changed backup");
        assert_ne!(changed.backup_path, first.backup_path);
        assert!(first.backup_path.exists(), "original backup is retained");
        assert!(changed.backup_path.exists(), "new backup is retained");
        assert!(
            std::fs::read_dir(data.path().join(MIGRATION_DIRNAME))
                .expect("migration dir")
                .all(|entry| !entry
                    .expect("migration entry")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".incoming")),
            "no temporary backup remains"
        );
    }
}

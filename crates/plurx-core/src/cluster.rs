//! Cluster identity compatibility for the Phase 4 transition.
//!
//! M0 deliberately keeps SQLite in production. Its job is to stop using the
//! logical server id as an implicit machine id without changing the ownership
//! key of any bytes already on disk. The first node therefore seeds
//! `<data_dir>/node.id` from the existing `instance.id`; future join work may
//! create a distinct local id before opening the replicated store.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::config::Config;
use crate::error::StoreError;
use crate::store::{SettingsStore, SqliteStore, Store};

pub const NODE_ID_FILENAME: &str = "node.id";
/// M0 runs the existing local store as the first and only voter.
pub const SINGLE_VOTER_RAFT_ID: u64 = 1;

/// The three identities that must not collapse into one value once clustering
/// starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterIdentity {
    /// Stable logical-server UUID stored as `instance.id`.
    pub cluster_id: String,
    /// Stable application-node UUID stored only in `<data_dir>/node.id`.
    pub node_id: String,
    /// Non-zero hiqlite membership id. M0 has exactly one voter.
    pub raft_id: u64,
}

/// Production-store opening result. The coordinator joins this handle when
/// the first replicated slice lands; M0 exposes the identity seam while still
/// returning the existing SQLite-backed trait object.
pub struct StoreHandle {
    pub store: Arc<dyn Store>,
    pub identity: ClusterIdentity,
}

/// Open the unchanged SQLite production store and initialize the node-local
/// identity before any cache/offline cleanup or background work can start.
pub async fn open_store(config: &Config) -> Result<StoreHandle, StoreError> {
    std::fs::create_dir_all(&config.storage.data_dir).map_err(|error| {
        StoreError::Identity(format!(
            "creating data directory {}: {error}",
            config.storage.data_dir.display()
        ))
    })?;
    let db_path = config.storage.data_dir.join("plurx.db");
    let sqlite = SqliteStore::open(&db_path)?;
    let cluster_id = sqlite.instance_id().await?;
    let identity = initialize_identity(&config.storage.data_dir, &cluster_id)?;

    Ok(StoreHandle {
        store: Arc::new(sqlite),
        identity,
    })
}

/// Read or atomically create this process's local identity.
///
/// The initial value is the logical id on purpose: all cache locations and
/// offline packages written before M0 use that exact string as their owner.
/// Seeding rather than minting preserves those rows and their bytes without a
/// data migration.
pub fn initialize_identity(
    data_dir: &Path,
    cluster_id: &str,
) -> Result<ClusterIdentity, StoreError> {
    validate_uuid("instance.id", cluster_id)?;
    std::fs::create_dir_all(data_dir).map_err(|error| {
        StoreError::Identity(format!(
            "creating data directory {}: {error}",
            data_dir.display()
        ))
    })?;

    let path = data_dir.join(NODE_ID_FILENAME);
    let node_id = match read_node_id(&path) {
        Ok(id) => id,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_node_id_noclobber(data_dir, &path, cluster_id)?;
            read_node_id(&path).map_err(|error| identity_io("reading", &path, error))?
        }
        Err(error) => return Err(identity_io("reading", &path, error)),
    };
    validate_uuid("node.id", &node_id)?;

    Ok(ClusterIdentity {
        cluster_id: cluster_id.to_owned(),
        node_id,
        raft_id: SINGLE_VOTER_RAFT_ID,
    })
}

fn read_node_id(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let id = raw.trim();
    if id.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "the file is empty",
        ));
    }
    Ok(id.to_owned())
}

/// Publish a complete, fsynced file without ever replacing an identity another
/// process won the race to create.
fn create_node_id_noclobber(
    data_dir: &Path,
    destination: &Path,
    seed: &str,
) -> Result<(), StoreError> {
    let temporary = temporary_path(data_dir);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let result = (|| -> Result<(), StoreError> {
        let mut file = options
            .open(&temporary)
            .map_err(|error| identity_io("creating", &temporary, error))?;
        file.write_all(seed.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|error| identity_io("writing", &temporary, error))?;

        #[cfg(unix)]
        {
            let mode = file
                .metadata()
                .map_err(|error| identity_io("reading metadata for", &temporary, error))?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o600 {
                return Err(StoreError::Identity(format!(
                    "{} was created with mode {mode:o}, expected 600",
                    temporary.display()
                )));
            }
        }
        drop(file);

        match std::fs::hard_link(&temporary, destination) {
            Ok(()) => sync_directory(data_dir),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(identity_io("publishing", destination, error)),
        }
    })();

    let cleanup = std::fs::remove_file(&temporary);
    if let Err(error) = cleanup {
        if error.kind() != ErrorKind::NotFound && result.is_ok() {
            return Err(identity_io("removing temporary", &temporary, error));
        }
    }
    result
}

fn temporary_path(data_dir: &Path) -> PathBuf {
    data_dir.join(format!(".{NODE_ID_FILENAME}.{}.tmp", uuid::Uuid::new_v4()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| identity_io("syncing directory", path, error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn validate_uuid(label: &str, value: &str) -> Result<(), StoreError> {
    let id = uuid::Uuid::parse_str(value)
        .map_err(|error| StoreError::Identity(format!("{label} is not a UUID: {error}")))?;
    if id.is_nil() {
        return Err(StoreError::Identity(format!("{label} must not be nil")));
    }
    Ok(())
}

fn identity_io(action: &str, path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Identity(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_directories_seed_distinct_node_ids_from_their_cluster_ids() {
        let first_dir = tempfile::tempdir().expect("first data dir");
        let second_dir = tempfile::tempdir().expect("second data dir");
        let first_cluster = uuid::Uuid::new_v4().to_string();
        let second_cluster = uuid::Uuid::new_v4().to_string();

        let first = initialize_identity(first_dir.path(), &first_cluster).expect("first identity");
        let second =
            initialize_identity(second_dir.path(), &second_cluster).expect("second identity");

        assert_eq!(first.node_id, first.cluster_id);
        assert_eq!(second.node_id, second.cluster_id);
        assert_ne!(first.node_id, second.node_id);
        assert_eq!(first.raft_id, SINGLE_VOTER_RAFT_ID);
    }

    #[test]
    fn restart_preserves_an_existing_local_node_id() {
        let dir = tempfile::tempdir().expect("data dir");
        let cluster_id = uuid::Uuid::new_v4().to_string();
        let first = initialize_identity(dir.path(), &cluster_id).expect("initialize");
        let second = initialize_identity(dir.path(), &cluster_id).expect("restart");

        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(NODE_ID_FILENAME))
                .expect("node id file")
                .trim(),
            cluster_id
        );

        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(dir.path().join(NODE_ID_FILENAME))
                .expect("node id metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn an_existing_joiner_identity_is_not_replaced_by_the_cluster_id() {
        let dir = tempfile::tempdir().expect("data dir");
        let cluster_id = uuid::Uuid::new_v4().to_string();
        let node_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(dir.path().join(NODE_ID_FILENAME), format!("{node_id}\n"))
            .expect("preseed node id");

        let identity = initialize_identity(dir.path(), &cluster_id).expect("identity");
        assert_eq!(identity.cluster_id, cluster_id);
        assert_eq!(identity.node_id, node_id);
    }

    #[test]
    fn concurrent_initializers_converge_without_overwriting() {
        let dir = tempfile::tempdir().expect("data dir");
        let cluster_id = uuid::Uuid::new_v4().to_string();
        let handles = (0..8)
            .map(|_| {
                let path = dir.path().to_owned();
                let seed = cluster_id.clone();
                std::thread::spawn(move || initialize_identity(&path, &seed))
            })
            .collect::<Vec<_>>();

        for handle in handles {
            let identity = handle.join().expect("thread").expect("identity");
            assert_eq!(identity.node_id, cluster_id);
        }
        let leftovers = std::fs::read_dir(dir.path())
            .expect("list data dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "temporary identity files must be removed");
    }

    #[test]
    fn invalid_identity_files_fail_closed() {
        let dir = tempfile::tempdir().expect("data dir");
        let cluster_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(dir.path().join(NODE_ID_FILENAME), "copied-node\n")
            .expect("write invalid id");

        let error = initialize_identity(dir.path(), &cluster_id).expect_err("invalid node id");
        assert!(error.to_string().contains("node.id is not a UUID"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(NODE_ID_FILENAME)).expect("preserved"),
            "copied-node\n",
            "startup must not silently replace a copied or corrupt identity"
        );
    }
}

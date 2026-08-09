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

pub mod migration;

pub const NODE_ID_FILENAME: &str = "node.id";
/// M0 runs the existing local store as the first and only voter.
pub const SINGLE_VOTER_RAFT_ID: u64 = 1;

/// The three identities that must not collapse into one value once clustering
/// starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterIdentity {
    /// Stable logical-server identifier stored as `instance.id`.
    pub cluster_id: String,
    /// Stable application-node identifier stored only in `<data_dir>/node.id`.
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
    if identity.node_id != cluster_id {
        let legacy_rows = sqlite.local_ownership_rows(&cluster_id).await?;
        if legacy_rows > 0 {
            return Err(StoreError::Identity(format!(
                "node.id {} differs from instance.id {cluster_id}, but {legacy_rows} cache or \
                 offline row(s) still use instance.id; refusing to strand owned bytes",
                identity.node_id
            )));
        }
        tracing::warn!(
            node_id = %identity.node_id,
            cluster_id,
            "using a node-local identity distinct from instance.id"
        );
    }

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
    std::fs::create_dir_all(data_dir).map_err(|error| {
        StoreError::Identity(format!(
            "creating data directory {}: {error}",
            data_dir.display()
        ))
    })?;

    let path = data_dir.join(NODE_ID_FILENAME);
    let raw_node_id = match read_node_id(&path) {
        Ok(id) => id,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_node_id_noclobber(data_dir, &path, cluster_id)?;
            read_node_id(&path).map_err(|error| identity_io("reading", &path, error))?
        }
        Err(error) => return Err(identity_io("reading", &path, error)),
    };
    let node_id = normalize_node_id(cluster_id, &raw_node_id)?;

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
            if !mode_is_owner_only(mode) {
                tracing::warn!(
                    path = %temporary.display(),
                    mode = format_args!("{mode:o}"),
                    "cluster identity filesystem exposed group or other permissions despite requesting mode 600"
                );
            }
        }
        drop(file);

        match std::fs::hard_link(&temporary, destination) {
            Ok(()) => sync_directory(data_dir),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
            Err(error) if hard_links_unavailable(&error) => {
                publish_with_rename_fallback(data_dir, &temporary, destination)
            }
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

fn hard_links_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::Unsupported
    )
}

#[cfg(unix)]
fn mode_is_owner_only(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Best-effort no-clobber fallback for filesystems that cannot create hard
/// links. A create-new destination probe elects one publisher; only that
/// process may replace its own empty placeholder with the already-fsynced
/// temporary file.
fn publish_with_rename_fallback(
    data_dir: &Path,
    temporary: &Path,
    destination: &Path,
) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    match options.open(destination) {
        Ok(placeholder) => {
            drop(placeholder);
            std::fs::rename(temporary, destination)
                .map_err(|error| identity_io("publishing", destination, error))?;
            sync_directory(data_dir)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(identity_io("reserving", destination, error)),
    }
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

fn validate_uuid(label: &str, value: &str) -> Result<uuid::Uuid, StoreError> {
    let id = uuid::Uuid::parse_str(value)
        .map_err(|error| StoreError::Identity(format!("{label} is not a UUID: {error}")))?;
    if id.is_nil() {
        return Err(StoreError::Identity(format!("{label} must not be nil")));
    }
    Ok(id)
}

fn normalize_node_id(cluster_id: &str, node_id: &str) -> Result<String, StoreError> {
    if node_id == cluster_id {
        return Ok(cluster_id.to_owned());
    }

    let parsed = validate_uuid("node.id", node_id)?;
    if uuid::Uuid::parse_str(cluster_id).is_ok_and(|cluster| cluster == parsed) {
        // Preserve the exact legacy ownership key stored in instance.id even
        // if an operator wrote an equivalent UUID spelling to node.id.
        Ok(cluster_id.to_owned())
    } else {
        Ok(parsed.to_string())
    }
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
    fn equivalent_uuid_spellings_preserve_the_exact_legacy_ownership_key() {
        let dir = tempfile::tempdir().expect("data dir");
        let cluster_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            dir.path().join(NODE_ID_FILENAME),
            format!("{}\n", cluster_id.to_uppercase()),
        )
        .expect("preseed non-canonical node id");

        let identity = initialize_identity(dir.path(), &cluster_id).expect("identity");
        assert_eq!(identity.node_id, cluster_id);
    }

    #[test]
    fn legacy_non_uuid_instance_ids_are_seeded_byte_for_byte() {
        let dir = tempfile::tempdir().expect("data dir");
        let identity = initialize_identity(dir.path(), "fixture-instance").expect("identity");

        assert_eq!(identity.cluster_id, "fixture-instance");
        assert_eq!(identity.node_id, "fixture-instance");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(NODE_ID_FILENAME)).expect("node id"),
            "fixture-instance\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stricter_owner_only_modes_are_accepted() {
        for mode in [0o600, 0o400, 0o200, 0o000] {
            assert!(mode_is_owner_only(mode));
        }
        for mode in [0o640, 0o604, 0o666] {
            assert!(!mode_is_owner_only(mode));
        }
    }

    #[test]
    fn rename_fallback_publishes_complete_bytes_without_replacing_a_winner() {
        let dir = tempfile::tempdir().expect("data dir");
        let temporary = dir.path().join("candidate");
        let destination = dir.path().join(NODE_ID_FILENAME);
        std::fs::write(&temporary, "candidate\n").expect("candidate");

        publish_with_rename_fallback(dir.path(), &temporary, &destination).expect("publish");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("published identity"),
            "candidate\n"
        );

        let loser = dir.path().join("loser");
        std::fs::write(&loser, "loser\n").expect("loser");
        publish_with_rename_fallback(dir.path(), &loser, &destination)
            .expect("existing winner is accepted");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("preserved winner"),
            "candidate\n"
        );
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

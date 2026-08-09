//! Backend-neutral behavioral contract for the durable store boundary.
//!
//! Every scenario receives only `Arc<dyn Store>` and runs against both SQLite
//! modes. With `hiqlite-store`, the same scenarios also run through a remote
//! client backed by three separate voter processes.

#[cfg(feature = "hiqlite-store")]
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::future::Future;
#[cfg(feature = "hiqlite-store")]
use std::io::{BufRead, BufReader, Write};
#[cfg(feature = "hiqlite-store")]
use std::net::TcpListener;
use std::path::PathBuf;
#[cfg(feature = "hiqlite-store")]
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
#[cfg(feature = "hiqlite-store")]
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "hiqlite-store")]
use hiqlite::tls::ServerTlsConfig;
#[cfg(feature = "hiqlite-store")]
use hiqlite::{Client, Node, NodeConfig};
#[cfg(feature = "hiqlite-store")]
use plurx_core::cluster::migration::prepare_sqlite_import;
use plurx_core::domain::{
    scopes, ArtworkAttempt, ItemEdit, ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem,
    NewLibrary, NewOfflinePackage, OfflineCreateOutcome, OfflineLeaseOutcome, PlaybackEvent,
    PlaybackEventQuery, ProbeResult, TraktAuth,
};
#[cfg(feature = "hiqlite-store")]
use plurx_core::store::{
    HiqliteAuthStore, MediaStore, PlaybackTelemetryStore, UserStore, WatchStore,
};
use plurx_core::store::{OutboxEntry, ReconcileOutcome, RootFingerprintStatus, SqliteStore, Store};
#[cfg(feature = "hiqlite-store")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "hiqlite-store")]
use tokio::io::AsyncReadExt;

#[cfg(feature = "hiqlite-store")]
const CONTRACT_RAFT_SECRET: &str = "plurx-store-contract-raft";
#[cfg(feature = "hiqlite-store")]
const CONTRACT_API_SECRET: &str = "plurx-store-contract-api";
#[cfg(feature = "hiqlite-store")]
const CONTRACT_INSTANCE_ID: &str = "00000000-0000-4000-8000-000000000090";

#[cfg(feature = "hiqlite-store")]
static HIQLITE_CASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SETTINGS_METHODS: &[&str] = &["ping", "get_setting", "put_setting", "instance_id"];
const USER_METHODS: &[&str] = &[
    "count_users",
    "create_user",
    "get_user",
    "get_user_by_username",
    "list_users",
    "delete_user",
    "count_admins",
    "set_password",
    "set_admin",
    "delete_tokens_for_user",
    "create_token",
    "user_for_token",
    "delete_token",
];
const LIBRARY_METHODS: &[&str] = &[
    "create_library",
    "update_library",
    "delete_library",
    "set_library_schedule",
    "mark_library_scanned",
    "get_library",
    "list_libraries",
];
const MEDIA_METHODS: &[&str] = &[
    "item_by_external_id",
    "find_movie",
    "find_show",
    "find_season",
    "find_episode",
    "find_child_item",
    "insert_item",
    "get_item",
    "get_item_children",
    "list_top_items_in_genre",
    "list_top_items",
    "recently_added",
    "search_items",
    "apply_metadata",
    "items_needing_metadata",
    "episodes_for_show",
    "items_needing_artwork",
    "items_missing_artwork",
    "items_missing_genres",
    "update_item_fields",
    "set_nfo_seeded",
    "get_file_by_path",
    "upsert_file",
    "get_file",
    "media_shape",
    "files_for_item",
    "child_counts",
    "item_max_heights",
    "item_media_facts",
    "set_file_audio_offset",
    "get_file_probe_json",
    "merge_file_probe_chapters",
    "files_missing_probe",
    "library_file_paths",
    "ensure_library_root_fingerprint",
    "reset_library_root_fingerprint",
    "rebuild_search_index",
    "reconcile_library",
    "delete_files",
    "prune_empty_items",
];
const WATCH_METHODS: &[&str] = &[
    "watch_state",
    "watch_map",
    "put_progress",
    "put_progress_if_current",
    "put_progress_at",
    "set_watched",
    "set_watched_tree",
    "watch_rollup",
    "watch_rollups",
    "continue_watching",
    "next_up",
    "apply_remote_watch",
];
const TRAKT_METHODS: &[&str] = &[
    "get_trakt_auth",
    "list_trakt_auth",
    "put_trakt_auth",
    "delete_trakt_auth",
    "delete_trakt_auth_if_current",
    "update_trakt_tokens",
    "set_trakt_sync",
    "trakt_sync_candidates",
];
const API_KEY_METHODS: &[&str] = &[
    "create_api_key",
    "list_api_keys",
    "api_key_for_hash",
    "touch_api_key",
    "delete_api_key",
    "set_api_key_disabled",
];
const OUTBOX_METHODS: &[&str] = &[
    "enqueue_watched",
    "due_watched",
    "settle_watched",
    "watched_outbox_counts",
];
const CACHE_METHODS: &[&str] = &[
    "cache_hit",
    "claim_cache_entry",
    "touch_cache_claim",
    "complete_cache_entry",
    "touch_cache_entry",
    "cache_by_age",
    "stale_cache_claims",
    "all_cache_rows",
    "forget_cache_entry",
    "cache_bytes",
];
const OFFLINE_METHODS: &[&str] = &[
    "create_offline_package",
    "offline_package_for_user",
    "renew_offline_package_for_user",
    "offline_activity_packages",
    "offline_package_stats",
    "reset_interrupted_offline_packages",
    "claim_next_offline_package",
    "requeue_offline_package",
    "set_offline_package_recipe",
    "update_offline_progress",
    "fail_offline_package",
    "put_offline_lease",
    "offline_package_for_lease",
    "mark_offline_package_ready",
    "delete_offline_package",
    "expire_offline_packages",
];
const TELEMETRY_METHODS: &[&str] = &[
    "record_playback_event",
    "prune_playback_events",
    "playback_events",
];

struct StoreFixture {
    name: &'static str,
    store: Arc<dyn Store>,
    _directory: Option<tempfile::TempDir>,
}

fn sqlite_fixtures() -> Vec<StoreFixture> {
    let directory = tempfile::tempdir().expect("file-backed contract directory");
    let file_store =
        SqliteStore::open(&directory.path().join("plurx.db")).expect("file-backed contract store");
    vec![
        StoreFixture {
            name: "memory",
            store: Arc::new(SqliteStore::open_in_memory().expect("in-memory contract store")),
            _directory: None,
        },
        StoreFixture {
            name: "file",
            store: Arc::new(file_store),
            _directory: Some(directory),
        },
    ]
}

async fn for_each_backend<F, Fut>(mut contract: F)
where
    F: FnMut(Arc<dyn Store>, &'static str) -> Fut,
    Fut: Future<Output = ()>,
{
    for fixture in sqlite_fixtures() {
        contract(Arc::clone(&fixture.store), fixture.name).await;
    }

    #[cfg(feature = "hiqlite-store")]
    {
        let _case = HIQLITE_CASE.lock().await;
        let store = open_contract_hiqlite_store().await;
        store
            .validation_reset_contract_state()
            .await
            .expect("reset replicated contract state");
        contract(Arc::new(store), "hiqlite-3-voter").await;
    }
}

#[cfg(feature = "hiqlite-store")]
async fn open_contract_hiqlite_store() -> HiqliteAuthStore {
    let cluster = contract_cluster();
    let client = Client::remote(
        cluster.addresses.clone(),
        true,
        true,
        CONTRACT_API_SECRET.to_owned(),
        true,
        None,
    )
    .await
    .expect("connect contract client to three voters");
    let telemetry_path = cluster._root.path().join("contract-client-telemetry.db");
    HiqliteAuthStore::bootstrap(client, CONTRACT_INSTANCE_ID, &telemetry_path)
        .await
        .expect("bootstrap contract store")
}

#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContractNodeSpec {
    id: u64,
    raft: String,
    api: String,
}

#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContractNodeLaunch {
    node_id: u64,
    root: PathBuf,
    nodes: Vec<ContractNodeSpec>,
}

#[cfg(feature = "hiqlite-store")]
struct ContractNodeProcess {
    _child: Child,
    _input: ChildStdin,
    _output: ChildStdout,
}

#[cfg(feature = "hiqlite-store")]
struct ContractCluster {
    addresses: Vec<String>,
    _root: tempfile::TempDir,
    _nodes: Vec<ContractNodeProcess>,
}

#[cfg(feature = "hiqlite-store")]
fn contract_cluster() -> &'static ContractCluster {
    static CLUSTER: OnceLock<ContractCluster> = OnceLock::new();
    CLUSTER.get_or_init(ContractCluster::start)
}

#[cfg(feature = "hiqlite-store")]
impl ContractCluster {
    fn start() -> Self {
        install_contract_crypto_provider();
        let root = tempfile::tempdir().expect("three-voter contract root");
        let specs = (1..=3)
            .map(|id| ContractNodeSpec {
                id,
                raft: format!("127.0.0.1:{}", contract_free_port()),
                api: format!("127.0.0.1:{}", contract_free_port()),
            })
            .collect::<Vec<_>>();
        let executable = std::env::current_exe().expect("contract test executable");
        let mut starting = Vec::new();
        for node_id in 1..=3 {
            let launch = ContractNodeLaunch {
                node_id,
                root: root.path().to_path_buf(),
                nodes: specs.clone(),
            };
            let mut child = Command::new(&executable)
                .arg("hiqlite_contract_node_process")
                .arg("--ignored")
                .arg("--exact")
                .arg("--nocapture")
                .env(
                    "PLURX_CONTRACT_NODE_LAUNCH",
                    serde_json::to_string(&launch).expect("serialize node launch"),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn contract voter");
            let input = child.stdin.take().expect("contract voter stdin");
            let output = child.stdout.take().expect("contract voter stdout");
            starting.push((node_id, child, input, output));
        }

        let mut nodes = Vec::new();
        for (node_id, child, input, mut output) in starting {
            let mut reader = BufReader::new(output);
            let mut line = String::new();
            loop {
                line.clear();
                let bytes = reader
                    .read_line(&mut line)
                    .expect("read contract voter startup");
                assert!(bytes > 0, "contract voter {node_id} exited before ready");
                if line.trim() == format!("PLURX_CONTRACT_NODE_READY {node_id}") {
                    break;
                }
            }
            output = reader.into_inner();
            nodes.push(ContractNodeProcess {
                _child: child,
                _input: input,
                _output: output,
            });
        }
        Self {
            addresses: specs.into_iter().map(|node| node.api).collect(),
            _root: root,
            _nodes: nodes,
        }
    }
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawned by the backend-neutral contract factory"]
async fn hiqlite_contract_node_process() {
    install_contract_crypto_provider();
    let launch: ContractNodeLaunch = serde_json::from_str(
        &std::env::var("PLURX_CONTRACT_NODE_LAUNCH").expect("contract node launch"),
    )
    .expect("decode contract node launch");
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;
    let data_dir = launch.root.join(format!("node-{}", launch.node_id));
    std::fs::create_dir_all(&data_dir).expect("contract node data directory");
    let client = hiqlite::start_node(NodeConfig {
        node_id: launch.node_id,
        nodes: launch
            .nodes
            .iter()
            .map(|node| Node {
                id: node.id,
                addr_raft: node.raft.clone(),
                addr_api: node.api.clone(),
            })
            .collect(),
        listen_addr_api: Cow::Borrowed("127.0.0.1"),
        listen_addr_raft: Cow::Borrowed("127.0.0.1"),
        data_dir: Cow::Owned(data_dir.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed("contract.db"),
        secret_raft: CONTRACT_RAFT_SECRET.to_owned(),
        secret_api: CONTRACT_API_SECRET.to_owned(),
        tls_raft: Some(ServerTlsConfig::TlsAutoCertificates),
        tls_api: Some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: 2 * 1024 * 1024,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    })
    .await
    .expect("start contract voter");
    tokio::time::timeout(Duration::from_secs(45), client.wait_until_healthy_db())
        .await
        .expect("contract voter health timeout");
    println!("PLURX_CONTRACT_NODE_READY {}", launch.node_id);
    std::io::stdout().flush().expect("flush contract readiness");
    let mut sink = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut sink)
        .await
        .expect("wait for contract parent");
    std::process::exit(0);
}

#[cfg(feature = "hiqlite-store")]
fn contract_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind contract port")
        .local_addr()
        .expect("contract port address")
        .port()
}

#[cfg(feature = "hiqlite-store")]
fn install_contract_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(feature = "hiqlite-store")]
fn populated_current_import_fixture(data_dir: &std::path::Path) -> PathBuf {
    let path = data_dir.join("plurx.db");
    drop(SqliteStore::open(&path).expect("create current SQLite fixture"));

    let connection = rusqlite::Connection::open(&path).expect("open SQLite import fixture");
    connection
        .execute(
            "UPDATE settings SET value = ?1, updated_at = 101 WHERE key = 'instance.id'",
            [CONTRACT_INSTANCE_ID],
        )
        .expect("set import fixture identity");
    connection
        .execute_batch(
            "INSERT INTO settings (key, value, updated_at)
                 VALUES ('migration.fixture', 'v14', 102);
             INSERT INTO users (id, username, password_hash, is_admin, created_at)
                 VALUES (7, 'Import Admin', 'fixture-password-hash', 1, 103);
             INSERT INTO tokens (token_hash, user_id, device, created_at, last_seen_at)
                 VALUES ('fixture-token-hash', 7, 'fixture-device', 104, 105);
             INSERT INTO api_keys
                 (id, name, key_hash, scopes, created_at, last_used_at, disabled)
                 VALUES (8, 'fixture key', 'fixture-key-hash', '[\"status:read\"]',
                         106, 107, 0);
             INSERT INTO libraries
                 (id, name, kind, paths, anime, created_at, scan_interval_mins,
                  refresh_interval_mins, last_scan_at, last_refresh_at)
                 VALUES (9, 'Imported Shows', 'shows', '[\"/fixture/shows\"]', 0,
                         108, 30, 60, 109, 110);
             INSERT INTO items
                 (id, library_id, kind, parent_id, title, sort_title, year, overview,
                  added_at, updated_at, tags, genres)
                 VALUES (20, 9, 'show', NULL, 'Imported Show', 'Imported Show', 2024,
                         'fixture parent', 111, 112, '[\"fixture\"]', '[\"Drama\"]');
             INSERT INTO items
                 (id, library_id, kind, parent_id, title, sort_title, season_number,
                  added_at, updated_at, tags, genres)
                 VALUES (10, 9, 'season', 20, 'Season 1', 'Season 1', 1,
                         113, 114, '[]', '[]');
             INSERT INTO files
                 (id, item_id, path, size, mtime, duration_ms, container, video_codec,
                  audio_streams, subtitle_streams, scanned_at, audio_offset_ms)
                 VALUES (30, 10, '/fixture/shows/season-1.mkv', 4096, 115, 3600000,
                         'matroska', 'h264', '[]', '[]', 116, 25);
             INSERT INTO watch_state
                 (user_id, item_id, position_ms, duration_ms, watched, updated_at)
                 VALUES (7, 10, 120000, 3600000, 0, 117);
             INSERT INTO trakt_auth
                 (user_id, access_token, refresh_token, expires_at, trakt_username,
                  connected_at, last_sync_at, last_activities)
                 VALUES (7, 'fixture-access', 'fixture-refresh', 999999,
                         'fixture-user', 118, 119, '{\"movies\":1}');
             INSERT INTO watched_outbox
                 (id, payload, attempts, last_error, status, next_at, created_at,
                  updated_at, claim_until)
                 VALUES (40, '{\"fixture\":true}', 1, '', 'pending', 120, 121, 122, 130);
             INSERT INTO transcode_cache_recipes
                 (recipe_hash, file_id, recipe_version, created_at)
                 VALUES ('fixture-recipe', 30, 1, 123);
             INSERT INTO transcode_cache_locations
                 (recipe_hash, node_id, storage_class, relative_dir, bytes, complete,
                  last_used_at, last_seen_at)
                 VALUES ('fixture-recipe', 'fixture-node', 'local', 'fixture-recipe',
                         2048, 1, 124, 125);
             INSERT INTO offline_packages
                 (id, request_id, user_id, file_id, node_id, source_path, source_size,
                  source_mtime, target_height, subtitle_mode, state, phase, expires_at)
                 VALUES ('fixture-package', 'fixture-request', 7, 30, 'fixture-node',
                         '/fixture/shows/season-1.mkv', 4096, 115, 720, 'none',
                         'ready', 'complete', 999999);
             INSERT INTO offline_package_leases
                 (token_hash, package_id, created_at, last_access_at, expires_at)
                 VALUES ('fixture-lease-hash', 'fixture-package', 126, 127, 999999);
             INSERT INTO library_roots (library_id, fingerprint)
                 VALUES (9, 'fixture-root-fingerprint');
             INSERT INTO scan_reconcile_guards (library_id) VALUES (9);
             INSERT INTO scan_reconcile_items (library_id, item_id) VALUES (9, 10);
             INSERT INTO playback_events
                 (at_unix_ms, user_id, file_id, event, detail)
                 VALUES (131000, 7, 30, 'fixture-local-only', 'must not replicate');",
        )
        .expect("populate SQLite import fixture");
    drop(connection);
    path
}

#[cfg(feature = "hiqlite-store")]
fn populated_v14_import_fixture(data_dir: &std::path::Path) -> PathBuf {
    let path = populated_current_import_fixture(data_dir);
    let connection = rusqlite::Connection::open(&path).expect("open current SQLite fixture");
    // Recreate the exact two schema differences between v14 and current.
    // The v15 tables and v17 node-local telemetry did not exist; v16 had not
    // added the recoverable outbox claim deadline yet.
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TRIGGER library_roots_paths_au;
             DROP TABLE scan_reconcile_items;
             DROP TABLE scan_reconcile_guards;
             DROP TABLE library_roots;
             DROP TABLE playback_events;
             DROP INDEX watched_outbox_due;
             ALTER TABLE watched_outbox RENAME TO watched_outbox_current;
             CREATE TABLE watched_outbox (
                 id INTEGER PRIMARY KEY,
                 payload TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT NOT NULL DEFAULT '',
                 status TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'ok', 'failed')),
                 next_at INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             ) STRICT;
             INSERT INTO watched_outbox
                 (id, payload, attempts, last_error, status, next_at, created_at, updated_at)
                 SELECT id, payload, attempts, last_error, status, next_at, created_at, updated_at
                 FROM watched_outbox_current;
             DROP TABLE watched_outbox_current;
             CREATE INDEX watched_outbox_due ON watched_outbox(status, next_at);
             PRAGMA user_version = 14;
             PRAGMA foreign_keys = ON;",
        )
        .expect("downgrade fixture shape to v14");
    drop(connection);
    path
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populated_v14_sqlite_import_has_exact_three_voter_parity() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated import target");

    let source = tempfile::tempdir().expect("SQLite import fixture directory");
    populated_v14_import_fixture(source.path());
    let prepared = prepare_sqlite_import(source.path()).expect("prepare v14 import backup");
    assert_eq!(prepared.schema_version, 14);

    let report = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect("import populated v14 backup");
    assert_eq!(report.source_schema_version, 14);
    assert_eq!(report.backup_sha256, prepared.backup_sha256);
    assert_eq!(report.tables.len(), 17);
    assert_eq!(report.search_rows, 2);
    assert!(report.imported_rows >= 16);
    for table in [
        "library_roots",
        "scan_reconcile_guards",
        "scan_reconcile_items",
    ] {
        assert_eq!(
            report
                .tables
                .iter()
                .find(|digest| digest.table == table)
                .expect("v14 compatibility table digest")
                .row_count,
            0,
            "{table} must be empty for a v14 source"
        );
    }
    assert_eq!(store.count_users().await.expect("imported user count"), 1);
    assert_eq!(
        store
            .get_file(30)
            .await
            .expect("read imported file")
            .expect("imported file")
            .audio_offset_ms,
        25
    );
    assert_eq!(
        store
            .watch_state(7, 10)
            .await
            .expect("read imported watch state")
            .expect("imported watch state")
            .position_ms,
        120_000
    );
    assert!(
        store
            .playback_events(&PlaybackEventQuery::default())
            .await
            .expect("read node-local telemetry")
            .is_empty(),
        "SQLite playback telemetry must never enter replicated state"
    );

    let retry = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect_err("a populated target must refuse a merge-style retry");
    assert!(retry.to_string().contains("target is not fresh"));
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populated_current_sqlite_import_preserves_new_durable_rows_only() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated current-schema import target");

    let source = tempfile::tempdir().expect("current SQLite import fixture directory");
    populated_current_import_fixture(source.path());
    let prepared = prepare_sqlite_import(source.path()).expect("prepare current import backup");
    assert_eq!(
        prepared.schema_version,
        plurx_core::store::SQLITE_SCHEMA_VERSION
    );

    let checksum_error = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &"0".repeat(64),
            prepared.schema_version,
        )
        .await
        .expect_err("wrong content hash must fail before import");
    assert!(checksum_error.to_string().contains("checksum changed"));

    let report = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect("import populated current backup");
    assert_eq!(report.search_rows, 2);
    for table in [
        "library_roots",
        "scan_reconcile_guards",
        "scan_reconcile_items",
    ] {
        assert_eq!(
            report
                .tables
                .iter()
                .find(|digest| digest.table == table)
                .expect("current compatibility table digest")
                .row_count,
            1,
            "{table} must survive a current-schema import"
        );
    }
    assert!(
        store
            .playback_events(&PlaybackEventQuery::default())
            .await
            .expect("read node-local telemetry")
            .is_empty(),
        "source playback telemetry must never enter the Hiqlite sidecar"
    );
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_import_verification_refusals_have_teeth() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated verification target");

    let mismatch = tempfile::tempdir().expect("identity mismatch fixture directory");
    let mismatch_path = populated_current_import_fixture(mismatch.path());
    rusqlite::Connection::open(&mismatch_path)
        .expect("open identity mismatch fixture")
        .execute(
            "UPDATE settings SET value = 'different-cluster' WHERE key = 'instance.id'",
            [],
        )
        .expect("change source identity");
    let mismatch = prepare_sqlite_import(mismatch.path()).expect("prepare identity mismatch");
    let identity_error = store
        .import_sqlite_backup(
            &mismatch.backup_path,
            &mismatch.backup_sha256,
            mismatch.schema_version,
        )
        .await
        .expect_err("identity mismatch must refuse import");
    assert!(matches!(
        identity_error,
        plurx_core::error::StoreError::Identity(_)
    ));

    let too_old = tempfile::tempdir().expect("old-schema fixture directory");
    let too_old_path = populated_current_import_fixture(too_old.path());
    rusqlite::Connection::open(&too_old_path)
        .expect("open old-schema fixture")
        .pragma_update(None, "user_version", 13)
        .expect("mark fixture as schema v13");
    let too_old = prepare_sqlite_import(too_old.path()).expect("prepare old-schema backup");
    let schema_error = store
        .import_sqlite_backup(
            &too_old.backup_path,
            &too_old.backup_sha256,
            too_old.schema_version,
        )
        .await
        .expect_err("schema v13 must refuse clustering import");
    assert!(schema_error
        .to_string()
        .contains("supports SQLite schemas v14"));

    let cycle = tempfile::tempdir().expect("item-cycle fixture directory");
    let cycle_path = populated_current_import_fixture(cycle.path());
    rusqlite::Connection::open(&cycle_path)
        .expect("open item-cycle fixture")
        .execute("UPDATE items SET parent_id = 10 WHERE id = 20", [])
        .expect("create an FK-clean item cycle");
    let cycle = prepare_sqlite_import(cycle.path()).expect("prepare item-cycle backup");
    let cycle_error = store
        .import_sqlite_backup(
            &cycle.backup_path,
            &cycle.backup_sha256,
            cycle.schema_version,
        )
        .await
        .expect_err("an item cycle must refuse import");
    assert!(cycle_error.to_string().contains("parent_id cycle"));

    store
        .validation_reset_contract_state()
        .await
        .expect("discard partial cycle import");
    let parity = tempfile::tempdir().expect("parity-fault fixture directory");
    populated_current_import_fixture(parity.path());
    let parity = prepare_sqlite_import(parity.path()).expect("prepare parity-fault backup");
    let parity_error = store
        .validation_import_sqlite_backup_with_parity_fault(
            &parity.backup_path,
            &parity.backup_sha256,
            parity.schema_version,
        )
        .await
        .expect_err("target corruption must fail parity");
    assert!(parity_error
        .to_string()
        .contains("table settings failed SQLite-to-Hiqlite parity"));
}

#[test]
fn contract_inventory_matches_every_store_method() {
    let source = include_str!("../src/store/mod.rs");
    let declared = source
        .lines()
        .filter_map(|line| line.strip_prefix("    async fn "))
        .filter_map(|line| line.split_once('(').map(|(name, _)| name))
        .collect::<BTreeSet<_>>();
    let covered = [
        SETTINGS_METHODS,
        USER_METHODS,
        LIBRARY_METHODS,
        MEDIA_METHODS,
        WATCH_METHODS,
        TRAKT_METHODS,
        API_KEY_METHODS,
        OUTBOX_METHODS,
        CACHE_METHODS,
        OFFLINE_METHODS,
        TELEMETRY_METHODS,
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect::<BTreeSet<_>>();

    assert_eq!(declared.len(), 123, "review the Store method count");
    assert_eq!(
        covered, declared,
        "the declared async method name inventory changed"
    );
}

#[tokio::test]
async fn playback_telemetry_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let first = PlaybackEvent {
            at_unix_ms: 1_700_000_000_000,
            user_id: Some(7),
            session_id: Some("session-a".to_owned()),
            event: "ttff".to_owned(),
            ms: Some(684),
            ..PlaybackEvent::default()
        };
        let second = PlaybackEvent {
            at_unix_ms: 1_700_000_001_000,
            user_id: Some(7),
            session_id: Some("session-a".to_owned()),
            event: "stall".to_owned(),
            ms: Some(4584),
            ..PlaybackEvent::default()
        };
        let first_id = store
            .record_playback_event(&first)
            .await
            .expect("record first telemetry row");
        let second_id = store
            .record_playback_event(&second)
            .await
            .expect("record second telemetry row");
        assert!(first_id > 0 && second_id > first_id, "backend {backend}");

        let ttff = store
            .playback_events(&PlaybackEventQuery {
                event: Some("ttff".to_owned()),
                limit: 10,
                ..PlaybackEventQuery::default()
            })
            .await
            .expect("query telemetry by event");
        assert_eq!(ttff.len(), 1, "backend {backend}");
        assert_eq!(ttff[0].id, first_id, "backend {backend}");

        let newest = store
            .playback_events(&PlaybackEventQuery {
                since_ms: Some(first.at_unix_ms),
                limit: 1,
                ..PlaybackEventQuery::default()
            })
            .await
            .expect("query newest telemetry row");
        assert_eq!(newest.len(), 1, "backend {backend}");
        assert_eq!(newest[0].id, second_id, "backend {backend}");

        assert_eq!(
            store
                .prune_playback_events(second.at_unix_ms, 1)
                .await
                .expect("bounded telemetry prune"),
            1,
            "backend {backend}"
        );
        let remaining = store
            .playback_events(&PlaybackEventQuery {
                limit: 10,
                ..PlaybackEventQuery::default()
            })
            .await
            .expect("query remaining telemetry");
        assert_eq!(remaining.len(), 1, "backend {backend}");
        assert_eq!(remaining[0].id, second_id, "backend {backend}");
    })
    .await;
}

#[tokio::test]
async fn settings_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        store.ping().await.expect("ping");
        assert_eq!(store.get_setting("contract.key").await.expect("get"), None);
        store
            .put_setting("contract.key", "first")
            .await
            .expect("insert setting");
        store
            .put_setting("contract.key", "second")
            .await
            .expect("update setting");
        assert_eq!(
            store.get_setting("contract.key").await.expect("get"),
            Some("second".to_owned()),
            "backend {backend}"
        );
        let instance_id = store.instance_id().await.expect("instance id");
        uuid::Uuid::parse_str(&instance_id).expect("new instance ids are UUIDs");
        assert_eq!(
            store.instance_id().await.expect("stable instance id"),
            instance_id,
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn user_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        assert_eq!(store.count_users().await.expect("count"), 0);
        let admin = store
            .create_user("Admin", "hash-1", true)
            .await
            .expect("create admin");
        let viewer = store
            .create_user("Viewer", "hash-2", false)
            .await
            .expect("create viewer");
        assert_eq!(store.count_users().await.expect("count"), 2);
        assert_eq!(store.count_admins().await.expect("admins"), 1);
        assert_eq!(
            store
                .get_user(admin.id)
                .await
                .expect("get")
                .expect("admin")
                .username,
            "Admin"
        );
        assert_eq!(
            store
                .get_user_by_username("viewer")
                .await
                .expect("lookup")
                .expect("viewer")
                .id,
            viewer.id
        );
        assert_eq!(store.list_users().await.expect("list").len(), 2);
        assert!(store
            .set_password(viewer.id, "hash-3")
            .await
            .expect("password"));
        assert!(store.set_admin(viewer.id, true).await.expect("promote"));
        assert_eq!(store.count_admins().await.expect("admins"), 2);

        store
            .create_token("token-one", viewer.id, Some("contract"))
            .await
            .expect("token");
        store
            .create_token("token-two", viewer.id, None)
            .await
            .expect("token");
        assert_eq!(
            store
                .user_for_token("token-one")
                .await
                .expect("resolve")
                .expect("token user")
                .id,
            viewer.id,
            "backend {backend}"
        );
        assert!(store.delete_token("token-one").await.expect("delete token"));
        assert_eq!(
            store
                .delete_tokens_for_user(viewer.id)
                .await
                .expect("delete user tokens"),
            1
        );
        assert!(store.delete_user(admin.id).await.expect("delete user"));
    })
    .await;
}

#[tokio::test]
async fn api_key_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let expected_scopes = vec![
            scopes::SCAN_TRIGGER.to_owned(),
            scopes::STATUS_READ.to_owned(),
        ];
        let key = store
            .create_api_key("automation", "key-hash", &expected_scopes)
            .await
            .expect("create key");
        assert_eq!(key.scopes, expected_scopes);
        let listed = store.list_api_keys().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scopes, expected_scopes, "backend {backend}");
        let looked_up = store
            .api_key_for_hash("key-hash")
            .await
            .expect("lookup")
            .expect("key");
        assert_eq!(looked_up.id, key.id);
        assert_eq!(looked_up.scopes, expected_scopes);
        store.touch_api_key(key.id).await.expect("touch");
        assert!(store
            .api_key_for_hash("key-hash")
            .await
            .expect("lookup touched key")
            .expect("touched key")
            .last_used_at
            .is_some());
        assert!(store
            .set_api_key_disabled(key.id, true)
            .await
            .expect("disable"));
        let disabled = store
            .api_key_for_hash("key-hash")
            .await
            .expect("lookup disabled key")
            .expect("disabled key");
        assert!(disabled.disabled);
        assert!(!disabled.allows(scopes::SCAN_TRIGGER));
        assert!(store.delete_api_key(key.id).await.expect("delete"));
    })
    .await;
}

#[tokio::test]
async fn library_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let library = store
            .create_library(&NewLibrary {
                name: "Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/contract/movies")],
                anime: false,
            })
            .await
            .expect("create library");
        assert_eq!(
            store
                .get_library(library.id)
                .await
                .expect("get")
                .expect("library")
                .name,
            "Contract Movies"
        );
        let updated = store
            .update_library(
                library.id,
                &NewLibrary {
                    name: "Contract Films".into(),
                    kind: LibraryKind::Movies,
                    paths: vec![PathBuf::from("/contract/films")],
                    anime: false,
                },
            )
            .await
            .expect("update")
            .expect("updated library");
        let scheduled = store
            .set_library_schedule(updated.id, 15, 1_440)
            .await
            .expect("schedule")
            .expect("scheduled library");
        assert_eq!(
            (
                scheduled.scan_interval_mins,
                scheduled.refresh_interval_mins
            ),
            (15, 1_440)
        );
        store
            .mark_library_scanned(scheduled.id, true)
            .await
            .expect("mark scanned");
        let scanned = store
            .get_library(scheduled.id)
            .await
            .expect("get scanned library")
            .expect("scanned library");
        assert!(scanned.last_scan_at.is_some());
        assert!(scanned.last_refresh_at.is_some());
        assert_eq!(store.list_libraries().await.expect("list").len(), 1);
        assert!(
            store.delete_library(scheduled.id).await.expect("delete"),
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn media_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let movies = store
            .create_library(&NewLibrary {
                name: "Media Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/contract/movies")],
                anime: false,
            })
            .await
            .expect("movie library");
        let shows = store
            .create_library(&NewLibrary {
                name: "Media Contract Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![PathBuf::from("/contract/shows")],
                anime: false,
            })
            .await
            .expect("show library");
        let home = store
            .create_library(&NewLibrary {
                name: "Media Contract Home".into(),
                kind: LibraryKind::Home,
                paths: vec![PathBuf::from("/contract/home")],
                anime: false,
            })
            .await
            .expect("home library");

        let movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "The Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let empty_movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Empty Contract Movie".into(),
                year: Some(2023),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("empty movie");
        let show = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Contract Show".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let episode = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "Pilot".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("episode");
        let folder = store
            .insert_item(&NewItem {
                library_id: home.id,
                kind: ItemKind::Folder,
                parent_id: None,
                title: "Trips".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("home folder");

        assert_eq!(
            store
                .find_movie(movies.id, "The Contract Movie", Some(2024))
                .await
                .expect("find movie")
                .expect("movie")
                .id,
            movie
        );
        assert_eq!(
            store
                .find_show(shows.id, "Contract Show", Some(2024))
                .await
                .expect("find show")
                .expect("show")
                .id,
            show
        );
        assert_eq!(
            store
                .find_season(show, 1)
                .await
                .expect("find season")
                .expect("season")
                .id,
            season
        );
        assert_eq!(
            store
                .find_episode(season, 1)
                .await
                .expect("find episode")
                .expect("episode")
                .id,
            episode
        );
        assert_eq!(
            store
                .find_child_item(home.id, None, ItemKind::Folder, "Trips")
                .await
                .expect("find child")
                .expect("folder")
                .id,
            folder
        );
        assert_eq!(
            store.get_item(movie).await.expect("get").expect("movie").id,
            movie
        );
        assert_eq!(
            store.get_item_children(show).await.expect("children").len(),
            1
        );

        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    overview: Some("A backend-neutral movie".into()),
                    tmdb_id: Some(42),
                    imdb_id: Some("tt0000042".into()),
                    runtime_ms: Some(7_200_000),
                    genres: Some(vec!["Drama".into()]),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Failed("contract fixture".into())),
                    ..Default::default()
                },
            )
            .await
            .expect("movie metadata");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    tmdb_id: Some(84),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("show metadata");
        assert_eq!(
            store
                .item_by_external_id(ItemKind::Movie, Some(42), None)
                .await
                .expect("external id")
                .expect("external movie")
                .id,
            movie
        );
        let metadata_queue = store
            .items_needing_metadata(Some(movies.id), false, None)
            .await
            .expect("metadata queue");
        assert!(metadata_queue.iter().any(|item| item.id == empty_movie));
        assert!(!metadata_queue.iter().any(|item| item.id == movie));
        assert_eq!(
            store.episodes_for_show(show).await.expect("episodes").len(),
            1
        );
        let home_artwork = store
            .items_needing_artwork(home.id, false, None)
            .await
            .expect("home artwork");
        assert!(home_artwork.iter().any(|item| item.id == folder));
        assert!(!home_artwork.iter().any(|item| item.id == movie));
        let missing_artwork = store
            .items_missing_artwork(Some(movies.id), 0, 10)
            .await
            .expect("missing artwork");
        assert!(missing_artwork.iter().any(|item| item.id == movie));
        assert!(!missing_artwork.iter().any(|item| item.id == show));
        let missing_genres = store
            .items_missing_genres(0, 10)
            .await
            .expect("missing genres");
        assert!(missing_genres.iter().any(|item| item.id == show));
        assert!(!missing_genres.iter().any(|item| item.id == movie));
        let edited = store
            .update_item_fields(
                folder,
                &ItemEdit {
                    title: Some("Edited Trips".into()),
                    tags: Some(vec!["family".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("edit")
            .expect("edited folder");
        assert_eq!(edited.title, "Edited Trips");
        store.set_nfo_seeded(folder).await.expect("NFO stamp");
        assert!(store
            .get_item(folder)
            .await
            .expect("get NFO-stamped item")
            .expect("NFO-stamped item")
            .nfo_seeded_at
            .is_some());

        let movie_file = store
            .upsert_file(
                movie,
                "/contract/movies/movie.mkv",
                1_000,
                10,
                &ProbeResult {
                    duration_ms: Some(7_200_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(3_840),
                    height: Some(2_160),
                    bitrate: Some(20_000_000),
                    raw_json: Some(r#"{"format":{"filename":"movie.mkv"}}"#.into()),
                    ..Default::default()
                },
            )
            .await
            .expect("movie file");
        let empty_file = store
            .upsert_file(
                empty_movie,
                "/contract/movies/empty.mkv",
                2_000,
                20,
                &ProbeResult::default(),
            )
            .await
            .expect("unprobed file");
        let episode_file = store
            .upsert_file(
                episode,
                "/contract/shows/pilot.mkv",
                3_000,
                30,
                &ProbeResult {
                    duration_ms: Some(3_600_000),
                    container: Some("mkv".into()),
                    video_codec: Some("h264".into()),
                    height: Some(1_080),
                    ..Default::default()
                },
            )
            .await
            .expect("episode file");
        assert_eq!(
            store
                .get_file_by_path("/contract/movies/movie.mkv")
                .await
                .expect("file by path")
                .expect("file")
                .id,
            movie_file
        );
        assert_eq!(
            store
                .get_file(movie_file)
                .await
                .expect("file")
                .expect("file")
                .id,
            movie_file
        );
        assert!(store.media_shape().await.expect("media shape").probed >= 2);
        assert_eq!(store.files_for_item(movie).await.expect("files").len(), 1);
        assert_eq!(
            store
                .child_counts(&[show])
                .await
                .expect("counts")
                .get(&show),
            Some(&1)
        );
        assert_eq!(
            store
                .item_max_heights(&[movie])
                .await
                .expect("heights")
                .get(&movie),
            Some(&2_160)
        );
        assert!(store
            .item_media_facts(&[movie])
            .await
            .expect("facts")
            .contains_key(&movie));
        store
            .set_file_audio_offset(movie_file, 125)
            .await
            .expect("audio offset");
        assert_eq!(
            store
                .get_file(movie_file)
                .await
                .expect("get offset file")
                .expect("offset file")
                .audio_offset_ms,
            125
        );
        assert!(store
            .get_file_probe_json(movie_file)
            .await
            .expect("probe JSON")
            .expect("probe JSON")
            .contains("movie.mkv"));
        store
            .merge_file_probe_chapters(movie_file, r#"[{"start_time":"0.0","end_time":"10.0"}]"#)
            .await
            .expect("merge chapters");
        assert!(store
            .get_file_probe_json(movie_file)
            .await
            .expect("probe JSON")
            .expect("probe JSON")
            .contains("chapters"));
        let missing_probes = store
            .files_missing_probe(Some(movies.id))
            .await
            .expect("missing probes");
        assert!(missing_probes.iter().any(|file| file.id == empty_file));
        assert!(!missing_probes.iter().any(|file| file.id == movie_file));
        assert!(!missing_probes.iter().any(|file| file.id == episode_file));
        assert_eq!(
            store
                .library_file_paths(movies.id)
                .await
                .expect("paths")
                .len(),
            2
        );

        let genre_page = store
            .list_top_items_in_genre(movies.id, ItemSort::Title, 0, 20, Some("drama"))
            .await
            .expect("genre page");
        assert!(genre_page.items.iter().any(|item| item.id == movie));
        assert!(!genre_page.items.iter().any(|item| item.id == empty_movie));
        assert_eq!(
            store
                .list_top_items(movies.id, ItemSort::Added, 0, 20)
                .await
                .expect("page")
                .total,
            2
        );
        assert!(!store
            .recently_added(None, 20)
            .await
            .expect("recent")
            .is_empty());
        let search_ids = store
            .search_items("backend-neutral", 20)
            .await
            .expect("search")
            .into_iter()
            .map(|item| item.item.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(search_ids, BTreeSet::from([movie]));

        assert_eq!(
            store
                .ensure_library_root_fingerprint(movies.id, "contract-root", true)
                .await
                .expect("establish root"),
            RootFingerprintStatus::Established
        );
        assert_eq!(
            store
                .ensure_library_root_fingerprint(movies.id, "contract-root", true)
                .await
                .expect("match root"),
            RootFingerprintStatus::Matched
        );
        assert!(matches!(
            store
                .reconcile_library(movies.id, "stale-root", &[empty_file], 1)
                .await
                .expect("reject root"),
            ReconcileOutcome::RefusedRoot { .. }
        ));
        assert!(store
            .get_file(empty_file)
            .await
            .expect("kept file")
            .is_some());
        assert_eq!(
            store
                .reconcile_library(movies.id, "contract-root", &[empty_file], 0)
                .await
                .expect("reject bound"),
            ReconcileOutcome::RefusedPrune {
                requested: 1,
                limit: 0
            }
        );
        assert!(store
            .get_file(empty_file)
            .await
            .expect("kept file")
            .is_some());
        assert!(matches!(
            store
                .reconcile_library(movies.id, "contract-root", &[empty_file], 1)
                .await
                .expect("bounded reconcile"),
            ReconcileOutcome::Applied {
                deleted_files: 1,
                pruned_items: 1..
            }
        ));
        assert!(store
            .reset_library_root_fingerprint(movies.id)
            .await
            .expect("reset root"));
        assert_eq!(
            store
                .ensure_library_root_fingerprint(movies.id, "contract-root", false)
                .await
                .expect("refuse empty root establishment"),
            RootFingerprintStatus::Unestablished
        );
        assert_eq!(
            store
                .ensure_library_root_fingerprint(movies.id, "contract-root", true)
                .await
                .expect("re-establish root"),
            RootFingerprintStatus::Established
        );
        assert!(store.rebuild_search_index().await.expect("rebuild search") > 0);
        assert_eq!(store.delete_files(&[]).await.expect("empty delete"), 0);
        let legacy_orphan = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Legacy Prune Contract Orphan".into(),
                year: Some(2022),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("legacy prune orphan");
        assert!(
            store
                .prune_empty_items(movies.id)
                .await
                .expect("legacy prune")
                >= 1,
            "backend {backend} did not report its real orphan prune"
        );
        assert!(
            store
                .get_item(legacy_orphan)
                .await
                .expect("get pruned orphan")
                .is_none(),
            "backend {backend} reported a prune without removing the fixture"
        );
        assert!(store
            .get_file(episode_file)
            .await
            .expect("episode file")
            .is_some());
        assert!(
            store
                .get_item(movie)
                .await
                .expect("movie remains")
                .is_some(),
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn watch_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let user = store
            .create_user("watch-contract", "hash", false)
            .await
            .expect("user");
        let movies = store
            .create_library(&NewLibrary {
                name: "Watch Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/watch/movies")],
                anime: false,
            })
            .await
            .expect("movies");
        let shows = store
            .create_library(&NewLibrary {
                name: "Watch Contract Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![PathBuf::from("/watch/shows")],
                anime: false,
            })
            .await
            .expect("shows");
        let movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Watch Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let show = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Watch Contract Show".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let mut episodes = Vec::new();
        for number in 1..=2 {
            let episode = store
                .insert_item(&NewItem {
                    library_id: shows.id,
                    kind: ItemKind::Episode,
                    parent_id: Some(season),
                    title: format!("Episode {number}"),
                    year: None,
                    season_number: Some(1),
                    episode_number: Some(number),
                })
                .await
                .expect("episode");
            store
                .upsert_file(
                    episode,
                    &format!("/watch/shows/e{number}.mkv"),
                    1_000,
                    i64::from(number),
                    &ProbeResult {
                        duration_ms: Some(1_000),
                        container: Some("mkv".into()),
                        ..Default::default()
                    },
                )
                .await
                .expect("episode file");
            episodes.push(episode);
        }
        store
            .upsert_file(
                movie,
                "/watch/movies/movie.mkv",
                1_000,
                1,
                &ProbeResult {
                    duration_ms: Some(10_000),
                    container: Some("mkv".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("movie file");

        assert!(store
            .watch_state(user.id, movie)
            .await
            .expect("watch state")
            .is_none());
        assert!(store
            .watch_map(user.id, &[movie])
            .await
            .expect("watch map")
            .is_empty());
        let leading = store
            .put_progress(user.id, movie, 4_000, Some(10_000))
            .await
            .expect("progress");
        let trailing = store
            .put_progress_if_current(user.id, movie, &leading, 5_000, Some(10_000))
            .await
            .expect("compare-and-set progress")
            .expect("unchanged row accepts trailing progress");
        assert_eq!(trailing.position_ms, 5_000);
        store
            .set_watched(user.id, movie, false)
            .await
            .expect("competing manual write");
        assert!(store
            .put_progress_if_current(user.id, movie, &trailing, 6_000, Some(10_000))
            .await
            .expect("stale compare-and-set")
            .is_none());
        store
            .put_progress(user.id, movie, 4_000, Some(10_000))
            .await
            .expect("restore progress");
        store
            .put_progress_at(user.id, episodes[0], 200, Some(1_000), Some(1))
            .await
            .expect("dated progress");
        assert!(store
            .watch_state(user.id, movie)
            .await
            .expect("watch state")
            .is_some());
        assert_eq!(
            store
                .watch_map(user.id, &[movie])
                .await
                .expect("watch map")
                .len(),
            1
        );
        let continuing = store
            .continue_watching(user.id, 10)
            .await
            .expect("continue watching");
        assert!(continuing.iter().any(|item| item.item.id == movie));
        assert!(!continuing.iter().any(|item| item.item.id == episodes[1]));

        store
            .set_watched(user.id, movie, true)
            .await
            .expect("set watched");
        let changed = store
            .set_watched_tree(user.id, show, true)
            .await
            .expect("set watched tree");
        assert_eq!(changed.len(), 2);
        let rollup = store.watch_rollup(user.id, show).await.expect("rollup");
        assert_eq!((rollup.watched, rollup.leaves), (2, 2));
        assert_eq!(
            store
                .watch_rollups(user.id, &[show, season])
                .await
                .expect("rollups")
                .len(),
            2
        );

        store
            .set_watched_tree(user.id, show, false)
            .await
            .expect("clear tree");
        store
            .apply_remote_watch(user.id, episodes[0], true, 1_000, Some(1_000), 10)
            .await
            .expect("remote watch");
        let next_up = store.next_up(user.id, 10).await.expect("next up");
        assert_eq!(
            next_up
                .into_iter()
                .map(|item| item.item.id)
                .collect::<Vec<_>>(),
            vec![episodes[1]],
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn trakt_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let user = store
            .create_user("trakt-contract", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Trakt Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/trakt/movies")],
                anime: false,
            })
            .await
            .expect("library");
        let movie = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Trakt Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(4242),
                    imdb_id: Some("tt0004242".into()),
                    runtime_ms: Some(7_200_000),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("metadata");
        store
            .put_progress(user.id, movie, 1_000, Some(7_200_000))
            .await
            .expect("watch state");

        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
        store
            .put_trakt_auth(&TraktAuth {
                user_id: user.id,
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: 100,
                trakt_username: Some("contract".into()),
                connected_at: 1,
                last_sync_at: 0,
                last_activities: None,
            })
            .await
            .expect("put auth");
        let linked = store
            .get_trakt_auth(user.id)
            .await
            .expect("get linked auth")
            .expect("linked auth");
        assert_eq!(linked.access_token, "access-1");
        assert_eq!(linked.refresh_token, "refresh-1");
        assert_eq!(linked.expires_at, 100);
        assert_eq!(linked.trakt_username.as_deref(), Some("contract"));
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);
        assert!(store
            .update_trakt_tokens(user.id, "refresh-1", "access-2", "refresh-2", 200)
            .await
            .expect("update tokens"));
        assert!(!store
            .update_trakt_tokens(user.id, "refresh-1", "loser", "loser-refresh", 300)
            .await
            .expect("reject stale refresh"));
        assert!(!store
            .delete_trakt_auth_if_current(user.id, "refresh-1")
            .await
            .expect("reject stale unlink"));
        let refreshed = store
            .get_trakt_auth(user.id)
            .await
            .expect("get refreshed auth")
            .expect("refreshed auth");
        assert_eq!(refreshed.access_token, "access-2");
        assert_eq!(refreshed.refresh_token, "refresh-2");
        assert_eq!(refreshed.expires_at, 200);
        store
            .set_trakt_sync(user.id, 50, Some(r#"{"movies":{"watched_at":50}}"#))
            .await
            .expect("set sync");
        let synced = store
            .get_trakt_auth(user.id)
            .await
            .expect("get synced auth")
            .expect("synced auth");
        assert_eq!(synced.last_sync_at, 50);
        assert_eq!(
            synced.last_activities.as_deref(),
            Some(r#"{"movies":{"watched_at":50}}"#)
        );
        let candidates = store
            .trakt_sync_candidates(user.id)
            .await
            .expect("candidates");
        assert_eq!(
            candidates
                .into_iter()
                .map(|candidate| candidate.item_id)
                .collect::<Vec<_>>(),
            vec![movie],
            "backend {backend}"
        );
        store.delete_trakt_auth(user.id).await.expect("delete auth");
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
    })
    .await;
}

#[tokio::test]
async fn watched_outbox_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let id = store
            .enqueue_watched(r#"{"type":"movie","watched":true}"#)
            .await
            .expect("enqueue");
        let mut due = store.due_watched(10).await.expect("due");
        assert_eq!(due.len(), 1, "backend {backend}");
        assert!(
            store
                .due_watched(10)
                .await
                .expect("claimed row is not due")
                .is_empty(),
            "backend {backend} returned one claim to two workers"
        );
        let entry = due.pop().expect("entry");
        assert_eq!(entry.id, id);
        store
            .settle_watched(&OutboxEntry {
                attempts: 1,
                status: "ok".into(),
                ..entry
            })
            .await
            .expect("settle");
        assert_eq!(
            store.watched_outbox_counts().await.expect("counts"),
            (0, 1, 0)
        );
    })
    .await;
}

async fn seed_file(store: &Arc<dyn Store>, prefix: &str) -> (i64, i64) {
    let user = store
        .create_user(&format!("{prefix}-user"), "hash", false)
        .await
        .expect("seed user");
    let library = store
        .create_library(&NewLibrary {
            name: format!("{prefix} Library"),
            kind: LibraryKind::Movies,
            paths: vec![PathBuf::from(format!("/{prefix}"))],
            anime: false,
        })
        .await
        .expect("seed library");
    let item = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("{prefix} Movie"),
            year: Some(2024),
            season_number: None,
            episode_number: None,
        })
        .await
        .expect("seed item");
    let file = store
        .upsert_file(
            item,
            &format!("/{prefix}/movie.mkv"),
            10_000,
            1,
            &ProbeResult {
                duration_ms: Some(7_200_000),
                container: Some("mkv".into()),
                ..Default::default()
            },
        )
        .await
        .expect("seed file");
    (user.id, file)
}

fn unix_seconds() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_secs(),
    )
    .expect("Unix time fits i64")
}

async fn wait_until_after(second: i64) {
    while unix_seconds() <= second {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn transcode_cache_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let (_, file) = seed_file(&store, "cache-contract").await;
        let node = "cache-node";
        assert!(store
            .cache_hit("recipe", node)
            .await
            .expect("miss")
            .is_none());
        assert!(store
            .claim_cache_entry("recipe", file, 1, node, "aa/recipe")
            .await
            .expect("claim"));
        assert!(!store
            .claim_cache_entry("recipe", file, 1, node, "bb/loser")
            .await
            .expect("duplicate claim"));
        let claimed = store
            .all_cache_rows(node)
            .await
            .expect("claimed row")
            .pop()
            .expect("one claimed row");
        wait_until_after(claimed.last_used_at).await;
        let stale_cutoff = unix_seconds();
        assert_eq!(
            store
                .stale_cache_claims(node, stale_cutoff)
                .await
                .expect("stale claim before touch")
                .len(),
            1
        );
        store
            .touch_cache_claim("recipe", node)
            .await
            .expect("touch claim");
        assert!(store
            .stale_cache_claims(node, i64::MIN)
            .await
            .expect("fresh claims")
            .is_empty());
        assert!(store
            .stale_cache_claims(node, stale_cutoff)
            .await
            .expect("touched claim")
            .is_empty());
        assert_eq!(
            store
                .stale_cache_claims(node, i64::MAX)
                .await
                .expect("stale claims")
                .len(),
            1
        );
        assert_eq!(store.all_cache_rows(node).await.expect("all rows").len(), 1);
        store
            .complete_cache_entry("recipe", node, 4_096)
            .await
            .expect("complete");
        assert!(store
            .cache_hit("recipe", node)
            .await
            .expect("hit")
            .is_some());
        let completed_at = store
            .cache_hit("recipe", node)
            .await
            .expect("completed hit")
            .expect("completed cache row")
            .last_used_at;
        wait_until_after(completed_at).await;
        store
            .touch_cache_entry("recipe", node)
            .await
            .expect("touch entry");
        assert!(
            store
                .cache_hit("recipe", node)
                .await
                .expect("touched hit")
                .expect("touched cache row")
                .last_used_at
                > completed_at
        );
        assert_eq!(store.cache_by_age(node, 10).await.expect("by age").len(), 1);
        assert_eq!(store.cache_bytes(node).await.expect("bytes"), 4_096);
        store
            .forget_cache_entry("recipe", node, "local")
            .await
            .expect("forget");
        assert!(
            store
                .all_cache_rows(node)
                .await
                .expect("all rows")
                .is_empty(),
            "backend {backend}"
        );
    })
    .await;
}

fn offline_request(id: &str, request_id: &str, user_id: i64, file_id: i64) -> NewOfflinePackage {
    NewOfflinePackage {
        id: id.into(),
        request_id: request_id.into(),
        user_id,
        file_id,
        node_id: "offline-node".into(),
        source_path: "/offline-contract/movie.mkv".into(),
        source_size: 10_000,
        source_mtime: 1,
        target_height: 1_080,
        output_width: Some(1_920),
        output_height: Some(1_080),
        audio_index: Some(0),
        audio_offset_ms: 0,
        subtitle_index: None,
        subtitle_language: None,
        subtitle_mode: "none".into(),
        estimated_bytes: 5_000,
        reserved_bytes: 5_000,
        expires_at: 10_000,
    }
}

#[tokio::test]
async fn offline_package_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let (user_id, file_id) = seed_file(&store, "offline-contract").await;
        let first = offline_request("package-1", "request-1", user_id, file_id);
        assert!(matches!(
            store
                .create_offline_package(&first, 10, 100_000, 100_000)
                .await
                .expect("create package"),
            OfflineCreateOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .create_offline_package(&first, 10, 100_000, 100_000)
                .await
                .expect("idempotent create"),
            OfflineCreateOutcome::Existing(_)
        ));
        let mut changed_expiry = first.clone();
        changed_expiry.expires_at += 1;
        let OfflineCreateOutcome::Existing(existing) = store
            .create_offline_package(&changed_expiry, 10, 100_000, 100_000)
            .await
            .expect("server-clock retry")
        else {
            panic!("backend {backend} rejected a retry after server-derived expiry advanced");
        };
        assert_eq!(existing.expires_at, first.expires_at);
        let mut changed_estimate = first.clone();
        changed_estimate.estimated_bytes += 1;
        assert_eq!(
            store
                .create_offline_package(&changed_estimate, 10, 100_000, 100_000)
                .await
                .expect("estimate conflict"),
            OfflineCreateOutcome::RequestConflict,
            "backend {backend} accepted changed estimate under one request id"
        );
        let mut changed_reservation = first.clone();
        changed_reservation.reserved_bytes += 1;
        assert_eq!(
            store
                .create_offline_package(&changed_reservation, 10, 100_000, 100_000)
                .await
                .expect("reservation conflict"),
            OfflineCreateOutcome::RequestConflict,
            "backend {backend} accepted changed reservation under one request id"
        );
        let mut other_node =
            offline_request("package-other-node", "request-other-node", user_id, file_id);
        other_node.node_id = "other-offline-node".into();
        assert!(
            matches!(
                store
                    .create_offline_package(&other_node, 10, 100_000, 5_000)
                    .await
                    .expect("per-node admission"),
                OfflineCreateOutcome::Created(_)
            ),
            "backend {backend} charged another node's bytes to the local budget"
        );
        let mut negative =
            offline_request("package-negative", "request-negative", user_id, file_id);
        negative.reserved_bytes = -1;
        assert!(matches!(
            store
                .create_offline_package(&negative, 10, 100_000, 100_000)
                .await
                .expect("negative reservation refusal"),
            OfflineCreateOutcome::ByteLimit { .. }
        ));
        let mut overflow =
            offline_request("package-overflow", "request-overflow", user_id, file_id);
        overflow.reserved_bytes = i64::MAX;
        assert!(matches!(
            store
                .create_offline_package(&overflow, 10, i64::MAX, i64::MAX)
                .await
                .expect("overflow-safe reservation refusal"),
            OfflineCreateOutcome::ByteLimit { .. }
        ));
        assert!(store
            .offline_package_for_user(&first.id, user_id)
            .await
            .expect("package lookup")
            .is_some());
        let renewed = store
            .renew_offline_package_for_user(&first.id, user_id, 20_000)
            .await
            .expect("renew package")
            .expect("renewed package");
        assert_eq!(renewed.expires_at, 20_000);
        assert!(!store
            .offline_activity_packages("offline-node", 1, 0, 10)
            .await
            .expect("activity")
            .is_empty());
        let stats = store
            .offline_package_stats("offline-node", 1)
            .await
            .expect("stats");
        assert_eq!(stats.queued, 1);
        assert_eq!(
            store
                .claim_next_offline_package("offline-node")
                .await
                .expect("claim")
                .expect("package")
                .id,
            first.id
        );
        assert_eq!(
            store
                .reset_interrupted_offline_packages("offline-node")
                .await
                .expect("reset"),
            1
        );
        assert_eq!(
            store
                .offline_package_for_user(&first.id, user_id)
                .await
                .expect("reset package lookup")
                .expect("reset package")
                .state,
            "queued"
        );
        store
            .claim_next_offline_package("offline-node")
            .await
            .expect("claim after reset")
            .expect("package after reset");
        assert!(store
            .requeue_offline_package(&first.id)
            .await
            .expect("requeue"));
        store
            .claim_next_offline_package("offline-node")
            .await
            .expect("claim")
            .expect("package");
        assert!(store
            .set_offline_package_recipe(&first.id, "offline-recipe")
            .await
            .expect("set recipe"));
        assert_eq!(
            store
                .offline_package_for_user(&first.id, user_id)
                .await
                .expect("recipe package lookup")
                .expect("recipe package")
                .recipe_hash
                .as_deref(),
            Some("offline-recipe")
        );
        assert!(store
            .update_offline_progress(&first.id, "video", 500)
            .await
            .expect("progress"));
        let progressing = store
            .offline_package_for_user(&first.id, user_id)
            .await
            .expect("progress package lookup")
            .expect("progress package");
        assert_eq!(progressing.phase, "video");
        assert_eq!(progressing.progress_millis, 500);
        assert!(store
            .mark_offline_package_ready(&first.id, "offline-recipe", 4_000, 7_200_000)
            .await
            .expect("ready"));
        assert!(matches!(
            store
                .put_offline_lease(&first.id, user_id, "lease-hash", 30_000)
                .await
                .expect("lease"),
            OfflineLeaseOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .put_offline_lease(&first.id, user_id, "lease-hash", 40_000)
                .await
                .expect("renew lease"),
            OfflineLeaseOutcome::Renewed(_)
        ));
        assert!(store
            .offline_package_for_lease("lease-hash", 1, 50_000)
            .await
            .expect("lease lookup")
            .is_some());

        let failed = offline_request("package-2", "request-2", user_id, file_id);
        store
            .create_offline_package(&failed, 10, 100_000, 100_000)
            .await
            .expect("create failed fixture");
        assert!(store
            .fail_offline_package(&failed.id, "video", "encoder", "contract failure")
            .await
            .expect("fail package"));
        let failed_package = store
            .offline_package_for_user(&failed.id, user_id)
            .await
            .expect("failed package lookup")
            .expect("failed package");
        assert_eq!(failed_package.state, "failed");
        assert_eq!(failed_package.error_code.as_deref(), Some("encoder"));
        assert_eq!(
            failed_package.error_message.as_deref(),
            Some("contract failure")
        );

        let mut expired = offline_request("package-3", "request-3", user_id, file_id);
        expired.expires_at = 1;
        store
            .create_offline_package(&expired, 10, 100_000, 100_000)
            .await
            .expect("create expired fixture");
        assert!(store.expire_offline_packages(2).await.expect("expire") >= 1);
        assert!(
            store
                .delete_offline_package(&first.id, user_id)
                .await
                .expect("delete"),
            "backend {backend}"
        );
    })
    .await;
}

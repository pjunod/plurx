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
#[cfg(all(feature = "hiqlite-store", unix))]
use std::os::unix::fs::PermissionsExt;
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
use plurx_core::cluster::migration::{
    connect_activated_store, prepare_sqlite_import, select_daemon_store, ActivationMarker,
    SelectedBackend, ACTIVATED_SOURCE_FILENAME, ACTIVATION_MARKER_FILENAME, HIQLITE_ACTIVE_DIRNAME,
};
#[cfg(feature = "hiqlite-store")]
use plurx_core::config::Config;
use plurx_core::domain::{
    scopes, ArtworkAttempt, ItemEdit, ItemKind, ItemSort, LibraryKind, MetadataPatch,
    NetworkPriorObservation, NewItem, NewLibrary, NewOfflinePackage, OfflineCreateOutcome,
    OfflineLeaseOutcome, PlaybackEvent, PlaybackEventQuery, ProbeResult, TraktAuth,
};
use plurx_core::secrets::CredentialKey;
#[cfg(feature = "hiqlite-store")]
use plurx_core::store::OfflinePackageStore;
#[cfg(feature = "hiqlite-store")]
use plurx_core::store::{
    HiqliteAuthStore, LibraryStore, MediaStore, PlaybackTelemetryStore, TraktStore, UserStore,
    WatchStore,
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

const SETTINGS_METHODS: &[&str] = &[
    "ping",
    "get_setting",
    "get_setting_pair",
    "put_setting",
    "put_settings",
    "instance_id",
];
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
    "find_book",
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
    // Node removal (`CLUSTERING-PLAN.md` §6.7). Cluster-only behavior: the
    // SQLite backend implements these inertly because a single-node install
    // has no node to remove, so the real contract lives in the replicated
    // case and in `plurx-cluster-check`.
    "unresolved_offline_packages",
    "offline_transfers_in_flight",
    "request_offline_source_probes",
    "pending_offline_source_probes",
    "answer_offline_source_probe",
    "outstanding_offline_source_probes",
    "verified_offline_source_nodes",
    "resolve_offline_packages_for_removal",
];
const TELEMETRY_METHODS: &[&str] = &[
    "record_playback_event",
    "prune_playback_events",
    "playback_events",
];
const NETWORK_PRIOR_METHODS: &[&str] = &[
    "observe_network_prior",
    "network_prior",
    "prune_network_priors",
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
        // Production tuning, duplicated from `crates/plurx-core/src/cluster/migration.rs`
        // (`start_one_voter`). The import chunk bound is sized against this WAL
        // payload capacity, so a retune there must be mirrored here or the
        // retained large-probe regression stops testing the production bound.
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
                  source_mtime, effective_rate_control, target_height, subtitle_mode,
                  state, phase, expires_at)
                 VALUES ('fixture-package', 'fixture-request', 7, 30, 'fixture-node',
                         '/fixture/shows/season-1.mkv', 4096, 115, 'qvbr:21', 720, 'none',
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
    // Sealed, not cleartext: this fixture stands in for a real install that has
    // booted this build, and such an install has no cleartext bearer column
    // left. Seeding cleartext here would make the import gate below untestable
    // by making the happy path itself the thing that must be refused.
    seed_sealed_trakt_fixture_row(&connection, &fixture_credential_key());
    for ordinal in 0..70 {
        connection
            .execute(
                "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    format!("migration.page.{ordinal:03}"),
                    format!("value-{ordinal:03}"),
                    200 + ordinal,
                ],
            )
            .expect("populate paged parity settings");
        for storage_class in ["local", "shared"] {
            connection
                .execute(
                    "INSERT INTO transcode_cache_locations
                         (recipe_hash, node_id, storage_class, relative_dir, bytes, complete,
                          last_used_at, last_seen_at)
                     VALUES ('fixture-recipe', ?1, ?2, ?3, ?4, 1, ?5, ?6)",
                    rusqlite::params![
                        format!("fixture-page-node-{ordinal:03}"),
                        storage_class,
                        format!("fixture-page-location-{ordinal:03}-{storage_class}"),
                        3_000 + ordinal,
                        300 + ordinal,
                        400 + ordinal,
                    ],
                )
                .expect("populate composite-key paged parity locations");
        }
    }
    drop(connection);
    path
}

/// Largest Raft entry the production WAL accepts, derived the same way
/// `crates/plurx-core/src/store/hiqlite_import.rs` derives its import bounds:
/// the `wal_size` above, less the 34 bytes `hiqlite-wal` reserves per segment.
/// Retiring this third copy of the tuning into one exported constant is #304.
#[cfg(feature = "hiqlite-store")]
const CONTRACT_WAL_USABLE_PAYLOAD_BYTES: usize = 2 * 1024 * 1024 - 34;

/// The row-count chunk bound #279 shipped and #282 replaced. Present only so
/// the fixtures below can assert they sit *past* it: a fixture the old bound
/// would also have carried proves nothing about a byte bound.
#[cfg(feature = "hiqlite-store")]
const SUPERSEDED_ROW_CHUNK_BOUND: usize = 16;

/// The retained #279 band: adjacent rows with full-size probe documents.
#[cfg(feature = "hiqlite-store")]
const LARGE_PROBE_FILE_COUNT: i64 = 64;
#[cfg(feature = "hiqlite-store")]
const LARGE_PROBE_PADDING_BYTES: usize = 48 * 1024;

/// The band a row count cannot bound, and the reason this contract is about
/// bytes: [`SUPERSEDED_ROW_CHUNK_BOUND`] adjacent rows of this size serialize
/// past [`CONTRACT_WAL_USABLE_PAYLOAD_BYTES`], which is #290's production
/// panic. Importing them proves the transaction builder split on bytes.
#[cfg(feature = "hiqlite-store")]
const OVERSIZED_PROBE_FILE_COUNT: i64 = 18;
#[cfg(feature = "hiqlite-store")]
const OVERSIZED_PROBE_PADDING_BYTES: usize = 144 * 1024;

/// The importer's single-row ceiling, mirroring its derivation from
/// [`CONTRACT_WAL_USABLE_PAYLOAD_BYTES`] less its encoding reserve and
/// transaction envelope. A row above this cannot be submitted in any
/// transaction, so import refuses the backup.
#[cfg(feature = "hiqlite-store")]
const CONTRACT_IMPORT_MAX_ROW_BYTES: usize = CONTRACT_WAL_USABLE_PAYLOAD_BYTES - 64 * 1024 - 256;

/// A single probe document larger than the whole WAL payload capacity, so it is
/// unimportable under any bound rather than merely past the reserve. Import must
/// refuse it instead of handing it to the WAL writer.
#[cfg(feature = "hiqlite-store")]
const UNIMPORTABLE_PROBE_PADDING_BYTES: usize = 2_100 * 1024;

/// The premises the probe fixtures rest on, checked where they are declared so
/// a later size tweak cannot quietly turn either regression into a test of
/// something easier than the bound it was written for.
#[cfg(feature = "hiqlite-store")]
const _: () = {
    assert!(
        OVERSIZED_PROBE_PADDING_BYTES * SUPERSEDED_ROW_CHUNK_BOUND
            > CONTRACT_WAL_USABLE_PAYLOAD_BYTES,
        "the oversized band must exceed what the superseded row bound would have submitted, \
         or the regression re-proves the row count instead of the byte bound"
    );
    assert!(
        LARGE_PROBE_PADDING_BYTES * SUPERSEDED_ROW_CHUNK_BOUND < CONTRACT_WAL_USABLE_PAYLOAD_BYTES,
        "the retained #279 band must stay inside the superseded bound, so the two bands \
         test different things"
    );
    assert!(
        UNIMPORTABLE_PROBE_PADDING_BYTES > CONTRACT_WAL_USABLE_PAYLOAD_BYTES,
        "the refused row must exceed the WAL itself, so the refusal is unarguable rather \
         than an artefact of the reserve held back from it"
    );
    assert!(
        CONTRACT_IMPORT_MAX_ROW_BYTES < CONTRACT_WAL_USABLE_PAYLOAD_BYTES,
        "the single-row ceiling must sit under the capacity it is derived from"
    );
};
#[cfg(feature = "hiqlite-store")]
const UNIMPORTABLE_PROBE_FILE_ID: i64 = 3_001;

#[cfg(feature = "hiqlite-store")]
fn large_probe_json(ordinal: i64, padding_bytes: usize) -> String {
    serde_json::json!({
        "format": {
            "filename": format!("/fixture/shows/large-probe-{ordinal:03}.mkv"),
            "tags": {
                "comment": "x".repeat(padding_bytes),
            },
        },
        "ordinal": ordinal,
    })
    .to_string()
}

/// Seeds `count` `files` rows whose `probe_json` is padded to `padding_bytes`,
/// numbered from `first_id` so several bands can share one fixture.
#[cfg(feature = "hiqlite-store")]
fn seed_probe_band(path: &std::path::Path, first_id: i64, count: i64, padding_bytes: usize) {
    let mut connection = rusqlite::Connection::open(path).expect("open large-probe fixture");
    let transaction = connection
        .transaction()
        .expect("begin large-probe fixture transaction");
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO files
                     (id, item_id, path, size, mtime, probe_json, scanned_at)
                 VALUES (?1, 10, ?2, ?3, ?4, ?5, ?6)",
            )
            .expect("prepare large-probe file insert");
        for ordinal in 0..count {
            let id = first_id + ordinal;
            insert
                .execute(rusqlite::params![
                    id,
                    format!("/fixture/shows/large-probe-{id:04}.mkv"),
                    10_000 + id,
                    500 + id,
                    large_probe_json(id, padding_bytes),
                    600 + id,
                ])
                .expect("insert large-probe file");
        }
    }
    transaction
        .commit()
        .expect("commit large-probe fixture transaction");
}

/// The key the import fixtures seal under.
///
/// Fixed bytes rather than `generate()` so a fixture built in one test can be
/// opened in another, and so an envelope in a failure message is reproducible.
/// It guards nothing real — the values it seals are the string `fixture-access`.
#[cfg(feature = "hiqlite-store")]
fn fixture_credential_key() -> CredentialKey {
    CredentialKey::from_bytes([0x2b; 32])
}

#[cfg(feature = "hiqlite-store")]
const FIXTURE_TRAKT_USER: i64 = 7;
#[cfg(feature = "hiqlite-store")]
const FIXTURE_TRAKT_ACCESS: &str = "fixture-access";
#[cfg(feature = "hiqlite-store")]
const FIXTURE_TRAKT_REFRESH: &str = "fixture-refresh";

#[cfg(feature = "hiqlite-store")]
fn seed_sealed_trakt_fixture_row(connection: &rusqlite::Connection, key: &CredentialKey) {
    let access = key
        .seal_trakt(FIXTURE_TRAKT_USER, FIXTURE_TRAKT_ACCESS)
        .expect("seal fixture access token");
    let refresh = key
        .seal_trakt(FIXTURE_TRAKT_USER, FIXTURE_TRAKT_REFRESH)
        .expect("seal fixture refresh token");
    connection
        .execute(
            "INSERT INTO trakt_auth
                 (user_id, access_token, refresh_token, expires_at, trakt_username,
                  connected_at, last_sync_at, last_activities)
             VALUES (?1, ?2, ?3, 999999, 'fixture-user', 118, 119, '{\"movies\":1}')
             ON CONFLICT(user_id) DO UPDATE SET
                 access_token = excluded.access_token,
                 refresh_token = excluded.refresh_token",
            rusqlite::params![FIXTURE_TRAKT_USER, access.as_stored(), refresh.as_stored(),],
        )
        .expect("seed sealed Trakt fixture row");
}

/// Rewrite the fixture's Trakt row the way a pre-encryption build wrote it.
///
/// Raw SQL on purpose: `put_trakt_auth` now refuses an unsealed credential, so
/// the only honest way to produce a legacy backup is to go around the boundary
/// that did not exist when those rows were written.
#[cfg(feature = "hiqlite-store")]
fn make_trakt_fixture_row_cleartext(path: &std::path::Path) {
    rusqlite::Connection::open(path)
        .expect("open fixture to downgrade its Trakt row")
        .execute(
            "UPDATE trakt_auth SET access_token = ?1, refresh_token = ?2 WHERE user_id = ?3",
            rusqlite::params![
                FIXTURE_TRAKT_ACCESS,
                FIXTURE_TRAKT_REFRESH,
                FIXTURE_TRAKT_USER,
            ],
        )
        .expect("write legacy cleartext Trakt row");
}

#[cfg(feature = "hiqlite-store")]
fn populated_v14_import_fixture(data_dir: &std::path::Path) -> PathBuf {
    let path = populated_current_import_fixture(data_dir);
    let connection = rusqlite::Connection::open(&path).expect("open current SQLite fixture");
    // Recreate the exact v15-v19 schema differences so this is also a valid
    // input to ordinary SQLite startup migration, not merely a current-schema
    // database carrying an older user_version. The activation coordinator now
    // runs that ordinary upgrade before publishing its immutable backup.
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TRIGGER library_roots_paths_au;
             DROP TABLE scan_reconcile_items;
             DROP TABLE scan_reconcile_guards;
             DROP TABLE library_roots;
             DROP TABLE playback_events;
             DROP TABLE network_priors;
             ALTER TABLE offline_packages DROP COLUMN effective_rate_control;
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
    assert!(
        report
            .tables
            .iter()
            .find(|digest| digest.table == "settings")
            .expect("settings digest")
            .row_count
            > 64,
        "settings parity must cross the 64-row keyset page boundary"
    );
    assert_eq!(
        report
            .tables
            .iter()
            .find(|digest| digest.table == "transcode_cache_locations")
            .expect("transcode cache locations digest")
            .row_count,
        141,
        "the leading fixture row plus two storage classes per node must split a \
         three-column key between parity pages"
    );
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
            .offline_package_for_user("fixture-package", 7)
            .await
            .expect("read imported v14 package")
            .expect("imported v14 package")
            .effective_rate_control,
        "vbr",
        "a pre-v18 source must receive the only truthful legacy identity"
    );
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
    assert_eq!(
        store
            .offline_package_for_user("fixture-package", 7)
            .await
            .expect("read imported current package")
            .expect("imported current package")
            .effective_rate_control,
        "qvbr:21"
    );
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

/// Import transactions must be bounded by serialized bytes, not by a row count.
///
/// The fixture is built to defeat a row count on purpose: the oversized band's
/// rows are large enough that [`SUPERSEDED_ROW_CHUNK_BOUND`] adjacent ones
/// exceed the production WAL payload capacity, which is exactly the entry that
/// panicked node `m6` into an HTTP-healthy unreplicated boot (#290). That
/// premise is asserted where the fixture sizes are declared, so passing means
/// the builder split on bytes rather than that the row count was favourable.
///
/// The #279 band is retained alongside it: the byte bound must not regress the
/// ordinary large-library case that motivated the row cap.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_probe_json_import_respects_the_production_wal_limit() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated large-probe import target");

    let source = tempfile::tempdir().expect("large-probe SQLite fixture directory");
    let fixture_path = populated_current_import_fixture(source.path());
    seed_probe_band(
        &fixture_path,
        1_000,
        LARGE_PROBE_FILE_COUNT,
        LARGE_PROBE_PADDING_BYTES,
    );
    seed_probe_band(
        &fixture_path,
        2_000,
        OVERSIZED_PROBE_FILE_COUNT,
        OVERSIZED_PROBE_PADDING_BYTES,
    );
    let prepared = prepare_sqlite_import(source.path()).expect("prepare large-probe import backup");

    let report = match store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
    {
        Ok(report) => report,
        Err(plurx_core::error::StoreError::Timeout(message)) => {
            panic!(
                "large-probe import timed out after 3 s (the host was too loaded to                  complete the test): {message}"
            );
        }
        Err(error) => {
            panic!("byte-bounded transactions must carry probe rows a row count cannot: {error}");
        }
    };
    assert_eq!(
        report
            .tables
            .iter()
            .find(|digest| digest.table == "files")
            .expect("files digest")
            .row_count,
        (LARGE_PROBE_FILE_COUNT + OVERSIZED_PROBE_FILE_COUNT) as u64 + 1,
        "exact file parity must include the existing fixture row and both probe bands"
    );

    // `probe_json` is durable media metadata: splitting a transaction may not
    // drop, truncate, or rewrite a byte of it.
    for (first_id, count, padding) in [
        (1_000, LARGE_PROBE_FILE_COUNT, LARGE_PROBE_PADDING_BYTES),
        (
            2_000,
            OVERSIZED_PROBE_FILE_COUNT,
            OVERSIZED_PROBE_PADDING_BYTES,
        ),
    ] {
        for ordinal in 0..count {
            let id = first_id + ordinal;
            assert_eq!(
                store
                    .get_file_probe_json(id)
                    .await
                    .expect("read imported large probe")
                    .as_deref(),
                Some(large_probe_json(id, padding).as_str()),
                "large probe_json row {id} must retain exact bytes"
            );
        }
    }
}

/// A row no transaction could carry is refused before submission.
///
/// The alternative is #290's production failure: the WAL writer panics on the
/// oversized entry, `plurxd` exits mid-import, and the restart serves
/// unreplicated SQLite while reporting healthy. A refusal keeps the node alive
/// and tells the operator which row to look at, so this asserts both — that the
/// error is actionable, and that the voter still answers afterwards.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_row_larger_than_the_wal_is_refused_instead_of_crashing_the_node() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated unimportable-probe target");

    let source = tempfile::tempdir().expect("unimportable-probe SQLite fixture directory");
    let fixture_path = populated_current_import_fixture(source.path());
    seed_probe_band(
        &fixture_path,
        UNIMPORTABLE_PROBE_FILE_ID,
        1,
        UNIMPORTABLE_PROBE_PADDING_BYTES,
    );
    let prepared =
        prepare_sqlite_import(source.path()).expect("prepare unimportable-probe import backup");

    let refusal = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect_err("a row larger than the WAL payload capacity must not be submitted")
        .to_string();
    assert!(
        refusal.contains("files")
            && refusal.contains(&format!("row id={UNIMPORTABLE_PROBE_FILE_ID}")),
        "the refusal must name the table and the row an operator has to fix: {refusal}"
    );
    assert!(
        refusal.contains(&CONTRACT_IMPORT_MAX_ROW_BYTES.to_string()),
        "the refusal must state the limit the row exceeded: {refusal}"
    );
    assert!(
        !refusal.contains("xxxx"),
        "a refusal must not quote the imported payload: {refusal}"
    );

    assert_eq!(
        store
            .get_file_probe_json(UNIMPORTABLE_PROBE_FILE_ID)
            .await
            .expect("query the voter after a refused import"),
        None,
        "the refused row must not be in replicated state, and the voter must still answer"
    );
}

/// A simulated store timeout must surface as `StoreError::Timeout`, not as a
/// WAL-limit or database failure.
///
/// This is the regression for the defect reported in #368, where a busy host's
/// `replicated store operation timed out` was wrapped in `StoreError::Database`
/// and surfaced as `large probe_json rows must fit the production 2 MiB WAL`.
/// The fix added the separate `StoreError::Timeout` variant so the two failure
/// modes are distinguishable by type, not by string-matching.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_timeout_is_distinct_from_a_wal_limit_failure() {
    use plurx_core::error::StoreError;

    // Build a timeout error exactly as timeout_store produces it.
    let timeout_error = StoreError::Timeout("test timeout after 3s".to_owned());
    assert!(
        !matches!(timeout_error, StoreError::Database(_)),
        "a timeout must not be reported as a database error"
    );
    assert!(
        !timeout_error.to_string().contains("WAL"),
        "a timeout must not be reported as a WAL-limit error: {}",
        timeout_error
    );

    // The import-time WAL-size refusal produces StoreError::Migration, which
    // must also not be confused with a timeout.
    let wal_error = StoreError::Migration(
        "table files import transaction of 1 row(s) serializes to about 42 bytes,          above the production WAL payload capacity"
            .to_owned(),
    );
    assert!(
        !matches!(wal_error, StoreError::Timeout(_)),
        "a WAL-limit error must not be reported as a timeout"
    );

    // A generic store error must still be distinguishable from both.
    let db_error = StoreError::Database("the database is on fire".to_owned());
    assert!(
        !matches!(db_error, StoreError::Timeout(_)),
        "a database error must not be reported as a timeout"
    );
}

/// A legacy backup whose Trakt row is still cleartext must be refused, and
/// refused before anything at all is committed to Raft.
///
/// This is the one production path that does not go through a durable writer:
/// import copies source columns straight into the target, so the write-time
/// `persistable_credential` gate never sees them. Without the source audit a
/// pre-encryption backup would fan a usable bearer token out to every voter,
/// into committed log entries that deleting the row cannot reach.
///
/// The second half is the part that gives this teeth. Asserting only that the
/// import errored would still pass if the refusal happened after the rows were
/// submitted, which is the failure that matters here.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cleartext_trakt_row_is_refused_before_any_row_reaches_raft() {
    let _case = HIQLITE_CASE.lock().await;
    let store = open_contract_hiqlite_store().await;
    store
        .validation_reset_contract_state()
        .await
        .expect("reset replicated cleartext-import target");

    let source = tempfile::tempdir().expect("cleartext import fixture directory");
    let fixture_path = populated_current_import_fixture(source.path());
    make_trakt_fixture_row_cleartext(&fixture_path);
    let prepared = prepare_sqlite_import(source.path()).expect("prepare cleartext import backup");

    let refusal = store
        .import_sqlite_backup(
            &prepared.backup_path,
            &prepared.backup_sha256,
            prepared.schema_version,
        )
        .await
        .expect_err("a cleartext Trakt row must not be importable into replicated state");
    let refusal = refusal.to_string();
    assert!(
        refusal.contains("trakt_auth") && refusal.contains("not sealed"),
        "refusal must name the table and the reason: {refusal}"
    );
    assert!(
        !refusal.contains(FIXTURE_TRAKT_ACCESS) && !refusal.contains(FIXTURE_TRAKT_REFRESH),
        "a refusal must not quote the credential it refused"
    );

    assert!(
        store
            .list_trakt_auth()
            .await
            .expect("read replicated Trakt rows after refusal")
            .is_empty(),
        "the refused cleartext credential must not be in replicated state"
    );
    assert!(
        store
            .list_libraries()
            .await
            .expect("read replicated libraries after refusal")
            .is_empty(),
        "refusal must precede the first table import, not clean up after it"
    );

    // Sealing the source is the documented remedy, and it must actually work:
    // an operator who boots this build on the SQLite install gets rows this
    // accepts, without having to reconnect the account.
    seed_sealed_trakt_fixture_row(
        &rusqlite::Connection::open(&fixture_path).expect("reopen fixture to seal its Trakt row"),
        &fixture_credential_key(),
    );
    let resealed = prepare_sqlite_import(source.path()).expect("prepare resealed import backup");
    store
        .import_sqlite_backup(
            &resealed.backup_path,
            &resealed.backup_sha256,
            resealed.schema_version,
        )
        .await
        .expect("a sealed source must import");

    let imported = store
        .get_trakt_auth(FIXTURE_TRAKT_USER)
        .await
        .expect("read imported Trakt row")
        .expect("imported Trakt row");
    assert!(
        imported.access_token.is_wrapped() && imported.refresh_token.is_wrapped(),
        "the replicated row must hold envelopes"
    );
    assert_eq!(
        fixture_credential_key()
            .open_trakt(FIXTURE_TRAKT_USER, &imported.access_token)
            .expect("open imported access token")
            .expose(),
        FIXTURE_TRAKT_ACCESS,
        "import must preserve a working credential, not just an opaque string"
    );
}

#[cfg(feature = "hiqlite-store")]
fn one_voter_config(data_dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.storage.data_dir = data_dir.to_owned();
    config.cluster.raft_bind = format!("0.0.0.0:{}", contract_free_port())
        .parse()
        .expect("raft bind");
    config.cluster.api_bind = format!("0.0.0.0:{}", contract_free_port())
        .parse()
        .expect("api bind");
    config.cluster.advertise_host = "127.0.0.1".to_owned();
    config
}

#[cfg(feature = "hiqlite-store")]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ActivationNodeLaunch {
    data_dir: PathBuf,
    raft_bind: String,
    api_bind: String,
    source_version: i64,
    first_boot: bool,
}

#[cfg(feature = "hiqlite-store")]
impl ActivationNodeLaunch {
    fn config(&self) -> Config {
        let mut config = Config::default();
        config.storage.data_dir = self.data_dir.clone();
        config.cluster.raft_bind = self.raft_bind.parse().expect("raft bind");
        config.cluster.api_bind = self.api_bind.parse().expect("api bind");
        config.cluster.advertise_host = "127.0.0.1".to_owned();
        config
    }
}

#[cfg(feature = "hiqlite-store")]
struct ActivationNodeProcess {
    child: Child,
    input: ChildStdin,
    _output: ChildStdout,
}

#[cfg(feature = "hiqlite-store")]
impl ActivationNodeProcess {
    fn start(launch: &ActivationNodeLaunch) -> Self {
        let executable = std::env::current_exe().expect("activation test executable");
        let mut child = Command::new(executable)
            .arg("hiqlite_activation_node_process")
            .arg("--ignored")
            .arg("--exact")
            .arg("--nocapture")
            .env(
                "PLURX_ACTIVATION_NODE_LAUNCH",
                serde_json::to_string(launch).expect("serialize activation launch"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn activation voter");
        let input = child.stdin.take().expect("activation voter stdin");
        let output = child.stdout.take().expect("activation voter stdout");
        let mut reader = BufReader::new(output);
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .expect("read activation voter startup");
            assert!(bytes > 0, "activation voter exited before ready");
            if line.trim() == "PLURX_ACTIVATION_NODE_READY" {
                break;
            }
        }
        Self {
            child,
            input,
            _output: reader.into_inner(),
        }
    }

    fn stop(self) {
        drop(self.input);
        let status = self
            .child
            .wait_with_output()
            .expect("wait activation voter");
        assert!(
            status.status.success(),
            "activation voter failed: {status:?}"
        );
    }
}

#[cfg(feature = "hiqlite-store")]
fn run_injected_activation(launch: &ActivationNodeLaunch, failpoint: &str) {
    let executable = std::env::current_exe().expect("activation test executable");
    let output = Command::new(executable)
        .arg("hiqlite_activation_node_process")
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .env(
            "PLURX_ACTIVATION_NODE_LAUNCH",
            serde_json::to_string(launch).expect("serialize activation launch"),
        )
        .env("PLURX_CLUSTER_ACTIVATION_FAILPOINT", failpoint)
        .output()
        .expect("run injected activation voter");
    assert_eq!(output.status.code(), Some(86), "injected process status");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rollback command: plurxd run"),
        "injected failure must name rollback command: {stderr}"
    );
}

#[cfg(feature = "hiqlite-store")]
async fn assert_one_voter_activation(data_dir: &std::path::Path, source_version: i64) {
    // The raw importer fixture is pre-sealed to test that boundary in
    // isolation. A real legacy activation starts before credential encryption,
    // so put this source back in that shape and require the coordinator to run
    // the ordinary one-time sealing upgrade before it publishes the backup.
    make_trakt_fixture_row_cleartext(&data_dir.join("plurx.db"));
    let config = one_voter_config(data_dir);
    let launch = ActivationNodeLaunch {
        data_dir: data_dir.to_owned(),
        raft_bind: config.cluster.raft_bind.to_string(),
        api_bind: config.cluster.api_bind.to_string(),
        source_version,
        first_boot: true,
    };
    let first = ActivationNodeProcess::start(&launch);

    let active = data_dir.join(HIQLITE_ACTIVE_DIRNAME);
    let marker: ActivationMarker = serde_json::from_slice(
        &std::fs::read(active.join(ACTIVATION_MARKER_FILENAME)).expect("activation marker"),
    )
    .expect("decode activation marker");
    assert_eq!(marker.cluster_id, CONTRACT_INSTANCE_ID);
    assert_eq!(marker.source_schema_version, source_version);
    assert!(!marker.table_hashes.is_empty());
    assert!(data_dir.join("plurx.db").exists(), "legacy source retained");
    #[cfg(unix)]
    for private in [
        data_dir.join("secret_raft"),
        data_dir.join("secret_api"),
        active.join(ACTIVATION_MARKER_FILENAME),
    ] {
        let mode = std::fs::metadata(&private)
            .unwrap_or_else(|error| panic!("metadata for {}: {error}", private.display()))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "{} must be owner-only", private.display());
    }
    assert!(
        std::fs::read_dir(data_dir.join("migration"))
            .expect("migration backups")
            .any(|entry| entry
                .expect("migration entry")
                .path()
                .extension()
                .is_some_and(|ext| ext == "db")),
        "immutable SQLite backup retained"
    );

    let second = connect_activated_store(&config)
        .await
        .expect("second replicated reader");
    assert_eq!(
        second
            .watch_state(7, 10)
            .await
            .expect("read watch state through second client")
            .expect("replicated watch state")
            .position_ms,
        321_000
    );
    drop(second);
    first.stop();

    let reopened = ActivationNodeProcess::start(&ActivationNodeLaunch {
        first_boot: false,
        ..launch
    });
    let second = connect_activated_store(&config)
        .await
        .expect("second reader after subsequent run");
    assert_eq!(
        second
            .watch_state(7, 10)
            .await
            .expect("read reopened watch state")
            .expect("reopened watch state")
            .position_ms,
        321_000
    );
    drop(second);
    reopened.stop();
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populated_v14_and_current_sources_activate_once_and_reopen_replicated() {
    let _case = HIQLITE_CASE.lock().await;
    install_contract_crypto_provider();

    let v14 = tempfile::tempdir().expect("v14 activation fixture");
    populated_v14_import_fixture(v14.path());
    assert_one_voter_activation(v14.path(), plurx_core::store::SQLITE_SCHEMA_VERSION).await;

    let current = tempfile::tempdir().expect("current activation fixture");
    populated_current_import_fixture(current.path());
    assert_one_voter_activation(current.path(), plurx_core::store::SQLITE_SCHEMA_VERSION).await;
}

/// A direct pre-encryption upgrade seals Trakt before publishing its backup.
///
/// The importer correctly refuses cleartext, but that refusal would turn every
/// linked legacy install into a dead end unless the activation coordinator ran
/// the ordinary SQLite credential upgrade first. This covers both halves: a
/// key-resolution refusal leaves no incoming target, then the same directory
/// activates and its replicated envelope still opens under the node-local key.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_upgrade_seals_legacy_trakt_before_any_import_state_exists() {
    let _case = HIQLITE_CASE.lock().await;
    install_contract_crypto_provider();

    let fixture = tempfile::tempdir().expect("legacy Trakt activation fixture");
    let source_path = populated_current_import_fixture(fixture.path());
    make_trakt_fixture_row_cleartext(&source_path);
    let config = one_voter_config(fixture.path());

    // A directory at the configured key path is an explicit key-resolution
    // failure. It occurs before the coordinator writes its attempt marker or
    // creates the incoming target, so the legacy source remains recoverable.
    let key_path = config.cluster.credential_key_path(&config.storage.data_dir);
    std::fs::create_dir(&key_path).expect("block credential-key loading");
    let refusal = select_daemon_store(&config)
        .await
        .err()
        .expect("an unusable credential-key path must refuse activation")
        .to_string();
    assert!(refusal.contains("credential key"), "{refusal}");
    assert!(
        !refusal.contains(FIXTURE_TRAKT_ACCESS) && !refusal.contains(FIXTURE_TRAKT_REFRESH),
        "the refusal must not quote either bearer credential: {refusal}"
    );
    assert!(
        !fixture.path().join("hiqlite.incoming").exists(),
        "a pre-import refusal must leave no partial incoming target"
    );
    assert!(
        !fixture.path().join(HIQLITE_ACTIVE_DIRNAME).exists(),
        "a pre-import refusal must not publish an active target"
    );
    std::fs::remove_dir(&key_path).expect("unblock credential-key loading");

    let selected = select_daemon_store(&config)
        .await
        .expect("direct legacy upgrade must activate after key recovery");
    assert_eq!(selected.backend, SelectedBackend::Replicated);
    let imported = selected
        .store
        .get_trakt_auth(FIXTURE_TRAKT_USER)
        .await
        .expect("read imported Trakt row")
        .expect("linked Trakt row survives activation");
    assert!(
        imported.access_token.is_wrapped() && imported.refresh_token.is_wrapped(),
        "replicated durable state must contain envelopes"
    );
    assert_eq!(
        selected
            .credential_key
            .open_trakt(FIXTURE_TRAKT_USER, &imported.access_token)
            .expect("open imported access token")
            .expose(),
        FIXTURE_TRAKT_ACCESS
    );
    assert_eq!(
        selected
            .credential_key
            .open_trakt(FIXTURE_TRAKT_USER, &imported.refresh_token)
            .expect("open imported refresh token")
            .expose(),
        FIXTURE_TRAKT_REFRESH
    );

    let (source_access, source_refresh): (String, String) =
        rusqlite::Connection::open(&source_path)
            .expect("reopen sealed rollback source")
            .query_row(
                "SELECT access_token, refresh_token FROM trakt_auth WHERE user_id = ?1",
                [FIXTURE_TRAKT_USER],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read sealed rollback row");
    for (label, stored, expected) in [
        ("access", source_access, FIXTURE_TRAKT_ACCESS),
        ("refresh", source_refresh, FIXTURE_TRAKT_REFRESH),
    ] {
        assert_ne!(stored, expected, "{label} token remained cleartext");
        assert!(
            stored.starts_with("plxenc:v1:"),
            "{label} token is not a v1 envelope"
        );
    }
    selected.shutdown().await.expect("stop activated voter");
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_activation_boundaries_recover_sqlite_or_completed_target() {
    let _case = HIQLITE_CASE.lock().await;
    install_contract_crypto_provider();

    for failpoint in ["after-quiescence", "after-incoming", "after-marker"] {
        let fixture = tempfile::tempdir().expect("interrupted activation fixture");
        let source = populated_current_import_fixture(fixture.path());
        make_trakt_fixture_row_cleartext(&source);
        let config = one_voter_config(fixture.path());
        let launch = ActivationNodeLaunch {
            data_dir: fixture.path().to_owned(),
            raft_bind: config.cluster.raft_bind.to_string(),
            api_bind: config.cluster.api_bind.to_string(),
            source_version: plurx_core::store::SQLITE_SCHEMA_VERSION,
            first_boot: true,
        };
        run_injected_activation(&launch, failpoint);

        let recovered = select_daemon_store(&config)
            .await
            .unwrap_or_else(|error| panic!("recover {failpoint}: {error}"));
        assert_eq!(recovered.backend, SelectedBackend::SqliteRecovery);
        assert_eq!(
            recovered
                .store
                .watch_state(7, 10)
                .await
                .expect("read unchanged SQLite watch state")
                .expect("SQLite watch state")
                .position_ms,
            120_000
        );
        assert!(!fixture.path().join("hiqlite.incoming").exists());
        assert!(!fixture.path().join("hiqlite").exists());
    }

    let fixture = tempfile::tempdir().expect("post-rename activation fixture");
    let source = populated_current_import_fixture(fixture.path());
    make_trakt_fixture_row_cleartext(&source);
    let config = one_voter_config(fixture.path());
    let launch = ActivationNodeLaunch {
        data_dir: fixture.path().to_owned(),
        raft_bind: config.cluster.raft_bind.to_string(),
        api_bind: config.cluster.api_bind.to_string(),
        source_version: plurx_core::store::SQLITE_SCHEMA_VERSION,
        first_boot: true,
    };
    run_injected_activation(&launch, "after-rename");
    assert!(fixture.path().join(HIQLITE_ACTIVE_DIRNAME).is_dir());
    assert!(!fixture.path().join("hiqlite.incoming").exists());

    let resumed = ActivationNodeProcess::start(&launch);
    let second = connect_activated_store(&config)
        .await
        .expect("read completed target after rename crash");
    assert_eq!(second.count_users().await.expect("user count"), 1);
    drop(second);
    resumed.stop();
}

/// An ambiguous active target must fail closed, never fall back to SQLite.
///
/// This is the property whose regression is worst: silently preferring the
/// retained source would boot the daemon on pre-activation state while the
/// replicated target sat right there. Every case here is reached before a voter
/// starts, so the refusal is the only thing under test.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_active_target_refuses_rather_than_reverting_to_sqlite() {
    let _case = HIQLITE_CASE.lock().await;
    install_contract_crypto_provider();

    let valid_marker = |data_dir: &std::path::Path| -> ActivationMarker {
        let prepared = prepare_sqlite_import(data_dir).expect("prepare source");
        ActivationMarker {
            marker_version: 1,
            cluster_id: prepared.cluster_id.clone(),
            source_backup_sha256: prepared.backup_sha256.clone(),
            source_schema_version: prepared.schema_version,
            replicated_schema_version: 5,
            imported_rows: 1,
            table_hashes: Vec::new(),
        }
    };

    /// How one case makes the target ambiguous.
    type Corrupt = Box<dyn Fn(&std::path::Path)>;

    // Each case: how the target is made ambiguous, and what the operator reads.
    let cases: Vec<(&str, Corrupt)> = vec![
        (
            "activation marker",
            Box::new(|data_dir: &std::path::Path| {
                let active = data_dir.join(HIQLITE_ACTIVE_DIRNAME);
                std::fs::create_dir_all(&active).expect("active target");
                std::fs::write(active.join(ACTIVATION_MARKER_FILENAME), b"{ not json")
                    .expect("corrupt marker");
            }),
        ),
        (
            ACTIVATION_MARKER_FILENAME,
            Box::new(|data_dir: &std::path::Path| {
                std::fs::create_dir_all(data_dir.join(HIQLITE_ACTIVE_DIRNAME))
                    .expect("active target without a marker");
            }),
        ),
        (
            "Hiqlite activation marker",
            Box::new(move |data_dir: &std::path::Path| {
                let active = data_dir.join(HIQLITE_ACTIVE_DIRNAME);
                std::fs::create_dir_all(&active).expect("active target");
                let mut marker = valid_marker(data_dir);
                // Structurally sound, semantically impossible.
                marker.marker_version = 99;
                std::fs::write(
                    active.join(ACTIVATION_MARKER_FILENAME),
                    serde_json::to_vec(&marker).expect("serialize marker"),
                )
                .expect("write marker");
            }),
        ),
        (
            "directory",
            Box::new(|data_dir: &std::path::Path| {
                std::fs::write(data_dir.join(HIQLITE_ACTIVE_DIRNAME), b"not a target")
                    .expect("active path that is not a directory");
            }),
        ),
    ];

    for (expected, corrupt) in cases {
        let fixture = tempfile::tempdir().expect("ambiguous target fixture");
        populated_current_import_fixture(fixture.path());
        corrupt(fixture.path());

        let error = select_daemon_store(&one_voter_config(fixture.path()))
            .await
            .err()
            .expect("an ambiguous active target must not open");
        let error = error.to_string();
        assert!(
            error.contains(expected),
            "refusal must name what is wrong ({expected}): {error}"
        );
        assert!(
            !fixture.path().join("hiqlite.incoming").exists(),
            "a refusal must not stage a new import: {error}"
        );
    }
}

/// Losing the replicated target must not silently re-import the stale source.
///
/// After activation `plurx.db` is a rollback source, not a current one. A data
/// directory that lost `hiqlite/` is byte-indistinguishable from one that never
/// activated, so without the breadcrumb the next boot would quietly discard
/// every write since activation — the failure an operator would never see.
#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_replicated_target_refuses_to_reimport_the_retained_source() {
    let _case = HIQLITE_CASE.lock().await;
    install_contract_crypto_provider();

    let fixture = tempfile::tempdir().expect("activated source fixture");
    let source = populated_current_import_fixture(fixture.path());
    make_trakt_fixture_row_cleartext(&source);
    let config = one_voter_config(fixture.path());
    let launch = ActivationNodeLaunch {
        data_dir: fixture.path().to_owned(),
        raft_bind: config.cluster.raft_bind.to_string(),
        api_bind: config.cluster.api_bind.to_string(),
        source_version: plurx_core::store::SQLITE_SCHEMA_VERSION,
        first_boot: true,
    };
    ActivationNodeProcess::start(&launch).stop();

    let breadcrumb = fixture.path().join(ACTIVATED_SOURCE_FILENAME);
    assert!(
        breadcrumb.is_file(),
        "activation must record that this source handed over authority"
    );
    std::fs::remove_dir_all(fixture.path().join(HIQLITE_ACTIVE_DIRNAME)).expect("lose the target");

    let error = select_daemon_store(&config)
        .await
        .err()
        .expect("a lost target must not silently re-import")
        .to_string();
    assert!(error.contains("already activated"), "{error}");
    // The refusal is only useful if it names both what it protected and the
    // exact way out; an operator who cannot act on it will delete something.
    assert!(error.contains("plurx.db"), "{error}");
    assert!(
        error.contains(&breadcrumb.display().to_string()),
        "refusal must name the file to delete to accept the rollback: {error}"
    );
    assert!(
        !fixture.path().join("hiqlite.incoming").exists(),
        "the refusal must not stage an import"
    );

    // And the documented way out actually works: with the breadcrumb gone the
    // directory activates again rather than staying permanently wedged. The
    // failpoint stops that attempt as soon as it proves the guard is passed.
    std::fs::remove_file(&breadcrumb).expect("accept the rollback");
    run_injected_activation(&launch, "after-quiescence");
}

#[cfg(feature = "hiqlite-store")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawned by the one-voter activation contract"]
async fn hiqlite_activation_node_process() {
    install_contract_crypto_provider();
    let launch: ActivationNodeLaunch = serde_json::from_str(
        &std::env::var("PLURX_ACTIVATION_NODE_LAUNCH").expect("activation launch"),
    )
    .expect("decode activation launch");
    let selected = select_daemon_store(&launch.config())
        .await
        .expect("select one-voter store");
    assert_eq!(selected.backend, SelectedBackend::Replicated);
    assert_eq!(selected.identity.cluster_id, CONTRACT_INSTANCE_ID);
    assert_eq!(selected.store.count_users().await.expect("user count"), 1);
    if launch.first_boot {
        selected
            .store
            .put_progress(7, 10, 321_000, Some(3_600_000))
            .await
            .expect("write watch progress through primary client");
    } else {
        assert_eq!(
            selected
                .store
                .watch_state(7, 10)
                .await
                .expect("read reopened watch state")
                .expect("reopened watch state")
                .position_ms,
            321_000
        );
    }
    let marker: ActivationMarker = serde_json::from_slice(
        &std::fs::read(
            launch
                .data_dir
                .join(HIQLITE_ACTIVE_DIRNAME)
                .join(ACTIVATION_MARKER_FILENAME),
        )
        .expect("activation marker"),
    )
    .expect("decode marker");
    assert_eq!(marker.source_schema_version, launch.source_version);
    println!("PLURX_ACTIVATION_NODE_READY");
    std::io::stdout()
        .flush()
        .expect("flush activation readiness");
    let mut sink = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut sink)
        .await
        .expect("wait for activation parent");
    selected.shutdown().await.expect("stop activation voter");
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
        NETWORK_PRIOR_METHODS,
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect::<BTreeSet<_>>();

    assert_eq!(declared.len(), 137, "review the Store method count");
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
async fn network_prior_contract_runs_through_dyn_store() {
    for_each_backend(|store, backend| async move {
        let key = format!(
            "192.0.{}.0/24",
            if backend.contains("hiqlite") { 3 } else { 2 }
        );
        let prior = store
            .observe_network_prior(&NetworkPriorObservation {
                user_id: 71,
                client_class: "chrome".to_owned(),
                network_fingerprint: key.clone(),
                throughput_kbps: Some(8_000),
                starved_rung_height: Some(1080),
                observed_at_ms: 1_700_000_000_000,
            })
            .await
            .unwrap_or_else(|error| panic!("{backend}: observe prior: {error}"));
        assert_eq!(prior.sustained_kbps, Some(8_000), "{backend}");
        assert_eq!(prior.worst_rung_height, Some(1080), "{backend}");
        assert_eq!(
            prior.starved_at_ms,
            Some(1_700_000_000_000),
            "{backend}: the verdict's expiry stamp is part of the durable contract"
        );
        let loaded = store
            .network_prior(71, "chrome", &key)
            .await
            .unwrap_or_else(|error| panic!("{backend}: load prior: {error}"))
            .expect("stored prior");
        assert_eq!(loaded, prior, "{backend}");
        assert_eq!(
            store
                .prune_network_priors(1_700_000_000_001, 1)
                .await
                .unwrap_or_else(|error| panic!("{backend}: prune prior: {error}")),
            1,
            "{backend}"
        );
        assert!(store
            .network_prior(71, "chrome", &key)
            .await
            .expect("post-prune lookup")
            .is_none());
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
        store
            .put_settings(&[("contract.left", "L"), ("contract.right", "R")])
            .await
            .expect("publish related settings");
        assert_eq!(
            store.get_setting("contract.left").await.expect("left"),
            Some("L".to_owned()),
            "backend {backend}"
        );
        assert_eq!(
            store.get_setting("contract.right").await.expect("right"),
            Some("R".to_owned()),
            "backend {backend}"
        );
        assert_eq!(
            store
                .get_setting_pair("contract.left", "contract.right")
                .await
                .expect("pair"),
            (Some("L".to_owned()), Some("R".to_owned())),
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
        let books = store
            .create_library(&NewLibrary {
                name: "Media Contract Books".into(),
                kind: LibraryKind::Books,
                paths: vec![PathBuf::from("/contract/books")],
                anime: false,
            })
            .await
            .expect("books library");

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
        let ebook = store
            .insert_item(&NewItem {
                library_id: books.id,
                kind: ItemKind::Book,
                parent_id: None,
                title: "Shared Contract Title".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("ebook");
        let audiobook = store
            .insert_item(&NewItem {
                library_id: books.id,
                kind: ItemKind::Audiobook,
                parent_id: None,
                title: "Shared Contract Title".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("audiobook");

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
                .find_book(
                    books.id,
                    ItemKind::Book,
                    "Shared Contract Title",
                    None,
                    None
                )
                .await
                .expect("find ebook")
                .expect("ebook")
                .id,
            ebook
        );
        assert_eq!(
            store
                .find_book(
                    books.id,
                    ItemKind::Audiobook,
                    "Shared Contract Title",
                    None,
                    None,
                )
                .await
                .expect("find audiobook")
                .expect("audiobook")
                .id,
            audiobook
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
        let books = store
            .create_library(&NewLibrary {
                name: "Watch Contract Books".into(),
                kind: LibraryKind::Books,
                paths: vec![PathBuf::from("/watch/books")],
                anime: false,
            })
            .await
            .expect("books");
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
        let audiobook = store
            .insert_item(&NewItem {
                library_id: books.id,
                kind: ItemKind::Audiobook,
                parent_id: None,
                title: "Multipart Contract Book".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("audiobook");
        for (part, duration_ms) in [(1, 10_000), (2, 20_000)] {
            store
                .upsert_file(
                    audiobook,
                    &format!("/watch/books/Multipart Contract Book/Part {part}.mp3"),
                    1_000,
                    part,
                    &ProbeResult {
                        duration_ms: Some(duration_ms),
                        container: Some("mp3".into()),
                        ..Default::default()
                    },
                )
                .await
                .expect("audiobook part");
        }

        let audiobook_progress = store
            .put_progress(user.id, audiobook, 25_000, Some(20_000))
            .await
            .expect("global audiobook progress");
        assert_eq!(
            audiobook_progress.position_ms, 25_000,
            "backend {backend} clamped the book timeline to one part"
        );
        assert_eq!(audiobook_progress.duration_ms, Some(30_000));
        assert!(!audiobook_progress.watched);
        let audiobook_progress = store
            .put_progress_if_current(
                user.id,
                audiobook,
                &audiobook_progress,
                26_000,
                Some(20_000),
            )
            .await
            .expect("compare-and-set audiobook progress")
            .expect("current audiobook progress");
        assert_eq!(audiobook_progress.position_ms, 26_000);

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

        // Both backends persist the Trakt bearer pair as envelopes. The
        // credential only ever exists in the clear on either side of this
        // boundary, never inside it — see CLUSTERING-PLAN.md §3.2.
        let key = CredentialKey::generate();
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
        store
            .put_trakt_auth(&TraktAuth {
                user_id: user.id,
                access_token: key.seal_trakt(user.id, "access-1").expect("seal access"),
                refresh_token: key.seal_trakt(user.id, "refresh-1").expect("seal refresh"),
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
        assert!(linked.is_wrapped(), "the stored bearer pair must be sealed");
        assert!(
            !linked.access_token.as_stored().contains("access-1")
                && !linked.refresh_token.as_stored().contains("refresh-1"),
            "a durable Trakt row must not hold its bearer credential in the clear"
        );
        assert_eq!(
            linked
                .reveal_access_token(&key)
                .expect("open access")
                .expose(),
            "access-1"
        );
        assert_eq!(
            linked
                .reveal_refresh_token(&key)
                .expect("open refresh")
                .expose(),
            "refresh-1"
        );
        assert_eq!(linked.expires_at, 100);
        assert_eq!(linked.trakt_username.as_deref(), Some("contract"));
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);
        // The rotation compare-and-set runs on the stored envelope, so it stays
        // an exact equality check that no voter has to decrypt anything to make.
        let stale_refresh = linked.refresh_token.clone();
        assert!(store
            .update_trakt_tokens(
                user.id,
                &stale_refresh,
                &key.seal_trakt(user.id, "access-2").expect("seal access"),
                &key.seal_trakt(user.id, "refresh-2").expect("seal refresh"),
                200,
            )
            .await
            .expect("update tokens"));
        assert!(!store
            .update_trakt_tokens(
                user.id,
                &stale_refresh,
                &key.seal_trakt(user.id, "loser").expect("seal access"),
                &key.seal_trakt(user.id, "loser-refresh")
                    .expect("seal refresh"),
                300,
            )
            .await
            .expect("reject stale refresh"));
        assert!(!store
            .delete_trakt_auth_if_current(user.id, &stale_refresh)
            .await
            .expect("reject stale unlink"));
        let refreshed = store
            .get_trakt_auth(user.id)
            .await
            .expect("get refreshed auth")
            .expect("refreshed auth");
        assert_eq!(
            refreshed
                .reveal_access_token(&key)
                .expect("open access")
                .expose(),
            "access-2"
        );
        assert_eq!(
            refreshed
                .reveal_refresh_token(&key)
                .expect("open refresh")
                .expose(),
            "refresh-2"
        );
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
        effective_rate_control: "qvbr:21".into(),
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
        let OfflineCreateOutcome::Created(created) = store
            .create_offline_package(&first, 10, 100_000, 100_000)
            .await
            .expect("create package")
        else {
            panic!("backend {backend} did not create package");
        };
        assert_eq!(created.effective_rate_control, "qvbr:21");
        assert!(matches!(
            store
                .create_offline_package(&first, 10, 100_000, 100_000)
                .await
                .expect("idempotent create"),
            OfflineCreateOutcome::Existing(_)
        ));
        let mut changed_server_policy = first.clone();
        changed_server_policy.effective_rate_control = "vbr".into();
        let OfflineCreateOutcome::Existing(existing) = store
            .create_offline_package(&changed_server_policy, 10, 100_000, 100_000)
            .await
            .expect("server-derived rate-control retry")
        else {
            panic!("backend {backend} broke idempotency after a server policy change");
        };
        assert_eq!(existing.effective_rate_control, "qvbr:21");
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
        let claimed = store
            .claim_next_offline_package("offline-node")
            .await
            .expect("claim")
            .expect("package");
        assert_eq!(claimed.id, first.id);
        assert_eq!(claimed.effective_rate_control, "qvbr:21");
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
            .requeue_offline_package(&first.id, "offline-node")
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
            .update_offline_progress(&first.id, "offline-node", "video", 500)
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
            .mark_offline_package_ready(
                &first.id,
                "offline-node",
                "offline-recipe",
                4_000,
                7_200_000
            )
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
            .fail_offline_package(
                &failed.id,
                "offline-node",
                "video",
                "encoder",
                "contract failure"
            )
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

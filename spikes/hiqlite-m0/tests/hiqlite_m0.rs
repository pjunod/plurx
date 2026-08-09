//! M0 semantic proof for the pinned hiqlite backend.
//!
//! This has a standalone manifest because SQLite remains the production
//! backend in M0. Run it explicitly with `make hiqlite-spike`; ordinary
//! workspace builds do not resolve, compile, or link hiqlite.

use std::borrow::Cow;
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

use hiqlite::macros::params;
use hiqlite::tls::ServerTlsConfig;
use hiqlite::{Client, Node, NodeConfig};
use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};
use plurx_core::store::{LibraryStore, MediaStore, SqliteStore, UserStore, WatchStore};

const RAFT_SECRET: &str = "plurx-m0-raft-secret";
const API_SECRET: &str = "plurx-m0-api-secret";

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_voters_prove_the_sql_and_transport_contracts() {
    let root = tempfile::tempdir().expect("hiqlite data root");
    let nodes = allocate_nodes(3);
    // Match hiqlite's own provider choice. The feature graph also contains
    // aws-lc-rs through its S3 dependency, so installing explicitly avoids
    // rustls having to guess between two compiled providers.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // hiqlite's process-global auto-certificate cell must be initialized
    // before several embedded nodes start concurrently in one test process.
    // Production runs one node per process and cannot hit this harness-only
    // race.
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;

    let (first, second, third) = tokio::join!(
        hiqlite::start_node(semantic_node_config(root.path(), 1, nodes.clone())),
        hiqlite::start_node(semantic_node_config(root.path(), 2, nodes.clone())),
        hiqlite::start_node(semantic_node_config(root.path(), 3, nodes.clone())),
    );
    let clients = vec![
        first.expect("start node 1"),
        second.expect("start node 2"),
        third.expect("start node 3"),
    ];
    for client in &clients {
        tokio::time::timeout(Duration::from_secs(20), client.wait_until_healthy_db())
            .await
            .expect("three-voter cluster became healthy");
    }

    let schema = clients[0]
        .batch(
            r#"CREATE TABLE items (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL,
                 version INTEGER NOT NULL DEFAULT 5,
                 entropy INTEGER NOT NULL
             );
             CREATE VIRTUAL TABLE items_fts USING fts5(title, content='items', content_rowid='id');
             CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN
                 INSERT INTO items_fts(rowid, title) VALUES (new.id, new.title);
             END;
             CREATE VIRTUAL TABLE contentless_delete_probe USING fts5(
                 title, content='', contentless_delete=1
             );
             INSERT INTO contentless_delete_probe(rowid, title)
                 VALUES (1, 'existing search row');
             CREATE TABLE audit (item_id INTEGER PRIMARY KEY, owner TEXT NOT NULL);
             CREATE TABLE default_clock (
                 id INTEGER PRIMARY KEY,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );"#,
        )
        .await
        .expect("replicated schema batch");
    assert!(schema.into_iter().all(|result| result.is_ok()));

    // RETURNING must supply generated ids without connection-local
    // last_insert_rowid(), and writes sent through a follower must route.
    let mut returned = clients[1]
        .execute_returning_one(
            "INSERT INTO items (title, entropy) VALUES ($1, $2) RETURNING id",
            params!("Blade Runner", 4_242_i64),
        )
        .await
        .expect("INSERT RETURNING through node 2");
    let item_id = returned.get::<i64>("id");
    assert_eq!(item_id, 1);

    // The FTS trigger is replay-safe for deterministic inputs and produces the
    // same derived search result on every local SQLite state machine.
    for client in &clients {
        let title = eventually(Duration::from_secs(10), || async {
            client
                .query_as_one::<String, _>(
                    "SELECT title FROM items_fts WHERE items_fts MATCH $1",
                    params!("Blade"),
                )
                .await
                .ok()
        })
        .await
        .expect("FTS trigger result on every voter");
        assert_eq!(title, "Blade Runner");
    }

    // M1c's contentless-delete design deliberately tolerates a voter whose
    // derived FTS index is already missing the row deleted from the catalogue.
    // Prove the pinned SQLite/hiqlite combination treats that delete as a
    // no-op instead of aborting the replicated write.
    clients[0]
        .execute(
            "DELETE FROM contentless_delete_probe WHERE rowid = $1",
            params!(2_i64),
        )
        .await
        .expect("deleting an absent contentless FTS row is a no-op");
    let retained = clients[0]
        .query_as_one::<i64, _>(
            "SELECT COUNT(*) FROM contentless_delete_probe WHERE rowid = $1",
            params!(1_i64),
        )
        .await
        .expect("query surviving contentless FTS row");
    assert_eq!(retained, 1);

    // Reconcile carries gone ids as one JSON parameter instead of an inlined
    // SQL IN-list. Exercise the upper end of the deferred 100k-150k probe so
    // SQLite's host-parameter ceiling cannot silently return through this
    // path.
    let gone_ids = format!(
        "[{}]",
        (1_i64..=150_000)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let started = Instant::now();
    let decoded = clients[0]
        .query_as_one::<i64, _>("SELECT COUNT(*) FROM json_each($1)", params!(&gone_ids))
        .await
        .expect("decode a 150,000-id reconcile parameter");
    assert_eq!(decoded, 150_000);
    eprintln!(
        "150,000-id reconcile JSON parameter decoded in {:.3}s",
        started.elapsed().as_secs_f64()
    );

    // Two survivors that both observed epoch 5 race to claim epoch 6. Raft
    // serialization must admit exactly one; txn() reports the loser as Ok(0),
    // so the production fence API must inspect rows affected.
    let first = clients[2].txn([
        (
            "UPDATE items SET version = version + 1 WHERE id = $1 AND version = $2",
            params!(item_id, 5),
        ),
        (
            "INSERT INTO audit (item_id, owner) SELECT $1, $2 WHERE changes() = 1",
            params!(item_id, "node-3"),
        ),
    ]);
    let second = clients[0].txn([
        (
            "UPDATE items SET version = version + 1 WHERE id = $1 AND version = $2",
            params!(item_id, 5),
        ),
        (
            "INSERT INTO audit (item_id, owner) SELECT $1, $2 WHERE changes() = 1",
            params!(item_id, "node-1"),
        ),
    ]);
    let (first, second) = tokio::join!(first, second);
    let outcomes = [
        first.expect("first contended CAS transaction"),
        second.expect("second contended CAS transaction"),
    ];
    let winners = outcomes
        .iter()
        .filter(|outcome| outcome[0].as_ref().is_ok_and(|rows| *rows == 1))
        .count();
    let losers = outcomes
        .iter()
        .filter(|outcome| outcome[0].as_ref().is_ok_and(|rows| *rows == 0))
        .count();
    assert_eq!(winners, 1, "exactly one epoch-5 writer must win");
    assert_eq!(losers, 1, "the stale writer must report Ok(0)");
    for client in &clients {
        let version = eventually(Duration::from_secs(10), || async {
            client
                .query_as_one::<i64, _>("SELECT version FROM items WHERE id = $1", params!(item_id))
                .await
                .ok()
        })
        .await
        .expect("replicated CAS result");
        assert_eq!(version, 6);
    }

    // Reconcile's guard rows rely on txn() being atomic when a later
    // statement fails. A duplicate primary key is a deterministic
    // mid-transaction error; the first insert must not survive it.
    let failed_txn = clients[0]
        .txn([
            (
                "INSERT INTO audit (item_id, owner) VALUES ($1, $2)",
                params!(9_000_i64, "rollback-probe"),
            ),
            (
                "INSERT INTO audit (item_id, owner) VALUES ($1, $2)",
                params!(9_000_i64, "duplicate-must-fail"),
            ),
        ])
        .await;
    match failed_txn {
        Ok(results) => assert!(
            results.iter().any(Result::is_err),
            "the duplicate-key statement unexpectedly succeeded"
        ),
        Err(_) => {
            // Some hiqlite errors are lifted to the outer result; either
            // shape is acceptable so long as the transaction is rolled back.
        }
    }
    let rolled_back = clients[0]
        .query_as_one::<i64, _>(
            "SELECT COUNT(*) FROM audit WHERE item_id = $1",
            params!(9_000_i64),
        )
        .await
        .expect("verify failed transaction rollback");
    assert_eq!(rolled_back, 0, "txn() left its first statement behind");

    // Bound values converge, but that fact alone says nothing about whether
    // statements or row images are replicated. The direct guard probes below
    // establish the actual hiqlite 0.14 behavior.
    let entropies = local_values(&clients, item_id).await;
    assert_eq!(entropies, vec![4_242, 4_242, 4_242]);

    // The API secret is enforced even on an otherwise reachable cluster API.
    let addresses = nodes.iter().map(|node| node.addr_api.clone()).collect();
    let wrong_secret = tokio::time::timeout(
        Duration::from_secs(10),
        Client::remote(
            addresses,
            true,
            true,
            "definitely-the-wrong-secret".into(),
            true,
            None,
        ),
    )
    .await
    .expect("wrong-secret client construction did not hang")
    .expect("remote client can be constructed before authentication");
    let unauthorized = tokio::time::timeout(
        Duration::from_secs(3),
        wrong_secret.execute("INSERT INTO audit VALUES ($1, $2)", params!(99, "intruder")),
    )
    .await;
    assert!(
        !matches!(unauthorized, Ok(Ok(_))),
        "a client with the wrong API secret wrote to the cluster"
    );
    let intruder_rows = clients[0]
        .query_as_one::<i64, _>("SELECT COUNT(*) FROM audit WHERE item_id = $1", params!(99))
        .await
        .expect("verify rejected write");
    assert_eq!(intruder_rows, 0);

    // A small snapshot threshold makes compaction observable in a bounded M0
    // test instead of requiring the production default's 10,000 entries.
    for ordinal in 0..96_i64 {
        clients[0]
            .execute(
                "INSERT INTO audit (item_id, owner) VALUES ($1, $2)",
                params!(1_000 + ordinal, "compaction"),
            )
            .await
            .expect("compaction write");
    }
    eventually(Duration::from_secs(20), || async {
        let metrics = clients[0].metrics_db().await.ok()?;
        (metrics.snapshot.is_some() && metrics.purged.is_some()).then_some(())
    })
    .await
    .expect("snapshot and log purge after bounded writes");

    // hiqlite replicates SQL statements and parameters. The write connection
    // rejects function syntax such as unixepoch() on every voter, but SQLite's
    // CURRENT_TIMESTAMP keyword bypasses those function guards and is accepted.
    for (index, client) in clients.iter().enumerate() {
        let rejected = client
            .execute(
                "INSERT INTO audit (item_id, owner) VALUES ($1, unixepoch())",
                params!(8_000_i64 + index as i64),
            )
            .await;
        assert!(
            rejected.is_err(),
            "unixepoch() unexpectedly passed through voter {}",
            index + 1
        );
    }
    clients[0]
        .execute(
            "INSERT INTO audit (item_id, owner) VALUES ($1, CURRENT_TIMESTAMP)",
            params!(8_100_i64),
        )
        .await
        .expect("CURRENT_TIMESTAMP is an unguarded SQLite keyword in hiqlite 0.14");
    for client in &clients {
        let rows = eventually(Duration::from_secs(10), || async {
            client
                .query_as_one::<i64, _>(
                    "SELECT COUNT(*) FROM audit WHERE item_id = $1",
                    params!(8_100_i64),
                )
                .await
                .ok()
                .filter(|rows| *rows == 1)
        })
        .await
        .expect("CURRENT_TIMESTAMP row on every voter");
        assert_eq!(rows, 1);
    }
    let default_rejected = clients[0]
        .execute("INSERT INTO default_clock (id) VALUES ($1)", params!(1_i64))
        .await;
    assert!(
        default_rejected.is_err(),
        "DEFAULT (unixepoch()) DDL succeeded but its implicit insert must be rejected"
    );

    drop(wrong_secret);
    // hiqlite 0.14's TLS servers use axum-server, which does not expose the
    // graceful-shutdown hook used by its cleartext test harness. Dropping this
    // dedicated Tokio runtime aborts those listener tasks after the proof;
    // calling Client::shutdown here would correctly stop Raft but leave the
    // TLS listeners alive and hang the test process.
    drop(clients);
}

/// Reproduce the M0 single-voter cost table without putting a 10,000-fsync
/// benchmark on every ordinary test run.
///
/// This is a gate, not a microbenchmark score: it asserts only the budgets in
/// `docs/CLUSTERING-PLAN.md` section 1 and prints the values recorded in
/// `docs/PHASE3-SPIKE.md`. Run it with `make hiqlite-baseline` on a quiet host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual M0 latency, RSS, and 10,000-write baseline"]
async fn single_voter_cost_stays_inside_the_m0_budget() {
    const WRITES: i64 = 10_000;
    const LOGICAL_BYTES_PER_WRITE: u64 = 6 * std::mem::size_of::<i64>() as u64;
    const FIXED_GROWTH_BUDGET: u64 = 64 * 1024 * 1024;

    let sqlite_root = tempfile::tempdir().expect("SQLite baseline root");
    let sqlite_path = sqlite_root.path().join("plurx.db");
    let sqlite = SqliteStore::open(&sqlite_path).expect("open SQLite baseline");
    let user = sqlite
        .create_user("baseline", "unused", true)
        .await
        .expect("seed baseline user");
    let library = sqlite
        .create_library(&NewLibrary {
            name: "Baseline".into(),
            kind: LibraryKind::Movies,
            paths: Vec::new(),
            anime: false,
        })
        .await
        .expect("seed baseline library");
    let item_id = sqlite
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: "Baseline".into(),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await
        .expect("seed baseline item");
    for ordinal in 0..128_i64 {
        sqlite
            .put_progress_at(user.id, item_id, ordinal, Some(100_000), Some(ordinal + 1))
            .await
            .expect("warm SQLite");
    }
    let sqlite_bytes_before = directory_bytes(sqlite_root.path());
    let mut sqlite_latencies = Vec::with_capacity(WRITES as usize);
    for ordinal in 0..WRITES {
        let started = Instant::now();
        sqlite
            .put_progress_at(
                user.id,
                item_id,
                ordinal % 90_000,
                Some(100_000),
                Some(ordinal + 1),
            )
            .await
            .expect("SQLite progress write");
        sqlite_latencies.push(started.elapsed());
    }
    let sqlite_p95 = percentile_95(&mut sqlite_latencies);
    let sqlite_growth = directory_bytes(sqlite_root.path()).saturating_sub(sqlite_bytes_before);
    drop(sqlite);

    let rss_before = resident_bytes().expect("read baseline RSS from ps");
    let hiqlite_root = tempfile::tempdir().expect("hiqlite baseline root");
    let nodes = allocate_nodes(1);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;
    let hiqlite = hiqlite::start_node(production_node_config(hiqlite_root.path(), 1, nodes))
        .await
        .expect("start one-voter baseline");
    tokio::time::timeout(Duration::from_secs(20), hiqlite.wait_until_healthy_db())
        .await
        .expect("one-voter baseline became healthy");
    let schema = hiqlite
        .batch(
            r#"CREATE TABLE files (item_id INTEGER PRIMARY KEY, duration_ms INTEGER NOT NULL);
               INSERT INTO files VALUES (1, 100000);
               CREATE TABLE watch_state (
                   user_id INTEGER NOT NULL,
                   item_id INTEGER NOT NULL,
                   position_ms INTEGER NOT NULL,
                   duration_ms INTEGER,
                   watched INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   PRIMARY KEY (user_id, item_id)
               );"#,
        )
        .await
        .expect("create hiqlite baseline schema");
    assert!(schema.into_iter().all(|result| result.is_ok()));
    for ordinal in 0..128_i64 {
        hiqlite_progress(&hiqlite, ordinal, ordinal + 1).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let rss_after = resident_bytes().expect("read warmed hiqlite RSS from ps");
    let rss_delta = rss_after.saturating_sub(rss_before);
    let hiqlite_bytes_before = directory_bytes(hiqlite_root.path());
    let mut hiqlite_latencies = Vec::with_capacity(WRITES as usize);
    for ordinal in 0..WRITES {
        let started = Instant::now();
        hiqlite_progress(&hiqlite, ordinal % 90_000, ordinal + 1).await;
        hiqlite_latencies.push(started.elapsed());
    }
    let hiqlite_p95 = percentile_95(&mut hiqlite_latencies);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let hiqlite_growth = directory_bytes(hiqlite_root.path()).saturating_sub(hiqlite_bytes_before);
    let growth_budget = WRITES as u64 * LOGICAL_BYTES_PER_WRITE * 2 + FIXED_GROWTH_BUDGET;
    let relative_latency_budget = std::cmp::max(
        sqlite_p95.saturating_mul(2),
        sqlite_p95.saturating_add(Duration::from_micros(500)),
    );

    println!(
        "M0_BASELINE sqlite_p95_ms={:.6} hiqlite_p95_ms={:.6} \
         sqlite_growth_bytes={sqlite_growth} hiqlite_growth_bytes={hiqlite_growth} \
         hiqlite_rss_delta_bytes={rss_delta}",
        sqlite_p95.as_secs_f64() * 1_000.0,
        hiqlite_p95.as_secs_f64() * 1_000.0,
    );

    assert!(hiqlite_p95 <= Duration::from_millis(25));
    assert!(
        hiqlite_p95 <= relative_latency_budget,
        "hiqlite p95 {hiqlite_p95:?} exceeded the relative budget \
         {relative_latency_budget:?} for SQLite p95 {sqlite_p95:?}"
    );
    assert!(rss_delta <= 100 * 1024 * 1024);
    assert!(
        hiqlite_growth <= growth_budget,
        "hiqlite growth {hiqlite_growth} exceeded {growth_budget} bytes"
    );

    drop(hiqlite);
}

async fn hiqlite_progress(client: &Client, position_ms: i64, updated_at: i64) {
    let duration = client
        .query_as_one::<i64, _>(
            "SELECT duration_ms FROM files WHERE item_id = $1",
            params!(1),
        )
        .await
        .expect("read hiqlite duration");
    let watched = i64::from(position_ms as f64 / duration as f64 >= 0.95);
    let mut returned = client
        .execute_returning_one(
            "INSERT INTO watch_state \
             (user_id, item_id, position_ms, duration_ms, watched, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT(user_id, item_id) DO UPDATE SET \
             position_ms = excluded.position_ms, duration_ms = excluded.duration_ms, \
             watched = MAX(watch_state.watched, excluded.watched), \
             updated_at = excluded.updated_at RETURNING position_ms",
            params!(1, 1, position_ms, duration, watched, updated_at),
        )
        .await
        .expect("hiqlite progress write");
    assert_eq!(returned.get::<i64>("position_ms"), position_ms);
}

fn percentile_95(durations: &mut [Duration]) -> Duration {
    durations.sort_unstable();
    durations[(durations.len() * 95).div_ceil(100).saturating_sub(1)]
}

fn resident_bytes() -> Option<u64> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", pid.as_str()])
        .output()
        .ok()?;
    let kibibytes = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kibibytes * 1024)
}

fn directory_bytes(root: &std::path::Path) -> u64 {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn allocate_nodes(count: u64) -> Vec<Node> {
    (1..=count)
        .map(|id| Node {
            id,
            addr_raft: format!("127.0.0.1:{}", free_port()),
            addr_api: format!("127.0.0.1:{}", free_port()),
        })
        .collect()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .expect("reserved address")
        .port()
}

fn semantic_node_config(root: &std::path::Path, node_id: u64, nodes: Vec<Node>) -> NodeConfig {
    let mut config = production_node_config(root, node_id, nodes);
    // Keep snapshot/compaction observable inside the bounded semantic test.
    config.wal_size = 8 * 1024;
    config.raft_config = NodeConfig::default_raft_config(32);
    config
}

fn production_node_config(root: &std::path::Path, node_id: u64, nodes: Vec<Node>) -> NodeConfig {
    let data_dir = root.join(format!("node-{node_id}"));
    std::fs::create_dir_all(&data_dir).expect("node data dir");
    NodeConfig {
        node_id,
        nodes,
        listen_addr_api: Cow::Borrowed("127.0.0.1"),
        listen_addr_raft: Cow::Borrowed("127.0.0.1"),
        data_dir: Cow::Owned(data_dir.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed("m0.db"),
        secret_raft: RAFT_SECRET.into(),
        secret_api: API_SECRET.into(),
        tls_raft: Some(ServerTlsConfig::TlsAutoCertificates),
        tls_api: Some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: 2 * 1024 * 1024,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    }
}

async fn local_values(clients: &[Client], item_id: i64) -> Vec<i64> {
    let mut values = Vec::with_capacity(clients.len());
    for client in clients {
        values.push(
            eventually(Duration::from_secs(10), || async {
                client
                    .query_as_one::<i64, _>(
                        "SELECT entropy FROM items WHERE id = $1",
                        params!(item_id),
                    )
                    .await
                    .ok()
            })
            .await
            .expect("replicated entropy"),
        );
    }
    values
}

async fn eventually<T, F, Fut>(timeout: Duration, mut operation: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = operation().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

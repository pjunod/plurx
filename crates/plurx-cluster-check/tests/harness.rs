//! Tests for the cluster harness itself.
//!
//! The three-voter controller (`cargo run -p plurx-cluster-check -- check`)
//! proves what happens when a voter dies, and it needs a quorum to do it. That
//! is the wrong shape for a test suite: it cannot run without three healthy
//! raft peers, so on a contended host it reports the host rather than the code.
//!
//! Everything below the failure choreography does not need a quorum. A
//! **one-voter** cluster is a real harness cluster — a real child process, the
//! real stdin/stdout protocol, the real embedded hiqlite node, the real
//! `HiqliteAuthStore` — and it elects itself leader immediately. So these tests
//! drive the identical request handlers, replicated-store exercise, dump
//! validators, and preflight refusal against one voter, and assert the outcome
//! of each. The multi-voter loss contracts stay where they belong, in
//! `make cluster-check`.

use std::path::PathBuf;
use std::process::Command;

use plurx_cluster_check::{
    allocate_nodes, free_port, harness_executable, install_crypto_provider, is_port_collision,
    node_config, prove_local_fts_rebuild, prove_local_telemetry_sidecars, require_dump_setting,
    require_quorum_error, run, run_incompatible_preflight, start_cluster_with_port_retry,
    start_cluster_with_port_retry_using, unix_now, validate_compacted_growth, validate_known_dump,
    ClusterProcesses, CompactedGrowthReport, NodeLaunch, NodeProcess, NodeSpec, PortReservation,
    Preflight, Request, Response, GROWTH_BYTES_PER_BEAT_BUDGET, INSTANCE_ID,
};
use plurx_core::store::{ClusterCompatibility, AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION};
use serde_json::json;
use sha2::{Digest, Sha256};

fn harness_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_plurx-cluster-check"))
}

/// Run the joining-voter preflight in its own process and return its exit code
/// and printed verdict.
fn preflight(specs: &[NodeSpec], compatibility: ClusterCompatibility) -> (Option<i32>, String) {
    let input = Preflight {
        addresses: specs.iter().map(|node| node.api.clone()).collect(),
        compatibility,
    };
    let output = Command::new(harness_binary())
        .arg("preflight")
        .arg(serde_json::to_string(&input).expect("encode preflight launch"))
        .output()
        .expect("run the preflight candidate");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

fn require_ok(response: Response, what: &str) {
    response
        .require_ok()
        .unwrap_or_else(|error| panic!("{what} was refused: {error:#}"));
}

#[test]
fn compacted_growth_gate_rejects_uncoalesced_commit_and_byte_volume() {
    let compacted_growth_bytes = 8_194_680;
    let report = CompactedGrowthReport {
        incoming_beats: 10_000,
        active_streams: 80,
        logical_span_seconds: 620,
        physical_progress_commits: 10_000,
        applied_index_delta: 9_999,
        before_bytes: 1_000_000,
        after_bytes: 1_000_000 + compacted_growth_bytes,
        compacted_growth_bytes,
    };
    let error = format!(
        "{:#}",
        validate_compacted_growth(&report)
            .expect_err("one durable write per heartbeat must fail both budgets")
    );
    assert!(error.contains("physical progress commits 10000 exceeded 5120"));
    assert!(error.contains("compacted growth 8194680 bytes exceeded 5120000 bytes"));
}

#[test]
fn compacted_growth_gate_rejects_bytes_over_the_per_beat_budget() {
    let incoming_beats = 10_000;
    let compacted_growth_bytes = incoming_beats * GROWTH_BYTES_PER_BEAT_BUDGET + 1;
    let report = CompactedGrowthReport {
        incoming_beats,
        active_streams: 80,
        logical_span_seconds: 620,
        physical_progress_commits: 5_040,
        applied_index_delta: 5_039,
        before_bytes: 1_000_000,
        after_bytes: 1_000_000 + compacted_growth_bytes,
        compacted_growth_bytes,
    };
    let error = format!(
        "{:#}",
        validate_compacted_growth(&report)
            .expect_err("one byte over the compacted-growth budget must fail")
    );
    assert!(error.contains("compacted growth 5120001 bytes exceeded 5120000 bytes"));
}

#[test]
fn compacted_growth_gate_allows_one_commit_window_and_index_sample_headroom() {
    let report = CompactedGrowthReport {
        incoming_beats: 10_000,
        active_streams: 80,
        logical_span_seconds: 620,
        physical_progress_commits: 5_120,
        applied_index_delta: 5_118,
        before_bytes: 1_000_000,
        after_bytes: 6_120_000,
        compacted_growth_bytes: 5_120_000,
    };
    validate_compacted_growth(&report).expect("the declared headroom stays inside the gate");
}

/// One voter, the whole protocol.
///
/// This is deliberately a single test rather than many: bootstrapping a voter
/// and replaying `Exercise` is the expensive part, and every assertion below
/// depends on the state the previous one left behind, exactly as the shipped
/// controller does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_voter_cluster_proves_the_whole_request_protocol() {
    let root = tempfile::tempdir().expect("harness test data root");
    let (mut cluster, specs) = start_cluster_with_port_retry(&harness_binary(), root.path(), 1)
        .await
        .expect("start a one-voter cluster");
    assert_eq!(cluster.node_ids(), vec![1], "one voter should be running");

    let bootstrap = cluster
        .request(1, Request::Bootstrap)
        .await
        .expect("bootstrap the voter");
    require_ok(bootstrap, "bootstrap");

    // Re-bootstrapping under a different logical identity must be refused
    // rather than silently rewriting the cluster identity.
    let drift = cluster
        .request(1, Request::RejectIdentityDrift)
        .await
        .expect("drive the identity-drift guard");
    require_ok(drift, "the identity-drift guard");

    let open = cluster.request(1, Request::Open).await.expect("reopen");
    require_ok(open, "reopening the store");

    cluster
        .wait_for_voters(&[1])
        .await
        .expect("a one-voter cluster converges on itself");
    assert_eq!(
        cluster.leader().await.expect("a leader is elected"),
        1,
        "the only voter must lead"
    );

    prove_local_telemetry_sidecars(&mut cluster)
        .await
        .expect("playback telemetry stays node-local and survives a reopen");

    for ordinal in 1..=3 {
        let exercised = cluster
            .request(1, Request::Exercise { ordinal })
            .await
            .unwrap_or_else(|error| panic!("exercise {ordinal} failed: {error:#}"));
        require_ok(exercised, &format!("exercise {ordinal}"));
    }

    // Every replicated table now holds exactly the rows the harness wrote, and
    // the digest the voter reports is anchored to the rows it returned.
    cluster
        .wait_for_equal_dumps()
        .await
        .expect("the local dump validates and its digest matches its rows");
    let catalog = cluster
        .wait_for_equal_catalog_views()
        .await
        .expect("the catalogue view settles");
    assert_eq!(
        catalog.search.len(),
        3,
        "each exercised ordinal should contribute one searchable item"
    );

    prove_local_fts_rebuild(&mut cluster, &catalog, 1)
        .await
        .expect("the search index is node-local and rebuilds to the same view");

    // That proof is only worth running if it can fail. Re-run it against a
    // baseline whose search rows differ: the delete and the rebuild behave
    // exactly as before, and the comparison at the end must reject the view.
    let mut wrong = catalog.clone();
    wrong.search.push(-1);
    let error = format!(
        "{:#}",
        prove_local_fts_rebuild(&mut cluster, &wrong, 1)
            .await
            .expect_err("a rebuilt view that differs from the baseline must be rejected")
    );
    assert!(
        error.contains("FTS rebuild did not restore the baseline view"),
        "the failure should name the mismatched rebuild, got: {error}"
    );
    // The rebuild itself still restored the real view, so the run continues.
    assert_eq!(
        cluster
            .wait_for_equal_catalog_views()
            .await
            .expect("the catalogue view settles after the rejected comparison"),
        catalog,
        "the rejected comparison must not have changed the voter's view"
    );

    let verified = cluster
        .request(1, Request::VerifyProof)
        .await
        .expect("verify the acknowledged state");
    require_ok(verified, "the acknowledged-state proof");

    let post_loss = cluster
        .request(
            1,
            Request::PostLossWrite {
                target: "follower".to_owned(),
                position_ms: 60_000,
            },
        )
        .await
        .expect("write after the simulated loss");
    require_ok(post_loss, "the post-loss write");

    let dump = match cluster.request(1, Request::Dump).await.expect("dump") {
        Response::Dump { digest, dump } => {
            assert_eq!(
                digest,
                hex::encode(Sha256::digest(dump.as_bytes())),
                "the reported digest must be anchored to the returned rows"
            );
            dump
        }
        response => panic!("unexpected dump response: {response:?}"),
    };
    require_dump_setting(&dump, "post_loss.follower", "acknowledged")
        .expect("the acknowledged post-loss setting is readable from the dump");
    assert!(
        require_dump_setting(&dump, "post_loss.follower", "denied").is_err(),
        "a setting with the wrong value must not satisfy the dump check"
    );

    // With quorum intact these are ordinary successful operations; the failure
    // shape they exist to detect is asserted directly in `require_quorum_error`.
    for request in [
        Request::Ping,
        Request::ReadWithoutQuorum,
        Request::WriteWithoutQuorum,
    ] {
        let label = format!("{request:?}");
        let response = cluster
            .request(1, request)
            .await
            .unwrap_or_else(|error| panic!("{label} failed: {error:#}"));
        require_ok(response, &label);
    }
    cluster
        .wait_for_ready(1)
        .await
        .expect("the voter reports readiness");
    cluster
        .assert_running()
        .await
        .expect("no voter exited during the run");

    // A candidate one schema version behind is refused, and refusing it must not
    // touch raft membership.
    let voters_before = metrics_voters(&mut cluster).await;
    let refused = run_incompatible_preflight(&harness_binary(), &specs)
        .await
        .expect("run the incompatible candidate");
    assert!(
        refused.contains("incompatible with voter schema"),
        "an old-schema candidate must be refused by name, got: {refused}"
    );
    assert_eq!(
        metrics_voters(&mut cluster).await,
        voters_before,
        "a refused preflight must not change raft membership"
    );

    // The same code path accepts a candidate that matches.
    assert_eq!(
        preflight(
            &specs,
            ClusterCompatibility {
                schema_version: AUTH_SCHEMA_VERSION,
                protocol_version: AUTH_PROTOCOL_VERSION,
            },
        ),
        (Some(0), "compatible".to_owned()),
        "a matching candidate must be admitted"
    );

    // Stop the voter in an orderly way rather than killing it. Loss of a killed
    // voter is proved by `a_killed_voter_leaves_the_running_set` below; killing
    // this one would also discard the coverage profile for everything it just
    // executed, because a SIGKILLed process never runs its atexit handlers.
    cluster
        .shutdown_all()
        .await
        .expect("the voter exits cleanly once its request stream closes");
    assert!(
        cluster.node_ids().is_empty(),
        "a shut-down voter must leave the running set"
    );
}

/// Killing a voter is how the controller proves process loss, so the harness
/// must treat a killed voter as gone rather than as merely quiet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_voter_leaves_the_running_set() {
    let root = tempfile::tempdir().expect("kill test root");
    let (mut cluster, _) = start_cluster_with_port_retry(&harness_binary(), root.path(), 1)
        .await
        .expect("start a one-voter cluster");
    assert_eq!(cluster.node_ids(), vec![1]);

    cluster.kill(1).await.expect("kill the voter");
    assert!(
        cluster.node_ids().is_empty(),
        "a killed voter must leave the running set"
    );
    assert!(
        cluster.request(1, Request::Ping).await.is_err(),
        "a killed voter cannot answer requests"
    );

    // Killing what is already gone is a no-op, not a second failure.
    cluster.kill(1).await.expect("re-killing a gone voter");
    cluster.kill_all().await;
}

async fn metrics_voters(cluster: &mut ClusterProcesses) -> Vec<u64> {
    match cluster
        .request(1, Request::Metrics)
        .await
        .expect("read raft metrics")
    {
        Response::Metrics { voters, .. } => voters,
        response => panic!("unexpected metrics response: {response:?}"),
    }
}

/// A voter that dies during startup is reported, not waited on.
///
/// A voter whose data directory cannot be created exits before it can answer
/// anything, so the harness is left reading a stream that is already closed.
/// The contract that matters is that it says so promptly instead of blocking
/// for `START_TIMEOUT`: a controller that hangs here would stall
/// `make cluster-check` for 45 seconds per voter with no useful message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_voter_that_dies_during_startup_is_reported_not_awaited() {
    let root = tempfile::tempdir().expect("failed-start test root");
    // A regular file, so creating `<root>/node-1` underneath it cannot succeed.
    let blocked = root.path().join("not-a-directory");
    std::fs::write(&blocked, b"").expect("create the blocking file");
    let launch = NodeLaunch {
        node_id: 1,
        root: blocked,
        nodes: vec![NodeSpec {
            id: 1,
            raft: format!("127.0.0.1:{}", free_port().expect("raft port")),
            api: format!("127.0.0.1:{}", free_port().expect("api port")),
        }],
    };

    let mut node = NodeProcess::spawn(&harness_binary(), &launch).expect("spawn the voter");
    let started = std::time::Instant::now();
    let error = format!(
        "{:#}",
        node.wait_ready()
            .await
            .expect_err("a voter that cannot bind must not report readiness")
    );
    let waited = started.elapsed();

    assert!(
        error.contains("closed its protocol stream"),
        "the failure should name the closed stream, got: {error}"
    );
    assert!(
        waited < std::time::Duration::from_secs(30),
        "a dead voter must be detected by its closed stream, not by the 45s \
         start timeout; waited {waited:?}"
    );
    let _ = node.kill().await;
}

/// Asking a cluster for a voter it does not have is an error, not a panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_voter_is_an_error_rather_than_a_panic() {
    let root = tempfile::tempdir().expect("unknown-voter test root");
    let mut cluster =
        ClusterProcesses::start(&harness_binary(), root.path(), PortReservation::empty())
            .await
            .expect("an empty cluster starts no processes");
    assert!(cluster.node_ids().is_empty());

    let error = format!(
        "{:#}",
        cluster
            .request(4, Request::Ping)
            .await
            .expect_err("voter 4 does not exist")
    );
    assert!(
        error.contains("voter 4 is not running"),
        "the error should name the missing voter, got: {error}"
    );
    let error = format!(
        "{:#}",
        cluster.kill(4).await.expect_err("voter 4 cannot be killed")
    );
    assert!(
        error.contains("unknown voter 4"),
        "the error should name the unknown voter, got: {error}"
    );
    cluster.kill_all().await;
}

fn known_dump() -> serde_json::Value {
    json!({
        "settings": [
            {"key": "instance.id", "value": INSTANCE_ID},
            {"key": "proof.node.1", "value": "acknowledged"},
            {"key": "proof.node.2", "value": "acknowledged"},
            {"key": "proof.node.3", "value": "acknowledged"},
        ],
        "users": [
            {"username": "survivor-1", "password_hash": "hash-v2-1", "is_admin": 1},
            {"username": "survivor-2", "password_hash": "hash-v2-2", "is_admin": 1},
            {"username": "survivor-3", "password_hash": "hash-v2-3", "is_admin": 1},
        ],
        "tokens": [
            {"token_hash": "survive-token-1", "device": "cluster-check"},
            {"token_hash": "survive-token-2", "device": "cluster-check"},
            {"token_hash": "survive-token-3", "device": "cluster-check"},
        ],
        "api_keys": [
            {"key_hash": "survive-key-1", "scopes": r#"["scan:trigger"]"#, "disabled": 0},
            {"key_hash": "survive-key-2", "scopes": r#"["scan:trigger"]"#, "disabled": 0},
            {"key_hash": "survive-key-3", "scopes": r#"["scan:trigger"]"#, "disabled": 0},
        ],
    })
}

#[test]
fn the_dump_validator_accepts_the_state_the_harness_writes() {
    validate_known_dump(&known_dump()).expect("the reference dump is the accepted shape");
}

/// One way a voter's dump can come back short of the acknowledged state: what
/// went wrong, the damage that produces it, and the message the validator owes.
type LostRowCase = (
    &'static str,
    Box<dyn Fn(&mut serde_json::Value)>,
    &'static str,
);

/// Every way a voter can come back short of the acknowledged state, and the
/// message the validator must produce for it. If any of these stopped failing,
/// `wait_for_equal_dumps` would pass a cluster that had silently lost rows.
#[test]
fn the_dump_validator_rejects_every_kind_of_lost_row() {
    let cases: Vec<LostRowCase> = vec![
        (
            "no settings table",
            Box::new(|dump| {
                dump.as_object_mut().expect("object").remove("settings");
            }),
            "local dump has no settings rows",
        ),
        (
            "settings is not an array",
            Box::new(|dump| dump["settings"] = json!("not-an-array")),
            "local dump has no settings rows",
        ),
        (
            "a drifted cluster identity",
            Box::new(|dump| dump["settings"][0]["value"] = json!("someone-elses-cluster")),
            "missing expected setting instance.id",
        ),
        (
            "a lost per-voter acknowledgement",
            Box::new(|dump| dump["settings"][2]["value"] = json!("pending")),
            "missing expected setting proof.node.2=acknowledged",
        ),
        (
            "no users table",
            Box::new(|dump| {
                dump.as_object_mut().expect("object").remove("users");
            }),
            "local dump has no users rows",
        ),
        (
            "a lost user",
            Box::new(|dump| {
                dump["users"].as_array_mut().expect("users").pop();
            }),
            "expected 3 surviving users, found 2",
        ),
        (
            "a user whose password replacement was lost",
            Box::new(|dump| dump["users"][1]["password_hash"] = json!("hash-v1")),
            "incorrect user proof for survivor-2",
        ),
        (
            "a user that lost its admin bit",
            Box::new(|dump| dump["users"][0]["is_admin"] = json!(0)),
            "incorrect user proof for survivor-1",
        ),
        (
            "no tokens table",
            Box::new(|dump| {
                dump.as_object_mut().expect("object").remove("tokens");
            }),
            "local dump has no tokens rows",
        ),
        (
            "a lost token",
            Box::new(|dump| {
                dump["tokens"].as_array_mut().expect("tokens").pop();
            }),
            "expected 3 surviving tokens, found 2",
        ),
        (
            "a token that lost its device",
            Box::new(|dump| dump["tokens"][2]["device"] = json!("somewhere-else")),
            "incorrect token proof for survive-token-3",
        ),
        (
            "no api_keys table",
            Box::new(|dump| {
                dump.as_object_mut().expect("object").remove("api_keys");
            }),
            "local dump has no api_keys rows",
        ),
        (
            "a lost API key",
            Box::new(|dump| {
                dump["api_keys"].as_array_mut().expect("keys").pop();
            }),
            "expected 3 surviving API keys, found 2",
        ),
        (
            "an API key that lost its scope",
            Box::new(|dump| dump["api_keys"][0]["scopes"] = json!("[]")),
            "incorrect API-key proof for survive-key-1",
        ),
        (
            "an API key that came back disabled",
            Box::new(|dump| dump["api_keys"][1]["disabled"] = json!(1)),
            "incorrect API-key proof for survive-key-2",
        ),
    ];

    for (what, damage, expected) in cases {
        let mut dump = known_dump();
        damage(&mut dump);
        let error = format!(
            "{:#}",
            validate_known_dump(&dump)
                .expect_err(&format!("{what} must be rejected by the dump validator"))
        );
        assert!(
            error.contains(expected),
            "{what}: expected an error naming {expected:?}, got: {error}"
        );
    }
}

#[test]
fn a_dump_setting_is_required_by_exact_key_and_value() {
    let dump = serde_json::to_string(&known_dump()).expect("encode dump");
    require_dump_setting(&dump, "instance.id", INSTANCE_ID).expect("an exact match is accepted");

    for (key, value) in [
        ("instance.id", "another-cluster"),
        ("proof.node.4", "acknowledged"),
    ] {
        let error = format!(
            "{:#}",
            require_dump_setting(&dump, key, value)
                .expect_err("a setting that is not present must be rejected")
        );
        assert!(
            error.contains(&format!("missing expected setting {key}={value}")),
            "the error should name the missing pair, got: {error}"
        );
    }

    assert!(
        require_dump_setting("{ not json", "instance.id", INSTANCE_ID).is_err(),
        "an undecodable dump is a failure, not an absent setting"
    );
    assert!(
        require_dump_setting("{}", "instance.id", INSTANCE_ID).is_err(),
        "a dump with no settings table is a failure"
    );
}

/// Quorum loss must fail readiness *for the right reason*. Accepting any error
/// would let an unrelated bug pass as a proof of the readiness contract.
#[test]
fn only_a_quorum_shaped_failure_proves_lost_readiness() {
    for message in [
        "not enough for a quorum",
        "request timed out",
        "there is no Raft Leader",
        "QUORUM lost",
    ] {
        require_quorum_error(Response::Error {
            message: message.to_owned(),
        })
        .unwrap_or_else(|error| panic!("{message:?} should prove lost quorum: {error:#}"));
    }

    let error = format!(
        "{:#}",
        require_quorum_error(Response::Error {
            message: "no space left on device".to_owned(),
        })
        .expect_err("an unrelated failure must not count")
    );
    assert!(
        error.contains("unrelated reason"),
        "the error should say the reason was unrelated, got: {error}"
    );

    let error = format!(
        "{:#}",
        require_quorum_error(Response::Ok).expect_err("success must never prove lost quorum")
    );
    assert!(
        error.contains("succeeded without quorum"),
        "the error should name the unexpected success, got: {error}"
    );
}

#[test]
fn only_an_ok_response_satisfies_require_ok() {
    Response::Ok.require_ok().expect("Ok is accepted");

    let error = format!(
        "{:#}",
        Response::Error {
            message: "the voter refused".to_owned(),
        }
        .require_ok()
        .expect_err("an error response must not be accepted")
    );
    assert_eq!(
        error, "the voter refused",
        "an error response should surface the voter's own message"
    );

    let error = format!(
        "{:#}",
        Response::TelemetryCount { count: 1 }
            .require_ok()
            .expect_err("a mismatched response must not be accepted")
    );
    assert!(
        error.contains("expected OK response"),
        "the error should name the mismatch, got: {error}"
    );
}

/// The protocol crosses a process boundary, so its encoding is a real contract.
#[test]
fn the_request_and_response_encoding_is_stable() {
    assert_eq!(
        serde_json::to_string(&Request::Bootstrap).expect("encode"),
        r#""Bootstrap""#
    );
    assert_eq!(
        serde_json::to_string(&Request::Exercise { ordinal: 2 }).expect("encode"),
        r#"{"Exercise":{"ordinal":2}}"#
    );
    assert_eq!(
        serde_json::to_string(&Response::Ready { node_id: 3 }).expect("encode"),
        r#"{"Ready":{"node_id":3}}"#
    );
    assert_eq!(
        serde_json::to_string(&Response::Metrics {
            leader: Some(1),
            voters: vec![1, 2, 3],
            applied_index: None,
        })
        .expect("encode"),
        r#"{"Metrics":{"leader":1,"voters":[1,2,3],"applied_index":null}}"#
    );

    let decoded: Request =
        serde_json::from_str(r#"{"PostLossWrite":{"target":"leader","position_ms":60000}}"#)
            .expect("decode");
    assert!(
        matches!(
            decoded,
            Request::PostLossWrite {
                ref target,
                position_ms: 60_000,
            } if target == "leader"
        ),
        "the post-loss target and watch position must survive the wire"
    );
    assert!(
        serde_json::from_str::<Request>(r#""Nonsense""#).is_err(),
        "an unknown request must not decode"
    );
}

#[tokio::test]
async fn every_argument_rejection_names_what_was_wrong() {
    let cases = [
        (vec!["harness", "bogus"], "unknown cluster-check mode bogus"),
        (
            vec!["harness", "node"],
            "node mode requires its launch JSON",
        ),
        (
            vec!["harness", "preflight"],
            "preflight mode requires its launch JSON",
        ),
    ];
    for (args, expected) in cases {
        let error = format!(
            "{:#}",
            run(args.iter().map(|arg| (*arg).to_owned()).collect())
                .await
                .expect_err(&format!("{args:?} must be rejected"))
        );
        assert!(
            error.contains(expected),
            "{args:?}: expected an error naming {expected:?}, got: {error}"
        );
    }

    assert!(
        run(vec![
            "harness".to_owned(),
            "node".to_owned(),
            "{not json".to_owned()
        ])
        .await
        .is_err(),
        "an undecodable launch payload must be rejected"
    );
}

/// Every verdict a crippled voter produces downstream of a lost port, so the
/// regression can assert the collision is reported *instead of* one of these.
///
/// `no such table: cluster_meta` is the one that started this: a linearizable
/// read on a voter whose raft listener died reads a state machine that never
/// applied the schema batch, which is indistinguishable from an un-migrated
/// store. The other two are what an API-port collision produces.
const STORE_VERDICTS: [&str; 4] = [
    "no such table",
    "cluster_meta",
    "replicated store operation timed out",
    "auth store has not been opened",
];

/// Start one voter with `held` already bound by somebody else and return how
/// its startup failed.
///
/// Expect a hiqlite bind panic on stderr while this runs. That panic is the
/// defect's mechanism, not a test failure: the voter's listener task is what
/// dies, and the whole point of the assertion below is that the parent now
/// hears about it.
async fn startup_error_with_an_occupied_port(occupied: Occupied) -> String {
    let root = tempfile::tempdir().expect("port collision test root");
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let held = squatter.local_addr().expect("held address").port();
    let free = free_port().expect("the voter's other port");
    let (raft, api) = match occupied {
        Occupied::Raft => (held, free),
        Occupied::Api => (free, held),
    };

    let launch = NodeLaunch {
        node_id: 1,
        root: root.path().to_path_buf(),
        nodes: vec![NodeSpec {
            id: 1,
            raft: format!("127.0.0.1:{raft}"),
            api: format!("127.0.0.1:{api}"),
        }],
    };
    let mut voter = NodeProcess::spawn(&harness_binary(), &launch).expect("spawn the voter");
    let error = voter
        .wait_ready()
        .await
        .expect_err("a voter that could not bind must not announce readiness");
    assert!(
        is_port_collision(&error),
        "a cluster start must be able to reallocate this, got: {error:#}"
    );
    let text = format!("{error:#}");
    assert!(
        text.contains(&format!("127.0.0.1:{held}")),
        "the verdict should name the address that collided, got: {text}"
    );
    for verdict in STORE_VERDICTS {
        assert!(
            !text.contains(verdict),
            "a busy port was reported as {verdict:?}: {text}"
        );
    }
    text
}

#[derive(Clone, Copy)]
enum Occupied {
    Raft,
    Api,
}

/// A port taken between allocation and bind is reported as a port collision,
/// never as a durable-state fault.
///
/// `free_port` can only observe a port free and then release it, so the voter
/// binds a port another process may already have claimed. hiqlite serves both
/// listeners from detached tasks that `.unwrap()` the serve future, so that
/// bind failure used to panic a background task and nothing else: `start_node`
/// returned `Ok`, the local-database health probe passed, and the voter
/// announced `Ready` with a dead listener. The collision then reached the gate
/// as whatever the crippled voter failed at next — for a raft-port collision,
/// `no such table: cluster_meta`, which reads as an un-migrated store.
///
/// Both listeners are asserted because they fail differently: a raft collision
/// used to bootstrap "successfully" and misreport much later, while an API
/// collision used to surface as a replicated deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_voter_handed_an_occupied_port_reports_a_collision_not_a_store_verdict() {
    for occupied in [Occupied::Raft, Occupied::Api] {
        startup_error_with_an_occupied_port(occupied).await;
    }
}

/// The classifier that decides whether a cluster start may reallocate must not
/// answer yes for anything the cluster contract asserts.
///
/// This is the other half of the regression above: reporting a collision is
/// only useful if a real store fault is still a verdict rather than something
/// the harness silently retries five times and then reports as a busy port.
#[test]
fn a_store_verdict_is_never_classified_as_a_port_collision() {
    assert!(is_port_collision(&anyhow::anyhow!(
        "port collision: a voter listener could not bind one of [127.0.0.1:9]"
    )));
    assert!(is_port_collision(&anyhow::anyhow!(
        "Os {{ code: 98, kind: AddrInUse, message: \"Address already in use\" }}"
    )));
    for verdict in STORE_VERDICTS {
        assert!(
            !is_port_collision(&anyhow::anyhow!("database error: {verdict}")),
            "{verdict:?} is a durable-state verdict, not an environment fact"
        );
    }
}

/// The production start wrapper reallocates after a classified collision.
///
/// The pre-start seam returns a classified collision on the first attempt while
/// its reservation is still held. The second call then returns a normal, fresh
/// reservation and the real cluster start must become ready on different ports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_cluster_with_port_retry_reallocates_after_a_collision() {
    let root = tempfile::tempdir().expect("port-collision retry test root");
    let mut allocations = Vec::new();
    let (mut cluster, started_specs) = start_cluster_with_port_retry_using(
        &harness_binary(),
        root.path(),
        1,
        |_| {
            let reservation = allocate_nodes(1).expect("allocate retry ports");
            allocations.push(reservation.specs().to_vec());
            Ok(reservation)
        },
        |attempt, reservation| {
            if attempt == 1 {
                anyhow::bail!(
                    "port collision: simulated bind refusal for {}",
                    reservation.specs()[0].raft
                );
            }
            Ok(())
        },
    )
    .await
    .expect("a fresh allocation should start after the collision");

    assert!(
        allocations.len() >= 2,
        "the injected collision must force at least one fresh allocation"
    );
    let successful_allocation = allocations
        .last()
        .expect("a successful attempt records its allocation");
    assert_ne!(
        allocations[0][0].raft, successful_allocation[0].raft,
        "the retry must allocate a fresh raft port"
    );
    assert_ne!(
        allocations[0][0].api, successful_allocation[0].api,
        "the retry must allocate a fresh API port"
    );
    assert_eq!(started_specs[0].id, successful_allocation[0].id);
    assert_eq!(started_specs[0].raft, successful_allocation[0].raft);
    assert_eq!(started_specs[0].api, successful_allocation[0].api);
    cluster
        .shutdown_all()
        .await
        .expect("shut down retried cluster");
}
#[test]
fn allocated_voters_get_distinct_loopback_ports() {
    let specs = allocate_nodes(3)
        .expect("allocate three voters")
        .into_specs();
    assert_eq!(
        specs.iter().map(|node| node.id).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "voter ids are one-based and dense"
    );

    let mut addresses = specs
        .iter()
        .flat_map(|node| [node.raft.clone(), node.api.clone()])
        .collect::<Vec<_>>();
    assert!(
        addresses
            .iter()
            .all(|address| address.starts_with("127.0.0.1:")),
        "the harness must stay on loopback: {addresses:?}"
    );
    let allocated = addresses.len();
    addresses.sort();
    addresses.dedup();
    assert_eq!(
        addresses.len(),
        allocated,
        "every raft and API port must be distinct"
    );

    assert!(
        allocate_nodes(0)
            .expect("allocate nothing")
            .into_specs()
            .is_empty(),
        "asking for no voters allocates no ports"
    );
    assert_ne!(free_port().expect("a free port"), 0);
}

#[test]
fn a_voter_config_lands_in_its_own_data_directory() {
    let root = tempfile::tempdir().expect("config test root");
    let launch = NodeLaunch {
        node_id: 2,
        root: root.path().to_path_buf(),
        nodes: allocate_nodes(3).expect("allocate voters").into_specs(),
    };

    let config = node_config(&launch).expect("build the voter config");
    assert_eq!(config.node_id, 2);
    assert_eq!(config.nodes.len(), 3, "every voter is in the peer list");
    assert_eq!(
        config.nodes[1].addr_raft, launch.nodes[1].raft,
        "peer addresses must come from the launch payload"
    );
    assert_eq!(config.filename_db, "auth.db");

    let data_dir = root.path().join("node-2");
    assert!(
        data_dir.is_dir(),
        "the voter's data directory must exist after configuration"
    );
    assert_eq!(config.data_dir, data_dir.to_string_lossy());
}

#[test]
fn the_harness_clock_and_crypto_provider_are_usable() {
    // The cache-staleness proof compares a store timestamp against this clock,
    // so it must be whole seconds since the epoch, not milliseconds.
    let now = unix_now().expect("read the clock");
    assert!(
        (1_700_000_000..4_000_000_000).contains(&now),
        "unix_now must be seconds since the epoch, got {now}"
    );

    // Installing twice is what happens across harness modes in one process.
    install_crypto_provider();
    install_crypto_provider();
}

/// The controller starts its voters by re-running itself, so the path it hands
/// to `NodeProcess::spawn` has to be a real executable file.
#[test]
fn the_harness_executable_is_the_running_program() {
    let executable = harness_executable().expect("resolve the harness executable");
    assert!(
        executable.is_file(),
        "voters are spawned from this path, so it must be an existing file: {}",
        executable.display()
    );
    assert_eq!(
        executable,
        std::env::current_exe().expect("read the current executable"),
        "the harness must spawn copies of itself, not some other binary"
    );
}

/// A request the voter cannot decode is answered, and the voter keeps serving.
///
/// The controller multiplexes every scenario over one long-lived stdin stream
/// per voter. If a single undecodable line ended that loop, the voter would go
/// quiet and every later request would surface as "closed its protocol stream",
/// reporting the failure against the wrong scenario. So the contract is that a
/// bad request produces an `Error` response *and* leaves the voter answering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_request_is_answered_and_the_voter_keeps_serving() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let root = tempfile::tempdir().expect("malformed-request test root");
    let launch = NodeLaunch {
        node_id: 1,
        root: root.path().to_path_buf(),
        nodes: allocate_nodes(1).expect("allocate one voter").into_specs(),
    };
    // Driven as a raw child rather than through `NodeProcess`, which can only
    // send a well-formed `Request`.
    let mut child = tokio::process::Command::new(harness_binary())
        .arg("node")
        .arg(serde_json::to_string(&launch).expect("encode the launch"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the voter");
    let mut input = child.stdin.take().expect("voter stdin");
    let mut output = BufReader::new(child.stdout.take().expect("voter stdout")).lines();

    let ready = output
        .next_line()
        .await
        .expect("read the readiness line")
        .expect("the voter announces readiness");
    assert!(
        ready.contains("Ready"),
        "the voter should announce readiness first, got: {ready}"
    );

    input
        .write_all(b"{\"NotARequest\":{}}\n")
        .await
        .expect("send an undecodable request");
    input.flush().await.expect("flush the undecodable request");
    let refused: Response = serde_json::from_str(
        &output
            .next_line()
            .await
            .expect("read the refusal")
            .expect("the voter answers an undecodable request"),
    )
    .expect("decode the refusal");
    match refused {
        Response::Error { message } => assert!(
            message.contains("unknown variant"),
            "the refusal should name the decode failure, got: {message}"
        ),
        response => panic!("an undecodable request must be refused, got: {response:?}"),
    }

    // The same voter, on the same stream, still answers the next request.
    input
        .write_all(b"\"Metrics\"\n")
        .await
        .expect("send a well-formed request");
    input.flush().await.expect("flush the well-formed request");
    let after: Response = serde_json::from_str(
        &output
            .next_line()
            .await
            .expect("read the response")
            .expect("the voter still answers after refusing one line"),
    )
    .expect("decode the metrics response");
    match after {
        Response::Metrics { .. } => {}
        response => panic!("the voter should still serve requests, got: {response:?}"),
    }

    drop(input);
    let _ = child.wait().await;
}

/// The preflight refusal is proved by exit code 42, not by a candidate merely
/// finishing. A candidate that exits any other way is a harness failure, so
/// that a silently-succeeding process can never be read as a refusal.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_that_does_not_exit_42_is_a_harness_failure() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("preflight exit test root");
    let candidate = root.path().join("candidate");
    std::fs::write(&candidate, "#!/bin/sh\nexit 7\n").expect("write the stand-in candidate");
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in candidate executable");

    let error = format!(
        "{:#}",
        run_incompatible_preflight(
            &candidate,
            &allocate_nodes(1).expect("allocate a voter").into_specs()
        )
        .await
        .expect_err("a candidate that does not exit 42 must not count as a refusal")
    );
    assert!(
        error.contains("incompatible voter exited Some(7)"),
        "the failure should name the unexpected exit status, got: {error}"
    );
}

/// A cluster whose voters cannot be spawned at all fails by naming the spawn,
/// not by reporting a converging cluster.
///
/// This asserts the reported cause only. It deliberately does not claim the
/// retry loop stopped early: the exhaustion arm returns its last error with the
/// same message, so the two are indistinguishable from the outside. See the
/// note on `start_cluster_with_port_retry` in the pull request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cluster_whose_voters_cannot_spawn_names_the_spawn_failure() {
    let root = tempfile::tempdir().expect("spawn failure test root");
    let missing = root.path().join("no-such-harness");

    let error = format!(
        "{:#}",
        start_cluster_with_port_retry(&missing, root.path(), 1)
            .await
            .map(|_| ())
            .expect_err("a missing executable cannot start a cluster")
    );
    assert!(
        error.contains("spawn cluster voter"),
        "the failure should name the spawn that failed, got: {error}"
    );
}

/// A voter that answers with an error ends the wait with that error.
///
/// Both convergence waits poll every voter until they agree. A voter that
/// reports a problem never will, so its message has to be surfaced as the
/// failure. Retrying it until the timeout would replace the real cause with a
/// generic "did not converge".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_voter_error_ends_a_convergence_wait_with_that_error() {
    let root = tempfile::tempdir().expect("voter error test root");
    let (cluster, _) = start_cluster_with_port_retry(&harness_binary(), root.path(), 1)
        .await
        .expect("start a one-voter cluster");
    // Deliberately not bootstrapped, so every store-backed request is refused.
    let mut cluster = cluster.with_convergence_timeout(std::time::Duration::from_secs(2));

    let error = format!(
        "{:#}",
        cluster
            .wait_for_equal_dumps()
            .await
            .expect_err("a voter with no store cannot produce a dump")
    );
    assert!(
        error.contains("auth store has not been opened"),
        "the dump wait should surface the voter's own error, got: {error}"
    );

    let error = format!(
        "{:#}",
        cluster
            .wait_for_equal_catalog_views()
            .await
            .expect_err("a voter with no store cannot produce a catalogue view")
    );
    assert!(
        error.contains("auth store has not been opened"),
        "the catalogue wait should surface the voter's own error, got: {error}"
    );

    // With no voters left there is no view to return, and the wait must say so
    // rather than reporting agreement it never observed.
    cluster.kill_all().await;
    let error = format!(
        "{:#}",
        cluster
            .wait_for_equal_catalog_views()
            .await
            .expect_err("an empty cluster has no catalogue view")
    );
    assert!(
        error.contains("catalog view set was empty"),
        "an empty cluster must not read as converged, got: {error}"
    );
}

/// The convergence helpers give up with a diagnostic that names what never
/// happened, instead of blocking forever.
///
/// Each state below is one the controller reads as a real failure, so the
/// message it reports is the whole value of the wait. The timeout is shortened
/// because these states never converge by construction; the controller keeps
/// the production default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_convergence_that_never_happens_is_reported_not_awaited() {
    let root = tempfile::tempdir().expect("convergence timeout test root");
    let (cluster, _) = start_cluster_with_port_retry(&harness_binary(), root.path(), 1)
        .await
        .expect("start a one-voter cluster");
    let mut cluster = cluster.with_convergence_timeout(std::time::Duration::from_secs(2));

    // A one-voter cluster never becomes a three-voter one.
    let error = format!(
        "{:#}",
        cluster
            .wait_for_voters(&[1, 2, 3])
            .await
            .expect_err("one voter cannot converge on a three-voter membership")
    );
    assert!(
        error.contains("did not converge to voters [1, 2, 3]"),
        "the failure should name the membership that never arrived, got: {error}"
    );

    cluster.kill_all().await;

    // Nothing is left to regain readiness or to elect a leader.
    let error = format!(
        "{:#}",
        cluster
            .wait_for_ready(1)
            .await
            .expect_err("a voter that is gone never becomes ready")
    );
    assert!(
        error.contains("voter 1 did not regain quorum readiness"),
        "the failure should name the voter that stayed unready, got: {error}"
    );

    let error = format!(
        "{:#}",
        cluster
            .leader()
            .await
            .expect_err("an empty cluster has no leader")
    );
    assert!(
        error.contains("did not report a leader"),
        "the failure should say no leader was reported, got: {error}"
    );
}

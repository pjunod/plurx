//! Separate-process M1b/M1c/M1d/M3a cluster validation.
//!
//! hiqlite owns process-global listener and shutdown state, so an in-process
//! three-client test cannot prove process loss. The controller starts this
//! executable three more times and drives each embedded client over a tiny
//! stdin/stdout protocol.
//!
//! The harness is a library with a thin binary on top. `cargo run -p
//! plurx-cluster-check -- check` still runs the identical three-voter
//! controller; the split exists so the crate's own tests can drive the same
//! protocol, request handlers, and validators against a one-voter cluster,
//! which needs no quorum and therefore no contended host.

use std::borrow::Cow;
use std::future::Future;
use std::io::Write as _;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use hiqlite::tls::ServerTlsConfig;
use hiqlite::{Client, Node, NodeConfig, Row};
use plurx_core::cluster::membership::{
    join_token_digest, ClusterAvailability, ClusterPeer, FinalizeJoinRequest, IssuedJoinToken,
    JoinSecrets, MembershipManager, MembershipStatus, RedeemJoinRequest,
};
use plurx_core::cluster::migration::status::{
    ReplicationHealth, ReplicationMonitor, ReplicationStatus,
};
use plurx_core::cluster::migration::{ActivationMarker, HIQLITE_WAL_SIZE_BYTES};
use plurx_core::cluster::ClusterIdentity;
use plurx_core::domain::{
    ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem, NewLibrary, NewOfflinePackage,
    OfflineCreateOutcome, OfflineLeaseOutcome, PlaybackEvent, PlaybackEventQuery, ProbeResult,
    TraktAuth,
};
use plurx_core::secrets::CredentialKey;
use plurx_core::store::{
    ApiKeyStore, ClusterCompatibility, HiqliteAuthStore, LibraryStore, MediaStore,
    OfflinePackageStore, PlaybackTelemetryStore, ReconcileOutcome, RootFingerprintStatus,
    SettingsStore, TraktStore, TranscodeCacheStore, UserStore, WatchStore, WatchedOutboxStore,
    AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::Instant as TokioInstant;

// Compile the production daemon policy directly. This avoids a test-only
// coalescer and keeps the already-governed plurxd source path as its sole home.
#[allow(dead_code)] // Response fields are consumed by HTTP, not this load driver.
#[path = "../../plurxd/src/progress.rs"]
mod production_progress;
use production_progress::ProgressCoalescer;

const RAFT_SECRET: &str = "plurx-m1b-raft-secret";
const API_SECRET: &str = "plurx-m1b-api-secret";
pub const INSTANCE_ID: &str = "m1b-cluster-check";
const START_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// The interface every harness listener binds. hiqlite composes each listener
/// from this and the port half of the node's own `addr_raft`/`addr_api`
/// (`hiqlite-0.14.0/src/start.rs:318`), which is why the two must agree.
const LISTEN_ADDR: &str = "127.0.0.1";
/// The phrase every port-collision verdict in this crate carries, and the one
/// [`is_port_collision`] recognises.
const PORT_COLLISION: &str = "port collision";
/// Exit status a voter uses when a listener lost its port. 98 is `EADDRINUSE`,
/// which is what the failed `bind` itself reported.
pub const BIND_FAILURE_EXIT: i32 = 98;
/// How many times a cluster start reallocates its ports before giving up.
const PORT_RETRY_ATTEMPTS: u32 = 5;
/// How long a voter waits for its own listeners to accept before it reports
/// them as never bound.
const LISTENER_PROOF_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the convergence helpers retry before reporting failure.
pub const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(45);
/// Incoming active-player heartbeats in the compacted-growth record.
pub const GROWTH_INCOMING_BEATS: u64 = 10_000;
/// Independent user/item streams represented by the growth load.
pub const GROWTH_ACTIVE_STREAMS: u64 = 80;
/// Logical playback cadence represented by each incoming heartbeat.
pub const GROWTH_BEAT_INTERVAL_SECS: u64 = 5;
/// Production progress coalescing window represented by the deterministic clock.
pub const GROWTH_COMMIT_WINDOW_SECS: u64 = 10;
/// Production hiqlite snapshot threshold used by the bounded gate.
pub const GROWTH_COMPACTION_LOGS: u64 = 10_000;
/// Maximum net compacted directory growth per incoming heartbeat.
pub const GROWTH_BYTES_PER_BEAT_BUDGET: u64 = 512;
/// One extra commit window per stream above the deterministic cadence result.
pub const GROWTH_COMMIT_HEADROOM_PER_STREAM: u64 = 1;
/// Maximum accepted lag or internal-entry drift in the sampled applied index.
pub const GROWTH_APPLIED_INDEX_TOLERANCE: u64 = 2;

/// Result of one post-coalescer compacted-growth load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactedGrowthReport {
    pub incoming_beats: u64,
    pub active_streams: u64,
    pub logical_span_seconds: u64,
    pub physical_progress_commits: u64,
    pub applied_index_delta: u64,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub compacted_growth_bytes: u64,
}

/// Apply the checked-in write-amplification and compacted-growth budgets.
pub fn validate_compacted_growth(report: &CompactedGrowthReport) -> Result<()> {
    if report.incoming_beats == 0 || report.active_streams == 0 {
        bail!("compacted-growth load must contain beats and active streams");
    }
    if !report.incoming_beats.is_multiple_of(report.active_streams) {
        bail!("compacted-growth beats must divide evenly across active streams");
    }
    let beats_per_stream = report.incoming_beats / report.active_streams;
    let expected_span_seconds = beats_per_stream
        .saturating_sub(1)
        .saturating_mul(GROWTH_BEAT_INTERVAL_SECS);
    if report.logical_span_seconds != expected_span_seconds {
        bail!(
            "compacted-growth logical span {} did not match {} seconds for the declared load",
            report.logical_span_seconds,
            expected_span_seconds
        );
    }
    let commit_windows = report.logical_span_seconds / GROWTH_COMMIT_WINDOW_SECS + 1;
    let commit_budget = report
        .active_streams
        .saturating_mul(commit_windows.saturating_add(GROWTH_COMMIT_HEADROOM_PER_STREAM));
    let mut violations = Vec::new();
    if report.physical_progress_commits > commit_budget {
        violations.push(format!(
            "physical progress commits {} exceeded {} for {} active streams",
            report.physical_progress_commits, commit_budget, report.active_streams
        ));
    }
    let growth_budget = report
        .incoming_beats
        .saturating_mul(GROWTH_BYTES_PER_BEAT_BUDGET);
    if report.compacted_growth_bytes > growth_budget {
        violations.push(format!(
            "compacted growth {} bytes exceeded {} bytes for {} incoming beats",
            report.compacted_growth_bytes, growth_budget, report.incoming_beats
        ));
    }
    let applied_drift = report
        .applied_index_delta
        .abs_diff(report.physical_progress_commits);
    if applied_drift > GROWTH_APPLIED_INDEX_TOLERANCE {
        violations.push(format!(
            "applied-index delta {} drifted {} entries from {} physical progress commits",
            report.applied_index_delta, applied_drift, report.physical_progress_commits
        ));
    }
    if !violations.is_empty() {
        bail!(violations.join("; "));
    }
    Ok(())
}

/// Dispatch one harness mode from a full `argv`.
///
/// `main` passes `std::env::args()` straight through, so the argument
/// contract — including every rejection — is exercised by the crate's tests.
pub async fn run(args: Vec<String>) -> Result<()> {
    match args.get(1).map(String::as_str) {
        None | Some("check") => {
            run_growth_subprocess().await?;
            controller().await
        }
        Some("membership") => run_membership_lifecycle_case().await,
        Some("growth") => compacted_growth_gate(args.get(2).map(PathBuf::from)).await,
        Some("node") => {
            let launch: NodeLaunch =
                serde_json::from_str(args.get(2).context("node mode requires its launch JSON")?)?;
            node(launch).await
        }
        Some("preflight") => {
            let preflight: Preflight = serde_json::from_str(
                args.get(2)
                    .context("preflight mode requires its launch JSON")?,
            )?;
            preflight_voter(preflight).await
        }
        Some(other) => bail!("unknown cluster-check mode {other}"),
    }
}

async fn run_growth_subprocess() -> Result<()> {
    let root = tempfile::tempdir().context("compacted-growth subprocess data root")?;
    let executable = harness_executable()?;
    with_port_retry(|attempt| {
        let executable = executable.clone();
        let attempt_root = root.path().join(format!("attempt-{attempt}"));
        async move {
            std::fs::create_dir_all(&attempt_root)?;
            let mut command = Command::new(&executable);
            command
                .arg("growth")
                .arg(&attempt_root)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            let mut child = command
                .spawn()
                .context("spawn compacted-growth subprocess")?;
            let status = match tokio::time::timeout(Duration::from_secs(300), child.wait()).await {
                Ok(status) => status.context("wait for compacted-growth subprocess")?,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    bail!("compacted-growth subprocess timed out after 300 seconds");
                }
            };
            if status.code() == Some(BIND_FAILURE_EXIT) {
                bail!("{PORT_COLLISION}: the compacted-growth voter lost one of its ports");
            }
            if !status.success() {
                bail!("compacted-growth subprocess exited {:?}", status.code());
            }
            Ok(())
        }
    })
    .await
}

async fn controller() -> Result<()> {
    println!("cluster-check: membership lifecycle 1 -> 3 -> 2");
    run_membership_lifecycle_case().await?;
    println!("cluster-check: follower loss and incompatible-voter guard");
    run_failure_case(FailureTarget::Follower).await?;
    println!("cluster-check: leader loss");
    run_failure_case(FailureTarget::Leader).await?;
    println!("cluster-check: all M1b/M1c/M1d/M3a/M3c/M3d failure contracts passed");
    Ok(())
}

async fn run_membership_lifecycle_case() -> Result<()> {
    let executable = harness_executable()?;
    let root = tempfile::tempdir().context("membership lifecycle data root")?;
    let (mut cluster, specs, cluster_root) = with_port_retry(|attempt| {
        let executable = executable.clone();
        let attempt_root = root.path().join(format!("attempt-{attempt}"));
        async move {
            let (listeners, all_specs) = allocate_nodes(3)?.into_inner();
            let cluster = ClusterProcesses::start(
                &executable,
                &attempt_root,
                PortReservation {
                    listeners,
                    specs: vec![all_specs[0].clone()],
                },
            )
            .await?;
            Ok((cluster, all_specs, attempt_root))
        }
    })
    .await?;
    cluster.request(1, Request::Bootstrap).await?.require_ok()?;

    let initial_dump = match cluster.request(1, Request::Dump).await? {
        Response::Dump { dump, .. } => dump,
        response => bail!("unexpected initial membership dump: {response:?}"),
    };
    require_dump_setting(&initial_dump, "instance.id", INSTANCE_ID)?;

    let mut first_redeemed = None;
    for node_id in 2..=3 {
        let issued = match cluster
            .request(1, Request::IssueJoinToken { ttl_ms: 120_000 })
            .await?
        {
            Response::IssuedJoinToken { token } => token,
            response => bail!("unexpected join-token response: {response:?}"),
        };
        if issued.raft_id != node_id {
            bail!(
                "join token assigned raft id {}, expected {node_id}",
                issued.raft_id
            );
        }
        let spec = specs[(node_id - 1) as usize].clone();
        let token_digest = join_token_digest(&issued.token);
        let request = RedeemJoinRequest {
            token_digest: token_digest.clone(),
            raft_id: issued.raft_id,
            node_id: format!("node-{node_id}"),
            raft_address: spec.raft,
            api_address: spec.api,
            schema_version: AUTH_SCHEMA_VERSION,
            protocol_version: AUTH_PROTOCOL_VERSION,
        };
        cluster
            .request(
                1,
                Request::RedeemJoin {
                    request: request.clone(),
                },
            )
            .await?
            .require_ok()?;
        cluster
            .spawn_node(
                &executable,
                NodeLaunch {
                    node_id,
                    root: cluster_root.clone(),
                    nodes: specs[..node_id as usize].to_vec(),
                },
            )
            .await?;
        cluster
            .request(node_id, Request::Open)
            .await?
            .require_ok()?;
        cluster
            .wait_for_voters(&(1..=node_id).collect::<Vec<_>>())
            .await?;
        cluster
            .request(
                1,
                Request::FinalizeJoin {
                    request: FinalizeJoinRequest {
                        token_digest,
                        raft_id: issued.raft_id,
                        node_id: request.node_id.clone(),
                    },
                },
            )
            .await?
            .require_ok()?;
        if node_id == 2 {
            first_redeemed = Some((request, issued.token));
        }
    }

    let (request, redeemed_token) = first_redeemed.context("missing first redeemed token")?;
    require_membership_error(
        cluster.request(1, Request::RedeemJoin { request }).await?,
        "join_token_reused",
    )?;
    let expired = match cluster
        .request(1, Request::IssueJoinToken { ttl_ms: 1 })
        .await?
    {
        Response::IssuedJoinToken { token } => token,
        response => bail!("unexpected expired-token issue response: {response:?}"),
    };
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    require_membership_error(
        cluster
            .request(
                1,
                Request::RedeemJoin {
                    request: RedeemJoinRequest {
                        token_digest: join_token_digest(&expired.token),
                        raft_id: expired.raft_id,
                        node_id: "expired-candidate".to_owned(),
                        raft_address: "127.0.0.1:1".to_owned(),
                        api_address: "127.0.0.1:2".to_owned(),
                        schema_version: AUTH_SCHEMA_VERSION,
                        protocol_version: AUTH_PROTOCOL_VERSION,
                    },
                },
            )
            .await?,
        "join_token_expired",
    )?;
    let status = match cluster.request(1, Request::MembershipStatus).await? {
        Response::MembershipStatus { status } => status,
        response => bail!("unexpected three-voter membership status: {response:?}"),
    };
    if status.availability != ClusterAvailability::HighAvailability
        || status.nodes.len() != 3
        || status.nodes.iter().any(|node| !node.reachable)
        || status.replication.health != ReplicationHealth::InSync
    {
        bail!("three-voter membership status was not healthy: {status:?}");
    }
    let public_status = serde_json::to_string(&status)?;
    if public_status.contains(&redeemed_token) || public_status.contains("127.0.0.1") {
        bail!("public node records exposed token or address material");
    }

    prove_cluster_discovery(&mut cluster).await?;

    let leader = cluster.leader().await?;
    let target = (2..=3)
        .find(|node_id| *node_id != leader)
        .context("choose a removable follower")?;
    // M3c (`CLUSTERING-PLAN.md` §6.7): removal resolves the offline work the
    // departing node owns instead of refusing forever. Seeded on the target's
    // own process so the fixture's source files exist where its packages say
    // they do.
    let target_node = format!("node-{target}");
    let media_dir = root.path().join(format!("media-{target_node}"));
    let offline_user = match cluster
        .request(
            target,
            Request::SeedOfflineRemovalWork {
                node_id: target_node.clone(),
                media_dir: media_dir.to_string_lossy().to_string(),
            },
        )
        .await?
    {
        Response::SeededOfflineRemovalWork { user_id } => user_id,
        response => bail!("unexpected offline removal seed response: {response:?}"),
    };

    // The refusal survives, narrowed to what the policy genuinely cannot
    // resolve: a download that is in flight right now. Lifting the blanket
    // refusal must not turn removal into "always succeeds".
    let transfer_package = format!("{TRANSFER_PACKAGE}-{target_node}");
    match cluster
        .request(
            1,
            Request::RemoveVoter {
                node_id: target_node.clone(),
            },
        )
        .await?
    {
        Response::MembershipError { code, message } if code == "node_owns_offline_work" => {
            if !message.contains("transferring") {
                bail!("in-flight transfer refusal gave the operator no reason: {message}");
            }
        }
        response => bail!("removal did not refuse an in-flight transfer: {response:?}"),
    }
    // The operator takes the action the refusal named.
    match cluster
        .request(
            1,
            Request::DeleteOfflinePackage {
                package_id: transfer_package,
                user_id: offline_user,
            },
        )
        .await?
    {
        Response::Flag { value: true } => {}
        response => bail!("could not clear the in-flight transfer fixture: {response:?}"),
    }

    // One more download is requested on the departing node while the removal
    // is resolving. The node serves its API until the membership change
    // commits, so a package created after the plan was drawn is real; a
    // removal that resolved only its opening snapshot would commit and leave
    // this one owned by a node that no longer exists.
    let late_package = format!("{LATE_PACKAGE}-{target_node}");
    cluster
        .request(
            target,
            Request::SeedOfflineWorkDuringRemoval {
                node_id: target_node.clone(),
                media_dir: media_dir.to_string_lossy().to_string(),
                user_id: offline_user,
                delay_ms: LATE_PACKAGE_DELAY_MS,
            },
        )
        .await?
        .require_ok()?;

    cluster
        .request(
            1,
            Request::RemoveVoter {
                node_id: target_node.clone(),
            },
        )
        .await?
        .require_ok()?;

    // Either §6.7 outcome closes it — moved to a survivor, or failed with its
    // reservation released. The outcome this exists to catch is the third one:
    // still queued on a node that is now a tombstone, holding the traveller's
    // byte budget until the seven-day expiry.
    match offline_summary(&mut cluster, &late_package, offline_user).await? {
        Response::OfflinePackageSummary {
            state,
            node_id,
            error_code,
            ..
        } if state == "queued" && node_id != target_node && error_code.is_none() => {}
        Response::OfflinePackageSummary {
            state,
            error_code,
            reserved_bytes,
            ..
        } if state == "failed"
            && error_code.as_deref() == Some("node_removed")
            && reserved_bytes == 0 => {}
        response => bail!(
            "a download requested while the removal was resolving was left owned by the removed \
             node: {response:?}"
        ),
    }

    // A survivor proved it reads the source, so the work moved rather than
    // dying with the node.
    let movable_package = format!("{MOVABLE_PACKAGE}-{target_node}");
    let movable = offline_summary(&mut cluster, &movable_package, offline_user).await?;
    match &movable {
        Response::OfflinePackageSummary {
            state,
            node_id,
            error_code,
            ..
        } if state == "queued" && *node_id != target_node && error_code.is_none() => {}
        response => bail!("a verified package was not requeued on a survivor: {response:?}"),
    }
    let Response::OfflinePackageSummary {
        node_id: new_owner, ..
    } = movable
    else {
        unreachable!("matched above")
    };

    // No survivor could read this one, so it failed loudly with the stable
    // code and gave its reservation back instead of holding the traveller's
    // byte budget until the seven-day expiry.
    match offline_summary(
        &mut cluster,
        &format!("{STRANDED_PACKAGE}-{target_node}"),
        offline_user,
    )
    .await?
    {
        Response::OfflinePackageSummary {
            state,
            error_code,
            reserved_bytes,
            ..
        } if state == "failed"
            && error_code.as_deref() == Some("node_removed")
            && reserved_bytes == 0 => {}
        response => bail!("an unverifiable package was not failed as node_removed: {response:?}"),
    }

    // A ready package whose bytes lived only on the removed node stops
    // advertising itself as ready, even though its source is perfectly
    // readable elsewhere. Requeueing it would risk handing a resuming
    // downloader a different encoder's generation behind the same lease URL.
    match offline_summary(
        &mut cluster,
        &format!("{READY_PACKAGE}-{target_node}"),
        offline_user,
    )
    .await?
    {
        Response::OfflinePackageSummary {
            state,
            error_code,
            reserved_bytes,
            actual_bytes,
            ..
        } if state == "failed"
            && error_code.as_deref() == Some("node_removed")
            && reserved_bytes == 0
            && actual_bytes.is_none() => {}
        response => bail!("a ready package survived its node's removal: {response:?}"),
    }

    // The re-homed package is ordinary claimable work on its new owner, and
    // the node that just left can no longer publish it.
    let new_owner_voter = (1..=3)
        .find(|voter| format!("node-{voter}") == new_owner)
        .context("requeued package landed on a node outside the cluster")?;
    // The package requested mid-removal can share this queue, so claim until
    // the one under test comes out instead of assuming it is first.
    let mut claimed_movable = false;
    for _ in 0..2 {
        match cluster
            .request(
                new_owner_voter,
                Request::ClaimNextOfflinePackage {
                    node_id: new_owner.clone(),
                },
            )
            .await?
        {
            Response::ClaimedOfflinePackage {
                package_id: Some(package_id),
            } if package_id == movable_package => {
                claimed_movable = true;
                break;
            }
            Response::ClaimedOfflinePackage {
                package_id: Some(package_id),
            } if package_id == late_package => {}
            response => {
                bail!("the requeued package was not claimable on its new owner: {response:?}")
            }
        }
    }
    if !claimed_movable {
        bail!("the requeued package never became claimable on its new owner");
    }
    match cluster
        .request(
            1,
            Request::PublishOfflinePackage {
                package_id: movable_package.clone(),
                node_id: target_node.clone(),
            },
        )
        .await?
    {
        Response::Flag { value: false } => {}
        response => bail!("the removed node still published re-homed work: {response:?}"),
    }
    match cluster
        .request(
            new_owner_voter,
            Request::PublishOfflinePackage {
                package_id: movable_package,
                node_id: new_owner,
            },
        )
        .await?
    {
        Response::Flag { value: true } => {}
        response => bail!("the new owner could not complete the requeued package: {response:?}"),
    }

    cluster
        .request(1, Request::ResetContractState)
        .await?
        .require_ok()?;
    cluster
        .request(
            target,
            Request::HeartbeatPreservesTombstone {
                node_id: format!("node-{target}"),
            },
        )
        .await?
        .require_ok()?;
    cluster
        .request(
            target,
            Request::TombstoneOfflineFence {
                node_id: format!("node-{target}"),
            },
        )
        .await?
        .require_ok()?;
    let remaining = (1..=3)
        .filter(|node_id| *node_id != target)
        .collect::<Vec<_>>();
    cluster.wait_for_voters(&remaining).await?;
    cluster.kill(target).await?;

    let status = match cluster.request(1, Request::MembershipStatus).await? {
        Response::MembershipStatus { status } => status,
        response => bail!("unexpected two-voter membership status: {response:?}"),
    };
    if status.availability != ClusterAvailability::DegradedReconfiguration
        || status.nodes.len() != 2
        || status.replication.health != ReplicationHealth::Degraded
    {
        bail!("two-voter membership status did not advertise degradation: {status:?}");
    }

    let leader = cluster.leader().await?;
    let quorum_target = remaining
        .iter()
        .copied()
        .find(|node_id| *node_id != leader)
        .context("choose non-leader for quorum-loss refusal")?;
    require_membership_error(
        cluster
            .request(
                1,
                Request::RemoveVoter {
                    node_id: format!("node-{quorum_target}"),
                },
            )
            .await?,
        "removal_would_lose_quorum",
    )?;
    let final_dump = match cluster.request(1, Request::Dump).await? {
        Response::Dump { dump, .. } => dump,
        response => bail!("unexpected final membership dump: {response:?}"),
    };
    require_dump_setting(&final_dump, "instance.id", INSTANCE_ID)?;
    cluster.kill(quorum_target).await?;
    let no_quorum_status = match cluster.request(leader, Request::MembershipStatus).await? {
        Response::MembershipStatus { status } => status,
        response => bail!("roster was unavailable during quorum loss: {response:?}"),
    };
    if no_quorum_status.availability != ClusterAvailability::DegradedReconfiguration
        || no_quorum_status.nodes.len() != 2
        || no_quorum_status.replication.health != ReplicationHealth::Degraded
    {
        bail!("quorum-loss roster did not explain the outage: {no_quorum_status:?}");
    }
    cluster.shutdown_all().await?;
    Ok(())
}

/// M3d: real voter processes must converge on one logical name while exposing
/// distinct node records through both LAN discovery projections.
async fn prove_cluster_discovery(cluster: &mut ClusterProcesses) -> Result<()> {
    let seeds = ["Living Room", "wrong-node-two", "wrong-node-three"];
    let mut node_ids = std::collections::BTreeSet::new();
    for (index, seed_name) in seeds.iter().enumerate() {
        let node = u64::try_from(index + 1)?;
        match cluster
            .request(
                node,
                Request::Advertisement {
                    seed_name: (*seed_name).to_owned(),
                },
            )
            .await?
        {
            Response::Advertisement {
                instance_id,
                node_id,
                name,
                gdm,
                mdns_name,
                mdns_instance_id,
                mdns_node_id,
            } => {
                if instance_id != INSTANCE_ID
                    || mdns_instance_id != INSTANCE_ID
                    || name != "Living Room"
                    || mdns_name != name
                    || mdns_node_id != node_id
                    || !gdm.contains(&format!("Resource-Identifier: {INSTANCE_ID}\r\n"))
                    || !gdm.contains(&format!("Node-Identifier: {node_id}\r\n"))
                    || !gdm.contains("Name: Living Room\r\n")
                {
                    bail!("voter {node} advertised inconsistent discovery identity");
                }
                node_ids.insert(node_id);
            }
            response => bail!("unexpected voter {node} advertisement: {response:?}"),
        }
    }
    if node_ids.len() != 3 {
        bail!("three voters did not advertise three distinct node records: {node_ids:?}");
    }

    cluster
        .request(
            2,
            Request::RenameServer {
                name: "Cinema".to_owned(),
            },
        )
        .await?
        .require_ok()?;
    for node in 1..=3 {
        match cluster
            .request(
                node,
                Request::Advertisement {
                    seed_name: "ignored-after-bootstrap".to_owned(),
                },
            )
            .await?
        {
            Response::Advertisement {
                name,
                mdns_name,
                gdm,
                ..
            } if name == "Cinema" && mdns_name == "Cinema" && gdm.contains("Name: Cinema\r\n") => {}
            response => {
                bail!("renamed discovery identity did not converge on voter {node}: {response:?}")
            }
        }
    }
    Ok(())
}

async fn offline_summary(
    cluster: &mut ClusterProcesses,
    package_id: &str,
    user_id: i64,
) -> Result<Response> {
    cluster
        .request(
            1,
            Request::OfflinePackageSummary {
                package_id: package_id.to_owned(),
                user_id,
            },
        )
        .await
}

fn require_membership_error(response: Response, expected: &str) -> Result<()> {
    match response {
        Response::MembershipError { code, .. } if code == expected => Ok(()),
        response => bail!("expected membership error {expected}, got {response:?}"),
    }
}

async fn compacted_growth_gate(root: Option<PathBuf>) -> Result<()> {
    println!("cluster-check: post-coalescer compacted growth");
    install_crypto_provider();
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;

    let owned_root = root
        .is_none()
        .then(|| tempfile::tempdir().context("compacted-growth data root"))
        .transpose()?;
    let root = root.unwrap_or_else(|| {
        owned_root
            .as_ref()
            .expect("missing owned compacted-growth root")
            .path()
            .to_path_buf()
    });
    let reservation = allocate_nodes(1)?;
    let (listeners, specs) = reservation.into_inner();
    let launch = NodeLaunch {
        node_id: 1,
        root,
        nodes: specs,
    };
    // This voter runs hiqlite in-process rather than behind the stdin/stdout
    // protocol, so a lost port would otherwise surface as a growth verdict.
    // The reservation is dropped here: hiqlite binds its own sockets from the
    // address strings, so we must release the port before it can bind it.
    drop(listeners);
    install_bind_failure_guard(BindFailureChannel::Stderr, voter_listen_addrs(&launch)?);
    let mut config = node_config(&launch)?;
    config.filename_db = Cow::Borrowed("growth.db");
    config.raft_config = NodeConfig::default_raft_config(GROWTH_COMPACTION_LOGS);
    let client = hiqlite::start_node(config)
        .await
        .context("start compacted-growth voter")?;
    tokio::time::timeout(START_TIMEOUT, client.wait_until_healthy_db())
        .await
        .context("compacted-growth voter health timed out")?;
    let metrics_client = client.clone();
    let telemetry_path = launch.root.join("node-1").join("telemetry-growth.db");
    let store = Arc::new(
        HiqliteAuthStore::bootstrap(client, "compacted-growth-check", &telemetry_path)
            .await
            .context("bootstrap compacted-growth store")?,
    );

    let (users, item_id) = growth_fixture(store.as_ref()).await?;
    let initial_snapshot = snapshot_index(&metrics_client).await?;
    let warm_snapshot = ensure_compaction_after(
        &metrics_client,
        store.as_ref(),
        initial_snapshot,
        "baseline warm-up",
    )
    .await?;
    // The first snapshot also lets SQLite settle its state-machine WAL. Take a
    // second compacted baseline so its one-time checkpoint is not mistaken for
    // negative progress growth in the measured cycle.
    let baseline_snapshot = ensure_compaction_after(
        &metrics_client,
        store.as_ref(),
        warm_snapshot,
        "baseline settle",
    )
    .await?;
    let data_dir = launch.root.join("node-1");
    let before_bytes = stable_directory_bytes(&data_dir).await?;

    let applied_before = applied_index(&metrics_client).await?;
    // Put the deterministic clock well ahead of wall time so the background
    // flush workers cannot race this accelerated, beat-by-beat load. Each
    // request still runs the production due/commit path against a real store.
    let logical_start = TokioInstant::now() + Duration::from_secs(24 * 60 * 60);
    let logical_now = Arc::new(RwLock::new(logical_start));
    let coalescer: Arc<ProgressCoalescer> = ProgressCoalescer::with_time_source(store.clone(), {
        let logical_now = Arc::clone(&logical_now);
        move || {
            *logical_now
                .read()
                .expect("compacted-growth logical clock poisoned")
        }
    });
    let beats_per_stream = GROWTH_INCOMING_BEATS / GROWTH_ACTIVE_STREAMS;
    let logical_span_seconds = beats_per_stream
        .saturating_sub(1)
        .saturating_mul(GROWTH_BEAT_INTERVAL_SECS);
    let expected_commits_per_stream = logical_span_seconds / GROWTH_COMMIT_WINDOW_SECS + 1;
    let expected_progress_commits = GROWTH_ACTIVE_STREAMS * expected_commits_per_stream;
    let mut synchronous_commits = 0_u64;
    for beat in 0..beats_per_stream {
        *logical_now
            .write()
            .expect("compacted-growth logical clock poisoned") =
            logical_start + Duration::from_secs(beat.saturating_mul(GROWTH_BEAT_INTERVAL_SECS));
        let mut tasks = tokio::task::JoinSet::new();
        for user_id in users.iter().copied() {
            let coalescer = Arc::clone(&coalescer);
            tasks.spawn(async move {
                let update = coalescer
                    .put(
                        user_id,
                        item_id,
                        i64::try_from((beat + 1) * 1_000)?,
                        Some(10_000_000),
                    )
                    .await?;
                Ok::<u64, anyhow::Error>(u64::from(update.committed))
            });
        }
        while let Some(result) = tasks.join_next().await {
            synchronous_commits += result.context("coalesced stream task panicked")??;
        }
    }
    let drained = u64::try_from(coalescer.drain().await.context("drain coalescer")?)?;
    let physical_progress_commits = synchronous_commits.saturating_add(drained);
    if physical_progress_commits != expected_progress_commits || drained != 0 {
        bail!(
            "coalescer produced {synchronous_commits} synchronous and {drained} trailing commits; expected {expected_progress_commits} synchronous and no trailing commits"
        );
    }
    let applied_after = applied_index(&metrics_client).await?;
    let applied_index_delta = applied_after.saturating_sub(applied_before);
    let measured_snapshot = ensure_compaction_after(
        &metrics_client,
        store.as_ref(),
        baseline_snapshot,
        "coalesced load",
    )
    .await?;
    // hiqlite's retained WAL segment alternates allocation across adjacent
    // compactions. Compare equally settled, two-cycle states so that rollover
    // is not reported as durable progress growth (or as a negative delta).
    let _ = ensure_compaction_after(
        &metrics_client,
        store.as_ref(),
        measured_snapshot,
        "coalesced settle",
    )
    .await?;
    let after_bytes = stable_directory_bytes(&data_dir).await?;
    let report = CompactedGrowthReport {
        incoming_beats: GROWTH_INCOMING_BEATS,
        active_streams: GROWTH_ACTIVE_STREAMS,
        logical_span_seconds,
        physical_progress_commits,
        applied_index_delta,
        before_bytes,
        after_bytes,
        compacted_growth_bytes: after_bytes.saturating_sub(before_bytes),
    };
    validate_compacted_growth(&report)?;

    let raw_before_bytes = after_bytes;
    let raw_snapshot = snapshot_index(&metrics_client).await?;
    let raw_applied_before = applied_index(&metrics_client).await?;
    for beat in 0..GROWTH_INCOMING_BEATS {
        let user_id = users[usize::try_from(beat % GROWTH_ACTIVE_STREAMS)?];
        store
            .put_progress(
                user_id,
                item_id,
                i64::try_from((beat / GROWTH_ACTIVE_STREAMS + 1) * 1_000)?,
                Some(10_000_000),
            )
            .await
            .context("raw induced-regression progress write")?;
    }
    let raw_applied_after = applied_index(&metrics_client).await?;
    let raw_measured_snapshot =
        ensure_compaction_after(&metrics_client, store.as_ref(), raw_snapshot, "raw control")
            .await?;
    let _ = ensure_compaction_after(
        &metrics_client,
        store.as_ref(),
        raw_measured_snapshot,
        "raw settle",
    )
    .await?;
    let raw_after_bytes = stable_directory_bytes(&data_dir).await?;
    let raw_report = CompactedGrowthReport {
        incoming_beats: GROWTH_INCOMING_BEATS,
        active_streams: GROWTH_ACTIVE_STREAMS,
        logical_span_seconds,
        physical_progress_commits: GROWTH_INCOMING_BEATS,
        applied_index_delta: raw_applied_after.saturating_sub(raw_applied_before),
        before_bytes: raw_before_bytes,
        after_bytes: raw_after_bytes,
        compacted_growth_bytes: raw_after_bytes.saturating_sub(raw_before_bytes),
    };
    let raw_rejection = validate_compacted_growth(&raw_report)
        .expect_err("bypassing the coalescer must fail the growth gate");
    let raw_rejection = format!("{raw_rejection:#}");
    for required in ["physical progress commits", "compacted growth"] {
        if !raw_rejection.contains(required) {
            bail!("raw control did not exercise the {required} budget: {raw_rejection}");
        }
    }

    println!(
        "CLUSTER_GROWTH incoming_beats={} active_streams={} beat_interval_seconds={} \
         logical_span_seconds={} physical_commits={} applied_index_delta={} commit_budget={} \
         before_bytes={} after_bytes={} compacted_growth_bytes={} \
         bytes_per_beat={:.6} budget_bytes_per_beat={} \
         raw_control_physical_commits={} raw_control_applied_index_delta={} \
         raw_control_growth_bytes={} raw_control_bytes_per_beat={:.6} \
         raw_control_rejected={}",
        report.incoming_beats,
        report.active_streams,
        GROWTH_BEAT_INTERVAL_SECS,
        report.logical_span_seconds,
        report.physical_progress_commits,
        report.applied_index_delta,
        report.active_streams * (expected_commits_per_stream + GROWTH_COMMIT_HEADROOM_PER_STREAM),
        report.before_bytes,
        report.after_bytes,
        report.compacted_growth_bytes,
        report.compacted_growth_bytes as f64 / report.incoming_beats as f64,
        GROWTH_BYTES_PER_BEAT_BUDGET,
        raw_report.physical_progress_commits,
        raw_report.applied_index_delta,
        raw_report.compacted_growth_bytes,
        raw_report.compacted_growth_bytes as f64 / raw_report.incoming_beats as f64,
        raw_rejection,
    );
    Ok(())
}

async fn growth_fixture(store: &HiqliteAuthStore) -> Result<(Vec<i64>, i64)> {
    let library = store
        .create_library(&NewLibrary {
            name: "Compacted growth".to_owned(),
            kind: LibraryKind::Movies,
            paths: vec![PathBuf::from("/cluster-growth")],
            anime: false,
        })
        .await?;
    let item_id = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: "Progress load".to_owned(),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await?;
    let mut users = Vec::with_capacity(usize::try_from(GROWTH_ACTIVE_STREAMS)?);
    for ordinal in 0..GROWTH_ACTIVE_STREAMS {
        users.push(
            store
                .create_user(&format!("growth-{ordinal}"), "hash", false)
                .await?
                .id,
        );
    }
    Ok((users, item_id))
}

async fn applied_index(client: &Client) -> Result<u64> {
    client
        .metrics_db()
        .await?
        .last_applied
        .map(|log| log.index)
        .context("replicated store has no applied log index")
}

async fn snapshot_index(client: &Client) -> Result<u64> {
    Ok(client
        .metrics_db()
        .await?
        .snapshot
        .map(|log| log.index)
        .unwrap_or(0))
}

async fn ensure_compaction_after(
    client: &Client,
    store: &HiqliteAuthStore,
    previous_snapshot: u64,
    phase: &str,
) -> Result<u64> {
    let marker = format!("cluster.growth.compaction.{phase}");
    for ordinal in 0..=GROWTH_COMPACTION_LOGS + 512 {
        if ordinal % 64 == 0 {
            let metrics = client.metrics_db().await?;
            if let Some(snapshot) = metrics.snapshot.filter(|log| log.index > previous_snapshot) {
                return wait_for_purge(client, snapshot.index, phase).await;
            }
        }
        store.put_setting(&marker, &ordinal.to_string()).await?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let metrics = client.metrics_db().await?;
        if let Some(snapshot) = metrics.snapshot.filter(|log| log.index > previous_snapshot) {
            return wait_for_purge(client, snapshot.index, phase).await;
        }
        if Instant::now() >= deadline {
            bail!("{phase} did not create a snapshot after the bounded write load");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_purge(client: &Client, snapshot: u64, phase: &str) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(30);
    // The production raft config deliberately retains one log that is already
    // represented by the snapshot. Everything before it must be gone.
    let required_purge = snapshot.saturating_sub(1);
    loop {
        let metrics = client.metrics_db().await?;
        if metrics
            .purged
            .is_some_and(|log| log.index >= required_purge)
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            bail!("{phase} snapshot {snapshot} was not followed by purge through {required_purge}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn stable_directory_bytes(root: &Path) -> Result<u64> {
    // Snapshot and WAL cleanup runs after the Raft purge metric advances.
    // Let that task begin, then require three seconds of an unchanged total.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = None;
    let mut stable_samples = 0_u8;
    loop {
        let current = directory_bytes(root)?;
        if previous == Some(current) {
            stable_samples += 1;
            if stable_samples >= 30 {
                return Ok(current);
            }
        } else {
            previous = Some(current);
            stable_samples = 0;
        }
        if Instant::now() >= deadline {
            bail!("compacted directory size did not settle under {root:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[derive(Clone, Copy, Debug)]
enum FailureTarget {
    Leader,
    Follower,
}

async fn run_failure_case(target: FailureTarget) -> Result<()> {
    let executable = harness_executable()?;
    let root = tempfile::tempdir().context("cluster-check data root")?;
    let (mut cluster, specs) = start_cluster_with_port_retry(&executable, root.path(), 3).await?;

    cluster.request(1, Request::Bootstrap).await?.require_ok()?;
    cluster
        .request(1, Request::RejectIdentityDrift)
        .await?
        .require_ok()?;
    for node_id in 2..=3 {
        cluster
            .request(node_id, Request::Open)
            .await?
            .require_ok()?;
    }
    cluster.wait_for_voters(&[1, 2, 3]).await?;
    for node_id in 1..=3 {
        let status = cluster
            .wait_for_replication_health(node_id, ReplicationHealth::InSync)
            .await?;
        if !status.clustered
            || status.last_applied_term.is_none()
            || status.last_applied_index.is_none()
            || status.last_converged_at.is_none()
        {
            bail!("voter {node_id} in-sync status omitted its convergence point: {status:?}");
        }
    }
    prove_local_telemetry_sidecars(&mut cluster).await?;

    // Every voter exercises an immediate cache read after its acknowledged
    // write. Record the current leader so the harness explicitly proves that
    // at least one of those call paths ran through a non-leader.
    let leader = cluster.leader().await?;
    let mut exercised_follower = false;
    for ordinal in 1..=3 {
        exercised_follower |= ordinal != leader;
        cluster
            .request(ordinal, Request::Exercise { ordinal })
            .await?
            .require_ok()?;
    }
    if !exercised_follower {
        bail!("cache read-after-write proof did not exercise a follower");
    }
    cluster.wait_for_equal_dumps().await?;
    let catalog = cluster.wait_for_equal_catalog_views().await?;
    if matches!(target, FailureTarget::Follower) {
        prove_local_fts_rebuild(&mut cluster, &catalog, 2).await?;
    }

    let leader = cluster.leader().await?;
    let target_id = match target {
        FailureTarget::Leader => leader,
        FailureTarget::Follower => (1..=3)
            .find(|node_id| *node_id != leader)
            .context("choose follower")?,
    };
    let failure_name = format!("{target:?}").to_ascii_lowercase();
    cluster.kill(target_id).await?;

    let survivor = (1..=3)
        .find(|node_id| *node_id != target_id)
        .context("choose survivor")?;
    cluster.wait_for_ready(survivor).await?;
    cluster
        .request(survivor, Request::VerifyProof)
        .await?
        .require_ok()?;
    cluster
        .request(
            survivor,
            Request::PostLossWrite {
                target: failure_name.clone(),
                position_ms: 60_000,
            },
        )
        .await?
        .require_ok()?;
    let current_leader = cluster.leader().await?;
    let first_degraded = cluster
        .wait_for_replication_health(current_leader, ReplicationHealth::Degraded)
        .await?;
    let first_lag = first_degraded
        .behind_by
        .filter(|lag| *lag > 0)
        .context("first degraded status omitted its known lag")?;
    let converged_at = first_degraded
        .last_converged_at
        .context("first degraded status omitted its prior convergence")?;

    let repeated_degraded = cluster
        .wait_for_replication_health(current_leader, ReplicationHealth::Degraded)
        .await?;
    if repeated_degraded
        .behind_by
        .is_none_or(|lag| lag < first_lag)
        || repeated_degraded.last_converged_at != Some(converged_at)
    {
        bail!(
            "repeated degraded sample forgot known lag or prior convergence: first={first_degraded:?}, repeated={repeated_degraded:?}"
        );
    }

    cluster
        .request(
            survivor,
            Request::PostLossWrite {
                target: format!("{failure_name}-later"),
                position_ms: 90_000,
            },
        )
        .await?
        .require_ok()?;
    let current_leader = cluster.leader().await?;
    let advanced_degraded = cluster
        .wait_for_replication_health(current_leader, ReplicationHealth::Degraded)
        .await?;
    if advanced_degraded
        .behind_by
        .is_none_or(|lag| lag <= first_lag)
        || advanced_degraded.last_converged_at != Some(converged_at)
    {
        bail!(
            "degraded status did not grow its lag after another acknowledged watch write: first={first_degraded:?}, advanced={advanced_degraded:?}"
        );
    }
    cluster.wait_for_equal_dumps().await?;
    let current_leader = cluster.leader().await?;
    let follower = cluster
        .node_ids()
        .into_iter()
        .find(|node_id| *node_id != current_leader)
        .context("choose surviving follower for replication status")?;
    let follower_status = cluster
        .wait_for_replication_health(follower, ReplicationHealth::InSync)
        .await?;
    if !follower_status
        .explanation
        .contains("other nodes is visible on the leader")
        || follower_status.explanation.contains("every reporting peer")
    {
        bail!(
            "follower replication status claimed peer visibility it does not have: {follower_status:?}"
        );
    }
    let post_loss_key = format!("post_loss.{failure_name}");
    let post_loss_dump = match cluster.request(survivor, Request::Dump).await? {
        Response::Dump { digest, dump } => {
            if digest != hex::encode(Sha256::digest(dump.as_bytes())) {
                bail!("post-loss dump digest is not anchored to the returned rows");
            }
            dump
        }
        response => bail!("unexpected post-loss dump response: {response:?}"),
    };
    require_dump_setting(&post_loss_dump, &post_loss_key, "acknowledged")?;
    if cluster.wait_for_equal_catalog_views().await?.search.len() != 3 {
        bail!("post-loss catalogue/search proof lost rows");
    }

    if matches!(target, FailureTarget::Follower) {
        let voters_before = match cluster.request(survivor, Request::Metrics).await? {
            Response::Metrics { voters, .. } => voters,
            response => bail!("unexpected pre-refusal metrics response: {response:?}"),
        };
        let refused = run_incompatible_preflight(&executable, &specs).await?;
        if !refused.contains("incompatible with voter schema") {
            bail!("old-schema voter was not refused: {refused}");
        }
        let voters_after = match cluster.request(survivor, Request::Metrics).await? {
            Response::Metrics { voters, .. } => voters,
            response => bail!("unexpected post-refusal metrics response: {response:?}"),
        };
        if voters_after != voters_before {
            bail!(
                "rejected preflight changed raft membership: {voters_before:?} -> {voters_after:?}"
            );
        }
    }

    // One more loss removes quorum. The remaining embedded process is alive,
    // but its Store ping must fail rather than advertise readiness.
    let second_loss = (1..=3)
        .find(|node_id| *node_id != target_id && *node_id != survivor)
        .context("choose second loss")?;
    cluster.kill(second_loss).await?;
    for request in [
        Request::Ping,
        Request::ReadWithoutQuorum,
        Request::WriteWithoutQuorum,
    ] {
        let response = cluster.request(survivor, request).await?;
        require_quorum_error(response)?;
    }
    cluster.assert_running().await?;

    cluster.kill_all().await;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: u64,
    pub raft: String,
    pub api: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeLaunch {
    pub node_id: u64,
    pub root: PathBuf,
    pub nodes: Vec<NodeSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preflight {
    pub addresses: Vec<String>,
    pub compatibility: ClusterCompatibility,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Bootstrap,
    RejectIdentityDrift,
    Open,
    IssueJoinToken {
        ttl_ms: u64,
    },
    RedeemJoin {
        request: RedeemJoinRequest,
    },
    FinalizeJoin {
        request: FinalizeJoinRequest,
    },
    MembershipStatus,
    Advertisement {
        seed_name: String,
    },
    RenameServer {
        name: String,
    },
    TombstoneOfflineFence {
        node_id: String,
    },
    HeartbeatPreservesTombstone {
        node_id: String,
    },
    RemoveVoter {
        node_id: String,
    },
    SeedOfflineRemovalWork {
        node_id: String,
        media_dir: String,
    },
    /// Request a download on this node `delay_ms` from now and answer
    /// immediately, so the package lands while a removal started right after
    /// this call is still resolving. Real operators do exactly this: the
    /// departing node keeps serving its API until the membership change
    /// commits.
    SeedOfflineWorkDuringRemoval {
        node_id: String,
        media_dir: String,
        user_id: i64,
        delay_ms: u64,
    },
    DeleteOfflinePackage {
        package_id: String,
        user_id: i64,
    },
    OfflinePackageSummary {
        package_id: String,
        user_id: i64,
    },
    ClaimNextOfflinePackage {
        node_id: String,
    },
    PublishOfflinePackage {
        package_id: String,
        node_id: String,
    },
    ResetContractState,
    RecordLocalTelemetry {
        marker: String,
    },
    CountLocalTelemetry {
        marker: String,
    },
    Exercise {
        ordinal: u64,
    },
    PostLossWrite {
        target: String,
        position_ms: i64,
    },
    VerifyProof,
    Dump,
    CatalogView,
    RebuildSearch,
    Metrics,
    ReplicationStatus,
    Ping,
    ReadWithoutQuorum,
    WriteWithoutQuorum,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ready {
        node_id: u64,
    },
    Ok,
    Flag {
        value: bool,
    },
    SeededOfflineRemovalWork {
        user_id: i64,
    },
    /// Just enough of a package to assert a §6.7 outcome. Deliberately does
    /// not carry `source_path`: a media path has no business crossing a
    /// harness wire any more than it does a log line.
    OfflinePackageSummary {
        state: String,
        node_id: String,
        error_code: Option<String>,
        reserved_bytes: i64,
        actual_bytes: Option<i64>,
    },
    ClaimedOfflinePackage {
        package_id: Option<String>,
    },
    TelemetryCount {
        count: usize,
    },
    Dump {
        digest: String,
        dump: String,
    },
    CatalogView {
        view: CatalogView,
    },
    Metrics {
        leader: Option<u64>,
        voters: Vec<u64>,
    },
    ReplicationStatus {
        status: ReplicationStatus,
    },
    IssuedJoinToken {
        token: IssuedJoinToken,
    },
    MembershipStatus {
        status: MembershipStatus,
    },
    Advertisement {
        instance_id: String,
        node_id: String,
        name: String,
        gdm: String,
        mdns_name: String,
        mdns_instance_id: String,
        mdns_node_id: String,
    },
    MembershipError {
        code: String,
        message: String,
    },
    Error {
        message: String,
    },
}

/// Prove playback telemetry stays node-local: only the voter that recorded the
/// marker can count it, and the count survives that voter reopening its store.
pub async fn prove_local_telemetry_sidecars(cluster: &mut ClusterProcesses) -> Result<()> {
    let marker = "voter-1-restart-proof".to_owned();
    cluster
        .request(
            1,
            Request::RecordLocalTelemetry {
                marker: marker.clone(),
            },
        )
        .await?
        .require_ok()?;
    for node_id in cluster.node_ids() {
        let expected = usize::from(node_id == 1);
        match cluster
            .request(
                node_id,
                Request::CountLocalTelemetry {
                    marker: marker.clone(),
                },
            )
            .await?
        {
            Response::TelemetryCount { count } if count == expected => {}
            response => {
                bail!("voter {node_id} local telemetry count was not {expected}: {response:?}")
            }
        }
    }
    cluster.request(1, Request::Open).await?.require_ok()?;
    match cluster
        .request(1, Request::CountLocalTelemetry { marker })
        .await?
    {
        Response::TelemetryCount { count: 1 } => Ok(()),
        response => bail!("voter-1 telemetry did not survive reopen: {response:?}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogView {
    pub authoritative_digest: String,
    pub full_digest: String,
    pub search: Vec<i64>,
}

impl Response {
    pub fn require_ok(self) -> Result<()> {
        match self {
            Self::Ok => Ok(()),
            Self::Error { message } => Err(anyhow!(message)),
            other => bail!("expected OK response, got {other:?}"),
        }
    }
}

/// Accept only a store failure that names the lost quorum. An ordinary success
/// or an unrelated error both mean the readiness contract was not proved.
pub fn require_quorum_error(response: Response) -> Result<()> {
    let Response::Error { message } = response else {
        bail!("ordinary store operation succeeded without quorum: {response:?}");
    };
    let normalized = message.to_ascii_lowercase();
    if !normalized.contains("quorum")
        && !normalized.contains("timed out")
        && !normalized.contains("raft leader")
    {
        bail!("store failed without quorum for an unrelated reason: {message}");
    }
    Ok(())
}

/// Require one `key=value` settings row inside a voter's local dump JSON.
pub fn require_dump_setting(dump: &str, key: &str, expected: &str) -> Result<()> {
    let dump: serde_json::Value = serde_json::from_str(dump)?;
    let settings = dump
        .get("settings")
        .and_then(serde_json::Value::as_array)
        .context("local dump has no settings rows")?;
    if !settings.iter().any(|row| {
        row.get("key").and_then(serde_json::Value::as_str) == Some(key)
            && row.get("value").and_then(serde_json::Value::as_str) == Some(expected)
    }) {
        bail!("local dump is missing expected setting {key}={expected}");
    }
    Ok(())
}

pub struct NodeProcess {
    id: u64,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl NodeProcess {
    pub fn spawn(executable: &Path, launch: &NodeLaunch) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .arg("node")
            .arg(serde_json::to_string(launch)?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().context("spawn cluster voter")?;
        let input = child.stdin.take().context("voter stdin")?;
        let output = BufReader::new(child.stdout.take().context("voter stdout")?);
        Ok(Self {
            id: launch.node_id,
            child,
            input,
            output,
        })
    }

    pub async fn wait_ready(&mut self) -> Result<()> {
        match self.read_response(START_TIMEOUT).await? {
            Response::Ready { node_id } if node_id == self.id => Ok(()),
            response => bail!("voter {} failed startup: {response:?}", self.id),
        }
    }

    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        self.read_response(REQUEST_TIMEOUT).await
    }

    async fn read_response(&mut self, timeout: Duration) -> Result<Response> {
        let mut line = String::new();
        let bytes = tokio::time::timeout(timeout, self.output.read_line(&mut line))
            .await
            .context("voter response timed out")??;
        if bytes == 0 {
            let status = self.child.try_wait()?;
            bail!("voter {} closed its protocol stream ({status:?})", self.id);
        }
        serde_json::from_str(line.trim()).context("decode voter response")
    }

    /// Close the request stream and wait for the voter to exit on its own.
    ///
    /// `kill` is what proves process loss; this is its orderly counterpart, and
    /// it is how a caller stops a voter it is finished with rather than one it
    /// is trying to destroy. It also makes an instrumented voter observable: a
    /// SIGKILLed process never runs its atexit handlers, so a voter that is
    /// only ever killed writes no coverage profile and reads as never executed.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            id,
            mut child,
            input,
            output,
        } = self;
        // Closing stdin ends the voter's request loop, which returns from
        // `node` and exits the process normally.
        drop(input);
        drop(output);
        let status = tokio::time::timeout(START_TIMEOUT, child.wait())
            .await
            .with_context(|| format!("voter {id} did not exit after its stream closed"))??;
        if !status.success() {
            bail!("voter {id} exited with {status} after an orderly shutdown");
        }
        Ok(())
    }

    pub async fn kill(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill().await?;
            let status = self.child.wait().await?;
            if status.success() {
                bail!(
                    "voter {} exited successfully after an intentional kill",
                    self.id
                );
            }
            #[cfg(unix)]
            if status.signal() != Some(9) {
                bail!(
                    "voter {} exited with unexpected status after kill: {status}",
                    self.id
                );
            }
        }
        Ok(())
    }
}

pub struct ClusterProcesses {
    nodes: Vec<Option<NodeProcess>>,
    root: PathBuf,
    convergence_timeout: Duration,
}

impl ClusterProcesses {
    /// Start one voter process per spec from `executable` and wait for each to
    /// announce readiness. The executable is explicit rather than
    /// `current_exe()` so a test binary can start real harness voters.
    pub async fn start(
        executable: &Path,
        root: &Path,
        reservation: PortReservation,
    ) -> Result<Self> {
        let (_listeners, specs) = reservation.into_inner();
        // Listeners are dropped here: the child process must bind the same
        // ports, so we cannot hold them across the spawn. The window between
        // releasing the port and the child binding it is the residual race
        // that [`install_bind_failure_guard`] + retry handle.
        drop(_listeners);
        let mut nodes = Vec::with_capacity(specs.len());
        for node_id in 1..=specs.len() as u64 {
            let launch = NodeLaunch {
                node_id,
                root: root.to_path_buf(),
                nodes: specs.clone(),
            };
            nodes.push(Some(NodeProcess::spawn(executable, &launch)?));
        }
        let mut cluster = Self {
            nodes,
            root: root.to_path_buf(),
            convergence_timeout: CONVERGENCE_TIMEOUT,
        };
        for node_id in 1..=specs.len() as u64 {
            cluster.node_mut(node_id)?.wait_ready().await?;
        }
        Ok(cluster)
    }

    /// Shorten how long the `wait_*` and `leader` helpers keep retrying before
    /// they give up.
    ///
    /// The controller keeps the default: [`CONVERGENCE_TIMEOUT`] is sized for a
    /// three-voter raft election on a loaded machine. Tests that assert the
    /// give-up path itself need a state that never converges, and waiting the
    /// full production timeout for each one would dominate the suite.
    #[must_use]
    pub fn with_convergence_timeout(mut self, timeout: Duration) -> Self {
        self.convergence_timeout = timeout;
        self
    }

    fn node_mut(&mut self, node_id: u64) -> Result<&mut NodeProcess> {
        self.nodes
            .get_mut((node_id - 1) as usize)
            .and_then(Option::as_mut)
            .with_context(|| format!("voter {node_id} is not running"))
    }

    /// The voters still running, in ascending id order.
    pub fn node_ids(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.as_ref().map(|_| index as u64 + 1))
            .collect()
    }

    pub async fn request(&mut self, node_id: u64, request: Request) -> Result<Response> {
        self.node_mut(node_id)?.request(&request).await
    }

    /// Add one real voter process to an already-running cluster.
    pub async fn spawn_node(&mut self, executable: &Path, launch: NodeLaunch) -> Result<()> {
        if launch.root != self.root {
            bail!("dynamic voter root did not match the running cluster");
        }
        let index = usize::try_from(launch.node_id.saturating_sub(1))?;
        if self.nodes.len() <= index {
            self.nodes.resize_with(index + 1, || None);
        }
        if self.nodes[index].is_some() {
            bail!("voter {} is already running", launch.node_id);
        }
        let mut process = NodeProcess::spawn(executable, &launch)?;
        process.wait_ready().await?;
        self.nodes[index] = Some(process);
        Ok(())
    }

    pub async fn kill(&mut self, node_id: u64) -> Result<()> {
        let slot = self
            .nodes
            .get_mut((node_id - 1) as usize)
            .with_context(|| format!("unknown voter {node_id}"))?;
        if let Some(mut node) = slot.take() {
            node.kill().await?;
        }
        Ok(())
    }

    /// Stop every remaining voter in an orderly way. See
    /// [`NodeProcess::shutdown`]; `kill_all` is the destructive counterpart.
    pub async fn shutdown_all(&mut self) -> Result<()> {
        for slot in &mut self.nodes {
            if let Some(node) = slot.take() {
                node.shutdown().await?;
            }
        }
        Ok(())
    }

    pub async fn kill_all(&mut self) {
        for node in &mut self.nodes {
            if let Some(node) = node.as_mut() {
                let _ = node.kill().await;
            }
            *node = None;
        }
    }

    pub async fn assert_running(&mut self) -> Result<()> {
        for node in self.nodes.iter_mut().flatten() {
            if let Some(status) = node.child.try_wait()? {
                bail!("voter {} exited unexpectedly with {status}", node.id);
            }
        }
        Ok(())
    }

    pub async fn leader(&mut self) -> Result<u64> {
        let deadline = Instant::now() + self.convergence_timeout;
        loop {
            for node_id in 1..=self.nodes.len() as u64 {
                if self.nodes[(node_id - 1) as usize].is_none() {
                    continue;
                }
                if let Ok(Response::Metrics {
                    leader: Some(leader),
                    ..
                }) = self.request(node_id, Request::Metrics).await
                {
                    if self.nodes.get((leader - 1) as usize).is_some_and(Option::is_some)
                        && self
                            .request(leader, Request::Metrics)
                            .await
                            .is_ok_and(|response| {
                                matches!(response, Response::Metrics { leader: Some(id), .. } if id == leader)
                            })
                    {
                        return Ok(leader);
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!("cluster did not report a leader");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait until voter 1 reports exactly `expected` as the raft membership.
    pub async fn wait_for_voters(&mut self, expected: &[u64]) -> Result<()> {
        let deadline = Instant::now() + self.convergence_timeout;
        loop {
            if let Ok(Response::Metrics { voters, .. }) = self.request(1, Request::Metrics).await {
                if voters == expected {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("cluster did not converge to voters {expected:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn wait_for_ready(&mut self, node_id: u64) -> Result<()> {
        let deadline = Instant::now() + self.convergence_timeout;
        loop {
            if self
                .request(node_id, Request::Ping)
                .await
                .is_ok_and(|response| matches!(response, Response::Ok))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("surviving voter {node_id} did not regain quorum readiness");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait for the exact production status projection on one voter.
    pub async fn wait_for_replication_health(
        &mut self,
        node_id: u64,
        expected: ReplicationHealth,
    ) -> Result<ReplicationStatus> {
        let deadline = Instant::now() + self.convergence_timeout;
        loop {
            if let Ok(Response::ReplicationStatus { status }) =
                self.request(node_id, Request::ReplicationStatus).await
            {
                if status.health == expected {
                    return Ok(status);
                }
            }
            if Instant::now() >= deadline {
                bail!("voter {node_id} did not report replication health {expected:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn wait_for_equal_dumps(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.convergence_timeout;
        loop {
            let mut dumps = Vec::new();
            for node_id in 1..=self.nodes.len() as u64 {
                if self.nodes[(node_id - 1) as usize].is_none() {
                    continue;
                }
                match self.request(node_id, Request::Dump).await? {
                    Response::Dump { digest, dump } => {
                        let computed = hex::encode(Sha256::digest(dump.as_bytes()));
                        if digest != computed {
                            bail!("local dump digest is not anchored to the returned rows");
                        }
                        validate_known_dump(&serde_json::from_str(&dump)?)?;
                        dumps.push((digest, dump));
                    }
                    Response::Error { message } => return Err(anyhow!(message)),
                    response => bail!("unexpected dump response: {response:?}"),
                }
            }
            if dumps.windows(2).all(|pair| pair[0] == pair[1]) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("local auth table dumps did not converge byte-for-byte");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn wait_for_equal_catalog_views(&mut self) -> Result<CatalogView> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut views = Vec::new();
            for node_id in 1..=self.nodes.len() as u64 {
                if self.nodes[(node_id - 1) as usize].is_none() {
                    continue;
                }
                match self.request(node_id, Request::CatalogView).await? {
                    Response::CatalogView { view } => views.push(view),
                    Response::Error { message } => return Err(anyhow!(message)),
                    response => bail!("unexpected catalog response: {response:?}"),
                }
            }
            if views.windows(2).all(|pair| pair[0] == pair[1]) {
                return views
                    .into_iter()
                    .next()
                    .context("catalog view set was empty");
            }
            if Instant::now() >= deadline {
                bail!("browse/search views did not converge on all three voters");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Delete `voter`'s derived FTS rows behind hiqlite's back and prove the search
/// index is local: replicated truth is unchanged, the full digest moves, search
/// goes empty, and a rebuild restores the baseline view exactly.
///
/// The voter is a parameter because the search index is node-local on every
/// voter, so the proof is the same wherever it runs; the three-voter controller
/// passes 2 exactly as it always did.
pub async fn prove_local_fts_rebuild(
    cluster: &mut ClusterProcesses,
    baseline: &CatalogView,
    voter: u64,
) -> Result<()> {
    let path = cluster
        .root
        .join(format!("node-{voter}"))
        .join("state_machine")
        .join("db")
        .join("auth.db");
    let connection = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open voter-{voter} local database at {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("set voter validation busy timeout")?;
    connection
        .execute("DELETE FROM items_fts", [])
        .context("delete voter derived FTS rows")?;
    let empty = match cluster.request(voter, Request::CatalogView).await? {
        Response::CatalogView { view } => view,
        response => bail!("unexpected post-delete catalog response: {response:?}"),
    };
    if empty.authoritative_digest != baseline.authoritative_digest
        || empty.full_digest == baseline.full_digest
        || !empty.search.is_empty()
    {
        bail!("deleting voter-{voter} FTS changed truth, escaped the digest, or left search rows");
    }
    drop(connection);
    cluster
        .request(1, Request::RebuildSearch)
        .await?
        .require_ok()?;
    let rebuilt = cluster.wait_for_equal_catalog_views().await?;
    if &rebuilt != baseline {
        bail!("voter-{voter} FTS rebuild did not restore the baseline view");
    }
    Ok(())
}

/// The path of the running harness executable, used to start voter children.
pub fn harness_executable() -> Result<PathBuf> {
    std::env::current_exe().context("cluster-check executable")
}

/// Retry a cluster start whose only failure was a port taken between
/// allocation and bind.
///
/// `attempt` is handed the attempt number so it can allocate fresh ports and a
/// fresh data root each time. Only [`is_port_collision`] errors are retried;
/// every other failure is a verdict and is returned immediately, which is the
/// whole point of classifying the collision rather than matching a message
/// here.
pub async fn with_port_retry<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;
    for number in 1..=PORT_RETRY_ATTEMPTS {
        match attempt(number).await {
            Ok(value) => return Ok(value),
            Err(error) if is_port_collision(&error) => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.with_context(|| {
        format!("cluster ports stayed occupied across {PORT_RETRY_ATTEMPTS} allocations")
    })?)
}

/// Allocate ports and start `voters` voter processes, retrying the whole
/// allocation when a port was taken between binding it and using it.
pub async fn start_cluster_with_port_retry(
    executable: &Path,
    root: &Path,
    voters: u64,
) -> Result<(ClusterProcesses, Vec<NodeSpec>)> {
    start_cluster_with_port_retry_using(
        executable,
        root,
        voters,
        |_| allocate_nodes(voters),
        |_, _| Ok(()),
    )
    .await
}

/// Start a cluster with injectable allocation and pre-start seams.
///
/// This is public only so the integration harness can deterministically return
/// a classified collision before the first start and prove the production
/// retry wrapper performs a fresh allocation. The reservation remains held
/// during that injection, so the regression does not recreate the
/// release-to-bind race this wrapper exists to handle. Production callers
/// should use [`start_cluster_with_port_retry`].
#[doc(hidden)]
pub async fn start_cluster_with_port_retry_using<A, B>(
    executable: &Path,
    root: &Path,
    voters: u64,
    mut allocate: A,
    mut before_start: B,
) -> Result<(ClusterProcesses, Vec<NodeSpec>)>
where
    A: FnMut(u32) -> Result<PortReservation>,
    B: FnMut(u32, &PortReservation) -> Result<()>,
{
    with_port_retry(|attempt| {
        let reservation = allocate(attempt);
        let before_start = match reservation.as_ref() {
            Ok(reservation) => before_start(attempt, reservation),
            Err(_) => Ok(()),
        };
        async move {
            let reservation = reservation?;
            before_start?;
            if reservation.specs.len() != voters as usize {
                bail!(
                    "cluster allocator returned {} voters, expected {voters}",
                    reservation.specs.len()
                );
            }
            let specs = reservation.specs.clone();
            let attempt_root = root.join(format!("attempt-{attempt}"));
            let cluster = ClusterProcesses::start(executable, &attempt_root, reservation).await?;
            Ok((cluster, specs))
        }
    })
    .await
}

/// Assert a voter's local dump carries every row the harness wrote: the
/// cluster identity, one acknowledged setting per voter, and exactly the three
/// surviving users, tokens, and API keys with their expected field values.
pub fn validate_known_dump(dump: &serde_json::Value) -> Result<()> {
    let rows = |name: &str| -> Result<&Vec<serde_json::Value>> {
        dump.get(name)
            .and_then(serde_json::Value::as_array)
            .with_context(|| format!("local dump has no {name} rows"))
    };

    let settings = rows("settings")?;
    let expected_settings = [
        ("instance.id", INSTANCE_ID),
        ("proof.node.1", "acknowledged"),
        ("proof.node.2", "acknowledged"),
        ("proof.node.3", "acknowledged"),
    ];
    for (key, value) in expected_settings {
        if !settings.iter().any(|row| {
            row.get("key").and_then(serde_json::Value::as_str) == Some(key)
                && row.get("value").and_then(serde_json::Value::as_str) == Some(value)
        }) {
            bail!("local dump is missing expected setting {key}={value}");
        }
    }

    let users = rows("users")?;
    if users.len() != 3 {
        bail!(
            "local dump expected 3 surviving users, found {}",
            users.len()
        );
    }
    for ordinal in 1..=3 {
        let username = format!("survivor-{ordinal}");
        let password = format!("hash-v2-{ordinal}");
        if !users.iter().any(|row| {
            row.get("username").and_then(serde_json::Value::as_str) == Some(username.as_str())
                && row.get("password_hash").and_then(serde_json::Value::as_str)
                    == Some(password.as_str())
                && row.get("is_admin").and_then(serde_json::Value::as_i64) == Some(1)
        }) {
            bail!("local dump has incorrect user proof for {username}");
        }
    }

    let tokens = rows("tokens")?;
    if tokens.len() != 3 {
        bail!(
            "local dump expected 3 surviving tokens, found {}",
            tokens.len()
        );
    }
    for ordinal in 1..=3 {
        let token = format!("survive-token-{ordinal}");
        if !tokens.iter().any(|row| {
            row.get("token_hash").and_then(serde_json::Value::as_str) == Some(token.as_str())
                && row.get("device").and_then(serde_json::Value::as_str) == Some("cluster-check")
        }) {
            bail!("local dump has incorrect token proof for {token}");
        }
    }

    let keys = rows("api_keys")?;
    if keys.len() != 3 {
        bail!(
            "local dump expected 3 surviving API keys, found {}",
            keys.len()
        );
    }
    for ordinal in 1..=3 {
        let key_hash = format!("survive-key-{ordinal}");
        if !keys.iter().any(|row| {
            row.get("key_hash").and_then(serde_json::Value::as_str) == Some(key_hash.as_str())
                && row.get("scopes").and_then(serde_json::Value::as_str)
                    == Some(r#"["scan:trigger"]"#)
                && row.get("disabled").and_then(serde_json::Value::as_i64) == Some(0)
        }) {
            bail!("local dump has incorrect API-key proof for {key_hash}");
        }
    }
    Ok(())
}

/// Run one embedded voter: start hiqlite, announce readiness, then serve the
/// line-delimited request protocol until stdin closes.
pub async fn node(launch: NodeLaunch) -> Result<()> {
    install_crypto_provider();
    // Arm this before hiqlite can spawn a listener, so a bind that lost its
    // port is reported as a port collision rather than surviving as a voter
    // that answers every later request with a durable-state symptom.
    let listeners = voter_listen_addrs(&launch)?;
    install_bind_failure_guard(BindFailureChannel::Protocol, listeners.clone());
    let _ = ServerTlsConfig::server_config_self_signed(LISTEN_ADDR).await;
    let client = match hiqlite::start_node(node_config(&launch)?).await {
        Ok(client) => client,
        Err(error) => {
            write_response(&Response::Error {
                message: format!("start hiqlite voter: {error}"),
            })
            .await?;
            return Err(error).context("start hiqlite voter");
        }
    };
    tokio::time::timeout(START_TIMEOUT, client.wait_until_healthy_db())
        .await
        .context("voter health timed out")?;
    if let Err(error) = prove_listeners_bound(&listeners).await {
        write_response(&Response::Error {
            message: format!("{error:#}"),
        })
        .await?;
        return Err(error);
    }
    let replication = ReplicationMonitor::replicated(client.clone());

    write_response(&Response::Ready {
        node_id: launch.node_id,
    })
    .await?;

    let telemetry_path = launch
        .root
        .join(format!("node-{}", launch.node_id))
        .join("telemetry.db");
    let mut store: Option<Arc<HiqliteAuthStore>> = None;
    let mut membership: Option<MembershipManager> = None;
    let stdin = tokio::io::stdin();
    let mut input = BufReader::new(stdin).lines();
    while let Some(line) = input.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                handle_request(
                    request,
                    &client,
                    &replication,
                    &launch,
                    &telemetry_path,
                    &mut store,
                    &mut membership,
                )
                .await
            }
            Err(error) => Err(error.into()),
        };
        match response {
            Ok(response) => write_response(&response).await?,
            Err(error) => {
                write_response(&Response::Error {
                    message: format!("{error:#}"),
                })
                .await?
            }
        }
    }
    Ok(())
}

async fn handle_request(
    request: Request,
    client: &Client,
    replication: &ReplicationMonitor,
    launch: &NodeLaunch,
    telemetry_path: &Path,
    store: &mut Option<Arc<HiqliteAuthStore>>,
    membership: &mut Option<MembershipManager>,
) -> Result<Response> {
    match request {
        Request::Bootstrap => {
            let opened = Arc::new(
                HiqliteAuthStore::bootstrap(client.clone(), INSTANCE_ID, telemetry_path).await?,
            );
            let opened_membership = membership_manager(client, opened.clone(), launch).await?;
            tokio::spawn(opened_membership.clone().offline_source_probe_loop());
            *membership = Some(opened_membership);
            *store = Some(opened);
            Ok(Response::Ok)
        }
        Request::RejectIdentityDrift => {
            match HiqliteAuthStore::bootstrap(client.clone(), "wrong-instance-id", telemetry_path)
                .await
            {
                Err(error) if error.to_string().contains("refusing bootstrap") => Ok(Response::Ok),
                Err(error) => bail!("identity drift failed for the wrong reason: {error}"),
                Ok(_) => bail!("bootstrap overwrote the immutable cluster identity"),
            }
        }
        Request::Open => {
            let opened = Arc::new(HiqliteAuthStore::open(client.clone(), telemetry_path).await?);
            let opened_membership = membership_manager(client, opened.clone(), launch).await?;
            tokio::spawn(opened_membership.clone().offline_source_probe_loop());
            *membership = Some(opened_membership);
            *store = Some(opened);
            Ok(Response::Ok)
        }
        Request::IssueJoinToken { ttl_ms } => membership_ref(membership)?
            .issue_token(Duration::from_millis(ttl_ms))
            .await
            .map(|token| Response::IssuedJoinToken { token })
            .or_else(|error| Ok(membership_error_response(error))),
        Request::RedeemJoin { request } => membership_ref(membership)?
            .redeem(&request)
            .await
            .map(|()| Response::Ok)
            .or_else(|error| Ok(membership_error_response(error))),
        Request::FinalizeJoin { request } => membership_ref(membership)?
            .finalize(&request)
            .await
            .map(|()| Response::Ok)
            .or_else(|error| Ok(membership_error_response(error))),
        Request::MembershipStatus => membership_ref(membership)?
            .status()
            .await
            .map(|status| Response::MembershipStatus { status })
            .or_else(|error| Ok(membership_error_response(error))),
        Request::Advertisement { seed_name } => {
            let name = store_ref(store)?
                .get_or_init_setting(plurx_core::store::keys::SERVER_NAME, &seed_name)
                .await?;
            let node_id = format!("node-{}", launch.node_id);
            let advertisement = plurx_compat_plex::gdm::Advertisement {
                instance_id: INSTANCE_ID,
                name: &name,
                node_id: Some(&node_id),
            };
            let gdm = String::from_utf8(plurx_compat_plex::gdm::response_for(
                &advertisement,
                "0.2.7",
                32400,
            ))?;
            Ok(Response::Advertisement {
                instance_id: INSTANCE_ID.to_owned(),
                node_id: node_id.clone(),
                name: name.clone(),
                gdm,
                mdns_name: name,
                mdns_instance_id: INSTANCE_ID.to_owned(),
                mdns_node_id: node_id,
            })
        }
        Request::RenameServer { name } => {
            store_ref(store)?
                .put_setting(plurx_core::store::keys::SERVER_NAME, &name)
                .await?;
            Ok(Response::Ok)
        }

        Request::TombstoneOfflineFence { ref node_id } => {
            let store = store_ref(store)?;
            let now = unix_now()?;
            let package = NewOfflinePackage {
                id: format!("tombstone-fence-{node_id}"),
                request_id: format!("fence-{node_id}-{now}"),
                user_id: 1,
                file_id: 0,
                node_id: node_id.clone(),
                source_path: "/dev/null/tombstone-proof".to_owned(),
                source_size: 10,
                source_mtime: 1_700_000_000,
                effective_rate_control: "vbr".to_owned(),
                target_height: 720,
                output_width: Some(1280),
                output_height: Some(720),
                audio_index: None,
                audio_offset_ms: 0,
                subtitle_index: None,
                subtitle_language: None,
                subtitle_mode: "none".to_owned(),
                estimated_bytes: 700,
                reserved_bytes: 900,
                expires_at: now.saturating_add(3_600),
            };
            match store
                .create_offline_package(&package, 10, 100_000, 1_000_000)
                .await
            {
                Ok(OfflineCreateOutcome::NodeIsTombstone) => Ok(Response::Ok),
                Ok(other) => bail!(
                    "removed node was not refused as tombstone: expected \
                     NodeIsTombstone, got {other:?}"
                ),
                Err(error) => bail!("tombstone offline fence failed: {error:#}"),
            }

        }
        Request::HeartbeatPreservesTombstone { node_id } => {
            membership_ref(membership)?.heartbeat().await?;
            let rows = client
                .query_consistent_map::<MembershipTombstoneRow, _>(
                    "SELECT removed_at FROM cluster_nodes WHERE node_id = $1",
                    hiqlite::macros::params!(node_id),
                )
                .await?;
            if rows.first().and_then(|row| row.removed_at).is_none() {
                bail!("removed node heartbeat cleared its durable tombstone");
            }
            Ok(Response::Ok)
        }
        Request::RemoveVoter { node_id } => membership_ref(membership)?
            .remove_voter(&node_id)
            .await
            .map(|_| Response::Ok)
            .or_else(|error| Ok(membership_error_response(error))),
        Request::SeedOfflineRemovalWork { node_id, media_dir } => {
            let user_id =
                seed_offline_removal_work(store_ref(store)?, &node_id, Path::new(&media_dir))
                    .await?;
            Ok(Response::SeededOfflineRemovalWork { user_id })
        }
        Request::SeedOfflineWorkDuringRemoval {
            node_id,
            media_dir,
            user_id,
            delay_ms,
        } => {
            // Answered before the package exists on purpose. The caller starts
            // the removal next, so the request lands mid-resolution the way a
            // real one would, rather than at a moment the harness chose.
            let store = store.clone().context("node store is not open")?;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if let Err(error) = seed_offline_work_during_removal(
                    &store,
                    &node_id,
                    Path::new(&media_dir),
                    user_id,
                )
                .await
                {
                    eprintln!("cluster-check: mid-removal offline seed failed: {error:#}");
                }
            });
            Ok(Response::Ok)
        }
        Request::DeleteOfflinePackage {
            package_id,
            user_id,
        } => Ok(Response::Flag {
            value: store_ref(store)?
                .delete_offline_package(&package_id, user_id)
                .await?,
        }),
        Request::OfflinePackageSummary {
            package_id,
            user_id,
        } => {
            let package = store_ref(store)?
                .offline_package_for_user(&package_id, user_id)
                .await?
                .with_context(|| format!("offline package {package_id} disappeared"))?;
            Ok(Response::OfflinePackageSummary {
                state: package.state,
                node_id: package.node_id,
                error_code: package.error_code,
                reserved_bytes: package.reserved_bytes,
                actual_bytes: package.actual_bytes,
            })
        }
        Request::ClaimNextOfflinePackage { node_id } => Ok(Response::ClaimedOfflinePackage {
            package_id: store_ref(store)?
                .claim_next_offline_package(&node_id)
                .await?
                .map(|package| package.id),
        }),
        Request::PublishOfflinePackage {
            package_id,
            node_id,
        } => Ok(Response::Flag {
            value: store_ref(store)?
                .mark_offline_package_ready(&package_id, &node_id, "rehomed-recipe", 900, 90_000)
                .await?,
        }),
        Request::ResetContractState => {
            store_ref(store)?.validation_reset_contract_state().await?;
            Ok(Response::Ok)
        }
        Request::RecordLocalTelemetry { marker } => {
            store_ref(store)?
                .record_playback_event(&PlaybackEvent {
                    at_unix_ms: 1_700_000_000_000,
                    event: "cluster_sidecar_marker".to_owned(),
                    detail: Some(marker),
                    ..PlaybackEvent::default()
                })
                .await?;
            Ok(Response::Ok)
        }
        Request::CountLocalTelemetry { marker } => {
            let count = store_ref(store)?
                .playback_events(&PlaybackEventQuery {
                    event: Some("cluster_sidecar_marker".to_owned()),
                    limit: 100,
                    ..PlaybackEventQuery::default()
                })
                .await?
                .into_iter()
                .filter(|event| event.detail.as_deref() == Some(marker.as_str()))
                .count();
            Ok(Response::TelemetryCount { count })
        }
        Request::Exercise { ordinal } => {
            exercise(store_ref(store)?, ordinal).await?;
            Ok(Response::Ok)
        }
        Request::PostLossWrite {
            target,
            position_ms,
        } => {
            let store = store_ref(store)?;
            store
                .put_setting(&format!("post_loss.{target}"), "acknowledged")
                .await?;
            if store
                .get_setting(&format!("post_loss.{target}"))
                .await?
                .as_deref()
                != Some("acknowledged")
            {
                bail!("post-loss acknowledged write was not readable");
            }
            // The lag signal must be driven by the user-visible state this
            // feature describes, not by an unrelated synthetic Raft entry.
            let user = store
                .get_user_by_username("survivor-1")
                .await?
                .context("post-loss watch user is missing")?;
            let movie = store
                .search_items("Replicated Catalog Proof 1", 10)
                .await?
                .into_iter()
                .find(|item| item.item.title == "Replicated Catalog Proof 1")
                .context("post-loss watch item is missing")?;
            let watch = store
                .put_progress(user.id, movie.item.id, position_ms, Some(120_000))
                .await?;
            if watch.position_ms != position_ms {
                bail!("post-loss watch progress was not acknowledged");
            }
            Ok(Response::Ok)
        }
        Request::VerifyProof => {
            verify_proof(store_ref(store)?).await?;
            Ok(Response::Ok)
        }
        Request::Dump => {
            let store = store_ref(store)?;
            Ok(Response::Dump {
                digest: store.local_dump_digest().await?,
                dump: store.validation_local_dump().await?,
            })
        }
        Request::CatalogView => Ok(Response::CatalogView {
            view: catalog_view(store_ref(store)?).await?,
        }),
        Request::RebuildSearch => {
            store_ref(store)?.rebuild_search_index().await?;
            Ok(Response::Ok)
        }
        Request::Metrics => {
            let metrics = client.metrics_db().await?;
            let mut voters = metrics.membership_config.voter_ids().collect::<Vec<_>>();
            voters.sort_unstable();
            Ok(Response::Metrics {
                leader: metrics.current_leader,
                voters,
            })
        }
        Request::ReplicationStatus => Ok(Response::ReplicationStatus {
            status: replication.status().await,
        }),
        Request::Ping => {
            store_ref(store)?.ping().await?;
            Ok(Response::Ok)
        }
        Request::ReadWithoutQuorum => {
            store_ref(store)?.get_setting("instance.id").await?;
            Ok(Response::Ok)
        }
        Request::WriteWithoutQuorum => {
            store_ref(store)?
                .put_setting("no-quorum.write", "must-not-ack")
                .await?;
            Ok(Response::Ok)
        }
    }
}

struct MembershipTombstoneRow {
    removed_at: Option<i64>,
}

impl From<&mut Row<'_>> for MembershipTombstoneRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            removed_at: row.get("removed_at"),
        }
    }
}

fn store_ref(store: &Option<Arc<HiqliteAuthStore>>) -> Result<&HiqliteAuthStore> {
    store.as_deref().context("auth store has not been opened")
}

fn membership_ref(membership: &Option<MembershipManager>) -> Result<&MembershipManager> {
    membership
        .as_ref()
        .context("membership manager has not been opened")
}

fn membership_error_response(error: plurx_core::cluster::membership::MembershipError) -> Response {
    Response::MembershipError {
        code: error.code().to_owned(),
        message: error.to_string(),
    }
}

async fn membership_manager(
    client: &Client,
    store: Arc<HiqliteAuthStore>,
    launch: &NodeLaunch,
) -> Result<MembershipManager> {
    let local = launch
        .nodes
        .iter()
        .find(|node| node.id == launch.node_id)
        .with_context(|| format!("voter {} has no local node spec", launch.node_id))?;
    MembershipManager::replicated(
        client.clone(),
        store,
        ClusterIdentity {
            cluster_id: INSTANCE_ID.to_owned(),
            node_id: format!("node-{}", launch.node_id),
            raft_id: launch.node_id,
        },
        ClusterPeer {
            raft_id: local.id,
            raft_address: local.raft.clone(),
            api_address: local.api.clone(),
        },
        "https://127.0.0.1:1".to_owned(),
        JoinSecrets {
            raft: RAFT_SECRET.to_owned(),
            api: API_SECRET.to_owned(),
            credential_key: "00".repeat(32),
        },
        ActivationMarker {
            marker_version: 1,
            cluster_id: INSTANCE_ID.to_owned(),
            source_backup_sha256: "00".repeat(32),
            source_schema_version: AUTH_SCHEMA_VERSION,
            replicated_schema_version: AUTH_SCHEMA_VERSION,
            imported_rows: 0,
            table_hashes: Vec::new(),
        },
    )
    .await
    .map_err(Into::into)
}

/// Package ids the removal scenario asserts on. Each names the §6.7 outcome
/// it is there to pin down.
const MOVABLE_PACKAGE: &str = "offline-movable";
const STRANDED_PACKAGE: &str = "offline-stranded";
const READY_PACKAGE: &str = "offline-ready";
const TRANSFER_PACKAGE: &str = "offline-transfer";
/// Requested after the removal has already drawn its plan.
const LATE_PACKAGE: &str = "offline-late";
/// Long enough to land after the removal snapshots the work it plans to
/// resolve, and well inside the bounded probe wait that follows it.
const LATE_PACKAGE_DELAY_MS: u64 = 250;

/// Seed the four packages a node removal has to deal with, backed by real
/// files on a real filesystem.
///
/// The sources are genuinely written to disk (and genuinely absent, for the
/// stranded one) because the whole contract under test is that a survivor
/// proves it can *read* the bytes. A fixture that only wrote rows would prove
/// the requeue mechanics and skip the part §6.7 actually cares about.
async fn seed_offline_removal_work(
    store: &HiqliteAuthStore,
    node_id: &str,
    media_dir: &Path,
) -> Result<i64> {
    std::fs::create_dir_all(media_dir).context("offline removal fixture media directory")?;
    let user = store
        .create_user(&format!("membership-offline-{node_id}"), "hash", false)
        .await?;
    let library = store
        .create_library(&NewLibrary {
            name: format!("Membership Offline {node_id}"),
            kind: LibraryKind::Movies,
            paths: vec![media_dir.to_path_buf()],
            anime: false,
        })
        .await?;
    let item = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("Membership Offline {node_id}"),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await?;

    // Created and transitioned one at a time so exactly one package is
    // claimable at each step: the fairness ordering in the queue is not what
    // this scenario is proving.
    for (package_id, present, target_state) in [
        (MOVABLE_PACKAGE, true, "preparing"),
        (READY_PACKAGE, true, "ready"),
        (TRANSFER_PACKAGE, true, "ready"),
        (STRANDED_PACKAGE, false, "queued"),
    ] {
        let source_path = media_dir.join(format!("{package_id}.mkv"));
        let (source_size, source_mtime) = if present {
            std::fs::write(&source_path, vec![0_u8; 4_096])
                .with_context(|| format!("write offline fixture source for {package_id}"))?;
            let metadata = std::fs::metadata(&source_path)?;
            let mtime = i64::try_from(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_secs())?;
            (i64::try_from(metadata.len())?, mtime)
        } else {
            // Nothing is written here on purpose: no node can prove a source
            // that does not exist, which is exactly the case that must resolve
            // to `node_removed` instead of a hopeful reassignment.
            (4_096, 1_700_000_000)
        };
        let source = source_path.to_string_lossy().to_string();
        let file = store
            .upsert_file(
                item,
                &source,
                source_size,
                source_mtime,
                &ProbeResult::default(),
            )
            .await?;
        let package = NewOfflinePackage {
            id: format!("{package_id}-{node_id}"),
            request_id: format!("{package_id}-request-{node_id}"),
            user_id: user.id,
            file_id: file,
            node_id: node_id.to_owned(),
            source_path: source,
            source_size,
            source_mtime,
            effective_rate_control: "vbr".to_owned(),
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: None,
            audio_offset_ms: 0,
            subtitle_index: None,
            subtitle_language: None,
            subtitle_mode: "none".to_owned(),
            estimated_bytes: 700,
            reserved_bytes: 900,
            expires_at: unix_now()?.saturating_add(3_600),
        };
        if !matches!(
            store
                .create_offline_package(&package, 10, 100_000, 1_000_000)
                .await?,
            OfflineCreateOutcome::Created(_)
        ) {
            bail!("membership offline fixture {package_id} was not admitted");
        }
        if target_state == "queued" {
            continue;
        }
        if store
            .claim_next_offline_package(node_id)
            .await?
            .is_none_or(|claimed| claimed.id != package.id)
        {
            bail!("membership offline fixture {package_id} did not claim in order");
        }
        if target_state == "ready"
            && !store
                .mark_offline_package_ready(&package.id, node_id, package_id, 900, 90_000)
                .await?
        {
            bail!("membership offline fixture {package_id} did not publish");
        }
    }

    // One downloader is mid-transfer. A lease touched this recently is what
    // the activity surface calls "sending", and removal refuses rather than
    // cutting it off.
    let expires_at = unix_now()?.saturating_add(3_600);
    if !matches!(
        store
            .put_offline_lease(
                &format!("{TRANSFER_PACKAGE}-{node_id}"),
                user.id,
                &format!("{:x}", Sha256::digest(b"membership-offline-transfer")),
                expires_at,
            )
            .await?,
        OfflineLeaseOutcome::Created(_) | OfflineLeaseOutcome::Renewed(_)
    ) {
        bail!("membership offline transfer lease was not admitted");
    }
    Ok(user.id)
}

/// Request one more download on the departing node, after that node's removal
/// has already snapshotted the work it planned to resolve.
///
/// The source is a real, readable file so a survivor can genuinely prove it.
/// What this fixture is about is whether the removal looks *again* before it
/// commits — the probe protocol itself is already covered by the four packages
/// seeded up front.
async fn seed_offline_work_during_removal(
    store: &HiqliteAuthStore,
    node_id: &str,
    media_dir: &Path,
    user_id: i64,
) -> Result<()> {
    let dir = media_dir.join("late");
    std::fs::create_dir_all(&dir).context("mid-removal offline fixture directory")?;
    let library = store
        .create_library(&NewLibrary {
            name: format!("Membership Offline Late {node_id}"),
            kind: LibraryKind::Movies,
            paths: vec![dir.clone()],
            anime: false,
        })
        .await?;
    let item = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("Membership Offline Late {node_id}"),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await?;
    let source_path = dir.join(format!("{LATE_PACKAGE}.mkv"));
    std::fs::write(&source_path, vec![0_u8; 4_096])
        .context("mid-removal offline fixture source")?;
    let metadata = std::fs::metadata(&source_path)?;
    let source_size = i64::try_from(metadata.len())?;
    let source_mtime = i64::try_from(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_secs())?;
    let source = source_path.to_string_lossy().to_string();
    let file = store
        .upsert_file(
            item,
            &source,
            source_size,
            source_mtime,
            &ProbeResult::default(),
        )
        .await?;
    let package = NewOfflinePackage {
        id: format!("{LATE_PACKAGE}-{node_id}"),
        request_id: format!("{LATE_PACKAGE}-request-{node_id}"),
        user_id,
        file_id: file,
        node_id: node_id.to_owned(),
        source_path: source,
        source_size,
        source_mtime,
        effective_rate_control: "vbr".to_owned(),
        target_height: 720,
        output_width: Some(1280),
        output_height: Some(720),
        audio_index: None,
        audio_offset_ms: 0,
        subtitle_index: None,
        subtitle_language: None,
        subtitle_mode: "none".to_owned(),
        estimated_bytes: 700,
        reserved_bytes: 900,
        expires_at: unix_now()?.saturating_add(3_600),
    };
    if !matches!(
        store
            .create_offline_package(&package, 10, 100_000, 1_000_000)
            .await?,
        OfflineCreateOutcome::Created(_)
    ) {
        bail!("the download requested during the removal was not admitted");
    }
    Ok(())
}

async fn exercise(store: &HiqliteAuthStore, ordinal: u64) -> Result<()> {
    store.ping().await?;
    if store.instance_id().await? != INSTANCE_ID {
        bail!("logical instance id drifted");
    }

    let suffix = ordinal.to_string();
    let setting = format!("proof.node.{suffix}");
    store.put_setting(&setting, "acknowledged").await?;
    if store.get_setting(&setting).await?.as_deref() != Some("acknowledged") {
        bail!("setting did not round-trip");
    }

    let username = format!("survivor-{suffix}");
    let initial_admin = ordinal.is_multiple_of(2);
    let user = store
        .create_user(&username, "hash-v1", initial_admin)
        .await
        .context("create proof user")?;
    if user.password_hash != "hash-v1" || user.is_admin != initial_admin {
        bail!("create user did not preserve password/admin fields");
    }
    if store
        .get_user(user.id)
        .await?
        .is_none_or(|row| row.password_hash != "hash-v1" || row.is_admin != initial_admin)
    {
        bail!("user id lookup did not preserve password/admin fields");
    }
    if store
        .get_user_by_username(&username.to_ascii_uppercase())
        .await?
        .map(|row| row.id)
        != Some(user.id)
    {
        bail!("case-insensitive username lookup failed");
    }
    let replacement = format!("hash-v2-{suffix}");
    let password_changed = store.set_password(user.id, &replacement).await?;
    let password_after = store.get_user(user.id).await?.map(|row| row.password_hash);
    if !password_changed || password_after.as_deref() != Some(replacement.as_str()) {
        bail!("password replacement failed: changed={password_changed}, value={password_after:?}");
    }
    if !store.set_admin(user.id, false).await?
        || store
            .get_user(user.id)
            .await?
            .is_none_or(|row| row.is_admin)
        || !store.set_admin(user.id, true).await?
    {
        bail!("admin mutation failed");
    }
    if store.count_users().await? < ordinal as i64
        || store.count_admins().await? < ordinal as i64
        || !store
            .list_users()
            .await?
            .iter()
            .any(|row| row.id == user.id)
    {
        bail!("user list/count contract failed");
    }

    let token = format!("survive-token-{suffix}");
    store
        .create_token(&token, user.id, Some("cluster-check"))
        .await?;
    if store.user_for_token(&token).await?.map(|row| row.id) != Some(user.id) {
        bail!("proof token lookup failed");
    }
    let temporary_token = format!("temporary-token-{suffix}");
    store.create_token(&temporary_token, user.id, None).await?;
    if !store.delete_token(&temporary_token).await?
        || store.user_for_token(&temporary_token).await?.is_some()
    {
        bail!("token deletion failed");
    }
    for token_index in 1..=2 {
        store
            .create_token(&format!("bulk-token-{suffix}-{token_index}"), user.id, None)
            .await?;
    }
    if store.delete_tokens_for_user(user.id).await? != 3 {
        bail!("bulk token revocation did not include every token");
    }
    store
        .create_token(&token, user.id, Some("cluster-check"))
        .await?;

    let disposable = store
        .create_user(&format!("disposable-{suffix}"), "hash", false)
        .await?;
    let disposable_token = format!("disposable-token-{suffix}");
    store
        .create_token(&disposable_token, disposable.id, None)
        .await?;
    if !store.delete_user(disposable.id).await?
        || store.get_user(disposable.id).await?.is_some()
        || store.user_for_token(&disposable_token).await?.is_some()
    {
        bail!("user deletion/cascade failed");
    }

    let key_hash = format!("survive-key-{suffix}");
    let key = store
        .create_api_key(
            &format!("node-{suffix}"),
            &key_hash,
            &["scan:trigger".to_owned()],
        )
        .await?;
    if !key.allows("scan:trigger")
        || store
            .api_key_for_hash(&key_hash)
            .await?
            .is_none_or(|row| row.id != key.id || !row.allows("scan:trigger"))
        || !store
            .list_api_keys()
            .await?
            .iter()
            .any(|row| row.id == key.id)
    {
        bail!("API key lookup/list failed");
    }
    store.touch_api_key(key.id).await?;
    if store
        .api_key_for_hash(&key_hash)
        .await?
        .is_none_or(|row| row.last_used_at.is_none())
        || !store.set_api_key_disabled(key.id, true).await?
        || store
            .api_key_for_hash(&key_hash)
            .await?
            .is_none_or(|row| !row.disabled || row.allows("scan:trigger"))
        || !store.set_api_key_disabled(key.id, false).await?
    {
        bail!("API key touch/disable contract failed");
    }
    let temporary_key = store
        .create_api_key(
            &format!("temporary-{suffix}"),
            &format!("temporary-key-{suffix}"),
            &[],
        )
        .await?;
    if !store.delete_api_key(temporary_key.id).await?
        || store
            .api_key_for_hash(&format!("temporary-key-{suffix}"))
            .await?
            .is_some()
    {
        bail!("API key deletion failed");
    }

    let library = store
        .create_library(&NewLibrary {
            name: format!("Cluster Movies {suffix}"),
            kind: LibraryKind::Movies,
            paths: vec![PathBuf::from(format!("/cluster/media/{suffix}"))],
            anime: false,
        })
        .await?;
    let movie = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("Replicated Catalog Proof {suffix}"),
            year: Some(2000 + ordinal as i32),
            season_number: None,
            episode_number: None,
        })
        .await?;
    store
        .apply_metadata(
            movie,
            &MetadataPatch {
                overview: Some(format!("replicated browse search voter {suffix}")),
                tmdb_id: Some(10_000 + ordinal as i64),
                genres: Some(vec!["Science Fiction".to_owned()]),
                enriched: true,
                ..MetadataPatch::default()
            },
        )
        .await?;
    let file = store
        .upsert_file(
            movie,
            &format!("/cluster/media/{suffix}/proof-{suffix}.mkv"),
            1_000 + ordinal as i64,
            1_700_000_000 + ordinal as i64,
            &ProbeResult {
                duration_ms: Some(120_000),
                container: Some("mkv".to_owned()),
                video_codec: Some("hevc".to_owned()),
                width: Some(3840),
                height: Some(2160),
                raw_json: Some(format!(r#"{{"voter":{ordinal}}}"#)),
                ..ProbeResult::default()
            },
        )
        .await?;
    let fingerprint = format!("root-fingerprint-{suffix}");
    if store
        .ensure_library_root_fingerprint(library.id, &fingerprint, true)
        .await?
        != RootFingerprintStatus::Established
    {
        bail!("new library root fingerprint was not established");
    }
    if !matches!(
        store
            .reconcile_library(library.id, "stale-root", &[file], 1)
            .await?,
        ReconcileOutcome::RefusedRoot { .. }
    ) || store.get_file(file).await?.is_none()
    {
        bail!("stale-root reconciliation committed a prune");
    }
    if store
        .reconcile_library(library.id, &fingerprint, &[file], 0)
        .await?
        != (ReconcileOutcome::RefusedPrune {
            requested: 1,
            limit: 0,
        })
        || store.get_file(file).await?.is_none()
    {
        bail!("over-budget reconciliation committed a prune");
    }
    let disposable_item = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("Disposable Search Trigger Proof {suffix}"),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await?;
    let disposable_file = store
        .upsert_file(
            disposable_item,
            &format!("/cluster/media/{suffix}/disposable-{suffix}.mkv"),
            42,
            1_700_000_100 + ordinal as i64,
            &ProbeResult::default(),
        )
        .await?;
    if !matches!(
        store
            .reconcile_library(library.id, &fingerprint, &[disposable_file], 1)
            .await?,
        ReconcileOutcome::Applied {
            deleted_files: 1,
            pruned_items: 1
        }
    ) || store.get_item(disposable_item).await?.is_some()
    {
        bail!("item deletion did not remove the file/item through the FTS trigger");
    }
    let state = store
        .put_progress(user.id, movie, 30_000, Some(10_000))
        .await?;
    if state.position_ms != 30_000 || state.duration_ms != Some(120_000) || state.watched {
        bail!("replicated watch progress did not prefer the probed duration");
    }

    // Trakt is the one durable credential plurx replays rather than verifies,
    // so what raft carries here must be ciphertext (CLUSTERING-PLAN.md §3.2).
    // The key is node-local and never replicated; this harness shares one
    // across the voters exactly as a real cluster distributes it out of band.
    let key = CredentialKey::generate();
    let sealed_refresh = key.seal_trakt(user.id, &format!("trakt-refresh-{suffix}"))?;
    let trakt = TraktAuth {
        user_id: user.id,
        access_token: key.seal_trakt(user.id, &format!("trakt-access-{suffix}"))?,
        refresh_token: sealed_refresh.clone(),
        expires_at: 4_000_000_000,
        trakt_username: Some(format!("trakt-{suffix}")),
        connected_at: 1_700_000_000 + ordinal as i64,
        last_sync_at: 0,
        last_activities: None,
    };
    store.put_trakt_auth(&trakt).await?;
    let trakt_sync_at = 1_700_000_100 + ordinal as i64;
    store
        .set_trakt_sync(user.id, trakt_sync_at, Some("{}"))
        .await?;
    // The compare-and-set operand is the replicated envelope, so the winner is
    // decided on bytes every voter already agrees on without holding the key.
    if !store
        .update_trakt_tokens(
            user.id,
            &sealed_refresh,
            &key.seal_trakt(user.id, &format!("trakt-access-new-{suffix}"))?,
            &key.seal_trakt(user.id, &format!("trakt-refresh-new-{suffix}"))?,
            4_000_000_001,
        )
        .await?
        || store
            .update_trakt_tokens(
                user.id,
                &sealed_refresh,
                &key.seal_trakt(user.id, &format!("trakt-access-loser-{suffix}"))?,
                &key.seal_trakt(user.id, &format!("trakt-refresh-loser-{suffix}"))?,
                4_000_000_002,
            )
            .await?
    {
        bail!("replicated Trakt token compare-and-set failed");
    }
    if store.get_trakt_auth(user.id).await?.is_none_or(|auth| {
        let replicated_cleartext = auth.access_token.as_stored().contains("trakt-access")
            || auth.refresh_token.as_stored().contains("trakt-refresh");
        let access = auth
            .reveal_access_token(&key)
            .ok()
            .is_none_or(|token| token.expose() != format!("trakt-access-new-{suffix}"));
        let refresh = auth
            .reveal_refresh_token(&key)
            .ok()
            .is_none_or(|token| token.expose() != format!("trakt-refresh-new-{suffix}"));
        replicated_cleartext
            || access
            || refresh
            || auth.expires_at != 4_000_000_001
            || auth.last_sync_at != trakt_sync_at
            || auth.last_activities.as_deref() != Some("{}")
    }) || !store
        .list_trakt_auth()
        .await?
        .iter()
        .any(|auth| auth.user_id == user.id)
        || !store
            .trakt_sync_candidates(user.id)
            .await?
            .iter()
            .any(|candidate| candidate.item_id == movie)
    {
        bail!("replicated Trakt link/candidate join failed");
    }
    if store
        .delete_trakt_auth_if_current(user.id, &sealed_refresh)
        .await?
        || store.get_trakt_auth(user.id).await?.is_none()
    {
        bail!("stale replicated Trakt unlink deleted a refreshed credential");
    }
    let unlink_user = store
        .create_user(&format!("trakt-unlink-{suffix}"), "hash", false)
        .await?;
    let mut unlink = trakt.clone();
    unlink.user_id = unlink_user.id;
    store.put_trakt_auth(&unlink).await?;
    store.delete_trakt_auth(unlink_user.id).await?;
    if store.get_trakt_auth(unlink_user.id).await?.is_some()
        || store.get_user(unlink_user.id).await?.is_none()
    {
        bail!("replicated Trakt unlink failed");
    }
    if !store.delete_user(unlink_user.id).await? {
        bail!("replicated Trakt unlink fixture user cleanup failed");
    }

    let outbox_id = store
        .enqueue_watched(&format!(r#"{{"item":{movie}}}"#))
        .await?;
    let mut outbox = store
        .due_watched(100)
        .await?
        .into_iter()
        .find(|entry| entry.id == outbox_id)
        .context("replicated outbox entry was not due")?;
    outbox.attempts = 1;
    outbox.status = "ok".to_owned();
    store.settle_watched(&outbox).await?;
    if store.watched_outbox_counts().await? != (0, ordinal as i64, 0) {
        bail!("replicated watched-outbox status counts drifted");
    }

    let node_id = format!("node-{suffix}");
    let recipe_hash = format!("recipe-{suffix}");
    if !store
        .claim_cache_entry(&recipe_hash, file, 1, &node_id, &format!("cache/{suffix}"))
        .await?
    {
        bail!("first replicated cache claim was not accepted");
    }
    if store
        .claim_cache_entry(
            &recipe_hash,
            file,
            1,
            &node_id,
            &format!("cache/moved-{suffix}"),
        )
        .await?
    {
        bail!("second replicated cache claim moved an existing owner");
    }
    store
        .complete_cache_entry(&recipe_hash, &node_id, 800 + ordinal as i64)
        .await?;
    store.touch_cache_entry(&recipe_hash, &node_id).await?;
    if store.cache_hit(&recipe_hash, &node_id).await?.is_none()
        || store.cache_hit(&recipe_hash, "other-node").await?.is_some()
        || !store
            .cache_by_age(&node_id, 100)
            .await?
            .iter()
            .any(|row| row.recipe_hash == recipe_hash)
        || !store
            .all_cache_rows(&node_id)
            .await?
            .iter()
            .any(|row| row.recipe_hash == recipe_hash)
    {
        bail!("replicated cache location lost node ownership");
    }
    let unpinned = format!("unpinned-recipe-{suffix}");
    if !store
        .claim_cache_entry(
            &unpinned,
            file,
            1,
            &node_id,
            &format!("cache/unpinned-{suffix}"),
        )
        .await?
    {
        bail!("replicated unpinned cache claim was not accepted");
    }
    store.complete_cache_entry(&unpinned, &node_id, 123).await?;
    let expected_cache_bytes = 923 + ordinal as i64;
    let cache_bytes = store.cache_bytes(&node_id).await?;
    if cache_bytes != expected_cache_bytes {
        bail!(
            "replicated cache read-after-write on voter {ordinal} expected \
             {expected_cache_bytes} bytes, found {cache_bytes}"
        );
    }
    let abandoned = format!("abandoned-recipe-{suffix}");
    if !store
        .claim_cache_entry(
            &abandoned,
            file,
            1,
            &node_id,
            &format!("cache/abandoned-{suffix}"),
        )
        .await?
    {
        bail!("replicated abandoned cache claim was not accepted");
    }
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let stale_cutoff = unix_now()?;
    if !store
        .stale_cache_claims(&node_id, stale_cutoff)
        .await?
        .iter()
        .any(|row| row.recipe_hash == abandoned)
    {
        bail!("replicated cache claim did not become stale before its heartbeat");
    }
    store.touch_cache_claim(&abandoned, &node_id).await?;
    let heartbeat_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !store
            .stale_cache_claims(&node_id, stale_cutoff)
            .await?
            .iter()
            .any(|row| row.recipe_hash == abandoned)
        {
            break;
        }
        if Instant::now() >= heartbeat_deadline {
            bail!("replicated cache heartbeat did not refresh the stale cutoff");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !store
        .stale_cache_claims(&node_id, i64::MAX)
        .await?
        .iter()
        .any(|row| row.recipe_hash == abandoned)
    {
        bail!("replicated stale cache claim was not visible");
    }
    store
        .forget_cache_entry(&abandoned, &node_id, "local")
        .await?;
    if store.cache_hit(&abandoned, &node_id).await?.is_some() {
        bail!("replicated cache forget left a serveable row");
    }

    let outsider = store
        .create_user(&format!("offline-outsider-{suffix}"), "hash", false)
        .await?;
    let offline = NewOfflinePackage {
        id: format!("offline-{suffix}"),
        request_id: format!("offline-request-{suffix}"),
        user_id: user.id,
        file_id: file,
        node_id: node_id.clone(),
        source_path: format!("/cluster/media/{suffix}/proof-{suffix}.mkv"),
        source_size: 1_000 + ordinal as i64,
        source_mtime: 1_700_000_000 + ordinal as i64,
        effective_rate_control: "vbr".to_owned(),
        target_height: 720,
        output_width: Some(1280),
        output_height: Some(720),
        audio_index: Some(1),
        audio_offset_ms: 0,
        subtitle_index: None,
        subtitle_language: None,
        subtitle_mode: "none".to_owned(),
        estimated_bytes: 700,
        reserved_bytes: 900,
        expires_at: 4_000_000_000,
    };
    if !matches!(
        store
            .create_offline_package(&offline, 10, 100_000, 1_000_000)
            .await?,
        OfflineCreateOutcome::Created(_)
    ) {
        bail!("replicated offline admission was not created");
    }
    if !matches!(
        store
            .create_offline_package(&offline, 10, 100_000, 1_000_000)
            .await?,
        OfflineCreateOutcome::Existing(_)
    ) {
        bail!("replicated offline admission was not idempotent");
    }
    let mut conflict = offline.clone();
    conflict.target_height = 1080;
    if store
        .create_offline_package(&conflict, 10, 100_000, 1_000_000)
        .await?
        != OfflineCreateOutcome::RequestConflict
    {
        bail!("replicated offline request conflict was not detected");
    }
    if store
        .offline_package_for_user(&offline.id, user.id)
        .await?
        .is_none_or(|package| {
            package.target_height != offline.target_height
                || package.estimated_bytes != offline.estimated_bytes
                || package.reserved_bytes != offline.reserved_bytes
                || package.expires_at != offline.expires_at
        })
    {
        bail!("replicated request conflict mutated the accepted package");
    }
    let mut rejected = offline.clone();
    rejected.id = format!("offline-rejected-{suffix}");
    rejected.request_id = format!("offline-rejected-request-{suffix}");
    if !matches!(
        store
            .create_offline_package(&rejected, 0, 100_000, 1_000_000)
            .await?,
        OfflineCreateOutcome::RowLimit { .. }
    ) || !matches!(
        store
            .create_offline_package(&rejected, 10, 1, 1_000_000)
            .await?,
        OfflineCreateOutcome::ByteLimit { .. }
    ) || !matches!(
        store
            .create_offline_package(&rejected, 10, 100_000, 1)
            .await?,
        OfflineCreateOutcome::GlobalByteLimit { .. }
    ) {
        bail!("replicated offline quota refusal contract failed");
    }
    if store
        .offline_package_for_user(&rejected.id, user.id)
        .await?
        .is_some()
    {
        bail!("replicated quota refusal left a package row behind");
    }
    let wrong_node = format!("wrong-node-{suffix}");
    let wrong_stats = store.offline_package_stats(&wrong_node, 1).await?;
    if store
        .claim_next_offline_package(&wrong_node)
        .await?
        .is_some()
        || store
            .reset_interrupted_offline_packages(&wrong_node)
            .await?
            != 0
        || !store
            .offline_activity_packages(&wrong_node, 1, 0, 100)
            .await?
            .is_empty()
        || wrong_stats.queued != 0
        || wrong_stats.preparing != 0
        || wrong_stats.ready != 0
        || wrong_stats.failed != 0
        || store.cache_bytes(&wrong_node).await? != 0
        || !store.cache_by_age(&wrong_node, 100).await?.is_empty()
        || !store.all_cache_rows(&wrong_node).await?.is_empty()
    {
        bail!("replicated node ownership predicates accepted another node's state");
    }
    if store
        .offline_package_for_user(&offline.id, outsider.id)
        .await?
        .is_some()
        || store
            .renew_offline_package_for_user(&offline.id, outsider.id, 4_000_000_003)
            .await?
            .is_some()
        || store
            .delete_offline_package(&offline.id, outsider.id)
            .await?
    {
        bail!("replicated offline owner predicates exposed another user's package");
    }
    if store
        .claim_next_offline_package(&node_id)
        .await?
        .is_none_or(|package| package.id != offline.id)
        || !store
            .set_offline_package_recipe(&offline.id, &recipe_hash)
            .await?
        || !store
            .update_offline_progress(&offline.id, &node_id, "video", 500)
            .await?
    {
        bail!("replicated offline preparation state machine failed");
    }
    // The producer fence, asserted while the package is genuinely
    // mid-preparation: its writes belong to whichever node owns it *now*.
    // Removal re-homes a package while the departing node's encoder may still
    // be running, and an unfenced yield would knock the survivor's claimed
    // work back to `queued` while its progress flapped.
    if store
        .requeue_offline_package(&offline.id, &wrong_node)
        .await?
        || store
            .update_offline_progress(&offline.id, &wrong_node, "stolen", 999)
            .await?
        || store
            .offline_package_for_user(&offline.id, user.id)
            .await?
            .is_none_or(|package| package.phase != "video" || package.progress_millis != 500)
    {
        bail!("replicated offline producer writes were not fenced to the owning node");
    }
    if !store
        .mark_offline_package_ready(&offline.id, &node_id, &recipe_hash, 800, 120_000)
        .await?
    {
        bail!("replicated offline preparation state machine failed");
    }
    if store
        .mark_offline_package_ready(&offline.id, &node_id, &recipe_hash, 801, 120_001)
        .await?
        || store.requeue_offline_package(&offline.id, &node_id).await?
        || store
            .set_offline_package_recipe(&offline.id, "wrong-recipe")
            .await?
        || store
            .update_offline_progress(&offline.id, &node_id, "wrong-state", 999)
            .await?
        || store
            .fail_offline_package(&offline.id, &node_id, "wrong-state", "wrong", "wrong")
            .await?
    {
        bail!("replicated offline terminal-state guards accepted a late mutation");
    }
    if store
        .offline_package_for_user(&offline.id, user.id)
        .await?
        .is_none_or(|package| {
            package.state != "ready"
                || package.recipe_hash.as_deref() != Some(recipe_hash.as_str())
                || package.actual_bytes != Some(800)
                || package.duration_ms != Some(120_000)
        })
    {
        bail!("replicated rejected state transitions mutated the ready package");
    }
    if !matches!(
        store
            .put_offline_lease(
                &offline.id,
                user.id,
                &format!("offline-token-{suffix}"),
                4_000_000_000,
            )
            .await?,
        OfflineLeaseOutcome::Created(_)
    ) || store.offline_package_stats(&node_id, 1).await?.ready != 1
        || store.cache_bytes(&node_id).await? != 123
    {
        bail!("replicated offline lease/pinned-cache accounting failed");
    }
    let wrong_lease = format!("offline-wrong-user-token-{suffix}");
    if store
        .put_offline_lease(&offline.id, outsider.id, &wrong_lease, 4_000_000_005)
        .await?
        != OfflineLeaseOutcome::PackageNotReady
        || store
            .offline_package_for_lease(&wrong_lease, 1, 4_000_000_005)
            .await?
            .is_some()
    {
        bail!("replicated lease ownership predicates accepted another user");
    }
    if !matches!(
        store
            .put_offline_lease(
                &offline.id,
                user.id,
                &format!("offline-token-{suffix}"),
                4_000_000_001,
            )
            .await?,
        OfflineLeaseOutcome::Renewed(_)
    ) || store
        .put_offline_lease(
            &offline.id,
            user.id,
            &format!("offline-token-conflict-{suffix}"),
            4_000_000_001,
        )
        .await?
        != OfflineLeaseOutcome::TokenConflict
        || store
            .renew_offline_package_for_user(&offline.id, user.id, 4_000_000_002)
            .await?
            .is_none()
        || !store
            .offline_activity_packages(&node_id, 1, 0, 100)
            .await?
            .iter()
            .any(|row| row.package.id == offline.id && row.lease_active)
    {
        bail!("replicated offline renewal/activity contract failed");
    }

    let mut work = offline.clone();
    work.id = format!("offline-work-{suffix}");
    work.request_id = format!("offline-work-request-{suffix}");
    store
        .create_offline_package(&work, 10, 100_000, 1_000_000)
        .await?;
    if store
        .claim_next_offline_package(&node_id)
        .await?
        .is_none_or(|package| package.id != work.id)
        || store.reset_interrupted_offline_packages(&node_id).await? != 1
        || store
            .claim_next_offline_package(&node_id)
            .await?
            .is_none_or(|package| package.id != work.id)
        || !store.requeue_offline_package(&work.id, &node_id).await?
        || store
            .claim_next_offline_package(&node_id)
            .await?
            .is_none_or(|package| package.id != work.id)
        || !store
            .fail_offline_package(&work.id, &node_id, "video", "proof", "expected")
            .await?
        || !store.delete_offline_package(&work.id, user.id).await?
    {
        bail!("replicated offline recovery/failure/delete contract failed");
    }

    let mut expired = offline.clone();
    expired.id = format!("offline-expired-{suffix}");
    expired.request_id = format!("offline-expired-request-{suffix}");
    expired.expires_at = 1;
    store
        .create_offline_package(&expired, 10, 100_000, 1_000_000)
        .await?;
    if store.expire_offline_packages(2).await? == 0 {
        bail!("replicated offline expiry did not remove an expired package");
    }
    if !store.delete_user(outsider.id).await? {
        bail!("replicated offline outsider fixture cleanup failed");
    }
    Ok(())
}

async fn catalog_view(store: &HiqliteAuthStore) -> Result<CatalogView> {
    let mut search = store
        .search_items("replicated browse", 100)
        .await?
        .into_iter()
        .map(|row| row.item.id)
        .collect::<Vec<_>>();
    search.sort_unstable();
    Ok(CatalogView {
        authoritative_digest: store.validation_local_catalog_truth_digest().await?,
        full_digest: store.local_dump_digest().await?,
        search,
    })
}

async fn verify_proof(store: &HiqliteAuthStore) -> Result<()> {
    for ordinal in 1..=3 {
        let suffix = ordinal.to_string();
        if store
            .get_setting(&format!("proof.node.{suffix}"))
            .await?
            .as_deref()
            != Some("acknowledged")
        {
            bail!("lost acknowledged setting from node {ordinal}");
        }
        let user = store
            .get_user_by_username(&format!("survivor-{suffix}"))
            .await?
            .with_context(|| format!("lost acknowledged user from node {ordinal}"))?;
        if store
            .user_for_token(&format!("survive-token-{suffix}"))
            .await?
            .map(|row| row.id)
            != Some(user.id)
        {
            bail!("lost acknowledged token from node {ordinal}");
        }
        if store
            .api_key_for_hash(&format!("survive-key-{suffix}"))
            .await?
            .is_none()
        {
            bail!("lost acknowledged API key from node {ordinal}");
        }
        let library = store
            .list_libraries()
            .await?
            .into_iter()
            .find(|library| library.name == format!("Cluster Movies {suffix}"))
            .with_context(|| format!("lost acknowledged library from node {ordinal}"))?;
        let page = store
            .list_top_items(library.id, ItemSort::Title, 0, 10)
            .await?;
        if page.items.len() != 1
            || store.files_for_item(page.items[0].id).await?.len() != 1
            || store
                .watch_state(user.id, page.items[0].id)
                .await?
                .is_none()
        {
            bail!("lost acknowledged media/watch state from node {ordinal}");
        }
        if store.get_trakt_auth(user.id).await?.is_none_or(|auth| {
            // `exercise` sealed these under a key it did not persist, so this
            // pass proves the durable form and not the cleartext: what came
            // back through raft is still an envelope, and still not the bearer
            // token. A voter disk alone is not enough to use the account.
            !auth.is_wrapped()
                || auth.access_token.as_stored().contains("trakt-access")
                || auth.refresh_token.as_stored().contains("trakt-refresh")
                || auth.expires_at != 4_000_000_001
                || auth.last_sync_at != 1_700_000_100 + ordinal as i64
                || auth.last_activities.as_deref() != Some("{}")
        }) || store
            .offline_package_for_user(&format!("offline-{suffix}"), user.id)
            .await?
            .is_none_or(|package| package.state != "ready")
            || store
                .offline_package_for_lease(&format!("offline-token-{suffix}"), 1, 4_000_000_000)
                .await?
                .is_none()
            || store
                .cache_hit(&format!("recipe-{suffix}"), &format!("node-{suffix}"))
                .await?
                .is_none()
            || store
                .cache_hit(
                    &format!("unpinned-recipe-{suffix}"),
                    &format!("node-{suffix}"),
                )
                .await?
                .is_none()
            || store.cache_bytes(&format!("node-{suffix}")).await? != 123
        {
            bail!("lost acknowledged Trakt/cache/offline state from node {ordinal}");
        }
    }
    let counts = store.watched_outbox_counts().await?;
    if counts != (0, 3, 0) {
        bail!("lost acknowledged watched-outbox rows after voter loss: {counts:?}");
    }
    if catalog_view(store).await?.search.len() != 3 {
        bail!("lost local FTS search rows after voter loss");
    }
    Ok(())
}

pub async fn write_response(response: &Response) -> Result<()> {
    let mut output = tokio::io::stdout();
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

/// Run the joining-voter compatibility preflight in its own process, exiting
/// 42 when the cluster refuses this candidate.
pub async fn preflight_voter(preflight: Preflight) -> Result<()> {
    install_crypto_provider();
    let remote = Client::remote(
        preflight.addresses,
        true,
        true,
        API_SECRET.to_owned(),
        true,
        None,
    )
    .await?;
    match HiqliteAuthStore::preflight_voter(&remote, preflight.compatibility).await {
        Ok(()) => {
            println!("compatible");
            Ok(())
        }
        Err(error) => {
            println!("{error}");
            std::process::exit(42);
        }
    }
}

/// Start a candidate voter one schema version behind and return the refusal it
/// printed. Any other exit status is itself a failure.
pub async fn run_incompatible_preflight(executable: &Path, specs: &[NodeSpec]) -> Result<String> {
    let input = Preflight {
        addresses: specs.iter().map(|node| node.api.clone()).collect(),
        compatibility: ClusterCompatibility {
            schema_version: AUTH_SCHEMA_VERSION - 1,
            protocol_version: AUTH_PROTOCOL_VERSION,
        },
    };
    let output = tokio::time::timeout(
        REQUEST_TIMEOUT,
        Command::new(executable)
            .arg("preflight")
            .arg(serde_json::to_string(&input)?)
            .output(),
    )
    .await
    .context("incompatible voter preflight timed out")??;
    if output.status.code() != Some(42) {
        bail!(
            "incompatible voter exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Build the hiqlite configuration for one voter and create its data dir.
pub fn node_config(launch: &NodeLaunch) -> Result<NodeConfig> {
    let data_dir = launch.root.join(format!("node-{}", launch.node_id));
    std::fs::create_dir_all(&data_dir)?;
    Ok(NodeConfig {
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
        listen_addr_api: Cow::Borrowed(LISTEN_ADDR),
        listen_addr_raft: Cow::Borrowed(LISTEN_ADDR),
        data_dir: Cow::Owned(data_dir.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed("auth.db"),
        secret_raft: RAFT_SECRET.to_owned(),
        secret_api: API_SECRET.to_owned(),
        tls_raft: Some(ServerTlsConfig::TlsAutoCertificates),
        tls_api: Some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: HIQLITE_WAL_SIZE_BYTES,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    })
}

/// A reserved set of ports whose listeners stay alive so no other process
/// can claim them before the intended voter binds.
///
/// Drop the reservation to release the ports. Pass it to
/// [`ClusterProcesses::start`] so the cluster holds the ports until every
/// voter has announced readiness, or to any call site that needs the
/// guarantee that these ports will not be reallocated while the caller
/// decides.
pub struct PortReservation {
    /// Held open so nothing else claims the port between allocation and bind.
    #[allow(dead_code)]
    listeners: Vec<TcpListener>,
    /// The voter specs describing the reserved ports.
    pub specs: Vec<NodeSpec>,
}

impl PortReservation {
    /// An empty reservation that holds no ports. Useful for tests that
    /// create a cluster with no voters.
    pub fn empty() -> Self {
        Self {
            listeners: Vec::new(),
            specs: Vec::new(),
        }
    }
    /// The [`NodeSpec`] entries allocated for each voter.
    pub fn specs(&self) -> &[NodeSpec] {
        &self.specs
    }

    /// Consume the reservation and return only the specs, releasing the ports.
    pub fn into_specs(self) -> Vec<NodeSpec> {
        self.specs
    }

    /// Recreate an allocation after releasing its reservations.
    ///
    /// This deliberately provides no allocation guarantee and exists only for
    /// the deterministic collision regression that takes one of these ports
    /// before the voter binds it.
    #[doc(hidden)]
    pub fn unreserved(specs: Vec<NodeSpec>) -> Self {
        Self {
            listeners: Vec::new(),
            specs,
        }
    }

    /// Returns the listeners, consuming the reservation.
    pub fn into_inner(self) -> (Vec<TcpListener>, Vec<NodeSpec>) {
        (self.listeners, self.specs)
    }
}

/// Reserve a raft and an API port for each of `count` voters, holding every
/// listener open so another process cannot claim the port between allocation
/// and bind.
///
/// Each port is bound to [`LISTEN_ADDR`] on a kernel-assigned port, the port
/// number is recorded in a [`NodeSpec`], and the listener is kept alive until
/// the [`PortReservation`] is consumed. Pass the reservation to
/// [`ClusterProcesses::start`] so the ports stay reserved until every voter
/// has announced readiness.
pub fn allocate_nodes(count: u64) -> Result<PortReservation> {
    let mut listeners = Vec::with_capacity(count as usize * 2);
    for _ in 0..count * 2 {
        listeners.push(TcpListener::bind((LISTEN_ADDR, 0)).context("reserve a harness port")?);
    }
    let mut ports = listeners
        .iter()
        .map(|listener| Ok(listener.local_addr()?.port()))
        .collect::<Result<Vec<_>>>()?
        .into_iter();
    let specs = (1..=count)
        .map(|id| {
            Ok(NodeSpec {
                id,
                raft: format!("{LISTEN_ADDR}:{}", ports.next().context("raft port")?),
                api: format!("{LISTEN_ADDR}:{}", ports.next().context("api port")?),
            })
        })
        .collect::<Result<_>>()?;
    Ok(PortReservation { listeners, specs })
}

/// Observe one free port.
///
/// The listener is released as the expression ends, so this reports a port
/// that *was* free rather than one this process holds. Nothing that starts a
/// voter may treat the result as a reservation; see [`allocate_nodes`].
///
/// # Correct use
///
/// `free_port` is for observation, not allocation. Use it to find a port for
/// a squatter in a collision test, or to assert that the OS assigned a
/// non-zero port. Never use it to choose a port a voter will later bind:
/// that is what [`allocate_nodes`] / [`PortReservation`] are for.
pub fn free_port() -> Result<u16> {
    Ok(TcpListener::bind((LISTEN_ADDR, 0))?.local_addr()?.port())
}

/// The last port collision this process observed, recorded by
/// [`install_bind_failure_guard`].
static BIND_FAILURE: OnceLock<String> = OnceLock::new();

/// Where a voter reports a listener that never bound.
#[derive(Clone, Copy, Debug)]
pub enum BindFailureChannel {
    /// Write a [`Response::Error`] on the stdin/stdout voter protocol, so the
    /// controller reads the collision as this voter's own startup verdict.
    Protocol,
    /// Print to stderr, for a harness voter that has no protocol peer.
    Stderr,
}

/// Is this error a port taken between allocation and bind, rather than
/// anything the cluster contract asserts?
///
/// A port is only ever observed free and then released, so a voter is always
/// started on a port another process may have claimed in the meantime. That is
/// an environment fact, not a durable-state fault. Classifying it here — once,
/// by name — is what keeps "the port moved" from being read as "the store is
/// un-migrated".
pub fn is_port_collision(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains(PORT_COLLISION) || text.contains("Address already in use")
}

/// Make a listener that never bound the voter's own verdict.
///
/// hiqlite serves its raft and its API listener from detached `tokio::spawn`
/// tasks that `.unwrap()` the serve future (`hiqlite-0.14.0/src/start.rs:148`
/// and `:231`). A bind that loses its port panics one of those tasks and
/// nothing else: `start_node` has already returned `Ok`,
/// `wait_until_healthy_db` probes only the *local* database, and the process
/// stays alive serving one dead listener. The collision then reached the
/// controller as whatever the crippled voter failed at next — a replicated
/// deadline for the API port, or `no such table: cluster_meta` for a
/// linearizable read that never reached a state machine, which reads as an
/// un-migrated store rather than a busy port.
///
/// This hook turns that panic into a classified verdict and stops the voter,
/// so a port collision cannot go on to be reported as durable-state damage.
/// Panics that are not bind failures keep their previous behaviour.
pub fn install_bind_failure_guard(channel: BindFailureChannel, addresses: Vec<String>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let Some(payload) = bind_failure_payload(info) else {
            previous(info);
            return;
        };
        let message = format!(
            "{PORT_COLLISION}: a voter listener could not bind one of [{}]: {payload}",
            addresses.join(", ")
        );
        let _ = BIND_FAILURE.set(message.clone());
        match channel {
            BindFailureChannel::Protocol => {
                // A panic hook cannot drive the async writer, so emit the same
                // newline framing the controller reads synchronously.
                if let Ok(mut line) = serde_json::to_vec(&Response::Error { message }) {
                    line.push(b'\n');
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(&line);
                    let _ = stdout.flush();
                }
            }
            BindFailureChannel::Stderr => eprintln!("{message}"),
        }
        // Leaving the voter alive is the defect being fixed: it would keep
        // answering with downstream symptoms of the dead listener. `exit`
        // rather than `abort` so an instrumented build still writes its
        // coverage profile.
        std::process::exit(BIND_FAILURE_EXIT);
    }));
}

/// The panic message, when the panic is a listener that could not bind.
fn bind_failure_payload(info: &PanicHookInfo<'_>) -> Option<String> {
    let payload = info.payload_as_str()?;
    (payload.contains("AddrInUse") || payload.contains("Address already in use"))
        .then(|| payload.to_owned())
}

/// The addresses this voter's hiqlite node will bind.
///
/// hiqlite builds each listener from `listen_addr_*` and the port half of this
/// node's own entry (`hiqlite-0.14.0/src/start.rs:318`), so these are the two
/// addresses a collision can be a collision *with*.
pub fn voter_listen_addrs(launch: &NodeLaunch) -> Result<Vec<String>> {
    let node = launch
        .nodes
        .iter()
        .find(|node| node.id == launch.node_id)
        .with_context(|| format!("voter {} has no spec of its own", launch.node_id))?;
    [&node.raft, &node.api]
        .into_iter()
        .map(|address| {
            let (_, port) = address
                .rsplit_once(':')
                .with_context(|| format!("voter address {address} has no port"))?;
            Ok(format!("{LISTEN_ADDR}:{port}"))
        })
        .collect()
}

/// Prove both of a voter's listeners accept before it announces readiness.
///
/// Readiness used to depend on nothing but the local database, so a voter
/// whose listener lost its port still wrote `Response::Ready`. Connecting to
/// each address is the positive half of the proof — it catches a listener that
/// is simply absent. The negative half is [`install_bind_failure_guard`],
/// which is what separates "my listener is up" from "somebody else's listener
/// is up on my port", because a squatter accepts connections too.
async fn prove_listeners_bound(addresses: &[String]) -> Result<()> {
    for address in addresses {
        let deadline = TokioInstant::now() + LISTENER_PROOF_TIMEOUT;
        loop {
            if let Some(failure) = BIND_FAILURE.get() {
                bail!("{failure}");
            }
            match tokio::net::TcpStream::connect(address).await {
                Ok(_) => break,
                Err(error) if TokioInstant::now() >= deadline => {
                    return Err(anyhow!(error)).with_context(|| {
                        format!("voter listener {address} never accepted a connection")
                    });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        // A successful connection means *something* is listening on the port,
        // but it may be a squatter rather than our own listener. Try to bind
        // the same address to check whether the port is actually free.
        // If we can bind, our listener never bound — the port was taken by
        // another process between allocation and hiqlite's bind attempt.
        // hiqlite's bind failure panics in a spawned task where the panic
        // hook does not fire, so BIND_FAILURE would not be set. This check
        // catches that case.
        let probe = std::net::TcpListener::bind(address);
        if let Ok(listener) = probe {
            // The port is free — our listener never bound. Release the probe
            // and report the collision so the controller can retry.
            drop(listener);
            bail!(
                "{PORT_COLLISION}: voter listener {address} was never bound; the port was taken between allocation and bind"
            );
        }
        // EADDRINUSE means someone has the port — either our listener or a
        // squatter. Check BIND_FAILURE in case the panic guard caught it.
        if let Some(failure) = BIND_FAILURE.get() {
            bail!("{failure}");
        }
    }
    Ok(())
}

pub fn unix_now() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voter_config_uses_the_production_wal_size() {
        let root = tempfile::tempdir().expect("config test root");
        let launch = NodeLaunch {
            node_id: 1,
            root: root.path().to_path_buf(),
            nodes: vec![NodeSpec {
                id: 1,
                raft: "127.0.0.1:19001".to_owned(),
                api: "127.0.0.1:19002".to_owned(),
            }],
        };

        let config = node_config(&launch).expect("build the voter config");
        assert_eq!(config.wal_size, HIQLITE_WAL_SIZE_BYTES);
    }
}

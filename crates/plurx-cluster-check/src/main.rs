//! Separate-process M1b/M1c/M1d cluster validation.
//!
//! hiqlite owns process-global listener and shutdown state, so an in-process
//! three-client test cannot prove process loss. The controller starts this
//! executable three more times and drives each embedded client over a tiny
//! stdin/stdout protocol.

use std::borrow::Cow;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use hiqlite::tls::ServerTlsConfig;
use hiqlite::{Client, Node, NodeConfig};
use plurx_core::domain::{
    ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem, NewLibrary, NewOfflinePackage,
    OfflineCreateOutcome, OfflineLeaseOutcome, ProbeResult, TraktAuth,
};
use plurx_core::store::{
    ApiKeyStore, ClusterCompatibility, HiqliteAuthStore, LibraryStore, MediaStore,
    OfflinePackageStore, ReconcileOutcome, RootFingerprintStatus, SettingsStore, TraktStore,
    TranscodeCacheStore, UserStore, WatchStore, WatchedOutboxStore, AUTH_PROTOCOL_VERSION,
    AUTH_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const RAFT_SECRET: &str = "plurx-m1b-raft-secret";
const API_SECRET: &str = "plurx-m1b-api-secret";
const INSTANCE_ID: &str = "m1b-cluster-check";
const START_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        None | Some("check") => controller().await,
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

async fn controller() -> Result<()> {
    println!("cluster-check: follower loss and incompatible-voter guard");
    run_failure_case(FailureTarget::Follower).await?;
    println!("cluster-check: leader loss");
    run_failure_case(FailureTarget::Leader).await?;
    println!("cluster-check: all M1b/M1c/M1d failure contracts passed");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum FailureTarget {
    Leader,
    Follower,
}

async fn run_failure_case(target: FailureTarget) -> Result<()> {
    let root = tempfile::tempdir().context("cluster-check data root")?;
    let specs = allocate_nodes(3)?;
    let mut cluster = ClusterProcesses::start(root.path(), specs.clone()).await?;

    cluster.request(1, Request::Bootstrap).await?.require_ok()?;
    for node_id in 2..=3 {
        cluster
            .request(node_id, Request::Open)
            .await?
            .require_ok()?;
    }
    cluster.wait_for_three_voters().await?;

    for ordinal in 1..=3 {
        cluster
            .request(ordinal, Request::Exercise { ordinal })
            .await?
            .require_ok()?;
    }
    cluster.wait_for_equal_dumps().await?;
    let catalog = cluster.wait_for_equal_catalog_views().await?;
    if matches!(target, FailureTarget::Follower) {
        prove_local_fts_rebuild(root.path(), &mut cluster, &catalog).await?;
    }

    let leader = cluster.leader().await?;
    let target_id = match target {
        FailureTarget::Leader => leader,
        FailureTarget::Follower => (1..=3)
            .find(|node_id| *node_id != leader)
            .context("choose follower")?,
    };
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
                target: format!("{target:?}").to_ascii_lowercase(),
            },
        )
        .await?
        .require_ok()?;

    if matches!(target, FailureTarget::Follower) {
        let refused = run_incompatible_preflight(&specs).await?;
        if !refused.contains("incompatible with voter schema") {
            bail!("old-schema voter was not refused: {refused}");
        }
        let current_leader = cluster.leader().await?;
        if current_leader == target_id {
            bail!("refused voter {target_id} became leader");
        }
    }

    // One more loss removes quorum. The remaining embedded process is alive,
    // but its Store ping must fail rather than advertise readiness.
    let second_loss = (1..=3)
        .find(|node_id| *node_id != target_id && *node_id != survivor)
        .context("choose second loss")?;
    cluster.kill(second_loss).await?;
    let response = cluster.request(survivor, Request::Ping).await?;
    if !matches!(response, Response::Error { .. }) {
        bail!("one voter reported ready without quorum: {response:?}");
    }

    cluster.kill_all().await;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeSpec {
    id: u64,
    raft: String,
    api: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeLaunch {
    node_id: u64,
    root: PathBuf,
    nodes: Vec<NodeSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Preflight {
    addresses: Vec<String>,
    compatibility: ClusterCompatibility,
}

#[derive(Debug, Serialize, Deserialize)]
enum Request {
    Bootstrap,
    Open,
    Exercise { ordinal: u64 },
    PostLossWrite { target: String },
    VerifyProof,
    Dump,
    CatalogView,
    Metrics,
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Ready {
        node_id: u64,
    },
    Ok,
    Dump {
        digest: String,
    },
    CatalogView {
        view: CatalogView,
    },
    Metrics {
        leader: Option<u64>,
        voters: Vec<u64>,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CatalogView {
    libraries: Vec<String>,
    browse: Vec<(i64, Vec<i64>)>,
    search: Vec<i64>,
}

impl Response {
    fn require_ok(self) -> Result<()> {
        match self {
            Self::Ok => Ok(()),
            Self::Error { message } => Err(anyhow!(message)),
            other => bail!("expected OK response, got {other:?}"),
        }
    }
}

struct NodeProcess {
    id: u64,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl NodeProcess {
    fn spawn(executable: &Path, launch: &NodeLaunch) -> Result<Self> {
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

    async fn wait_ready(&mut self) -> Result<()> {
        match self.read_response(START_TIMEOUT).await? {
            Response::Ready { node_id } if node_id == self.id => Ok(()),
            response => bail!("voter {} failed startup: {response:?}", self.id),
        }
    }

    async fn request(&mut self, request: &Request) -> Result<Response> {
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

    async fn kill(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill().await?;
            let _ = self.child.wait().await;
        }
        Ok(())
    }
}

struct ClusterProcesses {
    nodes: Vec<Option<NodeProcess>>,
}

impl ClusterProcesses {
    async fn start(root: &Path, specs: Vec<NodeSpec>) -> Result<Self> {
        let executable = std::env::current_exe().context("cluster-check executable")?;
        let mut nodes = Vec::with_capacity(specs.len());
        for node_id in 1..=specs.len() as u64 {
            let launch = NodeLaunch {
                node_id,
                root: root.to_path_buf(),
                nodes: specs.clone(),
            };
            nodes.push(Some(NodeProcess::spawn(&executable, &launch)?));
        }
        let mut cluster = Self { nodes };
        for node_id in 1..=specs.len() as u64 {
            cluster.node_mut(node_id)?.wait_ready().await?;
        }
        Ok(cluster)
    }

    fn node_mut(&mut self, node_id: u64) -> Result<&mut NodeProcess> {
        self.nodes
            .get_mut((node_id - 1) as usize)
            .and_then(Option::as_mut)
            .with_context(|| format!("voter {node_id} is not running"))
    }

    async fn request(&mut self, node_id: u64, request: Request) -> Result<Response> {
        self.node_mut(node_id)?.request(&request).await
    }

    async fn kill(&mut self, node_id: u64) -> Result<()> {
        let slot = self
            .nodes
            .get_mut((node_id - 1) as usize)
            .with_context(|| format!("unknown voter {node_id}"))?;
        if let Some(mut node) = slot.take() {
            node.kill().await?;
        }
        Ok(())
    }

    async fn kill_all(&mut self) {
        for node in &mut self.nodes {
            if let Some(node) = node.as_mut() {
                let _ = node.kill().await;
            }
            *node = None;
        }
    }

    async fn leader(&mut self) -> Result<u64> {
        let deadline = Instant::now() + Duration::from_secs(15);
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
                    return Ok(leader);
                }
            }
            if Instant::now() >= deadline {
                bail!("cluster did not report a leader");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_for_three_voters(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Response::Metrics { voters, .. }) = self.request(1, Request::Metrics).await {
                if voters == vec![1, 2, 3] {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!("cluster did not converge to three voters");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_for_ready(&mut self, node_id: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
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

    async fn wait_for_equal_dumps(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut dumps = Vec::new();
            for node_id in 1..=self.nodes.len() as u64 {
                match self.request(node_id, Request::Dump).await? {
                    Response::Dump { digest } => dumps.push(digest),
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

    async fn wait_for_equal_catalog_views(&mut self) -> Result<CatalogView> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut views = Vec::new();
            for node_id in 1..=self.nodes.len() as u64 {
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

async fn prove_local_fts_rebuild(
    root: &Path,
    cluster: &mut ClusterProcesses,
    baseline: &CatalogView,
) -> Result<()> {
    let path = root
        .join("node-2")
        .join("state_machine")
        .join("db")
        .join("auth.db");
    let connection = rusqlite::Connection::open(&path)
        .with_context(|| format!("open voter-2 local database at {}", path.display()))?;
    connection
        .execute("DELETE FROM items_fts", [])
        .context("delete voter-2 derived FTS rows")?;
    let empty = match cluster.request(2, Request::CatalogView).await? {
        Response::CatalogView { view } => view,
        response => bail!("unexpected post-delete catalog response: {response:?}"),
    };
    if empty.browse != baseline.browse || !empty.search.is_empty() {
        bail!("deleting voter-2 FTS changed browse truth or left search rows");
    }
    connection
        .execute("INSERT INTO items_fts(items_fts) VALUES('rebuild')", [])
        .context("rebuild voter-2 derived FTS rows")?;
    let rebuilt = cluster.wait_for_equal_catalog_views().await?;
    if &rebuilt != baseline {
        bail!("voter-2 FTS rebuild did not restore the baseline view");
    }
    Ok(())
}

async fn node(launch: NodeLaunch) -> Result<()> {
    install_crypto_provider();
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;
    let client = hiqlite::start_node(node_config(&launch)?)
        .await
        .context("start hiqlite voter")?;
    tokio::time::timeout(START_TIMEOUT, client.wait_until_healthy_db())
        .await
        .context("voter health timed out")?;

    write_response(&Response::Ready {
        node_id: launch.node_id,
    })
    .await?;

    let mut store: Option<HiqliteAuthStore> = None;
    let stdin = tokio::io::stdin();
    let mut input = BufReader::new(stdin).lines();
    while let Some(line) = input.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle_request(request, &client, &mut store).await,
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
    store: &mut Option<HiqliteAuthStore>,
) -> Result<Response> {
    match request {
        Request::Bootstrap => {
            *store = Some(HiqliteAuthStore::bootstrap(client.clone(), INSTANCE_ID).await?);
            Ok(Response::Ok)
        }
        Request::Open => {
            *store = Some(HiqliteAuthStore::open(client.clone()).await?);
            Ok(Response::Ok)
        }
        Request::Exercise { ordinal } => {
            exercise(store_ref(store)?, ordinal).await?;
            Ok(Response::Ok)
        }
        Request::PostLossWrite { target } => {
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
            Ok(Response::Ok)
        }
        Request::VerifyProof => {
            verify_proof(store_ref(store)?).await?;
            Ok(Response::Ok)
        }
        Request::Dump => Ok(Response::Dump {
            digest: store_ref(store)?.local_dump_digest().await?,
        }),
        Request::CatalogView => Ok(Response::CatalogView {
            view: catalog_view(store_ref(store)?).await?,
        }),
        Request::Metrics => {
            let metrics = client.metrics_db().await?;
            let mut voters = metrics.membership_config.voter_ids().collect::<Vec<_>>();
            voters.sort_unstable();
            Ok(Response::Metrics {
                leader: metrics.current_leader,
                voters,
            })
        }
        Request::Ping => {
            store_ref(store)?.ping().await?;
            Ok(Response::Ok)
        }
    }
}

fn store_ref(store: &Option<HiqliteAuthStore>) -> Result<&HiqliteAuthStore> {
    store.as_ref().context("auth store has not been opened")
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
    let user = store
        .create_user(&username, "hash-v1", true)
        .await
        .context("create proof user")?;
    if store.get_user(user.id).await?.map(|row| row.id) != Some(user.id) {
        bail!("user id lookup failed");
    }
    if store
        .get_user_by_username(&username.to_ascii_uppercase())
        .await?
        .map(|row| row.id)
        != Some(user.id)
    {
        bail!("case-insensitive username lookup failed");
    }
    let password_changed = store.set_password(user.id, "hash-v2").await?;
    let password_after = store.get_user(user.id).await?.map(|row| row.password_hash);
    if !password_changed || password_after.as_deref() != Some("hash-v2") {
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
    if store.api_key_for_hash(&key_hash).await?.map(|row| row.id) != Some(key.id)
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
        .ensure_library_root_fingerprint(library.id, &fingerprint)
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
    let state = store
        .put_progress(user.id, movie, 30_000, Some(10_000))
        .await?;
    if state.position_ms != 30_000 || state.duration_ms != Some(120_000) || state.watched {
        bail!("replicated watch progress did not prefer the probed duration");
    }

    let trakt = TraktAuth {
        user_id: user.id,
        access_token: format!("trakt-access-{suffix}"),
        refresh_token: format!("trakt-refresh-{suffix}"),
        expires_at: 4_000_000_000,
        trakt_username: Some(format!("trakt-{suffix}")),
        connected_at: 1_700_000_000 + ordinal as i64,
        last_sync_at: 0,
        last_activities: None,
    };
    store.put_trakt_auth(&trakt).await?;
    store
        .set_trakt_sync(user.id, 1_700_000_100 + ordinal as i64, Some("{}"))
        .await?;
    store
        .update_trakt_tokens(
            user.id,
            &format!("trakt-access-new-{suffix}"),
            &format!("trakt-refresh-new-{suffix}"),
            4_000_000_001,
        )
        .await?;
    if store
        .get_trakt_auth(user.id)
        .await?
        .is_none_or(|auth| auth.access_token != format!("trakt-access-new-{suffix}"))
        || !store
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
    let unlink_user = store
        .create_user(&format!("trakt-unlink-{suffix}"), "hash", false)
        .await?;
    let mut unlink = trakt.clone();
    unlink.user_id = unlink_user.id;
    store.put_trakt_auth(&unlink).await?;
    store.delete_trakt_auth(unlink_user.id).await?;
    if store.get_trakt_auth(unlink_user.id).await?.is_some() {
        bail!("replicated Trakt unlink failed");
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
    store.touch_cache_claim(&abandoned, &node_id).await?;
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

    let offline = NewOfflinePackage {
        id: format!("offline-{suffix}"),
        request_id: format!("offline-request-{suffix}"),
        user_id: user.id,
        file_id: file,
        node_id: node_id.clone(),
        source_path: format!("/cluster/media/{suffix}/proof-{suffix}.mkv"),
        source_size: 1_000 + ordinal as i64,
        source_mtime: 1_700_000_000 + ordinal as i64,
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
        .claim_next_offline_package(&node_id)
        .await?
        .is_none_or(|package| package.id != offline.id)
        || !store
            .set_offline_package_recipe(&offline.id, &recipe_hash)
            .await?
        || !store
            .update_offline_progress(&offline.id, "video", 500)
            .await?
        || !store
            .mark_offline_package_ready(&offline.id, &recipe_hash, 800, 120_000)
            .await?
    {
        bail!("replicated offline preparation state machine failed");
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
        || store.cache_bytes(&node_id).await? != 0
    {
        bail!("replicated offline lease/pinned-cache accounting failed");
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
        || !store.requeue_offline_package(&work.id).await?
        || store
            .claim_next_offline_package(&node_id)
            .await?
            .is_none_or(|package| package.id != work.id)
        || !store
            .fail_offline_package(&work.id, "video", "proof", "expected")
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
    Ok(())
}

async fn catalog_view(store: &HiqliteAuthStore) -> Result<CatalogView> {
    let libraries = store.list_libraries().await?;
    let mut browse = Vec::with_capacity(libraries.len());
    for library in &libraries {
        let page = store
            .list_top_items(library.id, ItemSort::Title, 0, 100)
            .await?;
        browse.push((
            library.id,
            page.items.into_iter().map(|item| item.id).collect(),
        ));
    }
    let mut search = store
        .search_items("replicated browse", 100)
        .await?
        .into_iter()
        .map(|row| row.item.id)
        .collect::<Vec<_>>();
    search.sort_unstable();
    Ok(CatalogView {
        libraries: libraries.into_iter().map(|library| library.name).collect(),
        browse,
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
        if store.get_trakt_auth(user.id).await?.is_none()
            || store
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
        {
            bail!("lost acknowledged Trakt/cache/offline state from node {ordinal}");
        }
    }
    let (_, ok, _) = store.watched_outbox_counts().await?;
    if ok != 3 {
        bail!("lost acknowledged watched-outbox rows after voter loss: ok={ok}");
    }
    if catalog_view(store).await?.search.len() != 3 {
        bail!("lost local FTS search rows after voter loss");
    }
    Ok(())
}

async fn write_response(response: &Response) -> Result<()> {
    let mut output = tokio::io::stdout();
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

async fn preflight_voter(preflight: Preflight) -> Result<()> {
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

async fn run_incompatible_preflight(specs: &[NodeSpec]) -> Result<String> {
    let executable = std::env::current_exe().context("cluster-check executable")?;
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

fn node_config(launch: &NodeLaunch) -> Result<NodeConfig> {
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
        listen_addr_api: Cow::Borrowed("127.0.0.1"),
        listen_addr_raft: Cow::Borrowed("127.0.0.1"),
        data_dir: Cow::Owned(data_dir.to_string_lossy().into_owned()),
        filename_db: Cow::Borrowed("auth.db"),
        secret_raft: RAFT_SECRET.to_owned(),
        secret_api: API_SECRET.to_owned(),
        tls_raft: Some(ServerTlsConfig::TlsAutoCertificates),
        tls_api: Some(ServerTlsConfig::TlsAutoCertificates),
        health_check_delay_secs: 0,
        wal_size: 2 * 1024 * 1024,
        raft_config: NodeConfig::default_raft_config(10_000),
        ..Default::default()
    })
}

fn allocate_nodes(count: u64) -> Result<Vec<NodeSpec>> {
    (1..=count)
        .map(|id| {
            Ok(NodeSpec {
                id,
                raft: format!("127.0.0.1:{}", free_port()?),
                api: format!("127.0.0.1:{}", free_port()?),
            })
        })
        .collect()
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

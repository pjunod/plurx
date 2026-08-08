//! Separate-process M1b cluster validation.
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
use plurx_core::store::{
    ApiKeyStore, ClusterCompatibility, HiqliteAuthStore, SettingsStore, UserStore,
    AUTH_PROTOCOL_VERSION, AUTH_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const RAFT_SECRET: &str = "plurx-m1b-raft-secret";
const API_SECRET: &str = "plurx-m1b-api-secret";
const INSTANCE_ID: &str = "m1b-cluster-check";
const START_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(45);

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
    println!("cluster-check: all M1b failure contracts passed");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum FailureTarget {
    Leader,
    Follower,
}

async fn run_failure_case(target: FailureTarget) -> Result<()> {
    let root = tempfile::tempdir().context("cluster-check data root")?;
    let (mut cluster, specs) = start_cluster_with_port_retry(root.path()).await?;

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
    cluster.wait_for_three_voters().await?;

    for ordinal in 1..=3 {
        cluster
            .request(ordinal, Request::Exercise { ordinal })
            .await?
            .require_ok()?;
    }
    cluster.wait_for_equal_dumps().await?;

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
    for request in [Request::ReadWithoutQuorum, Request::WriteWithoutQuorum] {
        let response = cluster.request(survivor, request).await?;
        if !matches!(response, Response::Error { .. }) {
            bail!("ordinary store operation succeeded without quorum: {response:?}");
        }
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
    RejectIdentityDrift,
    Open,
    Exercise { ordinal: u64 },
    PostLossWrite { target: String },
    VerifyProof,
    Dump,
    Metrics,
    Ping,
    ReadWithoutQuorum,
    WriteWithoutQuorum,
}

#[derive(Debug, Serialize, Deserialize)]
enum Response {
    Ready {
        node_id: u64,
    },
    Ok,
    Dump {
        digest: String,
        dump: String,
    },
    Metrics {
        leader: Option<u64>,
        voters: Vec<u64>,
    },
    Error {
        message: String,
    },
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
        let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
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

    async fn wait_for_three_voters(&mut self) -> Result<()> {
        let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
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
        let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
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
        let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
        loop {
            let mut dumps = Vec::new();
            for node_id in 1..=self.nodes.len() as u64 {
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
}

async fn start_cluster_with_port_retry(root: &Path) -> Result<(ClusterProcesses, Vec<NodeSpec>)> {
    let mut last_error = None;
    for attempt in 1..=5 {
        let specs = allocate_nodes(3)?;
        let attempt_root = root.join(format!("attempt-{attempt}"));
        match ClusterProcesses::start(&attempt_root, specs.clone()).await {
            Ok(cluster) => return Ok((cluster, specs)),
            Err(error) if format!("{error:#}").contains("Address already in use") => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.context("cluster ports stayed occupied across five allocations")?)
}

fn validate_known_dump(dump: &serde_json::Value) -> Result<()> {
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

async fn node(launch: NodeLaunch) -> Result<()> {
    install_crypto_provider();
    let _ = ServerTlsConfig::server_config_self_signed("127.0.0.1").await;
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
        Request::RejectIdentityDrift => {
            match HiqliteAuthStore::bootstrap(client.clone(), "wrong-instance-id").await {
                Err(error) if error.to_string().contains("refusing bootstrap") => Ok(Response::Ok),
                Err(error) => bail!("identity drift failed for the wrong reason: {error}"),
                Ok(_) => bail!("bootstrap overwrote the immutable cluster identity"),
            }
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
        Request::Dump => {
            let store = store_ref(store)?;
            Ok(Response::Dump {
                digest: store.local_dump_digest().await?,
                dump: store.validation_local_dump().await?,
            })
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
    Ok(())
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

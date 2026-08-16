mod admission;
mod cachekeep;
mod copyseg;
mod delivery;
mod ffmpeg;
mod http;
mod logbuf;
mod meter;
mod offline;
mod pgs_overlay;
mod pipeprobe;
mod playstart;
mod produce;
mod progress;
mod progressive;
mod schedule;
mod state;
mod storeprobe;
mod subtitles;
mod telemetry;
mod trakt;
mod transcode;
mod version;
mod watched;

use std::future::IntoFuture;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use plurx_core::cluster::migration::{
    connect_activated_store, select_daemon_store, SelectedBackend,
};
use plurx_core::config::Config;
use plurx_core::domain::LibraryKind;
use plurx_core::metadata::{self, AniListClient, TmdbClient};
#[cfg(test)]
use plurx_core::store::SqliteStore;
use plurx_core::store::{keys, Store};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use crate::state::{AppState, SystemInfo};

#[derive(Parser)]
// `--version` carries the build stamp too: "0.1.0 (v0.1.0-14-gc0ffee)". The
// bare number is what a release *is*; the git description is what someone
// filing a bug is actually running.
#[command(
    name = "plurxd",
    version = crate::version::LONG,
    about = "plurx media server daemon"
)]
struct Cli {
    /// Path to a TOML config file (default: ./plurx.toml or
    /// /etc/plurx/plurx.toml if present).
    #[arg(long, global = true, env = "PLURX_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (the default when no subcommand is given).
    Run,
    /// Probe a running local server's /healthz and exit 0/1 (container
    /// health checks: no curl needed in the image).
    Healthcheck,
    /// Advertise a bridge-networked server on the host's Bonjour interfaces.
    /// This is intended for the discovery companion container; the HTTP server
    /// remains attached to its normal Docker networks.
    Advertise {
        /// Base URL where the server is published on the Docker host.
        #[arg(
            long,
            env = "PLURX_DISCOVERY_SERVER_URL",
            default_value = "http://127.0.0.1:32400"
        )]
        server: String,
    },
    /// Reset a user's password in the activated store — the recovery path
    /// when an admin password is forgotten (admins reset *other* users in
    /// the web UI). Requires the server running, because it shares that
    /// daemon's voter; revokes the user's sessions. In Docker:
    /// docker exec -it plurxd plurxd reset-password NAME
    ResetPassword {
        /// Username whose password to reset.
        username: String,
        /// New password (min 8 chars). Omit to be prompted on stdin, which
        /// keeps it out of your shell history.
        #[arg(long)]
        password: Option<String>,
    },
    /// Force provider metadata and artwork through the activated store. Useful
    /// after an artwork-quality upgrade; runs beside the server and requires
    /// it running, because it shares that daemon's voter.
    RefreshMetadata {
        /// Refresh one library id instead of every provider-backed library.
        #[arg(long)]
        library: Option<i64>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref()).context("loading configuration")?;
    dispatch(cli.command.unwrap_or(Command::Run), config).await
}

/// Route a parsed command, separated from `main` so every subcommand but the
/// server itself is reachable without a process launch.
async fn dispatch(command: Command, config: Config) -> anyhow::Result<()> {
    match command {
        Command::Run => run(config).await,
        Command::Healthcheck => {
            // One terse line either way — this output lands in `docker inspect`.
            if let Err(error) = healthcheck(&config) {
                eprintln!("unhealthy: {error:#}");
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Advertise { server } => advertise(&server).await,
        Command::ResetPassword { username, password } => {
            reset_password(&config, &username, password).await
        }
        Command::RefreshMetadata { library } => refresh_metadata(&config, library).await,
    }
}

/// Re-fetch provider-backed metadata through the activated replicated store.
/// The command refuses a legacy-only data directory because only `run` owns
/// the one-time import and activation sequence.
async fn refresh_metadata(config: &Config, library_id: Option<i64>) -> anyhow::Result<()> {
    let store = connect_activated_store(config).await.context(
        "connecting to the activated store: an unmigrated data directory needs \
             `plurxd run` to import it once, and an activated one needs that daemon \
             running because maintenance commands share its voter rather than \
             opening a second store",
    )?;
    let artwork_dir = config.storage.data_dir.join("artwork");
    std::fs::create_dir_all(&artwork_dir)?;
    refresh_metadata_with_store(store, &artwork_dir, library_id).await
}

async fn refresh_metadata_with_store(
    store: Arc<dyn Store>,
    artwork_dir: &std::path::Path,
    library_id: Option<i64>,
) -> anyhow::Result<()> {
    let libraries = store.list_libraries().await?;

    if let Some(id) = library_id {
        anyhow::ensure!(
            libraries.iter().any(|library| library.id == id),
            "no library with id {id}"
        );
    }

    let tmdb_key = store.get_setting(keys::TMDB_API_KEY).await?;
    for library in libraries
        .into_iter()
        .filter(|library| library_id.is_none_or(|id| library.id == id))
    {
        match library.kind {
            LibraryKind::Books | LibraryKind::Home => {
                println!("{}: skipped provider artwork", library.name);
            }
            LibraryKind::Shows if library.anime => {
                let report = metadata::enrich_anime_library(
                    store.as_ref(),
                    &AniListClient::new(),
                    artwork_dir,
                    library.id,
                    true,
                    None,
                )
                .await;
                println!("{}: {}", library.name, serde_json::to_string(&report)?);
            }
            LibraryKind::Movies | LibraryKind::Shows => {
                let key = tmdb_key
                    .as_deref()
                    .filter(|key| !key.is_empty())
                    .context("TMDB API key is not configured")?;
                let report = metadata::enrich_library(
                    store.as_ref(),
                    &TmdbClient::new(key),
                    artwork_dir,
                    Some(library.id),
                    true,
                    None,
                )
                .await;
                println!("{}: {}", library.name, serde_json::to_string(&report)?);
            }
        }
    }
    Ok(())
}

/// Console recovery path: rewrite one user's password hash and revoke their
/// sessions through the activated replicated store.
async fn reset_password(
    config: &Config,
    username: &str,
    password: Option<String>,
) -> anyhow::Result<()> {
    let password = match password {
        Some(p) => p,
        None => {
            eprint!("New password for `{username}` (min 8 chars): ");
            read_password(&mut std::io::stdin().lock())?
        }
    };
    anyhow::ensure!(
        password.len() >= 8,
        "password must be at least 8 characters"
    );

    let store = connect_activated_store(config).await.context(
        "connecting to the activated store: an unmigrated data directory needs \
             `plurxd run` to import it once, and an activated one needs that daemon \
             running because maintenance commands share its voter rather than \
             opening a second store",
    )?;
    reset_password_in_store(store.as_ref(), username, &password).await
}

async fn reset_password_in_store(
    store: &dyn Store,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let user = store
        .get_user_by_username(username)
        .await?
        .with_context(|| format!("no user named `{username}`"))?;
    let hash =
        plurx_core::auth::hash_password(password).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    store.set_password(user.id, &hash).await?;
    let revoked = store.delete_tokens_for_user(user.id).await?;
    println!("password reset for `{username}`; {revoked} session(s) revoked");
    Ok(())
}

/// One line of typed password, with the line ending removed.
///
/// Only the trailing CR/LF goes: a password may legitimately start or end with
/// a space, and trimming it would leave the operator with an account whose
/// password is not the one they typed and no way to find out why.
fn read_password(input: &mut impl std::io::BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

async fn run(config: Config) -> anyhow::Result<()> {
    let logs = init_logging();
    // Signal streams must exist before store activation, system probing, or
    // any listener can make this process externally reachable. The returned
    // future owns them until `boot` begins polling the graceful drain.
    let shutdown = shutdown_signal();

    let selected = select_daemon_store(&config)
        .await
        .with_context(|| format!("selecting store in {}", config.storage.data_dir.display()))?;
    // Which backend is serving is not otherwise observable. A recovery boot
    // binds and serves normally on unreplicated SQLite, so a persistently
    // failing activation otherwise looks only like a flapping container, with
    // nothing saying which boots were replicated and which were not.
    match selected.backend {
        SelectedBackend::Replicated => {
            tracing::info!("durable state: one-voter replicated store");
        }
        SelectedBackend::SqliteRecovery => tracing::warn!(
            "durable state: unreplicated SQLite, recovering from an interrupted activation; \
             this boot is not replicated and the next restart retries the import"
        ),
    }
    let serving = async {
        let store = Arc::clone(&selected.store);
        let replication = selected.replication_monitor();
        let dirs = create_dirs(&config.storage.data_dir)?;
        let (encoder_caps, system) = probe_system(&config, &store, &dirs.transcode).await?;
        let parts = Boot {
            store,
            replication,
            identity: selected.identity.clone(),
            credential_key: Arc::clone(&selected.credential_key),
            dirs,
            encoder_caps,
            system,
            logs,
        };
        boot(config, parts, start_mdns_advertiser, shutdown).await
    };
    let result = serving.await;
    let shutdown = selected
        .shutdown()
        .await
        .context("stopping local Hiqlite voter");
    result.and(shutdown)
}

/// What a measured node hands to the server it is about to become.
struct Boot {
    store: Arc<dyn plurx_core::store::Store>,
    replication: plurx_core::cluster::migration::status::ReplicationMonitor,
    identity: plurx_core::cluster::ClusterIdentity,
    /// Resolved before the store is handed on, so a node that cannot open its
    /// existing Trakt rows fails here rather than at the first sync.
    credential_key: Arc<plurx_core::secrets::CredentialKey>,
    dirs: crate::state::Dirs,
    encoder_caps: plurx_core::transcode::EncoderCaps,
    system: SystemInfo,
    logs: Arc<logbuf::LogBuffer>,
}

/// Everything from a measured node to a served, drained shutdown.
///
/// Separated from [`run`] at exactly the line where startup stops reaching
/// outside the process: above it are the store and the ffmpeg probes, below it
/// is wiring. Taking the shutdown future and the advertiser rather than
/// installing a signal handler and registering on the LAN is what lets the
/// whole sequence run in a test, on a temporary directory and a loopback port.
async fn boot(
    config: Config,
    parts: Boot,
    advertiser: MdnsAdvertiser,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let Boot {
        store,
        replication,
        identity,
        credential_key,
        dirs,
        encoder_caps,
        system,
        logs,
    } = parts;
    log_startup(&config, &identity);

    let instance_id = identity.cluster_id;
    let state = build_state(
        &config,
        identity.node_id,
        credential_key,
        replication,
        store,
        dirs,
        encoder_caps,
        system,
        logs,
    );
    // The stored request is not an ffmpeg contract. Exercise the exact
    // production rate-control arguments against this boot's real drivers and
    // publish only the effective result before any session can start.
    state.transcode.initialize_rate_control().await?;
    spawn_background_loops(&state);

    let progress = Arc::clone(&state.progress);
    let app = http::router(state);
    let listener = bind_listener(config.server.bind).await?;
    trigger_shutdown_registration_failpoint();
    let mdns = start_discovery(&config, &instance_id, advertiser);

    serve(listener, app, progress, mdns, shutdown).await
}

/// How a Bonjour record gets published. A parameter rather than a direct call
/// because registering one puts a service on the local network, which a test
/// must not do to check that discovery is best-effort.
type MdnsAdvertiser = fn(&str, &str, SocketAddr, &str) -> anyhow::Result<mdns_sd::ServiceDaemon>;

/// Console logging plus a bounded in-memory ring the admin UI can read.
///
/// The EnvFilter is global, so both sinks see the same events. `try_init`
/// rather than `init` because a second call is not a reason to abort a boot:
/// the daemon installs exactly one subscriber, and losing that race could only
/// ever mean logging is already going somewhere.
fn init_logging() -> Arc<logbuf::LogBuffer> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let logs = Arc::new(logbuf::LogBuffer::default());
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("PLURX_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .with(logbuf::BufferLayer(Arc::clone(&logs)))
        .try_init()
        .ok();
    logs
}

/// The one line that says what is running and where its data is.
///
/// Both identities are named because they are different things that a cluster
/// makes it possible to confuse: `instance_id` is the logical server clients
/// bind to, `node_id` is the process that owns the local bytes.
fn log_startup(config: &Config, identity: &plurx_core::cluster::ClusterIdentity) {
    tracing::info!(
        version = crate::version::SEMVER,
        build = crate::version::BUILD,
        server_name = %config.server.name,
        instance_id = %identity.cluster_id,
        node_id = %identity.node_id,
        data_dir = %config.storage.data_dir.display(),
        "plurxd starting"
    );
}

/// Bind the HTTP listener, naming the address when it cannot be had — the
/// common boot failure is a port already in use, and "Address already in use"
/// alone does not say which one.
async fn bind_listener(bind: SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    Ok(listener)
}

/// Start LAN discovery for a server other devices can actually reach.
///
/// A loopback-only bind is common in proxy deployments, and publishing it would
/// produce a tempting setup button in every client on the network that can
/// never connect — so nothing is advertised at all in that case. Both
/// protocols are best-effort past that point: failing to join multicast must
/// not take down the HTTP server.
fn start_discovery(
    config: &Config,
    instance_id: &str,
    advertiser: MdnsAdvertiser,
) -> Option<mdns_sd::ServiceDaemon> {
    let lan_discovery = !config.server.bind.ip().is_loopback();
    if let Some(gdm_port) = gdm_responder_port(lan_discovery, config.server.bind) {
        // GDM keeps direct-connect Plex clients working. Bonjour is the native
        // plurx contract used by the Apple apps.
        spawn_gdm_responder(config, instance_id.to_owned(), gdm_port);
    }
    start_bonjour(config, instance_id, advertiser, lan_discovery)
}

/// The native plurx discovery contract, which the Apple clients browse for.
///
/// Best-effort: a node that cannot register still serves HTTP, and clients
/// reach it by address instead. Compose deployments turn the in-process
/// advertiser off and run `plurxd advertise` in a host-network companion,
/// because multicast cannot cross a bridge.
fn start_bonjour(
    config: &Config,
    instance_id: &str,
    advertiser: MdnsAdvertiser,
    lan_discovery: bool,
) -> Option<mdns_sd::ServiceDaemon> {
    if !lan_discovery {
        return None;
    }
    if !mdns_advertising_enabled() {
        tracing::info!("in-process Bonjour advertising disabled");
        return None;
    }
    let discovery_name = discovery_display_name(
        &config.server.name,
        system_hostname().as_deref(),
        primary_lan_address().as_ref(),
    );
    match advertiser(
        instance_id,
        &discovery_name,
        config.server.bind,
        crate::version::SEMVER,
    ) {
        Ok(daemon) => Some(daemon),
        Err(error) => {
            tracing::warn!(%error, "Bonjour discovery advertiser unavailable");
            None
        }
    }
}

/// Answer Plex clients' GDM searches for as long as the server runs.
///
/// Best-effort by construction: a box where the multicast join is refused
/// still serves HTTP, and the only trace is one warning.
fn spawn_gdm_responder(config: &Config, instance_id: String, gdm_port: u16) {
    let name = config.server.name.clone();
    let http_port = config.server.bind.port();
    tokio::spawn(async move {
        if let Err(e) = gdm_responder(instance_id, name, http_port, gdm_port).await {
            tracing::warn!(error = %e, "GDM discovery responder unavailable");
        }
    });
}

/// Lay out the data dir, and clear exactly the one directory that must not
/// survive a restart.
///
/// Which of these persists is load-bearing. The session scratch is cleared at
/// every boot, because a half-written segment from a killed process is worse
/// than no segment; the finished-transcode cache and the extracted-subtitle
/// cache are its *siblings* rather than its children precisely so they are not
/// caught by that — a cache that empties on restart is a warm-up cost with none
/// of the benefit.
fn create_dirs(data_dir: &std::path::Path) -> anyhow::Result<crate::state::Dirs> {
    let artwork = data_dir.join("artwork");
    std::fs::create_dir_all(&artwork)
        .with_context(|| format!("creating artwork directory {}", artwork.display()))?;
    let cache = data_dir.join("cache").join("transcode");
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("creating cache directory {}", cache.display()))?;
    let subs = data_dir.join("cache").join("subs");
    std::fs::create_dir_all(&subs)
        .with_context(|| format!("creating subtitle cache {}", subs.display()))?;
    let transcode = data_dir.join("transcode");
    // Clear any stale sessions from a previous run, then recreate.
    let _ = std::fs::remove_dir_all(&transcode);
    std::fs::create_dir_all(&transcode)
        .with_context(|| format!("creating transcode directory {}", transcode.display()))?;
    Ok(crate::state::Dirs {
        artwork,
        transcode,
        cache,
        subs,
    })
}

/// Measure this node's encoders and tone-map graph, and record what was found.
///
/// The subprocess shell of startup: everything here that decides something —
/// which preference the probe runs under, what an env override means — is a
/// tested function it calls.
async fn probe_system(
    config: &Config,
    store: &Arc<dyn plurx_core::store::Store>,
    transcode_dir: &std::path::Path,
) -> anyhow::Result<(plurx_core::transcode::EncoderCaps, SystemInfo)> {
    let ffmpeg = crate::ffmpeg::ffmpeg_bin();
    // Detect available hardware encoders once at startup.
    let encoder_caps = plurx_core::transcode::detect_encoders(&ffmpeg).await;

    let hwaccel_pref = resolve_hwaccel_pref(store).await?;
    let probe_pref = probe_preference(&hwaccel_pref);
    let encoder_selected = encoder_caps.choose(&probe_pref).label().to_owned();
    // Which tone-map graph this node may use. After encoder detection, because
    // a graph is only worth probing if it can feed the encoder that won. Costs
    // a few seconds on a box with a GPU worth testing and nothing at all on one
    // without.
    let tone_map = pipeprobe::probe(transcode_dir, encoder_caps.choose(&probe_pref)).await;
    let measured = Measured {
        ffmpeg_version: ffmpeg_version(&ffmpeg).await,
        pacing: crate::ffmpeg::pacing_caps().await,
        dovi_rpu: crate::ffmpeg::has_dovi_rpu().await,
        encoder_selected,
        tone_map,
    };
    let system = system_info(config, ffmpeg, hwaccel_pref, encoder_caps.clone(), measured);
    Ok((encoder_caps, system))
}

/// What the boot probes learned about this machine's ffmpeg.
struct Measured {
    ffmpeg_version: Option<String>,
    pacing: crate::ffmpeg::PacingCaps,
    dovi_rpu: bool,
    encoder_selected: String,
    tone_map: pipeprobe::PipelineReport,
}

/// Assemble what the settings page and every session read.
///
/// Separated from the probing so the record itself is checkable: the tone-map
/// report and the encoder here are what a session consults, and a `SystemInfo`
/// whose `encoder_selected` disagreed with its `encoders` would be a settings
/// page describing a node nobody is running.
fn system_info(
    config: &Config,
    ffmpeg: String,
    hwaccel_pref: String,
    encoders: plurx_core::transcode::EncoderCaps,
    measured: Measured,
) -> SystemInfo {
    SystemInfo {
        data_dir: config.storage.data_dir.display().to_string(),
        ffmpeg_version: measured.ffmpeg_version,
        ffmpeg,
        ffprobe: crate::ffmpeg::ffprobe_bin(),
        hwaccel_pref,
        encoders,
        encoder_selected: measured.encoder_selected,
        tone_map: measured.tone_map,
        pacing: measured.pacing,
        dovi_rpu: measured.dovi_rpu,
    }
}

/// The encoder preference this boot runs under.
///
/// `PLURX_HWACCEL` (documented since Phase 2, previously read by nothing) seeds
/// the stored setting at boot: env wins over the setting, and then the setting
/// is the single source both this probe and the admin UI read, so the two can
/// never disagree about what the node was told to use.
async fn resolve_hwaccel_pref(store: &Arc<dyn plurx_core::store::Store>) -> anyhow::Result<String> {
    if let Some(pref) = hwaccel_override(std::env::var("PLURX_HWACCEL").ok()) {
        store.put_setting(keys::HWACCEL, &pref).await?;
    }
    Ok(store
        .get_setting(keys::HWACCEL)
        .await?
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "auto".to_owned()))
}

/// What an `PLURX_HWACCEL` value seeds the stored setting with, if anything.
///
/// An empty value is what Compose produces for an unset variable, and storing
/// it would overwrite an operator's chosen encoder with "no preference" on
/// every restart.
fn hwaccel_override(raw: Option<String>) -> Option<String> {
    raw.filter(|v| !v.is_empty())
        .map(|v| v.to_lowercase().trim().to_owned())
}

/// The string the encoder chooser is asked with. "auto" is the absence of a
/// preference, not a preference named "auto" — passed through literally it
/// would match no encoder and silently select the software fallback.
fn probe_preference(hwaccel_pref: &str) -> String {
    if hwaccel_pref == "auto" {
        String::new()
    } else {
        hwaccel_pref.to_owned()
    }
}

#[allow(clippy::too_many_arguments)]
fn build_state(
    config: &Config,
    node_id: String,
    credential_key: Arc<plurx_core::secrets::CredentialKey>,
    replication: plurx_core::cluster::migration::status::ReplicationMonitor,
    store: Arc<dyn plurx_core::store::Store>,
    dirs: crate::state::Dirs,
    encoder_caps: plurx_core::transcode::EncoderCaps,
    system: SystemInfo,
    logs: Arc<logbuf::LogBuffer>,
) -> AppState {
    AppState::new_configured(
        crate::state::AppConfig {
            server_name: config.server.name.clone(),
            node_id,
            scan_prune_percent: config.storage.scan_prune_percent,
            credential_key,
            replication,
        },
        store,
        dirs,
        encoder_caps,
        system,
        logs,
    )
}

/// Every loop that has to keep running whether or not a request arrives.
///
/// A retry scheduled two minutes out has no request to wake it, and a monarr
/// that is down must not stall anything a viewer is waiting on — so each of
/// these owns its own timing rather than riding on traffic.
fn spawn_background_loops(state: &AppState) {
    tokio::spawn(std::sync::Arc::clone(&state.transcode).rate_control_refresh_loop());
    // Reap idle transcode sessions in the background.
    tokio::spawn(std::sync::Arc::clone(&state.transcode).reap_loop());
    tokio::spawn(std::sync::Arc::clone(&state.offline).run());

    // What the libraries' storage reads at. Deliberately after the listener
    // would come up rather than inline with the encoder and tone-map probes:
    // those measure local hardware in milliseconds, this one may be talking to
    // a NAS that is asleep, on a link that is down, or behind a mount that
    // will block for thirty seconds before admitting it. None of that should
    // delay a server answering requests, and all of it is worth knowing.
    tokio::spawn(crate::http::system::probe_storage(state.clone(), 0.0));

    // Scheduled jobs: library scans, metadata refreshes, probe retries, and
    // transcode-cache cleanup. Every interval defaults to off, so this loop
    // does nothing until someone sets one in Settings.
    tokio::spawn(
        std::sync::Arc::clone(&state.jobs).schedule_loop(std::sync::Arc::clone(&state.transcode)),
    );

    // Trakt: hourly (and on-demand) two-way sync + the scrobble-pause sweep.
    tokio::spawn(std::sync::Arc::clone(&state.trakt).sync_loop());
    tokio::spawn(std::sync::Arc::clone(&state.trakt).sweep_loop());
    // The watched outbox. Its own loop because a retry scheduled two minutes
    // out has no request to wake it, and a monarr that is down must not stall
    // anything a viewer is waiting on.
    tokio::spawn(std::sync::Arc::clone(&state.watched).run());
}

/// Which port the GDM responder should answer on, or `None` when it must not
/// run at all. A loopback bind is never advertised: publishing it would produce
/// a setup button in every client that can never connect.
fn gdm_responder_port(lan_discovery: bool, bind: SocketAddr) -> Option<u16> {
    if !lan_discovery {
        tracing::info!(%bind, "LAN discovery disabled for loopback bind");
        return None;
    }
    match gdm_bind_port(std::env::var("PLURX_GDM_PORT").ok().as_deref()) {
        Ok(port) => Some(port),
        Err(error) => {
            tracing::warn!(%error, "GDM discovery responder disabled");
            None
        }
    }
}

/// Serve until `shutdown` fires, then drain — for a bounded time.
///
/// Taking the shutdown future rather than installing the signal handler itself
/// is what makes the drain testable, and the drain is the part worth testing. A
/// playback response is a stream: a paced remux holds its connection open for a
/// quarter of the film's runtime, and an HLS playlist poll is only ever moments
/// from the next one. Waiting for all of them means never finishing, so `docker
/// stop` spends its ten-second grace period achieving nothing and then SIGKILLs
/// us — which surfaces as exit 137, indistinguishable at a glance from an
/// out-of-memory kill. Bounding the drain turns that into an orderly exit 0
/// with a line in the log saying what was still open.
async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    progress: Arc<crate::progress::ProgressCoalescer>,
    mdns: Option<mdns_sd::ServiceDaemon>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (drain_started, drain_signal) = tokio::sync::oneshot::channel();
    // `WithGracefulShutdown` is IntoFuture rather than Future, and select!
    // needs the future itself to poll it more than once.
    let server = axum::serve(
        listener,
        // Connect info is load-bearing for the network-priors handlers in
        // `http::network`, which extract the peer address; serving the bare
        // router would make them reject at runtime with nothing to compile
        // against.
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        let _ = drain_started.send(());
    })
    .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            result?;
            tracing::info!("shutdown complete");
        }
        // Only starts counting once the signal has actually arrived: if the
        // channel never fires, this branch stays pending and the server runs.
        _ = async {
            let _ = drain_signal.await;
            tokio::time::sleep(SHUTDOWN_DRAIN_TIMEOUT).await;
        } => {
            tracing::warn!(
                after = ?SHUTDOWN_DRAIN_TIMEOUT,
                "drain timed out with connections still open; exiting anyway"
            );
        }
    }
    match tokio::time::timeout(PROGRESS_DRAIN_TIMEOUT, progress.drain()).await {
        Ok(Ok(flushed)) if flushed > 0 => {
            tracing::info!(
                flushed,
                "flushed coalesced playback progress during shutdown"
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not flush coalesced playback progress during shutdown");
        }
        Err(_) => {
            tracing::warn!(
                after = ?PROGRESS_DRAIN_TIMEOUT,
                "timed out flushing coalesced playback progress during shutdown"
            );
        }
    }
    if let Some(mdns) = mdns {
        if let Err(error) = mdns.shutdown() {
            tracing::warn!(%error, "Bonjour discovery shutdown failed");
        }
    }
    Ok(())
}

/// How long to wait for open connections to finish after a shutdown signal.
/// Comfortably inside Docker's default ten-second stop grace period, so the
/// process gets to choose its own exit rather than being killed mid-drain.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Leaves headroom inside Docker's ten-second stop window after HTTP drain.
const PROGRESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Native plurx discovery contract. The Apple clients declare this exact type
/// in `NSBonjourServices`, so changing it is a protocol change, not a rename.
const MDNS_SERVICE_TYPE: &str = "_plurx._tcp.local.";

/// The normal server advertises directly when it runs on a LAN interface.
/// Docker deployments turn this off and run `plurxd advertise` in a tiny
/// host-network companion instead, because multicast cannot cross a bridge.
fn mdns_advertising_enabled() -> bool {
    std::env::var("PLURX_MDNS_ADVERTISE")
        .ok()
        .map(|value| mdns_advertising_value_enabled(&value))
        .unwrap_or(true)
}

fn mdns_advertising_value_enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// A fresh Compose deployment is named `plurx`, which is useful branding but
/// useless when three machines appear in one picker. Use the machine hostname
/// for that default only; an operator's explicit server name remains theirs.
fn discovery_display_name(
    configured: &str,
    hostname: Option<&str>,
    address: Option<&IpAddr>,
) -> String {
    let configured = configured.trim();
    let label = if !configured.is_empty() && !configured.eq_ignore_ascii_case("plurx") {
        configured.to_owned()
    } else {
        hostname
            .and_then(normalized_hostname)
            .unwrap_or_else(|| "plurx".to_owned())
    };
    let Some(address) = address else { return label };
    let identified = format!("{label} · {address}");
    // A DNS-SD service instance is one DNS label (63 bytes). A long custom
    // name remains more useful than a registration failure merely to add IP.
    if identified.len() <= 63 {
        identified
    } else {
        label
    }
}

fn normalized_hostname(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    let without_local = trimmed.strip_suffix(".local").unwrap_or(trimmed).trim();
    (!without_local.is_empty() && !without_local.eq_ignore_ascii_case("localhost"))
        .then(|| without_local.to_owned())
}

/// Ask the routing table which local IPv4 address carries mDNS. UDP `connect`
/// chooses an interface without sending a packet, so this works while the
/// server is still offline and avoids accidentally showing a Docker bridge IP.
fn primary_lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("224.0.0.251:5353").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then_some(address)
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: `buffer` is writable for exactly the length passed to libc, and
    // it remains alive until the returned bytes are copied into a Rust String.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    normalized_hostname(&String::from_utf8_lossy(&buffer[..end]))
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .and_then(|value| normalized_hostname(&value))
}

#[derive(Deserialize)]
struct DiscoveryServerInfo {
    name: String,
    version: String,
    instance_id: String,
}

/// Fetch identity from the running server, then publish that identity through
/// the host's mDNS interfaces. Keeping this process separate is what allows
/// the main container to stay on an external Compose network such as `media`.
async fn advertise(server: &str) -> anyhow::Result<()> {
    init_companion_logging();
    advertise_with(
        server,
        start_mdns_advertiser,
        Duration::from_secs(2),
        shutdown_signal(),
    )
    .await
}

async fn advertise_with(
    server: &str,
    advertiser: MdnsAdvertiser,
    retry_after: Duration,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let (base, port, info_url) = discovery_endpoints(server)?;
    let client = discovery_client()?;
    tokio::pin!(shutdown);

    let Some(info) =
        await_discovery_identity(&client, &base, info_url, retry_after, &mut shutdown).await
    else {
        return Ok(());
    };

    let host_name = system_hostname();
    let lan_address = primary_lan_address();
    let discovery_name =
        discovery_display_name(&info.name, host_name.as_deref(), lan_address.as_ref());
    let daemon = advertiser(
        &info.instance_id,
        &discovery_name,
        SocketAddr::from(([0, 0, 0, 0], port)),
        &info.version,
    )?;
    tracing::info!(server = %base, "host Bonjour discovery companion ready");
    (&mut shutdown).await;
    daemon
        .shutdown()
        .context("shutting down host Bonjour advertiser")?;
    Ok(())
}

/// Console logging for the discovery companion. It has no admin UI behind it,
/// so there is no in-memory ring to feed — only the container's stdout.
fn init_companion_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PLURX_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init()
        .ok();
}

/// The HTTP client the companion asks the server with.
///
/// Bounded, because the companion's whole loop is "ask again in two seconds":
/// a request with no timeout against a half-open connection would hang the
/// retry loop forever and the server would never be advertised.
fn discovery_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building discovery HTTP client")
}

/// The base URL, the port to advertise, and where the server identity lives.
///
/// The port is taken from the URL rather than assumed, because the whole
/// purpose of this companion is a server published on the *host* at whatever
/// port the operator mapped — advertising the container's port would produce a
/// Bonjour record no client on the LAN can connect to.
fn discovery_endpoints(server: &str) -> anyhow::Result<(reqwest::Url, u16, reqwest::Url)> {
    let base = reqwest::Url::parse(server).context("parsing discovery server URL")?;
    anyhow::ensure!(
        matches!(base.scheme(), "http" | "https"),
        "discovery server URL must use http or https"
    );
    let port = base
        .port_or_known_default()
        .context("discovery server URL has no port")?;
    let info_url = base
        .join("/api/v1/server")
        .context("building discovery server-info URL")?;
    Ok((base, port, info_url))
}

/// Wait for the server to come up and tell us who it is, or for the process to
/// be asked to stop.
///
/// The companion container starts beside the server, not after it, so a refused
/// connection is the normal first few seconds rather than a failure. It retries
/// indefinitely — there is nothing else for it to do — but logs the first
/// attempt and then only every fifteenth, so a server that never appears leaves
/// a trail without filling the log with one line every two seconds.
async fn await_discovery_identity(
    client: &reqwest::Client,
    base: &reqwest::Url,
    info_url: reqwest::Url,
    retry_after: Duration,
    shutdown: &mut (impl std::future::Future<Output = ()> + Unpin),
) -> Option<DiscoveryServerInfo> {
    let mut failed_attempts = 0_u64;
    loop {
        match fetch_discovery_server_info(client, info_url.clone()).await {
            Ok(info) => return Some(info),
            Err(error) => {
                failed_attempts += 1;
                if should_log_wait(failed_attempts) {
                    tracing::warn!(
                        %error,
                        server = %base,
                        "waiting for server before advertising Bonjour"
                    );
                }
                tokio::select! {
                    _ = &mut *shutdown => return None,
                    _ = tokio::time::sleep(retry_after) => {}
                }
            }
        }
    }
}

/// The first failure, then one line every thirty seconds. A server that never
/// appears has to leave a trail; it must not fill the log while doing so.
fn should_log_wait(failed_attempts: u64) -> bool {
    failed_attempts == 1 || failed_attempts.is_multiple_of(15)
}

async fn fetch_discovery_server_info(
    client: &reqwest::Client,
    url: reqwest::Url,
) -> anyhow::Result<DiscoveryServerInfo> {
    client
        .get(url)
        .send()
        .await
        .context("requesting server identity")?
        .error_for_status()
        .context("server identity request failed")?
        .json()
        .await
        .context("decoding server identity")
}

/// The two `ServiceDaemon` calls a registration makes. Naming them is what lets
/// a test decide a registration — and each way one fails — against a fake,
/// instead of publishing a real Bonjour record on the local network.
trait MdnsRegistrar {
    /// The daemon's event stream. It ends when the daemon is gone.
    fn monitor(&self) -> anyhow::Result<Box<dyn Iterator<Item = mdns_sd::DaemonEvent> + Send>>;

    /// Publish one record.
    fn register(&self, service: mdns_sd::ServiceInfo) -> anyhow::Result<()>;
}

impl MdnsRegistrar for mdns_sd::ServiceDaemon {
    fn monitor(&self) -> anyhow::Result<Box<dyn Iterator<Item = mdns_sd::DaemonEvent> + Send>> {
        let events = mdns_sd::ServiceDaemon::monitor(self).context("monitoring mDNS daemon")?;
        Ok(Box::new(std::iter::from_fn(move || events.recv().ok())))
    }

    fn register(&self, service: mdns_sd::ServiceInfo) -> anyhow::Result<()> {
        mdns_sd::ServiceDaemon::register(self, service).context("registering plurx Bonjour service")
    }
}

/// Drain a daemon's events, logging the ones that report a failure.
///
/// This is where an unusable interface or a socket the kernel refused actually
/// surfaces: `mdns-sd` opens its multicast sockets on the daemon thread, so
/// `register` can return `Ok` for a record that never reaches the network.
fn log_mdns_events(events: impl Iterator<Item = mdns_sd::DaemonEvent>) {
    for event in events {
        if let mdns_sd::DaemonEvent::Error(error) = event {
            tracing::warn!(%error, "Bonjour discovery daemon error");
        }
    }
}

/// Put one record on a daemon and leave its error monitor draining.
///
/// Every decision `start_mdns_advertiser` makes is here: a daemon that cannot
/// be monitored, a record that cannot be built, and a registration the daemon
/// refuses are each an error the caller reports and carries on from. Monitoring
/// is established before the record exists, so a failure the registration
/// provokes cannot be missed.
fn register_advertiser(
    daemon: &impl MdnsRegistrar,
    instance_id: &str,
    name: &str,
    bind: SocketAddr,
    version: &str,
) -> anyhow::Result<()> {
    let monitor = daemon.monitor()?;
    std::thread::Builder::new()
        .name("plurx-mdns-monitor".to_owned())
        .spawn(move || log_mdns_events(monitor))
        .context("starting mDNS monitor")?;

    let service = mdns_service_info(instance_id, name, bind, version)?;
    daemon.register(service)?;
    tracing::info!(
        service = MDNS_SERVICE_TYPE,
        server_name = name,
        port = bind.port(),
        "Bonjour discovery advertiser registered"
    );
    Ok(())
}

/// Start the local DNS-SD advertiser and keep its daemon handle alive for the
/// lifetime of the HTTP server. Address auto-detection is correct for the
/// default wildcard bind; an explicit bind advertises only that address.
fn start_mdns_advertiser(
    instance_id: &str,
    name: &str,
    bind: SocketAddr,
    version: &str,
) -> anyhow::Result<mdns_sd::ServiceDaemon> {
    let daemon = mdns_sd::ServiceDaemon::new().context("starting mDNS daemon")?;
    register_advertiser(&daemon, instance_id, name, bind, version)?;
    Ok(daemon)
}

/// Build one testable DNS-SD record. The stable instance id names the `.local`
/// host; the human server name remains the service instance users see.
fn mdns_service_info(
    instance_id: &str,
    name: &str,
    bind: SocketAddr,
    version: &str,
) -> anyhow::Result<mdns_sd::ServiceInfo> {
    let suffix: String = instance_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect();
    let suffix = if suffix.is_empty() { "server" } else { &suffix };
    let hostname = format!("plurx-{suffix}.local.");
    let instance_name = if name.trim().is_empty() {
        "plurx"
    } else {
        name
    };
    let properties = [
        ("id", instance_id),
        ("name", instance_name),
        ("version", version),
        ("api", "/api/v1"),
    ];
    let addresses = if bind.ip().is_unspecified() {
        String::new()
    } else {
        bind.ip().to_string()
    };
    let service = mdns_sd::ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        instance_name,
        &hostname,
        addresses.as_str(),
        bind.port(),
        &properties[..],
    )?;
    Ok(if bind.ip().is_unspecified() {
        service.enable_addr_auto()
    } else {
        service
    })
}

/// First line of `ffmpeg -version` (e.g. "ffmpeg version 6.1.1 …"), if the
/// binary runs at all. Purely informational, for the settings page.
async fn ffmpeg_version(bin: &str) -> Option<String> {
    let out = tokio::process::Command::new(bin)
        .arg("-version")
        .output()
        .await
        .ok()?;
    first_version_line(&out.stdout)
}

/// The banner's first line, or nothing.
///
/// `None` rather than an empty string when the binary printed nothing: the
/// settings page renders this verbatim, and a blank version reads as "ffmpeg
/// reported no version" when what happened is that nothing reported at all.
fn first_version_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
}

/// GDM discovery responder: answers Plex clients' multicast `M-SEARCH` on the
/// LAN (docs/CLIENTS.md §3). Multicast is TTL-scoped to the local network, so
/// this never answers WAN queries (avoids GDM/SSDP reflection abuse).
async fn gdm_responder(
    instance_id: String,
    name: String,
    http_port: u16,
    gdm_port: u16,
) -> anyhow::Result<()> {
    let socket = gdm_socket(gdm_port).await?;
    gdm_serve(&socket, &instance_id, &name, http_port).await
}

/// Bind the GDM port and join the group Plex clients search on.
///
/// Multicast is TTL-scoped to the local network, so this can only ever be
/// reached from the LAN — which is what keeps a GDM responder from becoming
/// the reflection amplifier that the protocol has historically been abused as.
async fn gdm_socket(gdm_port: u16) -> anyhow::Result<tokio::net::UdpSocket> {
    use std::net::Ipv4Addr;

    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, gdm_port))
        .await
        .context("binding GDM port")?;
    socket
        .join_multicast_v4(
            plurx_compat_plex::gdm::GDM_MULTICAST_ADDR.parse()?,
            Ipv4Addr::UNSPECIFIED,
        )
        .context("joining GDM multicast group")?;
    tracing::info!(port = gdm_port, "GDM discovery responder listening");
    Ok(socket)
}

/// Answer GDM searches on an already-bound socket, forever.
///
/// Split from the socket setup so the *protocol* half is reachable without
/// joining a multicast group: what matters here is that only a real `M-SEARCH`
/// draws a reply. Answering anything else would turn the server into an
/// amplification reflector for whatever else arrives on the port.
async fn gdm_serve(
    socket: &tokio::net::UdpSocket,
    instance_id: &str,
    name: &str,
    http_port: u16,
) -> anyhow::Result<()> {
    // Plex clients parse what GDM advertises, so this stays bare semver — the
    // git build stamp would not survive their version comparisons.
    let version = crate::version::SEMVER;
    let mut buf = [0u8; 1024];
    loop {
        let (n, addr) = socket.recv_from(&mut buf).await?;
        if plurx_compat_plex::gdm::is_search(&buf[..n]) {
            let resp = plurx_compat_plex::gdm::response(instance_id, name, version, http_port);
            if let Err(e) = socket.send_to(&resp, addr).await {
                tracing::warn!(error = %e, "GDM response send failed");
            }
        }
    }
}

fn gdm_bind_port(value: Option<&str>) -> anyhow::Result<u16> {
    let Some(value) = value else {
        return Ok(plurx_compat_plex::gdm::GDM_PORT);
    };
    value
        .parse::<u16>()
        .with_context(|| format!("invalid PLURX_GDM_PORT {value:?}"))
}

fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    // Install both Unix streams before returning the future. The caller binds
    // the listener before `serve` first polls its graceful-shutdown future, so
    // creating either stream inside that future leaves a small interval where
    // the process is externally reachable but the signal still has its default
    // terminating action.
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("installing SIGINT handler");

    async move {
        #[cfg(unix)]
        let ctrl_c = async move {
            interrupt.recv().await;
        };
        #[cfg(not(unix))]
        let ctrl_c = async {
            tokio::signal::ctrl_c()
                .await
                .expect("installing Ctrl-C handler");
        };
        #[cfg(unix)]
        let terminate = async move {
            terminate.recv().await;
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
        tracing::info!("shutdown signal received, draining");
    }
}

/// Deterministically exercise the otherwise sub-millisecond listener/first-poll
/// boundary in the shipped binary. With lazy registration this signal uses its
/// default action and the process exits with signal 15; with eager registration
/// it is buffered for `shutdown_signal` and starts a clean drain.
#[cfg(unix)]
fn trigger_shutdown_registration_failpoint() {
    if matches!(
        std::env::var("PLURX_SHUTDOWN_REGISTRATION_FAILPOINT").as_deref(),
        Ok("after-listener-bind")
    ) {
        // SAFETY: this path is opt-in test instrumentation, and
        // `shutdown_signal` installed the process handler before startup.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    }
}

#[cfg(not(unix))]
fn trigger_shutdown_registration_failpoint() {}

/// Minimal HTTP/1.0 probe over std TcpStream — deliberately dependency-free.
fn healthcheck(config: &Config) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let addr = SocketAddr::from(([127, 0, 0, 1], config.server.bind.port()));
    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.starts_with("HTTP/1.0 200") || status_line.starts_with("HTTP/1.1 200") {
        println!("healthy");
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {status_line:?}");
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use plurx_core::domain::Library;
    use plurx_core::store::Store;

    fn config_in(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.storage.data_dir = dir.to_path_buf();
        config.server.bind = "127.0.0.1:0".parse().expect("bind");
        config
    }

    fn store_in(dir: &std::path::Path) -> Arc<dyn Store> {
        Arc::new(SqliteStore::open(&dir.join("plurx.db")).expect("store"))
    }

    /// A password may legitimately begin or end with a space. Trimming it would
    /// leave the operator locked out of the account they just repaired, with
    /// nothing in any log to explain it.
    #[test]
    fn only_the_line_ending_is_stripped_from_a_typed_password() {
        let read = |s: &str| {
            read_password(&mut std::io::Cursor::new(s.as_bytes().to_vec())).expect("read")
        };
        assert_eq!(read("hunter2hunter2\n"), "hunter2hunter2");
        assert_eq!(read("hunter2hunter2\r\n"), "hunter2hunter2");
        assert_eq!(read("  spaced pass  \n"), "  spaced pass  ");
        // A closed stdin is an empty password, which the length check then
        // rejects — it must not read as success.
        assert_eq!(read(""), "");
    }

    /// The scratch directory is cleared at boot because a half-written segment
    /// from a killed process is worse than no segment. The two caches are its
    /// siblings precisely so that clearing does not reach them: a cache that
    /// empties on restart is a warm-up cost with none of the benefit.
    #[test]
    fn boot_clears_the_session_scratch_and_nothing_else() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path();

        let first = create_dirs(data).expect("first boot");
        std::fs::write(first.transcode.join("stale-session.m4s"), b"half").expect("write");
        std::fs::write(first.cache.join("finished.mp4"), b"kept").expect("write");
        std::fs::write(first.subs.join("extracted.srt"), b"kept").expect("write");
        std::fs::write(first.artwork.join("poster.jpg"), b"kept").expect("write");

        let second = create_dirs(data).expect("second boot");
        assert_eq!(second.transcode, first.transcode);
        assert!(
            !second.transcode.join("stale-session.m4s").exists(),
            "a stale session must not survive a restart"
        );
        assert!(second.transcode.is_dir(), "and the directory is recreated");
        for kept in [
            second.cache.join("finished.mp4"),
            second.subs.join("extracted.srt"),
            second.artwork.join("poster.jpg"),
        ] {
            assert!(kept.exists(), "{} must survive a restart", kept.display());
        }
        // The caches are siblings of the scratch, not children of it: nested,
        // the boot clear above would take them with it.
        assert!(!second.cache.starts_with(&second.transcode));
        assert!(!second.subs.starts_with(&second.transcode));
    }

    #[test]
    fn a_data_dir_that_cannot_be_created_is_reported_with_its_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked = tmp.path().join("data");
        std::fs::write(&blocked, b"a file, not a directory").expect("write");
        let error = format!("{:#}", create_dirs(&blocked).expect_err("must fail"));
        assert!(error.contains("artwork"), "{error}");
    }

    /// An empty `PLURX_HWACCEL=` is what Compose produces for an unset
    /// variable. Seeding the stored setting from it would overwrite the
    /// operator's chosen encoder with "no preference" at every restart.
    #[test]
    fn an_empty_hwaccel_override_does_not_overwrite_the_stored_preference() {
        assert_eq!(hwaccel_override(None), None);
        assert_eq!(hwaccel_override(Some(String::new())), None);
        assert_eq!(
            hwaccel_override(Some("  NVENC  ".to_owned())),
            Some("nvenc".to_owned())
        );
    }

    /// "auto" is the *absence* of a preference. Passed to the chooser
    /// literally it matches no encoder, so a node with a working GPU would
    /// quietly fall back to software.
    #[test]
    fn auto_is_asked_as_no_preference_at_all() {
        assert_eq!(probe_preference("auto"), "");
        assert_eq!(probe_preference("nvenc"), "nvenc");
        assert_eq!(probe_preference(""), "");
    }

    /// Both the boot probe and the admin UI read the stored setting, so there
    /// is one answer rather than two that can disagree.
    #[tokio::test]
    async fn the_stored_encoder_preference_defaults_to_auto_and_is_read_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());

        assert_eq!(resolve_hwaccel_pref(&store).await.expect("pref"), "auto");

        // PLURX_HWACCEL seeds the stored setting, so the admin UI shows what
        // the deployment asked for rather than disagreeing with it. Set and
        // cleared here because the variable is process-wide.
        std::env::set_var("PLURX_HWACCEL", "  NVENC  ");
        assert_eq!(resolve_hwaccel_pref(&store).await.expect("pref"), "nvenc");
        assert_eq!(
            store.get_setting(keys::HWACCEL).await.expect("setting"),
            Some("nvenc".to_owned()),
            "the env override must be durable, not just this boot's answer"
        );
        std::env::remove_var("PLURX_HWACCEL");

        store
            .put_setting(keys::HWACCEL, "vaapi")
            .await
            .expect("put");
        assert_eq!(resolve_hwaccel_pref(&store).await.expect("pref"), "vaapi");

        // A setting cleared through the UI is "no preference", not the empty
        // encoder name — which would select nothing at all.
        store.put_setting(keys::HWACCEL, "").await.expect("put");
        assert_eq!(resolve_hwaccel_pref(&store).await.expect("pref"), "auto");
    }

    /// The settings page renders this verbatim, so nothing at all must not
    /// arrive as a blank version string.
    #[test]
    fn a_silent_binary_reports_no_version_rather_than_an_empty_one() {
        assert_eq!(
            first_version_line(b"ffmpeg version 7.1.1 Copyright (c)\nbuilt with clang\n"),
            Some("ffmpeg version 7.1.1 Copyright (c)".to_owned())
        );
        assert_eq!(first_version_line(b""), None);
        assert_eq!(first_version_line(b"   \nffmpeg version 7\n"), None);
    }

    /// A binary that is not there is not a startup failure — the settings page
    /// simply has no version to show.
    #[tokio::test]
    async fn a_missing_ffmpeg_has_no_version() {
        assert_eq!(ffmpeg_version("plurxd-no-such-binary-anywhere").await, None);
    }

    /// And a binary that does answer is reported by its own first line. Run
    /// against `/bin/echo` rather than ffmpeg so the spawn is real but the
    /// output is this test's to choose: what is being pinned is that the
    /// banner reaching the settings page is the process's actual stdout,
    /// which a version read from anywhere else would not be.
    #[tokio::test]
    async fn a_version_banner_is_read_from_what_the_binary_printed() {
        assert_eq!(
            ffmpeg_version("/bin/echo").await,
            Some("-version".to_owned()),
            "echo repeats the flag it was passed, so that is the banner"
        );
    }

    /// A server that never appears has to leave a trail without filling the
    /// log with one line every two seconds.
    #[test]
    fn waiting_for_a_server_logs_the_first_attempt_then_rations_itself() {
        assert!(should_log_wait(1), "the first failure is always reported");
        assert!(!should_log_wait(2));
        assert!(!should_log_wait(14));
        assert!(should_log_wait(15), "then one line every thirty seconds");
        assert!(should_log_wait(30));
        assert!(!should_log_wait(31));
    }

    /// The advertised port comes from the URL, because the whole point of the
    /// companion is a server published on the *host* at whatever port the
    /// operator mapped. Advertising the container's port would produce a
    /// Bonjour record no client on the LAN can connect to.
    #[test]
    fn the_advertised_port_is_the_one_the_host_published() {
        let (base, port, info) = discovery_endpoints("http://192.168.1.9:8096").expect("endpoints");
        assert_eq!(port, 8096);
        assert_eq!(info.as_str(), "http://192.168.1.9:8096/api/v1/server");
        assert_eq!(base.as_str(), "http://192.168.1.9:8096/");

        // A scheme's default port is a real answer.
        let (_, port, info) = discovery_endpoints("https://plurx.example").expect("endpoints");
        assert_eq!(port, 443);
        assert_eq!(info.as_str(), "https://plurx.example/api/v1/server");

        // Anything that is not an HTTP URL is refused rather than advertised
        // as something clients cannot reach.
        for bad in ["ftp://host:21", "not a url", "file:///etc/plurx"] {
            assert!(discovery_endpoints(bad).is_err(), "{bad} must be refused");
        }
    }

    /// A local server standing in for the real `/api/v1/server`.
    async fn identity_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new().route(
            "/api/v1/server",
            axum::routing::get(
                move || async move { ([("content-type", "application/json")], body) },
            ),
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    const SERVER_IDENTITY: &str = r#"{"name":"Living Room","version":"0.2.0","instance_id":"abc"}"#;

    #[tokio::test]
    async fn the_companion_advertises_the_identity_the_server_reports() {
        let (base_url, server) = identity_server(SERVER_IDENTITY).await;
        let (base, _, info_url) = discovery_endpoints(&base_url).expect("endpoints");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("client");

        let info = fetch_discovery_server_info(&client, info_url.clone())
            .await
            .expect("identity");
        assert_eq!(info.name, "Living Room");
        assert_eq!(info.instance_id, "abc");

        // And the retry loop returns that identity rather than waiting.
        let mut never = Box::pin(std::future::pending::<()>());
        let info = await_discovery_identity(
            &client,
            &base,
            info_url,
            Duration::from_millis(10),
            &mut never,
        )
        .await
        .expect("identity");
        assert_eq!(info.version, "0.2.0");
        server.abort();
    }

    /// The companion starts beside the server, not after it, so a refused
    /// connection is the normal first few seconds. It must keep retrying — and
    /// it must still stop when the container is asked to.
    #[tokio::test]
    async fn a_companion_waiting_on_a_dead_server_still_stops_on_shutdown() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("client");
        // Port 1 on loopback: nothing is listening, and connect fails fast.
        let (base, _, info_url) = discovery_endpoints("http://127.0.0.1:1").expect("endpoints");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(());
        });
        let mut shutdown = Box::pin(async move {
            let _ = rx.await;
        });

        let waited = await_discovery_identity(
            &client,
            &base,
            info_url,
            Duration::from_millis(20),
            &mut shutdown,
        )
        .await;
        assert!(
            waited.is_none(),
            "a shutdown while waiting must not advertise a server it never reached"
        );
    }

    /// The discovery record is withdrawn as part of the drain, and a
    /// withdrawal that cannot be made is a warning rather than a failed
    /// shutdown.
    ///
    /// The daemon here is already shut down before `serve` is given it, which
    /// is the awkward case: a node whose Bonjour daemon has gone must still
    /// exit 0, because the alternative is `docker stop` reporting a failed
    /// container for a server that drained perfectly well. Turning that warning
    /// back into a `?` or an `expect` is exactly what this catches.
    #[tokio::test]
    async fn a_discovery_daemon_that_cannot_be_withdrawn_still_drains_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        let progress = Arc::clone(&state.progress);
        let app = http::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");

        let daemon = daemon_off_the_mdns_port().expect("mDNS daemon");
        // Withdraw it first, and wait for the daemon to report that it has
        // actually stopped — otherwise the withdrawal inside `serve` races the
        // daemon thread and may still be accepted, which is not the case this
        // owns. Nothing was ever registered, so no record reaches the network.
        daemon
            .shutdown()
            .expect("the first withdrawal is the real one")
            .recv()
            .expect("the daemon reports when it has stopped");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let logs = Arc::new(logbuf::LogBuffer::new(64));
        let _capture = capturing(&logs);
        let served = tokio::spawn(serve(listener, app, progress, Some(daemon), async move {
            let _ = stopped.await;
        }));
        stop.send(()).expect("stop");

        tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("serve must finish inside the drain window")
            .expect("join")
            .expect("a discovery daemon that cannot be withdrawn is not a failed shutdown");

        // Exiting 0 is only half of it. A withdrawal that silently did not
        // happen leaves a record on the LAN for a server that is gone, so the
        // operator has to be told which half of the drain failed.
        let warned = logs.tail("warn", 64);
        assert!(
            warned
                .iter()
                .any(|entry| entry.message.contains("Bonjour discovery shutdown failed")),
            "the failed withdrawal must reach the operator: {warned:?}"
        );
    }

    /// Playback progress is coalesced in memory, so the beats between one
    /// durable commit and the next exist nowhere else. A `docker stop` in that
    /// window must commit them rather than drop them — resuming a film minutes
    /// behind where it was left is the failure an operator actually sees — and
    /// the drain says how many it saved.
    #[tokio::test]
    async fn playback_progress_pending_at_shutdown_is_committed_not_lost() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        let store = Arc::clone(&state.store);
        let progress = Arc::clone(&state.progress);

        let user = store
            .create_user("viewer", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Films".to_owned(),
                kind: LibraryKind::Movies,
                paths: vec![std::path::PathBuf::from("/films")],
                anime: false,
            })
            .await
            .expect("library");
        let item = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Interrupted".to_owned(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");

        // The first beat commits durably; the second lands inside the commit
        // window and so exists only in memory. That is the one at risk.
        progress
            .put(user.id, item, 1_000, Some(100_000))
            .await
            .expect("leading beat");
        progress
            .put(user.id, item, 7_000, Some(100_000))
            .await
            .expect("pending beat");

        let app = http::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let logs = Arc::new(logbuf::LogBuffer::new(64));
        let _capture = capturing(&logs);
        let served = tokio::spawn(serve(listener, app, progress, None, async move {
            let _ = stopped.await;
        }));
        stop.send(()).expect("stop");

        tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("serve must finish inside the drain window")
            .expect("join")
            .expect("a clean drain");

        assert_eq!(
            store
                .watch_state(user.id, item)
                .await
                .expect("watch state")
                .expect("a watched item")
                .position_ms,
            7_000,
            "the beat that was only in memory has to survive the shutdown"
        );
        let flushed = logs.tail("info", 64);
        assert!(
            flushed.iter().any(|entry| entry
                .message
                .contains("flushed coalesced playback progress during shutdown")
                && entry.message.contains("flushed=1")),
            "the drain must report what it saved: {flushed:?}"
        );
    }

    /// A loopback bind is common behind a proxy. Advertising it would put a
    /// setup button in every client on the LAN that can never connect.
    ///
    /// Only the loopback refusal is asserted here, because that decision is
    /// taken before `PLURX_GDM_PORT` is ever read and so cannot be perturbed by
    /// a test running beside this one. What the LAN case resolves to depends on
    /// that process-wide variable, so it belongs to the test that owns it.
    #[test]
    fn a_loopback_bind_is_never_advertised_over_gdm() {
        assert_eq!(
            gdm_responder_port(false, "127.0.0.1:32400".parse().expect("addr")),
            None
        );
    }

    /// Only a real `M-SEARCH` draws a reply. Answering anything that arrives
    /// would make the server an amplification reflector on its own LAN.
    #[tokio::test]
    async fn gdm_answers_a_search_and_ignores_everything_else() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind server");
        let server_addr = server.local_addr().expect("addr");
        let responder = tokio::spawn(async move {
            let _ = gdm_serve(&server, "instance-id", "Living Room", 32400).await;
        });

        let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind client");
        client.connect(server_addr).await.expect("connect");

        // Noise first: it must draw nothing, and must not stop the responder.
        client.send(b"hello?").await.expect("send noise");
        let mut buf = [0u8; 2048];
        let quiet = tokio::time::timeout(Duration::from_millis(150), client.recv(&mut buf)).await;
        assert!(quiet.is_err(), "unsolicited traffic must not draw a reply");

        client
            .send(b"M-SEARCH * HTTP/1.0\r\n\r\n")
            .await
            .expect("send search");
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("a GDM reply")
            .expect("bytes");
        let reply = String::from_utf8_lossy(&buf[..n]);
        assert!(reply.contains("Living Room"), "{reply}");
        assert!(reply.contains("32400"), "{reply}");
        assert!(reply.contains(crate::version::SEMVER), "{reply}");
        responder.abort();
    }

    /// The container health check. Its output lands in `docker inspect`, and
    /// its verdict decides whether an orchestrator restarts the server — so a
    /// server answering anything but 200 has to fail rather than pass quietly.
    #[test]
    fn the_health_check_believes_only_a_200() {
        fn serve_once(status_line: &'static str) -> (u16, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let handle = std::thread::spawn(move || {
                if let Ok((mut socket, _)) = listener.accept() {
                    let mut request = [0u8; 512];
                    let _ = socket.read(&mut request);
                    let _ = socket.write_all(status_line.as_bytes());
                }
            });
            (port, handle)
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = config_in(tmp.path());

        let (port, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        config.server.bind = format!("127.0.0.1:{port}").parse().expect("addr");
        healthcheck(&config).expect("a 200 is healthy");
        server.join().expect("server thread");

        let (port, server) = serve_once("HTTP/1.1 503 Service Unavailable\r\n\r\n");
        config.server.bind = format!("127.0.0.1:{port}").parse().expect("addr");
        let error = format!(
            "{:#}",
            healthcheck(&config).expect_err("503 is not healthy")
        );
        assert!(error.contains("503"), "{error}");
        server.join().expect("server thread");

        // Nothing listening at all is unhealthy, and says which address it
        // could not reach.
        config.server.bind = "127.0.0.1:1".parse().expect("addr");
        let error = format!("{:#}", healthcheck(&config).expect_err("no server"));
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }

    /// `plurxd healthcheck` is what Docker runs, so it has to reach the
    /// running server through the same config the server was started with.
    #[tokio::test]
    async fn the_healthcheck_subcommand_probes_the_configured_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut request = [0u8; 512];
                let _ = socket.read(&mut request);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = config_in(tmp.path());
        config.server.bind = format!("127.0.0.1:{port}").parse().expect("addr");
        dispatch(Command::Healthcheck, config)
            .await
            .expect("healthy");
        server.join().expect("server thread");
    }

    /// The recovery path when an admin password is forgotten. Resetting must
    /// also revoke the sessions, or a stolen token outlives the password it
    /// was issued against.
    #[tokio::test]
    async fn resetting_a_password_revokes_the_sessions_it_replaces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let hash = plurx_core::auth::hash_password("original-password").expect("hash");
        let user = store
            .create_user("owner", &hash, true)
            .await
            .expect("create user");
        store
            .create_token("session-token", user.id, None)
            .await
            .expect("token");
        reset_password_in_store(store.as_ref(), "owner", "a-new-password")
            .await
            .expect("reset");

        let after = store
            .get_user_by_username("owner")
            .await
            .expect("lookup")
            .expect("user");
        assert!(
            plurx_core::auth::verify_password("a-new-password", &after.password_hash),
            "the new password must work"
        );
        assert!(
            !plurx_core::auth::verify_password("original-password", &after.password_hash),
            "the old password must not"
        );
        assert!(
            store
                .user_for_token("session-token")
                .await
                .expect("token lookup")
                .is_none(),
            "an existing session must not outlive the password it was issued against"
        );
    }

    #[tokio::test]
    async fn a_password_reset_refuses_a_short_password_and_an_unknown_user() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        let store = store_in(tmp.path());
        let hash = plurx_core::auth::hash_password("original-password").expect("hash");
        store
            .create_user("owner", &hash, true)
            .await
            .expect("create user");
        drop(store);

        let error = format!(
            "{:#}",
            reset_password(&config, "owner", Some("short".to_owned()))
                .await
                .expect_err("too short")
        );
        assert!(error.contains("at least 8 characters"), "{error}");

        let store = store_in(tmp.path());
        let error = format!(
            "{:#}",
            reset_password_in_store(store.as_ref(), "ghost", "a-new-password")
                .await
                .expect_err("no such user")
        );
        assert!(error.contains("no user named `ghost`"), "{error}");

        // And the real account's password is untouched by either failure.
        let user = store
            .get_user_by_username("owner")
            .await
            .expect("lookup")
            .expect("user");
        assert!(plurx_core::auth::verify_password(
            "original-password",
            &user.password_hash
        ));
    }

    /// Maintenance commands must not become a second migration coordinator.
    /// Until `run` has activated Hiqlite they fail without creating an
    /// incoming or active target and leave the legacy database untouched.
    #[tokio::test]
    async fn maintenance_commands_refuse_an_unmigrated_data_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        let store = store_in(tmp.path());
        let hash = plurx_core::auth::hash_password("original-password").expect("hash");
        store
            .create_user("owner", &hash, true)
            .await
            .expect("create user");
        drop(store);

        for error in [
            reset_password(&config, "owner", Some("a-new-password".to_owned()))
                .await
                .expect_err("reset must refuse legacy SQLite"),
            refresh_metadata(&config, None)
                .await
                .expect_err("refresh must refuse legacy SQLite"),
        ] {
            let message = format!("{error:#}");
            assert!(
                message.contains("only `plurxd run` may import"),
                "{message}"
            );
        }
        assert!(!tmp.path().join("hiqlite.incoming").exists());
        assert!(!tmp.path().join("hiqlite").exists());

        let store = store_in(tmp.path());
        let user = store
            .get_user_by_username("owner")
            .await
            .expect("lookup")
            .expect("user");
        assert!(plurx_core::auth::verify_password(
            "original-password",
            &user.password_hash
        ));
    }

    async fn add_library(
        store: &Arc<dyn Store>,
        name: &str,
        kind: LibraryKind,
        anime: bool,
    ) -> Library {
        store
            .create_library(&plurx_core::domain::NewLibrary {
                name: name.to_owned(),
                kind,
                paths: vec![std::path::PathBuf::from("/media")],
                anime,
            })
            .await
            .expect("library")
    }

    /// Book and home-video libraries have no provider behind them, so a
    /// refresh must skip them rather than fail the whole run on a missing
    /// TMDB key it was never going to use.
    #[tokio::test]
    async fn a_refresh_skips_the_libraries_that_have_no_provider() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        add_library(&store, "Books", LibraryKind::Books, false).await;
        add_library(&store, "Home", LibraryKind::Home, false).await;
        let artwork = tmp.path().join("artwork");
        refresh_metadata_with_store(store, &artwork, None)
            .await
            .expect("a provider-less refresh must succeed");
    }

    /// Naming a library that does not exist is a mistake worth reporting: the
    /// alternative is a run that silently refreshes nothing and exits 0.
    #[tokio::test]
    async fn a_refresh_of_an_unknown_library_fails_rather_than_doing_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        add_library(&store, "Movies", LibraryKind::Movies, false).await;
        let artwork = tmp.path().join("artwork");

        let error = format!(
            "{:#}",
            refresh_metadata_with_store(store, &artwork, Some(4242))
                .await
                .expect_err("no such library")
        );
        assert!(error.contains("no library with id 4242"), "{error}");
    }

    /// A TMDB-backed library with no key configured stops with the reason,
    /// rather than reporting a successful refresh that fetched nothing.
    #[tokio::test]
    async fn a_provider_refresh_without_a_key_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        add_library(&store, "Movies", LibraryKind::Movies, false).await;
        let artwork = tmp.path().join("artwork");

        let error = format!(
            "{:#}",
            refresh_metadata_with_store(store, &artwork, None)
                .await
                .expect_err("no TMDB key")
        );
        assert!(error.contains("TMDB API key is not configured"), "{error}");
    }

    /// With a key present and nothing to enrich, both provider paths run to
    /// completion — an empty library is a no-op, not an error, and an anime
    /// library routes to AniList rather than TMDB.
    #[tokio::test]
    async fn an_empty_provider_library_refreshes_to_an_empty_report() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        store
            .put_setting(keys::TMDB_API_KEY, "not-a-real-key")
            .await
            .expect("key");
        let movies = add_library(&store, "Movies", LibraryKind::Movies, false).await;
        let anime = add_library(&store, "Anime", LibraryKind::Shows, true).await;
        assert!(anime.anime, "the anime flag must be stored");
        let artwork = tmp.path().join("artwork");

        refresh_metadata_with_store(Arc::clone(&store), &artwork, Some(movies.id))
            .await
            .expect("empty TMDB refresh");
        refresh_metadata_with_store(store, &artwork, Some(anime.id))
            .await
            .expect("empty AniList refresh");
    }

    /// Everything a request needs, assembled the way `run` assembles it.
    fn booted_state(dir: &std::path::Path) -> AppState {
        let config = config_in(dir);
        build_state(
            &config,
            "test-node".to_owned(),
            Arc::new(plurx_core::secrets::CredentialKey::generate()),
            plurx_core::cluster::migration::status::ReplicationMonitor::sqlite(),
            store_in(dir),
            create_dirs(dir).expect("dirs"),
            Default::default(),
            Default::default(),
            Arc::new(logbuf::LogBuffer::new(64)),
        )
    }

    #[test]
    fn the_assembled_state_carries_the_configured_identity_and_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        assert_eq!(state.server_name, "plurx");
        assert_eq!(state.node_id, "test-node");
        assert_eq!(state.artwork_dir, tmp.path().join("artwork"));
        assert_eq!(state.cache_dir, tmp.path().join("cache").join("transcode"));
        assert_eq!(state.subs_dir, tmp.path().join("cache").join("subs"));
        // The scratch and the persistent cache both reached the transcode
        // manager: with the cache wired, ffmpeg's runtime cache is placed
        // beside it rather than inside the scratch that gets cleared at boot.
        assert!(
            tmp.path().join("cache").join("runtime").is_dir(),
            "the ffmpeg runtime cache must exist before any session starts"
        );
    }

    /// The server answers requests, then stops when it is told to — and the
    /// coalesced playback progress is flushed on the way out rather than lost.
    #[tokio::test]
    async fn the_server_answers_until_it_is_asked_to_stop_and_then_drains() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        state
            .transcode
            .initialize_rate_control()
            .await
            .expect("rate control");
        spawn_background_loops(&state);

        let progress = Arc::clone(&state.progress);
        let app = http::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(serve(listener, app, progress, None, async move {
            let _ = stopped.await;
        }));

        let client = reqwest::Client::new();
        let health = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("healthz");
        assert!(health.status().is_success(), "{:?}", health.status());

        stop.send(()).expect("stop");
        let outcome = tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("serve must finish inside the drain window")
            .expect("join");
        outcome.expect("an orderly shutdown is exit 0, not an error");

        // And the listener really is gone: an orchestrator that restarts the
        // container must not race a socket that is still bound.
        assert!(
            reqwest::Client::new()
                .get(format!("http://{addr}/healthz"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_err(),
            "the port must be released once serve returns"
        );
    }

    /// A served request carries its socket peer, because the network-priors
    /// handlers read one.
    ///
    /// This is the quietest way this file can break. `http::network`'s
    /// `RemoteAddress` takes the peer from the request extensions as an
    /// `Option` and its rejection is `Infallible`, so serving the bare router
    /// instead of `into_make_service_with_connect_info` is not a compile error
    /// and not a failed request — every prior simply reduces to `None` and gets
    /// written under a key nothing reads. Nothing else in the crate would
    /// notice, so this asserts the peer arrives at a handler served through
    /// `serve` rather than trusting the call site to keep saying so.
    #[tokio::test]
    async fn a_served_request_arrives_with_its_socket_peer() {
        // Deliberately not `http::router`: `network` is private to `http`, and
        // this reads the extension exactly the way `RemoteAddress` does, so the
        // assertion tracks the mechanism the real extractor depends on.
        async fn peer(request: axum::extract::Request) -> String {
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|axum::extract::ConnectInfo(address)| address.to_string())
                .unwrap_or_default()
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        let progress = Arc::clone(&state.progress);
        let app = axum::Router::new().route("/peer", axum::routing::get(peer));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(serve(listener, app, progress, None, async move {
            let _ = stopped.await;
        }));

        let body = reqwest::Client::new()
            .get(format!("http://{addr}/peer"))
            .send()
            .await
            .expect("peer")
            .text()
            .await
            .expect("body");

        stop.send(()).expect("stop");
        tokio::time::timeout(Duration::from_secs(10), served)
            .await
            .expect("serve must finish inside the drain window")
            .expect("join")
            .expect("an orderly shutdown is exit 0, not an error");

        // An empty body is the revert symptom exactly: a bare router serves the
        // request perfectly well and simply has no peer to hand over.
        let seen: SocketAddr = body.parse().expect("a served request must carry a peer");
        // The /24 reduction in `http::network` admits IPv4 only, so a peer that
        // arrives as something other than the loopback the client actually came
        // from would silently produce no identity at all.
        let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        assert_eq!(seen.ip(), loopback);
    }

    /// The daemon installs exactly one subscriber, and losing that race is not
    /// a reason to abort a boot — it can only mean logging already goes
    /// somewhere. Both entry points have to survive being second.
    #[test]
    fn installing_logging_twice_does_not_abort_a_boot() {
        // A global subscriber outlives this test, so pin its filter to `off`
        // first: the only caller of either initialiser in this binary is right
        // here, and an `info` default would put daemon logging into every
        // other test's output.
        std::env::set_var("PLURX_LOG", "off");
        let logs = init_logging();
        assert!(
            logs.tail("trace", 8).is_empty(),
            "a fresh ring starts empty"
        );
        // Second call, and the companion's own initialiser: neither may panic.
        let again = init_logging();
        assert!(!Arc::ptr_eq(&logs, &again));
        init_companion_logging();
    }

    /// The common boot failure is a port already in use, and "Address already
    /// in use" on its own does not say which one.
    #[tokio::test]
    async fn a_port_already_taken_is_reported_with_its_address() {
        let held = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = held.local_addr().expect("addr");

        let listener = bind_listener("127.0.0.1:0".parse().expect("addr"))
            .await
            .expect("an ephemeral port is always available");
        assert_ne!(listener.local_addr().expect("addr").port(), 0);

        let error = format!("{:#}", bind_listener(addr).await.expect_err("taken"));
        assert!(error.contains(&addr.to_string()), "{error}");
    }

    /// One line naming both identities, which a cluster makes it possible to
    /// confuse: the logical server clients bind to, and the process that owns
    /// the local bytes.
    #[test]
    fn startup_names_the_server_and_the_node_separately() {
        let tmp = tempfile::tempdir().expect("tempdir");
        log_startup(
            &config_in(tmp.path()),
            &plurx_core::cluster::ClusterIdentity {
                cluster_id: "instance-1".to_owned(),
                node_id: "node-2".to_owned(),
                raft_id: 1,
            },
        );
    }

    /// An advertiser that never touches the network. Registering a real
    /// Bonjour record puts a service on the LAN, which a test must not do to
    /// check that discovery is best-effort.
    fn advertiser_that_fails(
        _instance_id: &str,
        _name: &str,
        _bind: SocketAddr,
        _version: &str,
    ) -> anyhow::Result<mdns_sd::ServiceDaemon> {
        anyhow::bail!("no mDNS daemon in this test")
    }

    /// The succeeding counterpart, for the callers that need a live daemon
    /// handle rather than a fake. It publishes nothing: only `register` puts a
    /// record on the network and this never calls it, and the daemon is bound
    /// off the mDNS port so it cannot answer for the standard group either.
    fn advertiser_without_a_record(
        _instance_id: &str,
        _name: &str,
        _bind: SocketAddr,
        _version: &str,
    ) -> anyhow::Result<mdns_sd::ServiceDaemon> {
        daemon_off_the_mdns_port()
    }

    /// A `ServiceDaemon` on an ephemeral port instead of 5353. Tests need the
    /// handle's shutdown behaviour, not its network reach, and the default port
    /// would make the mandatory gate contend with the host's own responder.
    fn daemon_off_the_mdns_port() -> anyhow::Result<mdns_sd::ServiceDaemon> {
        mdns_sd::ServiceDaemon::new_with_port(0).context("creating mDNS daemon")
    }

    /// A loopback bind advertises nothing at all — neither protocol — because
    /// a discovery record pointing at 127.0.0.1 is a setup button in every
    /// client on the LAN that can never connect.
    #[tokio::test]
    async fn a_loopback_server_starts_no_discovery_at_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        assert!(
            start_discovery(&config, "instance-1", advertiser_that_fails).is_none(),
            "a loopback bind must not be advertised"
        );
    }

    /// Discovery is best-effort past the loopback check: an advertiser that
    /// cannot start leaves the server running and unadvertised rather than
    /// failing the boot.
    #[test]
    fn an_advertiser_that_cannot_start_does_not_stop_the_server() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = config_in(tmp.path());
        config.server.bind = "0.0.0.0:32400".parse().expect("addr");
        assert!(
            start_bonjour(&config, "instance-1", advertiser_that_fails, true).is_none(),
            "a failed advertiser is a warning, not a boot failure"
        );
        // And a loopback server is not offered to the advertiser at all.
        assert!(start_bonjour(&config, "instance-1", advertiser_that_fails, false).is_none());
    }

    /// The whole boot below the ffmpeg probes: an empty data dir becomes a
    /// server that answers, and a shutdown drains it back out again.
    #[tokio::test]
    async fn a_measured_node_boots_serves_and_drains() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        let handle = plurx_core::cluster::open_store(&config)
            .await
            .expect("store");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();

        let booted = tokio::spawn(boot(
            config.clone(),
            Boot {
                store: handle.store,
                replication: plurx_core::cluster::migration::status::ReplicationMonitor::sqlite(),
                identity: handle.identity,
                credential_key: handle.credential_key,
                dirs: create_dirs(tmp.path()).expect("dirs"),
                encoder_caps: Default::default(),
                system: Default::default(),
                logs: Arc::new(logbuf::LogBuffer::new(64)),
            },
            advertiser_that_fails,
            async move {
                let _ = stopped.await;
            },
        ));

        // The data dir is laid out and the store is open before anything is
        // served — a boot that answered before that would serve 500s.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(tmp.path().join("plurx.db").is_file());
        assert!(tmp.path().join("transcode").is_dir());
        assert!(!booted.is_finished(), "the server must still be running");

        stop.send(()).expect("stop");
        tokio::time::timeout(Duration::from_secs(10), booted)
            .await
            .expect("boot must return once shutdown fires")
            .expect("join")
            .expect("an orderly shutdown is exit 0");
    }

    /// The settings page and every session read this record, so the encoder it
    /// names has to be the one the probe actually chose.
    #[test]
    fn the_system_record_reports_the_encoder_the_probe_selected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let caps = plurx_core::transcode::EncoderCaps::default();
        let selected = caps.choose("").label().to_owned();
        let system = system_info(
            &config_in(tmp.path()),
            "/opt/jellyfin-ffmpeg/ffmpeg".to_owned(),
            "auto".to_owned(),
            caps.clone(),
            Measured {
                ffmpeg_version: Some("ffmpeg version 7.1.1".to_owned()),
                pacing: crate::ffmpeg::PacingCaps {
                    readrate: true,
                    initial_burst: true,
                },
                dovi_rpu: true,
                encoder_selected: selected.clone(),
                tone_map: pipeprobe::PipelineReport::cpu_only("not probed"),
            },
        );

        assert_eq!(system.data_dir, tmp.path().display().to_string());
        assert_eq!(system.ffmpeg, "/opt/jellyfin-ffmpeg/ffmpeg");
        assert_eq!(system.encoder_selected, selected);
        assert_eq!(system.hwaccel_pref, "auto");
        assert!(system.dovi_rpu);
        assert!(system.pacing.readrate);
        // The tone-map answer a session consults is the one that was measured.
        assert_eq!(
            system.tone_map.selected(),
            plurx_core::transcode::Pipeline::Cpu
        );
    }

    /// The companion publishes the identity the server reported, at the port
    /// the host published it on — and a registration it cannot make is an
    /// error rather than a silent success.
    #[tokio::test]
    async fn the_companion_advertises_at_the_published_port() {
        let (base_url, server) = identity_server(SERVER_IDENTITY).await;
        let port = base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .expect("port");

        let advertised = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // A fn pointer cannot capture, so the record lands in a static the
        // test reads back.
        static SEEN: std::sync::Mutex<Vec<(String, String, u16, String)>> =
            std::sync::Mutex::new(Vec::new());
        fn record(
            instance_id: &str,
            name: &str,
            bind: SocketAddr,
            version: &str,
        ) -> anyhow::Result<mdns_sd::ServiceDaemon> {
            SEEN.lock().expect("lock").push((
                instance_id.to_owned(),
                name.to_owned(),
                bind.port(),
                version.to_owned(),
            ));
            anyhow::bail!("no mDNS daemon in this test")
        }

        let error = format!(
            "{:#}",
            advertise_with(
                &base_url,
                record,
                Duration::from_millis(10),
                std::future::pending(),
            )
            .await
            .expect_err("the stub advertiser refuses")
        );
        assert!(error.contains("no mDNS daemon"), "{error}");

        let seen = SEEN.lock().expect("lock").clone();
        advertised.lock().expect("lock").push(());
        assert_eq!(seen.len(), 1, "exactly one registration was attempted");
        let (instance_id, name, advertised_port, version) = seen[0].clone();
        assert_eq!(
            instance_id, "abc",
            "the server's own identity, not a new one"
        );
        assert_eq!(version, "0.2.0");
        assert_eq!(
            advertised_port, port,
            "the host's published port must be advertised, not the container's"
        );
        assert!(name.contains("Living Room"), "{name}");
        server.abort();
    }

    /// A companion that did register withdraws its record when it is told to
    /// stop, and reports that as a clean exit.
    ///
    /// The whole tail past a successful registration: without it a `docker
    /// stop` of the companion would leave a Bonjour record on the LAN pointing
    /// at a server nothing is answering for, and every client on the network
    /// would keep offering it until the record aged out. Stop is only sent
    /// once the registration has actually happened — signalling earlier exits
    /// through the "never reached a server" path instead, which is the case
    /// the test below owns.
    #[tokio::test]
    async fn a_registered_companion_withdraws_its_record_on_shutdown() {
        static REGISTERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        // The shared no-record advertiser, counted so the stop below is sent
        // only once the registration has actually happened.
        fn daemon_without_a_record(
            instance_id: &str,
            name: &str,
            bind: SocketAddr,
            version: &str,
        ) -> anyhow::Result<mdns_sd::ServiceDaemon> {
            let daemon = advertiser_without_a_record(instance_id, name, bind, version)?;
            REGISTERED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(daemon)
        }

        let (base_url, server) = identity_server(SERVER_IDENTITY).await;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let companion = tokio::spawn(async move {
            advertise_with(
                &base_url,
                daemon_without_a_record,
                Duration::from_millis(10),
                async {
                    let _ = stopped.await;
                },
            )
            .await
        });

        // Only ask it to stop once it is past the registration, so this
        // exercises the shutdown tail rather than the early return.
        tokio::time::timeout(Duration::from_secs(5), async {
            while REGISTERED.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the companion must register before it is asked to stop");

        stop.send(()).expect("the companion is still listening");
        let outcome = tokio::time::timeout(Duration::from_secs(5), companion)
            .await
            .expect("a stop must end the companion")
            .expect("join");
        outcome.expect("withdrawing a record it registered is a clean exit");
        assert_eq!(
            REGISTERED.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the identity is fetched and advertised exactly once"
        );
        server.abort();
    }

    /// A companion asked to stop before the server ever appeared exits cleanly
    /// rather than advertising something it never reached.
    #[tokio::test]
    async fn a_companion_that_never_reaches_a_server_exits_without_advertising() {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = stop.send(());
        });
        advertise_with(
            "http://127.0.0.1:1",
            advertiser_that_fails,
            Duration::from_millis(20),
            async move {
                let _ = stopped.await;
            },
        )
        .await
        .expect("a shutdown while waiting is not a failure");
    }

    /// The responder can take the port and the group. Multicast is TTL-scoped
    /// to the local network, which is what keeps GDM from becoming the
    /// reflection amplifier the protocol has been abused as.
    #[tokio::test]
    async fn the_gdm_responder_binds_its_port_and_joins_the_search_group() {
        let socket = gdm_socket(0).await.expect("an ephemeral GDM socket");
        let addr = socket.local_addr().expect("addr");
        assert_ne!(addr.port(), 0, "the socket must really be bound");
        assert!(addr.ip().is_unspecified(), "GDM listens on every interface");

        // And a port already held is refused rather than silently shared.
        let taken = gdm_socket(addr.port()).await;
        assert!(
            taken.is_err(),
            "two responders on one port would answer each other's searches"
        );
    }

    /// A free UDP port, released before it is handed back. Racy in principle
    /// and stable in practice; the alternative is hard-coding 32414 and
    /// fighting whatever else on the machine wants it.
    fn free_udp_port() -> u16 {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind");
        probe.local_addr().expect("addr").port()
    }

    /// A LAN-bound server answers Plex clients' searches with its own name and
    /// HTTP port. This is the wiring `plurxd` does at boot, driven end to end:
    /// the responder is spawned, it binds, and it replies.
    ///
    /// One test rather than several because `PLURX_GDM_PORT` is process-wide.
    #[tokio::test]
    async fn a_lan_server_answers_plex_searches_on_the_configured_port() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = config_in(tmp.path());
        config.server.name = "Loft".to_owned();
        config.server.bind = "0.0.0.0:32400".parse().expect("addr");

        // Unset, a LAN bind answers on the protocol's own port. Asserted here
        // rather than beside the loopback case because this test is the one
        // that owns the variable for the length of its run.
        std::env::remove_var("PLURX_GDM_PORT");
        assert_eq!(
            gdm_responder_port(true, config.server.bind),
            Some(plurx_compat_plex::gdm::GDM_PORT)
        );

        // An unparseable override disables the responder rather than falling
        // back to a port the operator did not ask for.
        std::env::set_var("PLURX_GDM_PORT", "not-a-port");
        assert_eq!(gdm_responder_port(true, config.server.bind), None);

        let port = free_udp_port();
        std::env::set_var("PLURX_GDM_PORT", port.to_string());
        assert_eq!(gdm_responder_port(true, config.server.bind), Some(port));

        // The full boot-time wiring: discovery starts the responder, and the
        // stub advertiser keeps Bonjour off the network.
        assert!(start_discovery(&config, "instance-1", advertiser_that_fails).is_none());

        let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("client");
        client
            .connect(("127.0.0.1", port))
            .await
            .expect("connect to the responder");

        let mut buf = [0u8; 2048];
        let mut reply = None;
        // The responder is spawned, so the first datagram can outrun its bind.
        for _ in 0..40 {
            client
                .send(b"M-SEARCH * HTTP/1.0\r\n\r\n")
                .await
                .expect("search");
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf)).await
            {
                reply = Some(String::from_utf8_lossy(&buf[..n]).into_owned());
                break;
            }
        }
        let reply = reply.expect("the GDM responder must answer a search");
        assert!(reply.contains("Name: Loft"), "{reply}");
        assert!(reply.contains("Port: 32400"), "{reply}");
        assert!(reply.contains("Resource-Identifier: instance-1"), "{reply}");
        std::env::remove_var("PLURX_GDM_PORT");
    }

    /// Compose deployments turn the in-process advertiser off and run
    /// `plurxd advertise` in a host-network companion, because multicast
    /// cannot cross a bridge. One test, because the variable is process-wide.
    #[test]
    fn compose_can_turn_the_in_process_advertiser_off() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = config_in(tmp.path());
        config.server.bind = "0.0.0.0:32400".parse().expect("addr");

        assert!(mdns_advertising_enabled(), "on unless told otherwise");
        // Enabled, the daemon a successful advertiser produced reaches the
        // caller: `serve` is what withdraws it on drain, so a `start_bonjour`
        // that dropped it here would leave the record on the LAN for as long
        // as it took to age out. Asserted in this test because it is the one
        // that owns `PLURX_MDNS_ADVERTISE` for the length of its run.
        let daemon = start_bonjour(&config, "instance-1", advertiser_without_a_record, true)
            .expect("an enabled, successful advertiser must yield its daemon");
        daemon.shutdown().expect("the returned daemon must be live");

        std::env::set_var("PLURX_MDNS_ADVERTISE", "0");
        assert!(!mdns_advertising_enabled());
        assert!(
            start_bonjour(&config, "instance-1", advertiser_that_fails, true).is_none(),
            "a disabled advertiser is never even started"
        );
        std::env::remove_var("PLURX_MDNS_ADVERTISE");
        assert!(mdns_advertising_enabled());
    }

    /// Run a closure with every event it emits captured, and hand back the log.
    ///
    /// A `tracing` macro only evaluates its fields when a subscriber wants
    /// them, so a test with no subscriber is not exercising the log line it
    /// thinks it is. Reuses the buffer layer the settings page reads.
    fn captured(body: impl FnOnce()) -> Arc<logbuf::LogBuffer> {
        use tracing_subscriber::prelude::*;
        let logs = Arc::new(logbuf::LogBuffer::new(64));
        let subscriber =
            tracing_subscriber::registry().with(logbuf::BufferLayer(Arc::clone(&logs)));
        tracing::subscriber::with_default(subscriber, body);
        logs
    }

    /// The async counterpart: capture on this thread until the guard drops.
    ///
    /// `#[tokio::test]` runs a current-thread runtime, so the tasks the code
    /// under test spawns are polled on this thread too and their events land in
    /// the same buffer. Attaching the subscriber to one future would not reach
    /// them, and a warning from a spawned task is exactly what a best-effort
    /// subsystem reports itself with.
    fn capturing(logs: &Arc<logbuf::LogBuffer>) -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::prelude::*;
        tracing::subscriber::set_default(
            tracing_subscriber::registry().with(logbuf::BufferLayer(Arc::clone(logs))),
        )
    }

    /// A daemon that records what it was asked to publish and can be told to
    /// refuse either call. Nothing here opens a socket, which is what keeps
    /// the mandatory unit and coverage gates off the local network.
    #[derive(Default)]
    struct FakeDaemon {
        monitor_refuses: bool,
        register_refuses: bool,
        events: std::sync::Mutex<Vec<mdns_sd::DaemonEvent>>,
        registered: std::sync::Mutex<Vec<mdns_sd::ServiceInfo>>,
    }

    impl MdnsRegistrar for FakeDaemon {
        fn monitor(&self) -> anyhow::Result<Box<dyn Iterator<Item = mdns_sd::DaemonEvent> + Send>> {
            if self.monitor_refuses {
                anyhow::bail!("monitoring mDNS daemon");
            }
            let events = std::mem::take(&mut *self.events.lock().expect("lock"));
            Ok(Box::new(events.into_iter()))
        }

        fn register(&self, service: mdns_sd::ServiceInfo) -> anyhow::Result<()> {
            if self.register_refuses {
                anyhow::bail!("registering plurx Bonjour service");
            }
            self.registered.lock().expect("lock").push(service);
            Ok(())
        }
    }

    /// The whole of what a successful registration publishes: the record every
    /// client on the LAN discovers this server by. A wrong port or a missing
    /// identity property here is a server nothing can connect to, and the log
    /// line is how an operator confirms which port went out.
    #[test]
    fn a_registration_publishes_the_record_clients_discover() {
        let daemon = FakeDaemon::default();
        let logs = captured(|| {
            register_advertiser(
                &daemon,
                "plurxd-selftest-instance",
                "Living Room",
                "192.168.1.9:32400".parse().expect("addr"),
                "0.2.0",
            )
            .expect("a daemon that accepts both calls registers");
        });

        let registered = daemon.registered.lock().expect("lock");
        assert_eq!(registered.len(), 1, "exactly one record is published");
        let service = &registered[0];
        assert_eq!(service.get_type(), MDNS_SERVICE_TYPE);
        assert_eq!(service.get_port(), 32400, "the bind's port, not a default");
        assert!(
            service.get_fullname().contains("Living Room"),
            "{}",
            service.get_fullname()
        );

        let announced = logs.tail("info", 8);
        let line = announced
            .iter()
            .find(|entry| {
                entry
                    .message
                    .contains("Bonjour discovery advertiser registered")
            })
            .unwrap_or_else(|| panic!("a registration must be logged: {announced:?}"));
        assert!(
            line.message.contains("port=32400"),
            "the log must name the port that was published: {}",
            line.message
        );
    }

    /// A record that cannot be built is the third way a registration fails,
    /// and the one an operator can cause by typing: a server name past the
    /// 255-byte TXT limit. It has to surface as an error the boot carries on
    /// from, never a panic, and nothing may be published for it.
    #[test]
    fn a_server_name_too_long_to_advertise_is_an_error_not_a_panic() {
        let daemon = FakeDaemon::default();
        let error = format!(
            "{:#}",
            register_advertiser(
                &daemon,
                "instance-1",
                &"Living Room ".repeat(30),
                "192.168.1.9:32400".parse().expect("addr"),
                "0.2.0",
            )
            .expect_err("a record that cannot be built is an error")
        );
        assert!(error.contains("255-byte limit"), "{error}");
        assert!(
            daemon.registered.lock().expect("lock").is_empty(),
            "nothing may be published for a record that was never built"
        );
    }

    /// The real daemon's monitor hands back a stream that ends when the daemon
    /// does. That is what lets the monitor thread exit with the server instead
    /// of outliving it; nothing is registered, so nothing reaches the network.
    #[test]
    fn a_real_daemons_monitor_ends_with_the_daemon() {
        let daemon = daemon_off_the_mdns_port().expect("mDNS daemon");
        let mut events = MdnsRegistrar::monitor(&daemon).expect("a live daemon can be monitored");
        daemon.shutdown().expect("withdrawal");

        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Drains whatever the shutdown itself emitted, then ends.
            while events.next().is_some() {}
            let _ = done.send(());
        });
        ended
            .recv_timeout(Duration::from_secs(5))
            .expect("a daemon that is gone must end its event stream");
    }

    /// A GDM port the kernel will not give up is the ordinary case on a box
    /// already running Plex. It must leave the server running and say so —
    /// discovery is best-effort, and a warning is the only trace there is.
    #[tokio::test]
    async fn a_gdm_port_that_cannot_be_bound_is_a_warning_not_a_failure() {
        // Hold the port first, so the responder's bind is refused. Loopback
        // UDP only: this never joins a multicast group.
        let held = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("holding a port");
        let taken = held.local_addr().expect("addr").port();

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        let logs = Arc::new(logbuf::LogBuffer::new(64));
        let _capture = capturing(&logs);
        // The responder is spawned, not awaited — a bind that fails must not be
        // something the caller waits on.
        spawn_gdm_responder(&config, "instance-1".to_owned(), taken);
        for _ in 0..50 {
            if !logs.tail("warn", 8).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let warned = logs.tail("warn", 8);
        let line = warned
            .iter()
            .find(|entry| {
                entry
                    .message
                    .contains("GDM discovery responder unavailable")
            })
            .unwrap_or_else(|| panic!("a refused GDM bind must be logged: {warned:?}"));
        assert!(
            line.message.contains("binding GDM port"),
            "the operator needs the reason, not just the symptom: {}",
            line.message
        );
    }

    /// The one line an operator reads to confirm which server, which node, and
    /// which data directory this process actually came up on. A cluster makes
    /// the two identities easy to confuse, so both have to be in it.
    #[test]
    fn the_startup_line_names_both_identities_and_the_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_in(tmp.path());
        let identity = plurx_core::cluster::ClusterIdentity {
            cluster_id: "cluster-abc".to_owned(),
            node_id: "node-2".to_owned(),
            raft_id: 1,
        };

        let logs = captured(|| log_startup(&config, &identity));
        let started = logs.tail("info", 8);
        let line = started
            .iter()
            .find(|entry| entry.message.contains("plurxd starting"))
            .unwrap_or_else(|| panic!("startup must be logged: {started:?}"));
        for expected in [
            "instance_id=cluster-abc",
            "node_id=node-2",
            crate::version::SEMVER,
            &tmp.path().display().to_string(),
        ] {
            assert!(
                line.message.contains(expected),
                "the startup line must name {expected}: {}",
                line.message
            );
        }
    }

    /// Monitoring is established before the record exists. A daemon that
    /// cannot be monitored publishes nothing at all rather than putting a
    /// record on the network no failure would ever be reported for.
    #[test]
    fn a_daemon_that_cannot_be_monitored_publishes_nothing() {
        let daemon = FakeDaemon {
            monitor_refuses: true,
            ..Default::default()
        };
        let error = format!(
            "{:#}",
            register_advertiser(
                &daemon,
                "instance-1",
                "Living Room",
                "192.168.1.9:32400".parse().expect("addr"),
                "0.2.0",
            )
            .expect_err("an unmonitorable daemon is an error")
        );
        assert!(error.contains("monitoring mDNS daemon"), "{error}");
        assert!(
            daemon.registered.lock().expect("lock").is_empty(),
            "a record must not be published before its monitor exists"
        );
    }

    /// A refused registration reports why. `start_bonjour` logs this and
    /// carries on unadvertised — discovery is best-effort, so it must be an
    /// error the caller can read, never a panic or a silent success.
    #[test]
    fn a_refused_registration_reports_why() {
        let daemon = FakeDaemon {
            register_refuses: true,
            ..Default::default()
        };
        let error = format!(
            "{:#}",
            register_advertiser(
                &daemon,
                "instance-1",
                "Living Room",
                "192.168.1.9:32400".parse().expect("addr"),
                "0.2.0",
            )
            .expect_err("a daemon that refuses the record is an error")
        );
        assert!(
            error.contains("registering plurx Bonjour service"),
            "{error}"
        );
    }

    /// The monitor is the only place a discovery failure can surface, because
    /// `mdns-sd` opens its sockets after `register` has returned. It logs the
    /// errors, ignores the ordinary traffic, and ends when the daemon does.
    #[test]
    fn the_monitor_logs_daemon_errors_and_nothing_else() {
        let logs = captured(|| {
            log_mdns_events(
                vec![
                    mdns_sd::DaemonEvent::Announce(
                        "plurx".to_owned(),
                        "192.168.1.9:32400".to_owned(),
                    ),
                    mdns_sd::DaemonEvent::Error(mdns_sd::Error::Msg(
                        "no usable multicast interface".to_owned(),
                    )),
                ]
                .into_iter(),
            );
        });

        let warned = logs.tail("warn", 16);
        assert_eq!(warned.len(), 1, "an announce is not a failure: {warned:?}");
        assert!(
            warned[0].message.contains("no usable multicast interface"),
            "the daemon's own reason must reach the operator: {}",
            warned[0].message
        );
    }

    /// A daemon whose events have run out ends the monitor thread rather than
    /// leaving one parked for the life of the process.
    #[test]
    fn the_monitor_ends_when_the_daemon_does() {
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            log_mdns_events(std::iter::empty());
            let _ = done.send(());
        });
        ended
            .recv_timeout(Duration::from_secs(5))
            .expect("an exhausted event stream must end the monitor, not park it");
    }

    /// SIGTERM is what `docker stop` sends, and it has to start the drain
    /// rather than kill the process mid-stream.
    #[tokio::test]
    async fn sigterm_starts_the_drain() {
        // Registering our own stream first replaces the default terminate
        // action process-wide, so raising the signal below cannot kill the
        // test runner even if the task under test has not been polled yet.
        let mut guard = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM stream");
        let waiting = tokio::spawn(shutdown_signal());
        tokio::task::yield_now().await;

        // SAFETY: raising a signal this process has a handler installed for.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        guard.recv().await;

        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("SIGTERM must end the wait")
            .expect("join");
    }

    /// A drain is bounded. A paced remux holds its connection open for a
    /// quarter of the film's runtime, so waiting for every request to finish
    /// means never finishing — `docker stop` would spend its grace period
    /// achieving nothing and then SIGKILL, which surfaces as exit 137.
    #[tokio::test]
    async fn a_connection_that_never_finishes_does_not_hold_the_drain_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = booted_state(tmp.path());
        let progress = Arc::clone(&state.progress);
        // A route that never responds, standing in for a paced remux.
        let app = axum::Router::new().route(
            "/forever",
            axum::routing::get(std::future::pending::<&'static str>),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(serve(listener, app, progress, None, async move {
            let _ = stopped.await;
        }));

        // Hold a request open, then ask the server to stop.
        let hanging = tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(format!("http://{addr}/forever"))
                .send()
                .await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.send(()).expect("stop");

        let outcome = tokio::time::timeout(
            SHUTDOWN_DRAIN_TIMEOUT + PROGRESS_DRAIN_TIMEOUT + Duration::from_secs(5),
            served,
        )
        .await
        .expect("the drain must be bounded, not indefinite")
        .expect("join");
        outcome.expect("a timed-out drain is still an orderly exit 0");
        hanging.abort();
    }

    /// A service instance with no name is still a service. An empty label
    /// would be a DNS-SD registration failure rather than an unnamed server.
    #[test]
    fn an_unnamed_server_still_registers_under_something() {
        let info = mdns_service_info(
            "",
            "   ",
            "0.0.0.0:32400".parse().expect("socket address"),
            "0.2.0",
        )
        .expect("service info");
        assert_eq!(info.get_property_val_str("name"), Some("plurx"));
        // With no instance id to derive from, the host label falls back too.
        assert_eq!(info.get_hostname(), "plurx-server.local.");
    }

    /// A DNS-SD service instance is one DNS label of 63 bytes. A custom name
    /// long enough to overflow it keeps the name and drops the address —
    /// a registration failure would be worse than a less specific label.
    #[test]
    fn a_name_too_long_for_dns_sd_keeps_the_name_and_drops_the_address() {
        let address: IpAddr = "192.168.100.200".parse().expect("IP");
        let long = "L".repeat(60);
        assert_eq!(
            discovery_display_name(&long, Some("m6"), Some(&address)),
            long,
            "the address must be dropped rather than overflow the label"
        );
        // One that still fits keeps both.
        assert_eq!(
            discovery_display_name("Loft", Some("m6"), Some(&address)),
            "Loft · 192.168.100.200"
        );
    }

    /// The companion's whole loop is "ask again in two seconds". A request
    /// with no timeout against a half-open connection would hang that loop
    /// forever and the server would never be advertised.
    #[tokio::test]
    async fn the_companion_client_gives_up_on_a_silent_server() {
        let client = discovery_client().expect("client");
        // A socket that accepts and then says nothing: without a timeout this
        // request never returns.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let held = std::thread::spawn(move || {
            let accepted = listener.accept();
            std::thread::sleep(Duration::from_secs(8));
            drop(accepted);
        });

        let started = std::time::Instant::now();
        let outcome = client
            .get(format!("http://{addr}/api/v1/server"))
            .send()
            .await;
        assert!(outcome.is_err(), "a silent server must not hang the loop");
        let waited = started.elapsed();
        assert!(waited < Duration::from_secs(7), "it waited {waited:?}");
        held.join().expect("thread");
    }

    /// `plurxd advertise` refuses a URL it could never reach, rather than
    /// publishing a Bonjour record pointing at nothing.
    #[tokio::test]
    async fn the_advertise_subcommand_refuses_an_unreachable_url() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let error = format!(
            "{:#}",
            dispatch(
                Command::Advertise {
                    server: "ftp://192.168.1.9:21".to_owned(),
                },
                config_in(tmp.path()),
            )
            .await
            .expect_err("not an HTTP server")
        );
        assert!(error.contains("http or https"), "{error}");
    }

    /// These read the machine rather than a fixture, so what is asserted is the
    /// contract every caller depends on: a usable label or nothing at all.
    /// A blank or loopback answer here becomes a discovery record no client can
    /// use.
    #[test]
    fn the_machine_identity_is_usable_or_absent_but_never_blank() {
        if let Some(host) = system_hostname() {
            assert!(!host.trim().is_empty());
            assert!(!host.eq_ignore_ascii_case("localhost"));
            assert!(!host.ends_with(".local"));
        }
        if let Some(address) = primary_lan_address() {
            assert!(!address.is_loopback(), "{address}");
            assert!(!address.is_unspecified(), "{address}");
        }
    }

    #[test]
    fn refresh_metadata_can_target_one_library() {
        let cli = Cli::try_parse_from(["plurxd", "refresh-metadata", "--library", "7"])
            .expect("refresh command");
        assert!(matches!(
            cli.command,
            Some(Command::RefreshMetadata { library: Some(7) })
        ));
    }

    #[test]
    fn gdm_port_defaults_and_accepts_an_override() {
        assert_eq!(
            gdm_bind_port(None).expect("default GDM port"),
            plurx_compat_plex::gdm::GDM_PORT
        );
        assert_eq!(
            gdm_bind_port(Some("32415")).expect("GDM port override"),
            32415
        );
        assert!(gdm_bind_port(Some("not-a-port")).is_err());
    }

    #[test]
    fn compose_can_disable_only_the_in_process_advertiser() {
        for value in ["0", "false", "FALSE", " no ", "off"] {
            assert!(!mdns_advertising_value_enabled(value), "{value:?}");
        }
        for value in ["1", "true", "yes", "on", "anything-else"] {
            assert!(mdns_advertising_value_enabled(value), "{value:?}");
        }
    }

    #[test]
    fn discovery_companion_decodes_the_public_server_identity() {
        let info: DiscoveryServerInfo = serde_json::from_str(
            r#"{
                "name": "Living Room",
                "version": "0.2.0",
                "build": "v0.2.0-4-gabc",
                "instance_id": "server-id",
                "uptime_seconds": 12,
                "setup_required": false,
                "android_app": true
            }"#,
        )
        .expect("server identity");

        assert_eq!(info.name, "Living Room");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.instance_id, "server-id");
    }

    #[test]
    fn default_discovery_name_uses_the_machine_but_custom_names_win() {
        let address: IpAddr = "192.168.1.20".parse().expect("IP");
        assert_eq!(
            discovery_display_name("plurx", Some("m6"), Some(&address)),
            "m6 · 192.168.1.20"
        );
        assert_eq!(
            discovery_display_name("PLURX", Some("nuc4.local."), None),
            "nuc4"
        );
        assert_eq!(
            discovery_display_name("Living Room", Some("m6"), Some(&address)),
            "Living Room · 192.168.1.20"
        );
        assert_eq!(
            discovery_display_name("plurx", Some("localhost"), None),
            "plurx"
        );
        assert_eq!(discovery_display_name("plurx", None, None), "plurx");
    }

    #[test]
    fn wildcard_bind_advertises_the_native_plurx_contract() {
        let info = mdns_service_info(
            "550e8400-e29b-41d4-a716-446655440000",
            "Living Room",
            "0.0.0.0:32400".parse().expect("socket address"),
            "0.2.0",
        )
        .expect("service info");

        assert_eq!(info.get_type(), MDNS_SERVICE_TYPE);
        assert_eq!(info.get_port(), 32400);
        assert_eq!(info.get_hostname(), "plurx-550e8400e29b.local.");
        assert_eq!(info.get_property_val_str("name"), Some("Living Room"));
        assert_eq!(info.get_property_val_str("version"), Some("0.2.0"));
        assert_eq!(info.get_property_val_str("api"), Some("/api/v1"));
        assert!(info.is_addr_auto());
    }

    #[test]
    fn explicit_bind_advertises_only_that_address() {
        let address = "192.168.1.20".parse().expect("IP address");
        let info = mdns_service_info(
            "server-id",
            "plurx",
            "192.168.1.20:32400".parse().expect("socket address"),
            "0.2.0",
        )
        .expect("service info");

        assert!(!info.is_addr_auto());
        assert!(info.get_addresses().contains(&address));
    }
}

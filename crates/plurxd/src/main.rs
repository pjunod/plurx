mod admission;
mod cachekeep;
mod copyseg;
mod ffmpeg;
mod http;
mod logbuf;
mod meter;
mod pipeprobe;
mod playstart;
mod produce;
mod progressive;
mod schedule;
mod state;
mod storeprobe;
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
use plurx_core::config::Config;
use plurx_core::domain::LibraryKind;
use plurx_core::metadata::{self, AniListClient, TmdbClient};
use plurx_core::store::{keys, LibraryStore, SettingsStore, SqliteStore, Store, UserStore};
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
    /// Reset a user's password directly in the database — the recovery path
    /// when an admin password is forgotten (admins reset *other* users in
    /// the web UI). Safe while the server runs (WAL); revokes the user's
    /// sessions. In Docker: docker exec -it plurxd plurxd reset-password NAME
    ResetPassword {
        /// Username whose password to reset.
        username: String,
        /// New password (min 8 chars). Omit to be prompted on stdin, which
        /// keeps it out of your shell history.
        #[arg(long)]
        password: Option<String>,
    },
    /// Force provider metadata and artwork back into the local cache. Useful
    /// after an artwork-quality upgrade; safe beside the running server.
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
    match cli.command.unwrap_or(Command::Run) {
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

/// Re-fetch provider-backed metadata without requiring an HTTP admin token.
/// SQLite WAL permits this maintenance process beside the daemon, and artwork
/// writes replace the same cache files the ordinary refresh path owns.
async fn refresh_metadata(config: &Config, library_id: Option<i64>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.storage.data_dir)?;
    let artwork_dir = config.storage.data_dir.join("artwork");
    std::fs::create_dir_all(&artwork_dir)?;
    let db_path = config.storage.data_dir.join("plurx.db");
    let store = SqliteStore::open(&db_path)
        .with_context(|| format!("opening database {}", db_path.display()))?;
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
            LibraryKind::Home => {
                println!("{}: skipped local artwork", library.name);
            }
            LibraryKind::Shows if library.anime => {
                let report = metadata::enrich_anime_library(
                    &store,
                    &AniListClient::new(),
                    &artwork_dir,
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
                    &store,
                    &TmdbClient::new(key),
                    &artwork_dir,
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
/// sessions. WAL + busy_timeout make this safe beside a running server.
async fn reset_password(
    config: &Config,
    username: &str,
    password: Option<String>,
) -> anyhow::Result<()> {
    let password = match password {
        Some(p) => p,
        None => {
            eprint!("New password for `{username}` (min 8 chars): ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_owned()
        }
    };
    anyhow::ensure!(
        password.len() >= 8,
        "password must be at least 8 characters"
    );

    let db_path = config.storage.data_dir.join("plurx.db");
    let store =
        SqliteStore::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let user = store
        .get_user_by_username(username)
        .await?
        .with_context(|| format!("no user named `{username}`"))?;
    let hash =
        plurx_core::auth::hash_password(&password).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    store.set_password(user.id, &hash).await?;
    let revoked = store.delete_tokens_for_user(user.id).await?;
    println!("password reset for `{username}`; {revoked} session(s) revoked");
    Ok(())
}

async fn run(config: Config) -> anyhow::Result<()> {
    // Console logging plus a bounded in-memory ring the admin UI can read.
    // The EnvFilter is global, so both sinks see the same events.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let logs = Arc::new(logbuf::LogBuffer::default());
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("PLURX_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .with(logbuf::BufferLayer(Arc::clone(&logs)))
        .init();

    std::fs::create_dir_all(&config.storage.data_dir).with_context(|| {
        format!(
            "creating data directory {}",
            config.storage.data_dir.display()
        )
    })?;
    let db_path = config.storage.data_dir.join("plurx.db");
    let store: Arc<dyn Store> = Arc::new(
        SqliteStore::open(&db_path)
            .with_context(|| format!("opening database {}", db_path.display()))?,
    );

    // Artwork cache and transcode scratch live under the data dir.
    let artwork_dir = config.storage.data_dir.join("artwork");
    std::fs::create_dir_all(&artwork_dir)
        .with_context(|| format!("creating artwork directory {}", artwork_dir.display()))?;
    // Finished transcodes, kept across restarts. Deliberately a sibling of the
    // session scratch below rather than a child: that one is cleared at every
    // boot, and a cache that empties on restart is a warm-up cost with none of
    // the benefit.
    let cache_dir = config.storage.data_dir.join("cache").join("transcode");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache directory {}", cache_dir.display()))?;
    // Extracted subtitles, cached by source fingerprint. Beside the transcode
    // cache and not inside the session scratch, which is cleared at boot.
    let subs_dir = config.storage.data_dir.join("cache").join("subs");
    std::fs::create_dir_all(&subs_dir)
        .with_context(|| format!("creating subtitle cache {}", subs_dir.display()))?;
    let transcode_dir = config.storage.data_dir.join("transcode");
    // Clear any stale sessions from a previous run, then recreate.
    let _ = std::fs::remove_dir_all(&transcode_dir);
    std::fs::create_dir_all(&transcode_dir)
        .with_context(|| format!("creating transcode directory {}", transcode_dir.display()))?;

    // Detect available hardware encoders once at startup.
    let ffmpeg = std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned());
    let encoder_caps = plurx_core::transcode::detect_encoders(&ffmpeg).await;

    // PLURX_HWACCEL (documented since Phase 2, previously read by nothing)
    // seeds the stored encoder preference at boot; env wins over the setting.
    if let Ok(pref) = std::env::var("PLURX_HWACCEL") {
        if !pref.is_empty() {
            store
                .put_setting(keys::HWACCEL, pref.to_lowercase().trim())
                .await?;
        }
    }
    let hwaccel_pref = store
        .get_setting(keys::HWACCEL)
        .await?
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "auto".to_owned());
    let probe_pref = if hwaccel_pref == "auto" {
        String::new()
    } else {
        hwaccel_pref.clone()
    };
    let encoder_selected = encoder_caps.choose(&probe_pref).label().to_owned();
    let ffprobe = std::env::var("PLURX_FFPROBE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned());
    // Which tone-map graph this node may use. After encoder detection, because
    // a graph is only worth probing if it can feed the encoder that won. Costs
    // a few seconds on a box with a GPU worth testing and nothing at all on one
    // without.
    let tone_map = pipeprobe::probe(&transcode_dir, encoder_caps.choose(&probe_pref)).await;
    let system = SystemInfo {
        data_dir: config.storage.data_dir.display().to_string(),
        ffmpeg_version: ffmpeg_version(&ffmpeg).await,
        ffmpeg: ffmpeg.clone(),
        ffprobe,
        hwaccel_pref,
        encoders: encoder_caps.clone(),
        encoder_selected,
        tone_map,
        dovi_rpu: crate::ffmpeg::has_dovi_rpu().await,
    };

    let instance_id = store.instance_id().await?;
    tracing::info!(
        version = crate::version::SEMVER,
        build = crate::version::BUILD,
        server_name = %config.server.name,
        %instance_id,
        data_dir = %config.storage.data_dir.display(),
        "plurxd starting"
    );

    let state = AppState::new(
        config.server.name.clone(),
        store,
        crate::state::Dirs {
            artwork: artwork_dir,
            transcode: transcode_dir,
            cache: cache_dir,
            subs: subs_dir,
        },
        instance_id.clone(),
        encoder_caps,
        system,
        logs,
    );
    // Reap idle transcode sessions in the background.
    tokio::spawn(std::sync::Arc::clone(&state.transcode).reap_loop());

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

    let app = http::router(state);
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("binding {}", config.server.bind))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    // Advertise only a server that another LAN device can actually reach.
    // A loopback-only bind is common in proxy deployments; publishing it would
    // produce a tempting setup button that can never connect.
    let lan_discovery = !config.server.bind.ip().is_loopback();
    if lan_discovery {
        // GDM keeps direct-connect Plex clients working. Bonjour is the native
        // plurx contract used by the Apple apps. Both are best-effort: failure
        // to join multicast must not take down the HTTP server.
        let gdm_id = instance_id.clone();
        let gdm_name = config.server.name.clone();
        let http_port = config.server.bind.port();
        match gdm_bind_port(std::env::var("PLURX_GDM_PORT").ok().as_deref()) {
            Ok(gdm_port) => {
                tokio::spawn(async move {
                    if let Err(e) = gdm_responder(gdm_id, gdm_name, http_port, gdm_port).await {
                        tracing::warn!(error = %e, "GDM discovery responder unavailable");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "GDM discovery responder disabled");
            }
        }
    } else {
        tracing::info!(bind = %config.server.bind, "LAN discovery disabled for loopback bind");
    }
    let host_name = system_hostname();
    let lan_address = primary_lan_address();
    let discovery_name = discovery_display_name(
        &config.server.name,
        host_name.as_deref(),
        lan_address.as_ref(),
    );
    let mdns = if lan_discovery && mdns_advertising_enabled() {
        match start_mdns_advertiser(
            &instance_id,
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
    } else {
        if lan_discovery {
            tracing::info!("in-process Bonjour advertising disabled");
        }
        None
    };

    // Drain in-flight requests on SIGTERM, but not forever. A playback
    // response is a stream: a paced remux holds its connection open for a
    // quarter of the film's runtime, and an HLS playlist poll is only ever
    // moments from the next one. Waiting for all of them to finish means
    // never finishing, so `docker stop` spends its ten-second grace period
    // achieving nothing and then SIGKILLs us — which surfaces as exit 137,
    // indistinguishable at a glance from an out-of-memory kill. Bounding the
    // drain turns that into an orderly exit 0 with a line in the log saying
    // what was still open.
    let (drain_started, drain_signal) = tokio::sync::oneshot::channel();
    // `WithGracefulShutdown` is IntoFuture rather than Future, and select!
    // needs the future itself to poll it more than once.
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PLURX_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init()
        .ok();

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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building discovery HTTP client")?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let mut failed_attempts = 0_u64;
    let info = loop {
        match fetch_discovery_server_info(&client, info_url.clone()).await {
            Ok(info) => break info,
            Err(error) => {
                failed_attempts += 1;
                if failed_attempts == 1 || failed_attempts.is_multiple_of(15) {
                    tracing::warn!(
                        %error,
                        server = %base,
                        "waiting for server before advertising Bonjour"
                    );
                }
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        }
    };

    let host_name = system_hostname();
    let lan_address = primary_lan_address();
    let discovery_name =
        discovery_display_name(&info.name, host_name.as_deref(), lan_address.as_ref());
    let daemon = start_mdns_advertiser(
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
    let monitor = daemon.monitor().context("monitoring mDNS daemon")?;
    std::thread::Builder::new()
        .name("plurx-mdns-monitor".to_owned())
        .spawn(move || {
            while let Ok(event) = monitor.recv() {
                if let mdns_sd::DaemonEvent::Error(error) = event {
                    tracing::warn!(%error, "Bonjour discovery daemon error");
                }
            }
        })
        .context("starting mDNS monitor")?;

    let service = mdns_service_info(instance_id, name, bind, version)?;
    daemon
        .register(service)
        .context("registering plurx Bonjour service")?;
    tracing::info!(
        service = MDNS_SERVICE_TYPE,
        server_name = name,
        port = bind.port(),
        "Bonjour discovery advertiser registered"
    );
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
    String::from_utf8_lossy(&out.stdout)
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

    // Plex clients parse what GDM advertises, so this stays bare semver — the
    // git build stamp would not survive their version comparisons.
    let version = crate::version::SEMVER;
    let mut buf = [0u8; 1024];
    loop {
        let (n, addr) = socket.recv_from(&mut buf).await?;
        if plurx_compat_plex::gdm::is_search(&buf[..n]) {
            let resp = plurx_compat_plex::gdm::response(&instance_id, &name, version, http_port);
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

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining");
}

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
mod discovery_tests {
    use super::*;

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

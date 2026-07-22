mod http;
mod logbuf;
mod state;
mod transcode;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use plurx_core::config::Config;
use plurx_core::store::{keys, SqliteStore, Store, UserStore};
use tracing_subscriber::EnvFilter;

use crate::state::{AppState, SystemInfo};

#[derive(Parser)]
#[command(name = "plurxd", version, about = "plurx media server daemon")]
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
    /// Reset a user's password directly in the database — the recovery path
    /// when an admin password is forgotten (admins reset *other* users in
    /// the web UI). Safe while the server runs (WAL); revokes the user's
    /// sessions. In Docker: docker exec -it plurxd plurxd reset-password paul
    ResetPassword {
        /// Username whose password to reset.
        username: String,
        /// New password (min 8 chars). Omit to be prompted on stdin, which
        /// keeps it out of your shell history.
        #[arg(long)]
        password: Option<String>,
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
        Command::ResetPassword { username, password } => {
            reset_password(&config, &username, password).await
        }
    }
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
    let encoder_selected = encoder_caps
        .choose(if hwaccel_pref == "auto" {
            ""
        } else {
            hwaccel_pref.as_str()
        })
        .label()
        .to_owned();
    let ffprobe = std::env::var("PLURX_FFPROBE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned());
    let system = SystemInfo {
        data_dir: config.storage.data_dir.display().to_string(),
        ffmpeg_version: ffmpeg_version(&ffmpeg).await,
        ffmpeg: ffmpeg.clone(),
        ffprobe,
        hwaccel_pref,
        encoders: encoder_caps.clone(),
        encoder_selected,
    };

    let instance_id = store.instance_id().await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        server_name = %config.server.name,
        %instance_id,
        data_dir = %config.storage.data_dir.display(),
        "plurxd starting"
    );

    let state = AppState::new(
        config.server.name.clone(),
        store,
        artwork_dir,
        transcode_dir,
        encoder_caps,
        system,
        logs,
    );
    // Reap idle transcode sessions in the background.
    tokio::spawn(std::sync::Arc::clone(&state.transcode).reap_loop());

    // GDM responder for Plex-client LAN discovery (best-effort).
    let gdm_id = instance_id.clone();
    let gdm_name = config.server.name.clone();
    let gdm_port = config.server.bind.port();
    tokio::spawn(async move {
        if let Err(e) = gdm_responder(gdm_id, gdm_name, gdm_port).await {
            tracing::warn!(error = %e, "GDM discovery responder unavailable");
        }
    });

    let app = http::router(state);
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("binding {}", config.server.bind))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shutdown complete");
    Ok(())
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
async fn gdm_responder(instance_id: String, name: String, port: u16) -> anyhow::Result<()> {
    use std::net::Ipv4Addr;

    let socket =
        tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, plurx_compat_plex::gdm::GDM_PORT))
            .await
            .context("binding GDM port")?;
    socket
        .join_multicast_v4(
            plurx_compat_plex::gdm::GDM_MULTICAST_ADDR.parse()?,
            Ipv4Addr::UNSPECIFIED,
        )
        .context("joining GDM multicast group")?;
    tracing::info!(
        port = plurx_compat_plex::gdm::GDM_PORT,
        "GDM discovery responder listening"
    );

    let version = env!("CARGO_PKG_VERSION");
    let mut buf = [0u8; 1024];
    loop {
        let (n, addr) = socket.recv_from(&mut buf).await?;
        if plurx_compat_plex::gdm::is_search(&buf[..n]) {
            let resp = plurx_compat_plex::gdm::response(&instance_id, &name, version, port);
            if let Err(e) = socket.send_to(&resp, addr).await {
                tracing::warn!(error = %e, "GDM response send failed");
            }
        }
    }
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

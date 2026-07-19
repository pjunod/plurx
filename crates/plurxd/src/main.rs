mod http;
mod state;
mod transcode;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use plurx_core::config::Config;
use plurx_core::store::{SqliteStore, Store};
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

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
    }
}

async fn run(config: Config) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PLURX_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
    );
    // Reap idle transcode sessions in the background.
    tokio::spawn(std::sync::Arc::clone(&state.transcode).reap_loop());
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

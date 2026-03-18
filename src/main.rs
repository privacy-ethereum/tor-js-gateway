mod dir;
mod server;
mod store;
mod sync;
mod ws_proxy;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::Parser;

use arti_client::{TorClient, TorClientConfig};

#[derive(Parser)]
#[command(name = "tor-js-gateway")]
#[command(about = "Long-running Tor directory cache — syncs like a relay")]
struct Cli {
    /// Output directory for cached documents
    #[arg(short, long)]
    output_dir: PathBuf,

    /// Exit after the first successful sync instead of looping
    #[arg(long)]
    once: bool,

    /// HTTP server port (0 to disable)
    #[arg(short, long, default_value_t = 42298)]
    port: u16,

    /// Serve uncompressed /bootstrap.zip (off by default; production should use /bootstrap.zip.br)
    #[arg(long)]
    allow_uncompressed: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.output_dir)
        .with_context(|| format!("creating output dir {:?}", cli.output_dir))?;

    let relay_allowlist: ws_proxy::RelayAllowlist = Arc::new(RwLock::new(HashSet::new()));

    // Start HTTP server (unless disabled with --port 0)
    if cli.port != 0 {
        let output_dir = cli.output_dir.clone();
        let port = cli.port;
        let allowlist = relay_allowlist.clone();
        tokio::spawn(async move {
            let allow_uncompressed = cli.allow_uncompressed;
            if let Err(e) = server::run(output_dir, port, allow_uncompressed, allowlist).await {
                tracing::error!("HTTP server failed: {:#}", e);
            }
        });
    }

    // Load stores from previous run
    let mut stores = store::Stores::load(&cli.output_dir, &SystemTime::now())?;

    tracing::info!("bootstrapping TorClient...");
    let config = TorClientConfig::default();
    let client = TorClient::create_bootstrapped(config)
        .await
        .context("bootstrapping TorClient")?;
    tracing::info!("TorClient bootstrapped");

    loop {
        match sync::sync_once(&client, &cli.output_dir, &mut stores, &relay_allowlist).await {
            Ok(Some(lifetime)) => {
                if cli.once {
                    return Ok(());
                }
                let delay =
                    sync::relay_sync_delay(lifetime.fresh_until(), lifetime.valid_until());
                tracing::info!(
                    "next sync in {} (at ~{})",
                    humantime::format_duration(delay),
                    humantime::format_rfc3339(SystemTime::now() + delay),
                );
                tokio::time::sleep(delay).await;
            }
            Ok(None) => {
                let retry = Duration::from_secs(60);
                tracing::info!("retrying in {}", humantime::format_duration(retry));
                tokio::time::sleep(retry).await;
            }
            Err(e) => {
                tracing::error!("sync failed: {:#}", e);
                let retry = Duration::from_secs(60);
                tracing::info!("retrying in {}", humantime::format_duration(retry));
                tokio::time::sleep(retry).await;
            }
        }
    }
}

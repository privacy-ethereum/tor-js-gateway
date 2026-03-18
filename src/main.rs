mod config;
mod dir;
mod server;
mod service;
mod store;
mod sync;
mod ws_proxy;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arti_client::{TorClient, TorClientConfig};

#[derive(Parser)]
#[command(name = "tor-js-gateway")]
#[command(about = "Gateway server for tor-js — bootstrap, WebSocket relay, peer discovery")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value_os_t = config::config_path())]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway server (default when no subcommand given)
    Run {
        /// Exit after the first successful sync instead of looping
        #[arg(long)]
        once: bool,
    },
    /// Create a default config file
    Init,
    /// Print the current config from disk
    ShowConfig,
    /// Print the hardcoded default config
    ShowDefaultConfig,
    /// Install and start a systemd user service
    Install,
    /// Stop and remove the systemd user service
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run { once: false }) {
        Command::Init => config::Config::init(&cli.config),
        Command::ShowConfig => {
            let cfg = config::Config::load(&cli.config)?;
            println!("{}", json5::to_string(&cfg)?);
            Ok(())
        }
        Command::ShowDefaultConfig => {
            println!("{}", config::Config::to_json5_with_comments());
            Ok(())
        }
        Command::Run { once } => run(&cli.config, once).await,
        Command::Install => service::install(&cli.config),
        Command::Uninstall => service::uninstall(),
    }
}

async fn run(config_path: &PathBuf, once: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::load(config_path)?;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {:?}", cfg.data_dir))?;

    let relay_allowlist: ws_proxy::RelayAllowlist = Arc::new(RwLock::new(HashSet::new()));
    let ws_limits = ws_proxy::WsLimits {
        max_connections: cfg.ws_max_connections,
        per_ip_limit: cfg.ws_per_ip_limit,
        idle_timeout: Duration::from_secs(cfg.ws_idle_timeout),
        max_lifetime: Duration::from_secs(cfg.ws_max_lifetime),
    };

    // Start HTTP server (unless disabled with port: 0)
    if cfg.port != 0 {
        let data_dir = cfg.data_dir.clone();
        let port = cfg.port;
        let allow_uncompressed = cfg.allow_uncompressed;
        let allowlist = relay_allowlist.clone();
        let limits = ws_limits.clone();
        tokio::spawn(async move {
            if let Err(e) =
                server::run(data_dir, port, allow_uncompressed, allowlist, limits).await
            {
                tracing::error!("HTTP server failed: {:#}", e);
            }
        });
    }

    // Load stores from previous run
    let mut stores = store::Stores::load(&cfg.data_dir, &SystemTime::now())?;

    tracing::info!("bootstrapping TorClient...");
    let tor_config = TorClientConfig::default();
    let client = TorClient::create_bootstrapped(tor_config)
        .await
        .context("bootstrapping TorClient")?;
    tracing::info!("TorClient bootstrapped");

    loop {
        match sync::sync_once(&client, &cfg.data_dir, &mut stores, &relay_allowlist).await {
            Ok(Some(lifetime)) => {
                if once {
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

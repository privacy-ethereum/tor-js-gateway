//! Configuration file handling.
//!
//! Config lives at `~/.config/tor-js-gateway/config.json5`.
//! Data lives at `~/.local/share/tor-js-gateway/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "tor-js-gateway";

/// Resolved paths for config and data directories.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join(APP_NAME)
        .join("config.json5")
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join(APP_NAME)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory for cached consensus data and bootstrap archives
    pub data_dir: PathBuf,

    /// HTTP server port (0 to disable)
    pub port: u16,

    /// Serve uncompressed /bootstrap.zip
    pub allow_uncompressed: bool,

    /// Max concurrent WebSocket relay connections
    pub ws_max_connections: usize,

    /// Max WebSocket relay connections per client IP
    pub ws_per_ip_limit: usize,

    /// WebSocket relay idle timeout in seconds
    pub ws_idle_timeout: u64,

    /// WebSocket relay max connection lifetime in seconds
    pub ws_max_lifetime: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            port: 42298,
            allow_uncompressed: false,
            ws_max_connections: 8192,
            ws_per_ip_limit: 16,
            ws_idle_timeout: 300,
            ws_max_lifetime: 3600,
        }
    }
}

impl Config {
    /// Serialize to pretty JSON5 with comments.
    pub fn to_json5_with_comments() -> String {
        let cfg = Self::default();
        format!(
            r#"{{
  // Directory for cached consensus data and bootstrap archives
  "data_dir": {},

  // HTTP server port (0 to disable)
  "port": {},

  // Serve uncompressed /bootstrap.zip (production should use /bootstrap.zip.br)
  "allow_uncompressed": {},

  // Max concurrent WebSocket relay connections
  "ws_max_connections": {},

  // Max WebSocket relay connections per client IP
  "ws_per_ip_limit": {},

  // WebSocket relay idle timeout in seconds
  "ws_idle_timeout": {},

  // WebSocket relay max connection lifetime in seconds
  "ws_max_lifetime": {},
}}"#,
            serde_json::to_string(&cfg.data_dir).unwrap(),
            cfg.port,
            cfg.allow_uncompressed,
            cfg.ws_max_connections,
            cfg.ws_per_ip_limit,
            cfg.ws_idle_timeout,
            cfg.ws_max_lifetime,
        )
    }

    /// Load config from the given path.
    pub fn load(path: &PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}\n\nRun `tor-js-gateway init` to create a default config.", path.display()))?;
        let cfg: Config =
            json5::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Create the default config file. Errors if it already exists.
    pub fn init(path: &PathBuf) -> Result<()> {
        if path.exists() {
            anyhow::bail!("config already exists at {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let content = Self::to_json5_with_comments();
        std::fs::write(&path, &content)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("Created config at {}", path.display());
        Ok(())
    }
}

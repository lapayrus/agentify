//! Runtime configuration.
//!
//! The data root layout lives in `agentify_proto` because every process needs
//! it; this module adds the daemon's own knobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// Environment variable that shortens the events heartbeat (milliseconds).
pub const HEARTBEAT_ENV: &str = "AGENTIFY_HEARTBEAT_MS";

/// Default heartbeat period for `GET /v1/events`.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding the state database, the discovery file, and (later)
    /// logs and checkpoints.
    pub data_root: PathBuf,
    /// Address to bind. Always loopback: the daemon is never reachable from
    /// another machine.
    pub bind_addr: std::net::SocketAddr,
    /// Period between SSE heartbeats.
    pub heartbeat: Duration,
}

impl Config {
    /// Configuration for an explicit data root, bound to an ephemeral loopback
    /// port. Tests use this; the binary uses [`Config::from_env`].
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            bind_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }

    pub fn from_env() -> Result<Self> {
        let mut config = Self::new(agentify_proto::resolve_data_root());
        if let Ok(raw) = std::env::var(HEARTBEAT_ENV) {
            let millis: u64 = raw.trim().parse().with_context(|| {
                format!("{HEARTBEAT_ENV}={raw} is not a whole number of milliseconds")
            })?;
            config.heartbeat = Duration::from_millis(millis.max(10));
        }
        Ok(config)
    }

    pub fn state_db_path(&self) -> PathBuf {
        agentify_proto::state_db_path(&self.data_root)
    }

    pub fn discovery_path(&self) -> PathBuf {
        agentify_proto::discovery_path(&self.data_root)
    }
}

/// Create the data root if it does not exist yet.
pub fn ensure_data_root(data_root: &Path) -> Result<()> {
    std::fs::create_dir_all(data_root)
        .with_context(|| format!("creating the data root {}", data_root.display()))
}

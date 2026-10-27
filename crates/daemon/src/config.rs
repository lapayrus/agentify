//! Runtime configuration.
//!
//! The data root layout lives in `agentify_proto` because every process needs
//! it; this module adds the daemon's own knobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::ports::PortRange;

/// Environment variable that shortens the events heartbeat (milliseconds).
pub const HEARTBEAT_ENV: &str = "AGENTIFY_HEARTBEAT_MS";

/// Environment variable that selects the Backend: `fake` until ticket 07.
pub const BACKEND_ENV: &str = "AGENTIFY_BACKEND";

/// Environment variable bounding a mutating HTTP call (milliseconds). On
/// timeout the daemon answers `504` with the operation id.
pub const OPERATION_TIMEOUT_ENV: &str = "AGENTIFY_OPERATION_TIMEOUT_MS";

/// Environment variable holding the host ports the daemon may lease, as
/// `40000-40999` or a single `40000`.
pub const LEASE_RANGE_ENV: &str = "AGENTIFY_LEASE_RANGE";

/// Environment variable bounding one TCP health probe (milliseconds).
pub const PROBE_TIMEOUT_ENV: &str = "AGENTIFY_PROBE_TIMEOUT_MS";

/// Environment variable capping one `exec` (milliseconds).
pub const EXEC_TIMEOUT_ENV: &str = "AGENTIFY_EXEC_TIMEOUT_MS";

/// Environment variable bounding the default log tail.
pub const LOG_TAIL_ENV: &str = "AGENTIFY_LOG_TAIL";

/// Environment variable carrying the fake Backend's script as JSON.
pub const FAKE_SCRIPT_ENV: &str = "AGENTIFY_FAKE_SCRIPT";

/// Set to `off` to run the daemon without local ingress. Nothing is published
/// at `<resource>.<project>.localhost`; tests that are not about ingress use it.
pub const INGRESS_ENV: &str = "AGENTIFY_INGRESS";

/// The explicit trust path: `install` or `remove`. Nothing touches a
/// certificate store unless this is set (or the installer asks through the same
/// call).
pub const TRUST_ENV: &str = "AGENTIFY_TRUST";

/// Default heartbeat period for `GET /v1/events`.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

/// Default bound on a mutating HTTP call.
pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on one health probe.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Default bound on one `exec`.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Default number of log lines a read returns when the caller does not ask.
pub const DEFAULT_LOG_TAIL: usize = 200;

/// Default host port range the daemon leases from.
pub const DEFAULT_LEASE_RANGE: PortRange = PortRange::new(40000, 40999);

/// The scripted Backend: deterministic, substrate-free, and the default.\r
pub const BACKEND_FAKE: &str = "fake";

/// One WSL2 distro per Project. Windows only.\r
pub const BACKEND_WSL: &str = "wsl";

/// An explicit trust action, taken at startup because a human asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustAction {
    /// Install the local root for the current user.
    Install,
    /// Remove it, for uninstall.
    Remove,
}

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
    /// Which Backend to drive. `fake` until ticket 07 wires WSL.
    pub backend: String,
    /// A script for the fake Backend, as JSON. `AGENTIFY_FAKE_SCRIPT` when the
    /// binary reads its configuration; set directly by tests, so two tests can
    /// run different scripts without racing on a process-wide environment.
    pub backend_script: Option<String>,
    /// How long a mutating call blocks before answering `504` with the
    /// operation id.
    pub operation_timeout: Duration,
    /// Host ports available to lease.
    pub lease_range: PortRange,
    /// How long one health probe waits for a connection.
    pub probe_timeout: Duration,
    /// Upper bound on one `exec`.
    pub exec_timeout: Duration,
    /// Default log tail.
    pub log_tail: usize,
    /// Local ingress, or `None` to run without publishing anything.
    ///
    /// Off for an in-process test unless it asks: starting Caddy binds real
    /// ports and writes a real config, which is exactly what a test that is not
    /// about ingress must not do. The binary turns it on.
    pub ingress: Option<agentify_ingress::IngressOptions>,
    /// An explicit, one-shot trust action: the manual path a human or the
    /// installer opts into.
    pub trust_action: Option<TrustAction>,
}

impl Config {
    /// Configuration for an explicit data root, bound to an ephemeral loopback
    /// port. Tests use this; the binary uses [`Config::from_env`].
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            bind_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            heartbeat: DEFAULT_HEARTBEAT,
            backend: BACKEND_FAKE.to_string(),
            backend_script: None,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            lease_range: DEFAULT_LEASE_RANGE,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
            log_tail: DEFAULT_LOG_TAIL,
            ingress: None,
            trust_action: None,
        }
    }

    pub fn from_env() -> Result<Self> {
        let mut config = Self::new(agentify_proto::resolve_data_root());
        if let Ok(raw) = std::env::var(HEARTBEAT_ENV) {
            config.heartbeat = Duration::from_millis(whole_number(HEARTBEAT_ENV, &raw)?.max(10));
        }
        if let Ok(raw) = std::env::var(BACKEND_ENV) {
            if !raw.trim().is_empty() {
                config.backend = raw.trim().to_string();
            }
        }
        if let Ok(raw) = std::env::var(OPERATION_TIMEOUT_ENV) {
            config.operation_timeout =
                Duration::from_millis(whole_number(OPERATION_TIMEOUT_ENV, &raw)?.max(1));
        }
        if let Ok(raw) = std::env::var(LEASE_RANGE_ENV) {
            config.lease_range =
                PortRange::parse(&raw).map_err(|err| anyhow::anyhow!("{LEASE_RANGE_ENV}: {err}"))?;
        }
        if let Ok(raw) = std::env::var(PROBE_TIMEOUT_ENV) {
            config.probe_timeout =
                Duration::from_millis(whole_number(PROBE_TIMEOUT_ENV, &raw)?.max(1));
        }
        if let Ok(raw) = std::env::var(EXEC_TIMEOUT_ENV) {
            config.exec_timeout = Duration::from_millis(whole_number(EXEC_TIMEOUT_ENV, &raw)?.max(1));
        }
        if let Ok(raw) = std::env::var(LOG_TAIL_ENV) {
            let tail = whole_number(LOG_TAIL_ENV, &raw)?;
            config.log_tail = (tail as usize).max(1);
        }
        if let Ok(raw) = std::env::var(FAKE_SCRIPT_ENV) {
            if !raw.trim().is_empty() {
                config.backend_script = Some(raw);
            }
        }
        if let Ok(raw) = std::env::var(TRUST_ENV) {
            config.trust_action = match raw.trim() {
                "" => None,
                "install" => Some(TrustAction::Install),
                "remove" | "uninstall" => Some(TrustAction::Remove),
                other => anyhow::bail!(
                    "{TRUST_ENV}={other:?} is not a trust action; use `install` or `remove`"
                ),
            };
        }
        let ingress_enabled = match std::env::var(INGRESS_ENV) {
            Ok(raw) => !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            ),
            Err(_) => true,
        };
        if ingress_enabled {
            let mut ingress = agentify_ingress::IngressOptions::new(&config.data_root);
            ingress
                .apply_env()
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            config.ingress = Some(ingress);
        }
        Ok(config)
    }

    /// Publish `<resource>.<project>.localhost` through Caddy with explicit
    /// options. Tests use this to attach to a fake admin endpoint.
    pub fn with_ingress(mut self, ingress: agentify_ingress::IngressOptions) -> Self {
        self.ingress = Some(ingress);
        self
    }

    /// Take an explicit trust action at startup.
    pub fn with_trust_action(mut self, action: TrustAction) -> Self {
        self.trust_action = Some(action);
        self
    }

    /// Drive a scripted fake Backend. Tests use this.
    pub fn with_backend_script(mut self, script: impl Into<String>) -> Self {
        self.backend_script = Some(script.into());
        self
    }

    /// Bound the block on a mutating call.
    pub fn with_operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = timeout;
        self
    }

    /// Lease host ports from an explicit range.
    pub fn with_lease_range(mut self, range: PortRange) -> Self {
        self.lease_range = range;
        self
    }

    pub fn state_db_path(&self) -> PathBuf {
        agentify_proto::state_db_path(&self.data_root)
    }

    pub fn discovery_path(&self) -> PathBuf {
        agentify_proto::discovery_path(&self.data_root)
    }
}

fn whole_number(name: &str, raw: &str) -> Result<u64> {
    raw.trim()
        .parse()
        .with_context(|| format!("{name}={raw} is not a whole number"))
}

/// Create the data root if it does not exist yet.
pub fn ensure_data_root(data_root: &Path) -> Result<()> {
    std::fs::create_dir_all(data_root)
        .with_context(|| format!("creating the data root {}", data_root.display()))
}

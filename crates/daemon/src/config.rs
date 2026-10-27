//! Runtime configuration.
//!
//! The data root layout lives in `agentify_proto` because every process needs
//! it; this module adds the daemon's own knobs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::consent::{self, ConsentMode};
use crate::ports::PortRange;
use crate::secrets::{self, SecretEnv, SecretStoreKind, Secrets};

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

/// Environment variable selecting how an Approval's interactive gesture is
/// answered: `os` (Windows Hello), `granted`, `declined`, or `unavailable`.
pub const CONSENT_ENV: &str = "AGENTIFY_CONSENT";

/// Environment variable shortening an Approval's lifetime (milliseconds).
pub const APPROVAL_TTL_ENV: &str = "AGENTIFY_APPROVAL_TTL_MS";

/// Environment variable selecting where secret values are kept: `keyring` (the
/// OS credential manager, the default for the shipped daemon) or `memory` (an
/// in-process store, for tests).
pub const SECRET_STORE_ENV: &str = "AGENTIFY_SECRET_STORE";

/// Environment variable naming the keyring service Agentify's credentials live
/// under. A test that exercises the real credential manager uses its own name.
pub const SECRET_SERVICE_ENV: &str = "AGENTIFY_SECRET_SERVICE";

/// Default heartbeat period for `GET /v1/events`.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

/// Default lifetime of an Approval: fifteen minutes.
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(15 * 60);

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
    /// How long an Approval stays usable. Fifteen minutes by default; a test
    /// shrinks it instead of waiting for the real thing.
    pub approval_ttl: Duration,
    /// How the interactive consent gesture is answered.
    pub consent: ConsentMode,
    /// Where Project secret values live. The shipped daemon uses the OS
    /// credential manager; a test-config daemon uses an in-process store so no
    /// suite can read or write the developer's real credentials.
    pub secret_store: SecretStoreKind,
    /// Keyring service name. One per installation keeps Agentify's credentials
    /// apart from every other application's.
    pub secret_service: String,
    /// Where a `fromEnv` import reads from. A test-config daemon is hermetic: an
    /// empty map, so a developer's shell cannot change what a test sees.
    pub secret_env: SecretEnv,
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
            approval_ttl: DEFAULT_APPROVAL_TTL,
            // A test-config daemon must never open an OS prompt: the gesture is
            // scripted, and refused unless a test asks for it.
            consent: ConsentMode::Declined,
            // Same rule for secrets: an in-process store and an empty host
            // environment, so a test cannot touch this machine's credentials or
            // depend on how the developer's shell is set up.
            secret_store: SecretStoreKind::Memory,
            secret_service: secrets::DEFAULT_SERVICE.to_string(),
            secret_env: SecretEnv::Fixed(BTreeMap::new()),
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
        if let Ok(raw) = std::env::var(APPROVAL_TTL_ENV) {
            config.approval_ttl =
                Duration::from_millis(whole_number(APPROVAL_TTL_ENV, &raw)?.max(1));
        }
        // The shipped daemon asks the OS unless told otherwise; a scripted
        // answer has to be asked for by name.
        config.consent = match std::env::var(CONSENT_ENV) {
            Ok(raw) => match ConsentMode::parse(&raw) {
                Some(mode) => mode,
                None => anyhow::bail!(
                    "{CONSENT_ENV}={raw} is not one of os, granted, declined, unavailable"
                ),
            },
            Err(_) => consent::default_mode(),
        };
        // The shipped daemon keeps secret values in the OS credential manager and
        // imports `fromEnv` from its own environment, falling back to the user's.
        match std::env::var(SECRET_STORE_ENV) {
            Ok(raw) => match SecretStoreKind::parse(&raw) {
                Some(kind) => config.secret_store = kind,
                None => anyhow::bail!("{SECRET_STORE_ENV}={raw} is not one of keyring, memory"),
            },
            Err(_) => config.secret_store = SecretStoreKind::Keyring,
        }
        if let Ok(raw) = std::env::var(SECRET_SERVICE_ENV) {
            if !raw.trim().is_empty() {
                config.secret_service = raw.trim().to_string();
            }
        }
        config.secret_env = SecretEnv::Process;
        Ok(config)
    }

    /// Answer the consent gesture with a scripted verifier. Tests use this; it
    /// is the only way a test can approve anything.
    pub fn with_consent(mut self, mode: ConsentMode) -> Self {
        self.consent = mode;
        self
    }

    /// Keep secret values in a chosen store. A test names the in-memory one; the
    /// shipped daemon picks the credential manager from the environment.
    pub fn with_secret_store(mut self, kind: SecretStoreKind) -> Self {
        self.secret_store = kind;
        self
    }

    /// Name the keyring service, so a test uses its own and can clean up after
    /// itself.
    pub fn with_secret_service(mut self, service: impl Into<String>) -> Self {
        self.secret_service = service.into();
        self
    }

    /// Read `fromEnv` imports from an explicit map instead of this process's
    /// environment. Tests use this so a machine's shell cannot change the result.
    pub fn with_secret_env<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.secret_env = SecretEnv::Fixed(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        );
        self
    }

    /// The secret machinery this configuration describes.
    pub fn secrets(&self) -> Secrets {
        Secrets::new(
            self.secret_store.store(&self.secret_service, &self.data_root),
            self.secret_env.clone(),
        )
    }

    /// Shorten an Approval's lifetime, so expiry can be tested without waiting
    /// fifteen minutes.
    pub fn with_approval_ttl(mut self, ttl: Duration) -> Self {
        self.approval_ttl = ttl;
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

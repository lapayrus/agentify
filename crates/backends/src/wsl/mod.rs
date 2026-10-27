//! The Windows Backend: one WSL2 distro per Environment.
//!
//! This adapter makes the contract real on Windows. `start` imports a
//! per-Project distro from a cached golden template, starts a **foreground**
//! supervisor inside it, and returns only once every leased port answers on
//! `127.0.0.1` from the host. `stop` terminates the distro and keeps its disk.
//! `remove` unregisters the distro, deletes its data, and verifies that neither
//! the distro nor its leases remain.
//!
//! The operational surface rides on that same distro and never on a host mount:
//!
//! * [`exec`](exec) runs a command as its own process group, keeps stdout and
//!   stderr apart, reports the exit code, and can terminate the group — and every
//!   process carrying the exec's marker — when the call is cancelled;
//! * [`log`] keeps the supervisor's output as a per-Environment log on the
//!   daemon's disk, so `tail`, `since`, and `follow` are exact and a slow reader
//!   cannot block the workload;
//! * [`stats`] reports what the distro can prove — CPU, memory, and disk from
//!   `/proc` and `df`, per-Resource figures from `docker stats` when the engine
//!   is there;
//! * [`transfer`] moves single files in and out through the launcher's pipes,
//!   base64 in both directions so the channel carries text and never a binary
//!   that some layer between two operating systems could transcode, after
//!   [`paths`] refuses a traversal or a host path the daemon does not own.
//!
//! What it deliberately does not do:
//!
//! * it never runs `wsl --shutdown` and never terminates or unregisters a distro
//!   whose name it did not mint, so the user's `Ubuntu` and `docker-desktop`
//!   are untouchable;
//! * it does not claim `events`, `pty`, `mount`, or `checkpoint`;
//! * it reports what the machine actually says. `status` reads `wsl -l -v` and
//!   probes the leased ports; a distro that died is `stopped`, not `running`.
//!
//! The substrate's real limit is worth stating where the code lives: all WSL2
//! distros share one kernel and one network namespace, so a leased port is a
//! machine-wide resource and the daemon — not this adapter — owns the lease.
//! See `research/windows-wsl-per-project.md`.

pub mod cli;
pub mod compose;
pub mod exec;
pub mod log;
pub mod paths;
pub mod rootfs;
pub mod stats;
pub mod template;
pub mod transfer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentify_proto::Capabilities;
use serde::{Deserialize, Serialize};

use crate::contract::{
    Backend, BackendInfo, BackendStatus, BoxFuture, CheckpointId, EventStream, ExecSession,
    ExecSpec, LogOptions, LogStream, ResourceStatus, Stats, STATE_ABSENT, STATE_DEGRADED,
    STATE_RUNNING, STATE_STOPPED,
};
use crate::error::{BackendError, Result};
use crate::spec::EnvironmentSpec;

use cli::{Distro, SupervisorLine, Wsl};
use log::LogSink;
pub use rootfs::RootfsSource;

/// Adapter name reported by [`WslBackend::describe`].
pub const BACKEND_NAME: &str = "wsl";
/// Adapter version. The daemon refuses an unexpected major.
pub const BACKEND_VERSION: &str = "0.1.0";
/// Platform string for diagnostics.
pub const PLATFORM: &str = "windows-wsl2";

/// Where the supervisor lives inside a distro.
pub const SUPERVISE_PATH: &str = "/usr/local/bin/agentify-supervise";
/// The supervisor, compiled into the daemon: no shared mount, no host path.
///
/// Read through [unix_script]: a Windows checkout can hand the file back with
/// CRLF, and /bin/sh on Linux does not tolerate that.
pub const SUPERVISE_SCRIPT: &str = include_str!("scripts/supervise.sh");

/// Text a shell on Linux will accept: LF endings, whatever the checkout did.
pub fn unix_script(source: &str) -> String {
    source.replace("\r\n", "\n")
}

/// The `resource` field of a line the supervisor itself wrote.
///
/// The Environment's log is the supervisor's output, which is not any one
/// Resource's: labelling it honestly is what lets a caller filter by Resource and
/// get nothing, instead of getting somebody else's lines under the wrong name.
pub const SUPERVISOR_LOG_RESOURCE: &str = "supervisor";

/// Environment variable pointing at a local rootfs tar instead of the pinned
/// download. Tests use it; a machine on a slow link may too.
pub const ROOTFS_ENV: &str = "AGENTIFY_WSL_ROOTFS";
/// Environment variable that turns the container engine in the golden template
/// off (`0`).
pub const ENGINE_ENV: &str = "AGENTIFY_WSL_ENGINE";
/// Environment variable that boots the distro with systemd (`1`).
pub const SYSTEMD_ENV: &str = "AGENTIFY_WSL_SYSTEMD";
/// Environment variable that overrides the distro name prefix.
pub const PREFIX_ENV: &str = "AGENTIFY_WSL_PREFIX";
/// Environment variable that turns workloads off (`0`): the distro then holds
/// its leases and runs nothing, which is what a conformance run wants.
pub const WORKLOADS_ENV: &str = "AGENTIFY_WSL_WORKLOADS";
/// Environment variable overriding how long `start` waits (milliseconds). A
/// first start builds an image and initialises a database, which outlasts the
/// default.
pub const START_TIMEOUT_ENV: &str = "AGENTIFY_WSL_START_TIMEOUT_MS";

/// How long `start` waits for the supervisor to have every leased port bound.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(180);
/// How long `stop` waits for the leases to be released.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `remove` waits for a distro to disappear from `wsl -l -v`.
pub const DEFAULT_REMOVE_TIMEOUT: Duration = Duration::from_secs(60);

/// How a WSL Backend is configured.
#[derive(Debug, Clone)]
pub struct WslConfig {
    /// Data root: rootfs cache, golden template, and one directory per Project.
    pub data_root: PathBuf,
    /// Distro name prefix. Production is `agentify`; a test picks its own, so
    /// its distros are obvious in `wsl -l -v` and cleanable by prefix.
    pub distro_prefix: String,
    /// Where the base rootfs comes from.
    pub rootfs: RootfsSource,
    /// Install the container engine into the golden template.
    pub engine: bool,
    /// Boot Project distros with systemd. Off by default: a systemd boot costs
    /// about twelve seconds per distro start on WSL 2.6.3, and the supervisor
    /// owns the workload either way.
    pub systemd: bool,
    pub start_timeout: Duration,
    pub stop_timeout: Duration,
    pub remove_timeout: Duration,
    /// The only host directory a `copyIn`/`copyOut` may name. `Some(data_root)`
    /// in production: the daemon owns every host path it hands a Backend, and
    /// this is the adapter's half of that promise. `None` accepts any absolute,
    /// non-climbing path, which only a test wants.
    pub host_transfer_root: Option<PathBuf>,
    /// Run the Environment's Resources. On in production: the distro starts the
    /// Project's containers with `docker compose`. Off — or with no workspace to
    /// run — the supervisor only holds the leases, which is the mode a Backend
    /// conformance run uses.
    pub workloads: bool,
}

impl WslConfig {
    /// Production defaults for one data root.
    ///
    /// The root is read as it was written: `..` and `.` are resolved lexically
    /// ([`paths::tidy`]), so a data root spelled with a `..` still means the
    /// directory it names — every path this adapter derives (`wsl_dir`,
    /// `env_dir`, `log_path`) and every clean path the daemon hands to a copy is
    /// then spelled without a `..`, and the transfer path rule is not tripped by
    /// configuration. The rule that a *transfer* path may not climb is untouched.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        let data_root = paths::tidy(&data_root.into());
        Self {
            host_transfer_root: Some(data_root.clone()),
            data_root,
            distro_prefix: "agentify".to_string(),
            rootfs: RootfsSource::Pinned,
            engine: true,
            systemd: false,
            start_timeout: DEFAULT_START_TIMEOUT,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            remove_timeout: DEFAULT_REMOVE_TIMEOUT,
            workloads: true,
        }
    }

    /// Read the configuration from the environment, the way the daemon does.
    pub fn from_env() -> Self {
        let mut config = Self::new(agentify_proto::resolve_data_root());
        if let Some(path) = std::env::var_os(ROOTFS_ENV) {
            config.rootfs = RootfsSource::from_override(Some(PathBuf::from(path)));
        }
        if let Ok(raw) = std::env::var(ENGINE_ENV) {
            config.engine = !matches!(raw.trim(), "0" | "false" | "no");
        }
        if let Ok(raw) = std::env::var(SYSTEMD_ENV) {
            config.systemd = matches!(raw.trim(), "1" | "true" | "yes");
        }
        if let Ok(raw) = std::env::var(PREFIX_ENV) {
            if !raw.trim().is_empty() {
                config.distro_prefix = raw.trim().to_ascii_lowercase();
            }
        }
        if let Ok(raw) = std::env::var(WORKLOADS_ENV) {
            config.workloads = !matches!(raw.trim(), "0" | "false" | "no");
        }
        if let Ok(raw) = std::env::var(START_TIMEOUT_ENV) {
            if let Ok(millis) = raw.trim().parse::<u64>() {
                config.start_timeout = Duration::from_millis(millis.max(1_000));
            }
        }
        config
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.distro_prefix = prefix.into().to_ascii_lowercase();
        self
    }

    pub fn with_rootfs(mut self, rootfs: RootfsSource) -> Self {
        self.rootfs = rootfs;
        self
    }

    pub fn with_engine(mut self, engine: bool) -> Self {
        self.engine = engine;
        self
    }

    pub fn with_systemd(mut self, systemd: bool) -> Self {
        self.systemd = systemd;
        self
    }

    pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }

    /// Run the Resources, or only hold the leases. A spec with no workspace is
    /// listener-only whatever this says: there is nothing to build.
    pub fn with_workloads(mut self, workloads: bool) -> Self {
        self.workloads = workloads;
        self
    }

    /// Where a transfer's host path is allowed to live. `None` lifts the
    /// daemon-owned root, which is only ever right in a test. A root is
    /// configuration, so it is read as written ([`paths::tidy`]).
    pub fn with_host_transfer_root(mut self, root: Option<PathBuf>) -> Self {
        self.host_transfer_root = root.map(|root| paths::tidy(&root));
        self
    }

    /// Move the whole adapter — every derived path and the transfer boundary —
    /// onto a new data root.
    ///
    /// `from_env` starts from the default root; a daemon that supplies its own
    /// must move both, or a transfer under the configured root is refused as
    /// "outside the daemon-owned root".
    pub fn set_data_root(&mut self, root: impl Into<PathBuf>) {
        let root = paths::tidy(&root.into());
        self.data_root = root.clone();
        self.host_transfer_root = Some(root);
    }

    /// Everything this adapter writes lives under `<data_root>/wsl/`.
    pub fn wsl_dir(&self) -> PathBuf {
        self.data_root.join("wsl")
    }

    /// The verified rootfs tar cache.
    pub fn rootfs_dir(&self) -> PathBuf {
        self.wsl_dir().join("rootfs")
    }

    /// Golden templates, one file per recipe and option set.
    pub fn template_dir(&self) -> PathBuf {
        self.wsl_dir().join("golden")
    }

    /// One Project's distro directory: `<data_root>/wsl/env/<id>/`.
    pub fn env_dir(&self, environment: &str) -> PathBuf {
        self.wsl_dir().join("env").join(sanitize(environment))
    }

    /// The record that lets `status` describe an Environment it did not start.
    pub fn env_record_path(&self, environment: &str) -> PathBuf {
        self.wsl_dir()
            .join("env")
            .join(format!("{}.json", sanitize(environment)))
    }

    /// The daemon-read log for one Environment: the supervisor's own output.
    pub fn log_path(&self, environment: &str) -> PathBuf {
        self.wsl_dir()
            .join("env")
            .join(format!("{}.log", sanitize(environment)))
    }

    /// `agentify-<id>`: unique, lower-case, and recognisably ours.
    pub fn distro_name(&self, environment: &str) -> String {
        format!("{}-{}", self.distro_prefix, sanitize(environment))
    }
}

/// Distro names are lower-case and may not carry path or shell punctuation, so
/// an Environment id is folded to `[a-z0-9-]`. Two ids that fold to the same
/// string cannot happen: Environment ids are UUIDs.
fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    let truncated: String = trimmed.chars().take(40).collect();
    if truncated.is_empty() {
        "env".to_string()
    } else {
        truncated
    }
}

/// What `start` writes down so `status` and `remove` can work after a restart
/// without asking the daemon anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvRecord {
    environment: String,
    distro: String,
    resources: Vec<String>,
    /// Leased host ports, bound inside the distro at the same numbers.
    ports: Vec<u16>,
    started_at: String,
    /// True when the distro runs the Environment's Resources with Compose. Read
    /// back by `exec`, `stop`, and `remove`, so a daemon that restarted still
    /// knows there are containers to reach and to take down. Absent in a record
    /// written by an older build, which is listener-only.
    #[serde(default)]
    workloads: bool,
}

impl EnvRecord {
    fn read(config: &WslConfig, environment: &str) -> Option<Self> {
        let raw = std::fs::read(config.env_record_path(environment)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn write(&self, config: &WslConfig) -> Result<()> {
        let path = config.env_record_path(&self.environment);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot create {}: {err}", parent.display()),
            })?;
        }
        let body = serde_json::to_vec_pretty(self).unwrap_or_default();
        std::fs::write(&path, body).map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot write {}: {err}", path.display()),
        })
    }

    fn delete(config: &WslConfig, environment: &str) {
        let _ = std::fs::remove_file(config.env_record_path(environment));
    }
}

/// A supervisor child the daemon holds open, plus the protocol lines it wrote.
struct Supervisor {
    child: Child,
    lines: Receiver<String>,
    ports: Vec<u16>,
    /// Everything the supervisor said that was not protocol, for diagnostics.
    chatter: Vec<String>,
    /// The Environment's log, shared with the tee threads and with exec, so a
    /// cancellation that did not take is written down where a human will see it.
    log: Arc<Mutex<LogSink>>,
}

impl Supervisor {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Shared state, so the async trait methods can hand the blocking WSL calls to
/// a worker thread.
struct Inner {
    config: WslConfig,
    wsl: Wsl,
    running: Mutex<HashMap<String, Supervisor>>,
}

/// The Windows Backend. Cheap to clone: the daemon holds one and the futures
/// share it.
#[derive(Clone)]
pub struct WslBackend {
    inner: Arc<Inner>,
}

impl WslBackend {
    pub fn new(config: WslConfig) -> Self {
        let wsl = Wsl::new(&config.distro_prefix);
        Self {
            inner: Arc::new(Inner {
                config,
                wsl,
                running: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The Backend the daemon selects with `AGENTIFY_BACKEND=wsl`.
    pub fn from_env() -> Self {
        Self::new(WslConfig::from_env())
    }

    pub fn config(&self) -> &WslConfig {
        &self.inner.config
    }

    pub fn distro_name(&self, environment: &str) -> String {
        self.inner.config.distro_name(environment)
    }

    /// Unregister every distro this adapter minted, and delete their data.
    /// The teardown a test calls, and the reconciliation a later ticket will.
    pub fn unregister_all(&self) -> Result<Vec<String>> {
        let removed = template::unregister_all(&self.inner.wsl)?;
        {
            let mut running = self.inner.running.lock().expect("wsl running lock");
            running.clear();
        }
        let env_dir = self.inner.config.wsl_dir().join("env");
        if env_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&env_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(removed)
    }

    /// Registered distros whose name this adapter minted. Everything else on
    /// the machine is none of its business.
    pub fn owned_distros(&self) -> Result<Vec<Distro>> {
        Ok(self
            .inner
            .wsl
            .list()?
            .into_iter()
            .filter(|distro| self.inner.wsl.owns(&distro.name))
            .collect())
    }

    fn blocking<T, F>(&self, work: F) -> BoxFuture<'_, Result<T>>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Inner>) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || work(inner))
                .await
                .map_err(|err| BackendError::BackendUnavailable {
                    detail: format!("the WSL worker did not finish: {err}"),
                })?
        })
    }
}

impl Backend for WslBackend {
    fn describe(&self) -> BackendInfo {
        BackendInfo {
            name: BACKEND_NAME.to_string(),
            version: BACKEND_VERSION.to_string(),
            platform: PLATFORM.to_string(),
            capabilities: Capabilities {
                // v0 has no interactive terminal: a command gets pipes.
                pty: false,
                // Data lives on ext4 inside the distro; there is no host mount.
                mount: false,
                checkpoint: false,
                events: false,
                // Per-Resource figures come from the container engine when the
                // distro has one, and the log honours a `since` cursor.
                resource_stats: true,
                log_since: true,
            },
        }
    }

    fn start<'a>(&'a self, spec: &'a EnvironmentSpec) -> BoxFuture<'a, Result<()>> {
        let spec = spec.clone();
        self.blocking(move |inner| start_environment(&inner, &spec))
    }

    fn stop<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<()>> {
        let environment = environment.to_string();
        self.blocking(move |inner| stop_environment(&inner, &environment))
    }

    fn remove<'a>(&'a self, environment: &'a str, keep_volumes: bool) -> BoxFuture<'a, Result<()>> {
        let environment = environment.to_string();
        self.blocking(move |inner| remove_environment(&inner, &environment, keep_volumes))
    }

    fn status<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<BackendStatus>> {
        let environment = environment.to_string();
        self.blocking(move |inner| describe_environment(&inner, &environment))
    }

    fn events<'a>(&'a self, _environment: &'a str) -> BoxFuture<'a, Result<EventStream>> {
        Box::pin(async move {
            Err(BackendError::unsupported("events"))
        })
    }

    fn exec<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        spec: &'a ExecSpec,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        let environment = environment.to_string();
        let target = target.map(str::to_string);
        let spec = spec.clone();
        Box::pin(async move {
            let inner = self.inner.clone();
            tokio::task::spawn_blocking(move || exec_environment(&inner, &environment, target.as_deref(), &spec))
                .await
                .map_err(|err| BackendError::BackendUnavailable {
                    detail: format!("the WSL exec worker did not finish: {err}"),
                })?
        })
    }

    fn logs<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        options: &'a LogOptions,
    ) -> BoxFuture<'a, Result<LogStream>> {
        let environment = environment.to_string();
        let target = target.map(str::to_string);
        let options = options.clone();
        Box::pin(async move {
            let inner = self.inner.clone();
            tokio::task::spawn_blocking(move || {
                open_log(&inner, &environment, target.as_deref(), &options)
            })
            .await
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("the WSL log worker did not finish: {err}"),
            })?
        })
    }

    fn stats<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<Stats>> {
        let environment = environment.to_string();
        self.blocking(move |inner| environment_stats(&inner, &environment))
    }

    fn copy_in<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        host_path: &'a Path,
        guest_path: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        let environment = environment.to_string();
        let target = target.map(str::to_string);
        let host_path = host_path.to_path_buf();
        let guest_path = guest_path.to_string();
        self.blocking(move |inner| {
            // The boundary check first: a traversal is refused before the runtime
            // is asked whether it exists at all.
            transfer::check_paths(
                &host_path,
                &guest_path,
                inner.config.host_transfer_root.as_deref(),
            )?;
            let (distro, _) = require_running(&inner, &environment, target.as_deref(), "copy into")?;
            transfer::copy_in(
                &inner.wsl,
                &distro,
                &host_path,
                &guest_path,
                inner.config.host_transfer_root.as_deref(),
            )
        })
    }

    fn copy_out<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        guest_path: &'a str,
        host_path: &'a Path,
    ) -> BoxFuture<'a, Result<()>> {
        let environment = environment.to_string();
        let target = target.map(str::to_string);
        let guest_path = guest_path.to_string();
        let host_path = host_path.to_path_buf();
        self.blocking(move |inner| {
            transfer::check_paths(
                &host_path,
                &guest_path,
                inner.config.host_transfer_root.as_deref(),
            )?;
            let (distro, _) = require_running(&inner, &environment, target.as_deref(), "copy out of")?;
            transfer::copy_out(
                &inner.wsl,
                &distro,
                &guest_path,
                &host_path,
                inner.config.host_transfer_root.as_deref(),
            )
        })
    }

    fn checkpoint<'a>(&'a self, _environment: &'a str) -> BoxFuture<'a, Result<CheckpointId>> {
        Box::pin(async move { Err(BackendError::unsupported("checkpoint")) })
    }

    fn restore<'a>(
        &'a self,
        _environment: &'a str,
        _checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(BackendError::unsupported("restore")) })
    }

    fn delete_checkpoint<'a>(
        &'a self,
        _environment: &'a str,
        _checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(BackendError::unsupported("deleteCheckpoint")) })
    }
}

fn require_supported() -> Result<()> {
    if cli::is_supported() {
        Ok(())
    } else {
        Err(BackendError::BackendUnavailable {
            detail: "the WSL Backend only runs on Windows".to_string(),
        })
    }
}

// -- the operational surface, in blocking form --------------------------------

/// The distro an operational call runs against, or a typed `NotProvisioned`.
///
/// `target` is the Resource the caller named. v0 runs one distro per Environment
/// and no containers of its own, so a command runs in the distro; the name is
/// still checked, because a typo deserves `NotProvisioned` and not a mystery.
fn require_running(
    inner: &Inner,
    environment: &str,
    target: Option<&str>,
    what: &str,
) -> Result<(String, EnvRecord)> {
    require_supported()?;
    let config = &inner.config;
    let distro = config.distro_name(environment);
    if !inner.wsl.is_running(&distro)? {
        return Err(BackendError::NotProvisioned {
            detail: format!(
                "no running runtime exists for Environment {environment}; start it before you {what}"
            ),
        });
    }
    let record = EnvRecord::read(config, environment).unwrap_or_else(|| EnvRecord {
        environment: environment.to_string(),
        distro: distro.clone(),
        resources: Vec::new(),
        ports: Vec::new(),
        started_at: String::new(),
        workloads: false,
    });
    if let Some(target) = target {
        if !record.resources.is_empty() && !record.resources.iter().any(|name| name == target) {
            return Err(BackendError::NotProvisioned {
                detail: format!(
                    "Environment {environment} has no Resource named {target:?}; it has: {}",
                    record.resources.join(", ")
                ),
            });
        }
    }
    Ok((distro, record))
}

fn exec_environment(
    inner: &Inner,
    environment: &str,
    target: Option<&str>,
    spec: &ExecSpec,
) -> Result<ExecSession> {
    let (distro, record) = require_running(inner, environment, target, "run a command in it")?;
    let marker = exec::marker(environment);
    let log = inner
        .running
        .lock()
        .expect("wsl running lock")
        .get(environment)
        .map(|supervisor| supervisor.log.clone());
    // A command runs where the Resource is: in its container when the
    // Environment runs workloads, and in the distro when it only holds leases.
    let spec = match (record.workloads, target) {
        (true, Some(service)) => {
            let wrapped = ExecSpec {
                command: compose_exec_argv(service, &spec.command, &spec.env)?,
                cwd: None,
                env: Default::default(),
                ..spec.clone()
            };
            wrapped
        }
        _ => spec.clone(),
    };
    exec::start(&inner.wsl, &distro, &spec, &marker, log)
}

/// `docker compose -f <file> exec -T [-e K=V]... <service> <command...>`.
///
/// `-T` because v0 has no PTY: a command gets pipes. The Compose file is named
/// absolutely, so the command does not depend on a working directory, and the
/// service is the Resource's name, which is what the generated file calls it.
fn compose_exec_argv(
    service: &str,
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<String>> {
    if command.is_empty() {
        return Err(BackendError::ExecutionFailed {
            code: 2,
            detail: "a command needs at least one argument".to_string(),
        });
    }
    let mut argv = vec![
        "docker".to_string(),
        "compose".to_string(),
        "-f".to_string(),
        compose::COMPOSE_PATH.to_string(),
        "exec".to_string(),
        "-T".to_string(),
    ];
    // A variable the caller asked for belongs to the command, not to the
    // distro's `docker` CLI, so it is forwarded into the container.
    for (key, value) in env {
        argv.push("-e".to_string());
        argv.push(format!("{key}={value}"));
    }
    argv.push(service.to_string());
    argv.extend(command.iter().cloned());
    Ok(argv)
}

/// Read one Environment's log. A stopped Environment keeps its log, so this
/// answers as long as the file is there: the point of keeping it is to read it
/// after the runtime has gone.
fn open_log(
    inner: &Inner,
    environment: &str,
    target: Option<&str>,
    options: &LogOptions,
) -> Result<LogStream> {
    require_supported()?;
    let path = inner.config.log_path(environment);
    if !path.is_file() {
        return Err(BackendError::NotProvisioned {
            detail: format!(
                "no log exists for Environment {environment}; start it first"
            ),
        });
    }
    if options.follow {
        return log::follow(&path, target, options);
    }
    let lines = log::read(&path, target, options)?;
    Ok(crate::contract::stream_of(
        lines.into_iter().map(Ok).collect(),
    ))
}

/// Environment-level figures. A runtime that is not running has none, and this
/// reports its state rather than an error: "stopped, nothing used" is an answer.
fn environment_stats(inner: &Inner, environment: &str) -> Result<Stats> {
    require_supported()?;
    let distro = inner.config.distro_name(environment);
    if inner.wsl.is_running(&distro)? {
        let figures = stats::measure(&inner.wsl, &distro)?;
        return Ok(figures.stats(environment, STATE_RUNNING));
    }
    let state = if inner.wsl.find(&distro)?.is_some() {
        STATE_STOPPED
    } else {
        STATE_ABSENT
    };
    Ok(stats::stopped(environment, state))
}

// -- the lifecycle, in blocking form -----------------------------------------

fn start_environment(inner: &Inner, spec: &EnvironmentSpec) -> Result<()> {
    require_supported()?;
    let config = &inner.config;
    let environment = &spec.environment_id;
    let distro = config.distro_name(environment);
    let ports = host_ports(spec);
    // Workloads need something to run: a spec with no workspace is a listener,
    // which is the arrangement a conformance case uses.
    let workloads = config.workloads
        && !spec.workspace.as_os_str().is_empty()
        && spec.workspace.is_dir()
        && !spec.resources.is_empty();

    // The substrate first: a verified rootfs and a cached golden template. Both
    // are machine-wide and idempotent, so a second Project pays for neither.
    let rootfs = rootfs::ensure(&config.rootfs, &config.rootfs_dir())?;
    let golden = template::ensure(&inner.wsl, config, &rootfs)?;

    // Then the Project's own distro, imported once and kept across stops.
    let registered = inner.wsl.find(&distro)?;
    if registered.is_none() {
        let dir = config.env_dir(environment);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot create {}: {err}", dir.display()),
        })?;
        inner.wsl.import(&distro, &dir, &golden.tar)?;
    }

    // Converge: a supervisor already holding exactly these ports is the goal
    // state, so a second start is a no-op rather than a restart.
    {
        let mut running = inner.running.lock().expect("wsl running lock");
        if let Some(supervisor) = running.get_mut(environment) {
            if supervisor.alive() && supervisor.ports == ports {
                return Ok(());
            }
        }
        running.remove(environment);
    }

    // Keep the in-distro helpers in step with this binary: they are written on
    // every start, so a daemon upgrade cannot leave an old copy behind.
    inner.wsl.write_file(
        &distro,
        SUPERVISE_PATH,
        unix_script(SUPERVISE_SCRIPT).as_bytes(),
        "0755",
    )?;
    inner.wsl.write_file(
        &distro,
        exec::EXEC_PATH,
        unix_script(exec::EXEC_SCRIPT).as_bytes(),
        "0755",
    )?;

    // The Project's own files and the Compose document, written before anything
    // runs: a distro that answers a probe but has no workloads must not happen.
    let mut argv: Vec<String> = vec![SUPERVISE_PATH.to_string()];
    if workloads {
        compose::push_workspace(
            &inner.wsl,
            &distro,
            spec,
            config.host_transfer_root.as_deref(),
        )?;
        let document = compose::compose_yaml(spec)?;
        inner.wsl.write_file(
            &distro,
            compose::COMPOSE_PATH,
            document.as_bytes(),
            "0600",
        )?;
        argv.push("--compose".to_string());
        argv.push(compose::COMPOSE_PATH.to_string());
        for relay in compose::relays(spec) {
            argv.push("--map".to_string());
            argv.push(relay.token());
        }
    } else {
        argv.extend(ports.iter().map(u16::to_string));
    }

    // The Environment's log: one append-only file per Environment, written by
    // dedicated threads, so nothing a follower does can slow the supervisor.
    let sink = Arc::new(Mutex::new(LogSink::open(&config.log_path(environment))?));

    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let child = inner.wsl.spawn_foreground(&distro, &argv_refs)?;

    match await_ready(child, &ports, config.start_timeout, sink) {
        Ok(supervisor) => {
            let record = EnvRecord {
                environment: environment.clone(),
                distro: distro.clone(),
                resources: spec.resource_names(),
                ports,
                started_at: now_rfc3339(),
                workloads,
            };
            record.write(config)?;
            inner
                .running
                .lock()
                .expect("wsl running lock")
                .insert(environment.clone(), supervisor);
            Ok(())
        }
        Err(err) => {
            // A start that did not reach readiness leaves nothing running: the
            // distro is stopped (data kept) so the next start converges cleanly.
            let _ = inner.wsl.terminate(&distro);
            Err(err)
        }
    }
}

/// Wait for the supervisor to say it bound everything, and for the host to be
/// able to reach it. The daemon decides health; `start` only guarantees that a
/// probe *can* succeed.
///
/// Both of the supervisor's pipes are drained by tee threads for the whole life
/// of the child: they append to the Environment's log and, until readiness is
/// settled, forward each line here. That is what makes the log complete and what
/// keeps a full pipe from ever stalling the workload.
fn await_ready(
    mut child: Child,
    ports: &[u16],
    timeout: Duration,
    sink: Arc<Mutex<LogSink>>,
) -> Result<Supervisor> {
    let (sender, lines) = std::sync::mpsc::channel();
    let forward = Arc::new(AtomicBool::new(true));
    if let Some(stdout) = child.stdout.take() {
        log::spawn_tee(
            stdout,
            sink.clone(),
            "stdout",
            SUPERVISOR_LOG_RESOURCE,
            forward.clone(),
            Some(sender.clone()),
        );
    }
    if let Some(stderr) = child.stderr.take() {
        log::spawn_tee(
            stderr,
            sink.clone(),
            "stderr",
            SUPERVISOR_LOG_RESOURCE,
            forward.clone(),
            Some(sender),
        );
    }

    let deadline = Instant::now() + timeout;
    let mut supervisor = Supervisor {
        child,
        lines,
        ports: ports.to_vec(),
        chatter: Vec::new(),
        log: sink,
    };
    loop {
        while let Ok(line) = supervisor.lines.try_recv() {
            match cli::parse_supervisor_line(&line) {
                SupervisorLine::Failed(port, reason) => {
                    forward.store(false, Ordering::SeqCst);
                    return Err(BackendError::PortUnavailable {
                        port,
                        holder: format!("the distro could not bind 127.0.0.1:{port}: {reason}"),
                    });
                }
                SupervisorLine::Other(text) => supervisor.chatter.push(text),
                SupervisorLine::Bound(_) | SupervisorLine::Ready => {}
            }
        }

        if !supervisor.alive() {
            forward.store(false, Ordering::SeqCst);
            let reason = supervisor
                .chatter
                .last()
                .cloned()
                .unwrap_or_else(|| "no output".to_string());
            return Err(BackendError::BackendUnavailable {
                detail: format!("the supervisor exited before the runtime was up: {reason}"),
            });
        }

        if ports.iter().all(|port| probe(*port)) {
            // Readiness is settled: from here on the log is the only consumer, and
            // the tee threads stop paying for a forward nobody reads.
            forward.store(false, Ordering::SeqCst);
            return Ok(supervisor);
        }

        if Instant::now() >= deadline {
            forward.store(false, Ordering::SeqCst);
            return Err(BackendError::Timeout {
                detail: format!(
                    "the runtime did not bind {} leased port(s) within {} s",
                    ports.len(),
                    timeout.as_secs()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_environment(inner: &Inner, environment: &str) -> Result<()> {
    require_supported()?;
    let config = &inner.config;
    let distro = config.distro_name(environment);
    let record = EnvRecord::read(config, environment);

    // Drop the launcher first: the distro is what owns the listeners.
    let held = inner
        .running
        .lock()
        .expect("wsl running lock")
        .remove(environment);
    let ports = held.as_ref().map(|supervisor| supervisor.ports.clone());
    drop(held);

    if inner.wsl.find(&distro)?.is_none() {
        // Never provisioned, or already removed: "stopped" is the honest answer
        // and the caller asked for it.
        return Ok(());
    }

    // The workloads go down before the distro does, and their volumes stay: that
    // is the whole difference between a stop and a remove.
    if record.as_ref().map(|record| record.workloads).unwrap_or(false)
        && inner.wsl.is_running(&distro)?
    {
        compose_down(inner, &distro, "down --remove-orphans");
    }

    if inner.wsl.is_running(&distro)? {
        // Terminate only this distro: never `wsl --shutdown`, which would take
        // every other distro on the machine with it.
        let _ = inner.wsl.terminate(&distro);
    }

    // The leases are released when the distro dies; a probe from the host is
    // what proves it, and the daemon's port registry relies on it.
    let leased = ports.unwrap_or_else(|| {
        record
            .as_ref()
            .map(|record| record.ports.clone())
            .unwrap_or_default()
    });
    wait_until(config.stop_timeout, || {
        let running = inner.wsl.is_running(&distro).unwrap_or(false);
        !running && leased.iter().all(|port| !probe(*port))
    })?;

    // The disk and the record stay: `stop` is the preservation path.
    Ok(())
}

/// Take the Environment's containers down inside its distro.
///
/// `down` keeps the named volumes, so a stop is the preservation path; `down -v`
/// deletes them, so a remove leaves nothing behind. Best effort by design: the
/// distro is terminated next either way, and a Compose file that is missing (a
/// listener-only Environment, or one whose start never got that far) has nothing
/// to take down.
fn compose_down(inner: &Inner, distro: &str, verb: &str) {
    let script = format!(
        "export COMPOSE_PROJECT_NAME={project}\n\
         test -f {path} || {{ echo 'no compose file'; exit 0; }}\n\
         if docker compose version >/dev/null 2>&1; then\n\
           docker compose -f {path} {verb}\n\
         elif command -v docker-compose >/dev/null 2>&1; then\n\
           docker-compose -f {path} {verb}\n\
         else\n\
           echo 'no compose'; exit 0\n\
         fi\n",
        project = compose::COMPOSE_PROJECT,
        path = compose::COMPOSE_PATH,
        verb = verb
    );
    match inner.wsl.run(distro, &["sh", "-c", &script]) {
        Ok(output) if output.code == 0 => {}
        Ok(output) => eprintln!(
            "agentify: `compose {verb}` in {distro} exited {}: {}",
            output.code,
            output.diagnostic()
        ),
        Err(err) => eprintln!("agentify: `compose {verb}` in {distro} failed: {err}"),
    }
}

fn remove_environment(inner: &Inner, environment: &str, keep_volumes: bool) -> Result<()> {
    require_supported()?;
    if keep_volumes {
        return Err(BackendError::unsupported("keepVolumes"));
    }
    let config = &inner.config;
    let distro = config.distro_name(environment);
    let record = EnvRecord::read(config, environment);

    let held = inner
        .running
        .lock()
        .expect("wsl running lock")
        .remove(environment);
    let mut ports = held.as_ref().map(|supervisor| supervisor.ports.clone());
    drop(held);
    if ports.is_none() {
        ports = record.as_ref().map(|record| record.ports.clone());
    }
    let ports = ports.unwrap_or_default();

    if inner.wsl.find(&distro)?.is_some() {
        // `down -v` first: the containers and their volumes are the Project's
        // data, and unregistering the distro without taking them down would leave
        // the engine writing into a disk that is about to be deleted.
        if record.as_ref().map(|record| record.workloads).unwrap_or(false)
            && inner.wsl.is_running(&distro)?
        {
            compose_down(inner, &distro, "down --volumes --remove-orphans");
        }
        if inner.wsl.is_running(&distro)? {
            let _ = inner.wsl.terminate(&distro);
        }
        inner.wsl.unregister(&distro)?;
    }

    // Verification is part of the operation, not a nicety: a distro that is
    // still registered, or a lease still held, is a failed remove.
    wait_until(config.remove_timeout, || {
        inner
            .wsl
            .find(&distro)
            .map(|found| found.is_none())
            .unwrap_or(false)
    })
    .map_err(|err| {
        let detail = match err {
            BackendError::Timeout { detail } => detail,
            other => other.to_string(),
        };
        BackendError::Timeout {
            detail: format!("the distro {distro} is still registered after remove: {detail}"),
        }
    })?;

    let dir = config.env_dir(environment);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot delete {}: {err}", dir.display()),
        })?;
    }
    EnvRecord::delete(config, environment);
    let _ = std::fs::remove_file(config.log_path(environment));

    if dir.exists() {
        return Err(BackendError::BackendUnavailable {
            detail: format!("{} still exists after remove", dir.display()),
        });
    }
    // The leases are released when the distro goes, but WSL's localhost
    // forwarder lets go of the Windows-side socket a moment later. Wait for it
    // rather than reporting a lease that is on its way out.
    let _ = wait_until(config.remove_timeout, || {
        ports.iter().all(|port| !probe(*port))
    });

    for port in ports {
        if probe(port) {
            return Err(BackendError::Timeout {
                detail: format!("port {port} is still bound after remove"),
            });
        }
    }
    Ok(())
}

fn describe_environment(inner: &Inner, environment: &str) -> Result<BackendStatus> {
    require_supported()?;
    let config = &inner.config;
    let distro = config.distro_name(environment);
    let record = EnvRecord::read(config, environment);
    let resources = record
        .as_ref()
        .map(|record| record.resources.clone())
        .unwrap_or_default();

    if inner.wsl.find(&distro)?.is_none() {
        // Not provisioned is a snapshot, not a failure: the Environment exists
        // in the daemon, its runtime does not exist on the machine.
        return Ok(BackendStatus {
            environment: environment.to_string(),
            state: STATE_ABSENT.to_string(),
            resources: named(&resources, STATE_ABSENT),
        });
    };

    if !inner.wsl.is_running(&distro)? {
        return Ok(BackendStatus {
            environment: environment.to_string(),
            state: STATE_STOPPED.to_string(),
            resources: named(&resources, STATE_STOPPED),
        });
    }

    // A distro can be running with its supervisor gone (a crashed launcher); the
    // leased ports are the difference, and reporting `running` then would be a
    // lie the daemon would only discover on the next probe.
    let supervised = {
        let mut running = inner.running.lock().expect("wsl running lock");
        running
            .get_mut(environment)
            .map(|supervisor| supervisor.alive())
            .unwrap_or(false)
    };
    let ports = record
        .as_ref()
        .map(|record| record.ports.clone())
        .unwrap_or_default();
    let bound = ports.iter().filter(|port| probe(**port)).count();

    let state = if !supervised || (ports.is_empty() || bound < ports.len()) {
        STATE_DEGRADED
    } else {
        STATE_RUNNING
    };
    let resource_state = if state == STATE_RUNNING {
        STATE_RUNNING
    } else {
        STATE_DEGRADED
    };
    Ok(BackendStatus {
        environment: environment.to_string(),
        state: state.to_string(),
        resources: named(&resources, resource_state),
    })
}

fn named(names: &[String], state: &str) -> Vec<ResourceStatus> {
    names
        .iter()
        .map(|name| ResourceStatus {
            name: name.clone(),
            state: state.to_string(),
            message: None,
        })
        .collect()
}

/// Every leased host port, deduplicated: two guest ports may share one lease on
/// a host that hands out the same ephemeral port twice, and one listener serves
/// both.
fn host_ports(spec: &EnvironmentSpec) -> Vec<u16> {
    let mut ports: Vec<u16> = spec.ports.iter().map(|lease| lease.host_port).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Can the host reach this port yet? A TCP connect is the whole test: the
/// daemon's own probe is the same one.
fn probe(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

fn wait_until(timeout: Duration, done: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BackendError::Timeout {
                detail: format!("gave up after {} s", timeout.as_secs()),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_distro_name_is_prefixed_and_lower_case() {
        let config = WslConfig::new("C:/data");
        assert_eq!(config.distro_name("AbC-123"), "agentify-abc-123");
        assert_eq!(
            config.clone().with_prefix("Agentify-T7").distro_name("x"),
            "agentify-t7-x"
        );
        assert!(config.distro_name("weird/id with spaces").starts_with("agentify-"));
        assert!(!config.distro_name("weird/id").contains('/'));
    }

    #[test]
    fn an_empty_id_still_produces_a_valid_distro_name() {
        let config = WslConfig::new("C:/data");
        assert_eq!(config.distro_name("---"), "agentify-env");
    }

    #[test]
    fn the_descriptor_admits_what_is_missing() {
        let backend = WslBackend::new(WslConfig::new("C:/data"));
        let info = backend.describe();
        assert_eq!(info.name, "wsl");
        assert_eq!(info.version, BACKEND_VERSION);
        assert_eq!(info.platform, "windows-wsl2");
        assert!(!info.capabilities.pty, "v0 has no PTY");
        assert!(!info.capabilities.checkpoint);
        assert!(!info.capabilities.events);
        assert!(!info.capabilities.mount, "there is no host mount");
        assert!(
            info.capabilities.resource_stats,
            "stats come from the container engine, or from /proc and df"
        );
        assert!(
            info.capabilities.log_since,
            "the Environment log honours a `since` cursor"
        );
        assert_eq!(unix_script("a\nb\r\nc"), "a\nb\nc");
    }

    #[tokio::test]
    async fn the_commands_this_adapter_does_not_serve_are_typed_unsupported() {
        let backend = WslBackend::new(WslConfig::new("C:/data"));
        let environment = "env-caps";

        match backend.checkpoint(environment).await {
            Err(BackendError::Unsupported { capability, .. }) => {
                assert_eq!(capability, "checkpoint")
            }
            other => panic!("checkpoint must be unsupported, got {other:?}"),
        }
        match backend.events(environment).await {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "events"),
            other => panic!("events must be unsupported, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn the_operational_calls_refuse_an_environment_that_was_never_started() {
        // A data root nothing has ever provisioned: no runtime, so every
        // operational call is `NotProvisioned` rather than a raw substrate error.
        let root = std::env::temp_dir().join(format!("agentify-wsl-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backend = WslBackend::new(WslConfig::new(&root));
        let environment = "env-never-started";

        match backend
            .exec(
                environment,
                None,
                &ExecSpec {
                    command: vec!["echo".into(), "hi".into()],
                    ..ExecSpec::default()
                },
            )
            .await
        {
            Err(BackendError::NotProvisioned { .. }) | Err(BackendError::BackendUnavailable { .. }) => {}
            other => panic!("exec on an unstarted Environment answered {other:?}"),
        }

        let logs = backend.logs(environment, None, &LogOptions::default()).await;
        assert!(
            logs.is_err(),
            "an Environment with no runtime has no log to read"
        );

        // Both path rules are enforced *before* the runtime is asked anything, so
        // this is a refusal and not "no such distro".
        let dir = std::env::temp_dir();
        let err = backend
            .copy_in(environment, None, &dir, "/../etc/passwd")
            .await
            .expect_err("a traversal is refused");
        assert!(!matches!(err, BackendError::Unsupported { .. }), "{err:?}");
    }

    #[test]
    fn a_transfer_host_root_defaults_to_the_daemon_data_root() {
        let config = WslConfig::new("C:/data");
        assert_eq!(config.host_transfer_root, Some(PathBuf::from("C:/data")));
        let relaxed = config.clone().with_host_transfer_root(None);
        assert_eq!(relaxed.host_transfer_root, None);
    }

    #[test]
    fn setting_the_data_root_moves_the_transfer_boundary_too() {
        let mut config = WslConfig::new("C:/default");
        config.set_data_root("C:/configured");
        assert_eq!(config.data_root, PathBuf::from("C:/configured"));
        assert_eq!(config.host_transfer_root, Some(PathBuf::from("C:/configured")));
    }

    #[test]
    fn a_data_root_is_read_as_written_so_every_derived_path_is_clean() {
        // A data root spelled with a `..` — `agentify-proto` reads the value of
        // an environment variable verbatim, and a test builds its root from
        // `CARGO_MANIFEST_DIR` — names one directory, and every path this adapter
        // derives from it must still pass the transfer path rule.
        let base = if cfg!(windows) {
            PathBuf::from("C:/checkout")
        } else {
            PathBuf::from("/checkout")
        };
        let spelled = base
            .join("crates")
            .join("backends")
            .join("..")
            .join("..")
            .join("target")
            .join("wsl-test");
        let config = WslConfig::new(&spelled);
        let clean = base.join("target").join("wsl-test");
        assert_eq!(config.data_root, clean);
        assert_eq!(config.host_transfer_root, Some(clean.clone()));
        for derived in [
            config.wsl_dir(),
            config.env_dir("env-1"),
            config.log_path("env-1"),
            config.env_record_path("env-1"),
        ] {
            assert!(
                !derived.components().any(|part| part == std::path::Component::ParentDir),
                "a derived path is spelled without `..`: {}",
                derived.display()
            );
        }
        let payload = clean.join("wsl").join("payload.txt");
        assert!(
            paths::host_path(&payload, config.host_transfer_root.as_deref()).is_ok(),
            "a clean daemon-owned path under the root is accepted"
        );
        // And the root itself, configured the same way, still refuses a climb.
        let climbing = clean.join("..").join("payload.txt");
        assert!(paths::host_path(&climbing, config.host_transfer_root.as_deref()).is_err());
    }

    #[test]
    fn keep_volumes_is_unsupported() {
        let backend = WslBackend::new(WslConfig::new("C:/data"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = rt
            .block_on(backend.remove("env-x", true))
            .expect_err("keepVolumes is Unsupported in v0");
        assert_eq!(err.code(), "Unsupported");
    }

    #[test]
    fn the_environment_record_round_trips() {
        let root = std::env::temp_dir().join(format!("agentify-wsl-record-{}", std::process::id()));
        let config = WslConfig::new(&root);
        let record = EnvRecord {
            environment: "env-1".to_string(),
            distro: config.distro_name("env-1"),
            resources: vec!["web".to_string(), "db".to_string()],
            ports: vec![41000, 41001],
            started_at: now_rfc3339(),
            workloads: true,
        };
        record.write(&config).expect("write");
        let read = EnvRecord::read(&config, "env-1").expect("read");
        assert_eq!(read.distro, "agentify-env-1");
        assert_eq!(read.resources, vec!["web", "db"]);
        assert!(
            read.workloads,
            "a daemon that restarts still knows there are containers to take down"
        );
        EnvRecord::delete(&config, "env-1");
        assert!(EnvRecord::read(&config, "env-1").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_without_the_workloads_field_is_listener_only() {
        // A record written by a build before workloads existed has containers to
        // take down and must not be read as anything else.
        let record: EnvRecord = serde_json::from_value(json!({
            "environment": "env-1",
            "distro": "agentify-env-1",
            "resources": ["web"],
            "ports": [41000],
            "started_at": "2026-01-01T00:00:00.000Z"
        }))
        .expect("an older record still parses");
        assert!(!record.workloads);
    }

    #[test]
    fn an_exec_in_a_workload_environment_runs_inside_the_container() {
        let argv = compose_exec_argv(
            "web",
            &["node".to_string(), "migrate.js".to_string()],
            &std::collections::BTreeMap::from([(
                "GREETING".to_string(),
                "hi".to_string(),
            )]),
        )
        .expect("argv");
        assert_eq!(
            argv,
            vec![
                "docker",
                "compose",
                "-f",
                compose::COMPOSE_PATH,
                "exec",
                "-T",
                "-e",
                "GREETING=hi",
                "web",
                "node",
                "migrate.js",
            ],
            "no PTY, the Project's own Compose file, and the env goes to the container"
        );
        assert!(compose_exec_argv("web", &[], &Default::default()).is_err());
    }

    #[test]
    fn workloads_are_only_run_when_there_is_something_to_run() {
        // The rule `start_environment` applies: the conformance suite hands the
        // adapter a spec with resources and no workspace, and that must bind its
        // leases without trying to build a container.
        let config = WslConfig::new("C:/data");
        assert!(config.workloads, "production runs workloads");
        assert!(!config.clone().with_workloads(false).workloads);
    }
}

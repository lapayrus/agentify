//! The Windows Backend: one WSL2 distro per Environment.
//!
//! This adapter makes the contract real on Windows. `start` imports a
//! per-Project distro from a cached golden template, starts a **foreground**
//! supervisor inside it, and returns only once every leased port answers on
//! `127.0.0.1` from the host. `stop` terminates the distro and keeps its disk.
//! `remove` unregisters the distro, deletes its data, and verifies that neither
//! the distro nor its leases remain.
//!
//! What it deliberately does not do:
//!
//! * it never runs `wsl --shutdown` and never terminates or unregisters a distro
//!   whose name it did not mint, so the user's `Ubuntu` and `docker-desktop`
//!   are untouchable;
//! * it does not claim exec, logs, stats, events, or copy: those are the next
//!   ticket, and they answer [`BackendError::Unsupported`] until then;
//! * it reports what the machine actually says. `status` reads `wsl -l -v` and
//!   probes the leased ports; a distro that died is `stopped`, not `running`.
//!
//! The substrate's real limit is worth stating where the code lives: all WSL2
//! distros share one kernel and one network namespace, so a leased port is a
//! machine-wide resource and the daemon — not this adapter — owns the lease.
//! See `research/windows-wsl-per-project.md`.

pub mod cli;
pub mod rootfs;
pub mod template;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentify_proto::Capabilities;
use serde::{Deserialize, Serialize};

use crate::contract::{
    Backend, BackendInfo, BackendStatus, BoxFuture, CheckpointId, EventStream,
    ExecSession, ExecSpec, LogOptions, LogStream, ResourceStatus, Stats, STATE_ABSENT,
    STATE_DEGRADED, STATE_RUNNING, STATE_STOPPED,
};
use crate::error::{BackendError, Result};
use crate::spec::EnvironmentSpec;

use cli::{Distro, SupervisorLine, Wsl};
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
}

impl WslConfig {
    /// Production defaults for one data root.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            distro_prefix: "agentify".to_string(),
            rootfs: RootfsSource::Pinned,
            engine: true,
            systemd: false,
            start_timeout: DEFAULT_START_TIMEOUT,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
            remove_timeout: DEFAULT_REMOVE_TIMEOUT,
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
                pty: false,
                mount: false,
                checkpoint: false,
                events: false,
                resource_stats: false,
                log_since: false,
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
        _environment: &'a str,
        _target: Option<&'a str>,
        _spec: &'a ExecSpec,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move { Err(BackendError::unsupported("exec")) })
    }

    fn logs<'a>(
        &'a self,
        _environment: &'a str,
        _target: Option<&'a str>,
        _options: &'a LogOptions,
    ) -> BoxFuture<'a, Result<LogStream>> {
        Box::pin(async move { Err(BackendError::unsupported("logs")) })
    }

    fn stats<'a>(&'a self, _environment: &'a str) -> BoxFuture<'a, Result<Stats>> {
        Box::pin(async move { Err(BackendError::unsupported("stats")) })
    }

    fn copy_in<'a>(
        &'a self,
        _environment: &'a str,
        _target: Option<&'a str>,
        _host_path: &'a Path,
        _guest_path: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(BackendError::unsupported("copyIn")) })
    }

    fn copy_out<'a>(
        &'a self,
        _environment: &'a str,
        _target: Option<&'a str>,
        _guest_path: &'a str,
        _host_path: &'a Path,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(BackendError::unsupported("copyOut")) })
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

// -- the lifecycle, in blocking form -----------------------------------------

fn start_environment(inner: &Inner, spec: &EnvironmentSpec) -> Result<()> {
    require_supported()?;
    let config = &inner.config;
    let environment = &spec.environment_id;
    let distro = config.distro_name(environment);
    let ports = host_ports(spec);

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

    // Keep the in-distro helper in step with this binary: it is written on every
    // start, so a daemon upgrade cannot leave an old supervisor behind.
    inner
        .wsl
        .write_file(&distro, SUPERVISE_PATH, unix_script(SUPERVISE_SCRIPT).as_bytes(), "0755")?;

    let argv: Vec<String> = std::iter::once(SUPERVISE_PATH.to_string())
        .chain(ports.iter().map(u16::to_string))
        .collect();
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let child = inner.wsl.spawn_foreground(&distro, &argv_refs)?;

    match await_ready(child, &ports, config.start_timeout) {
        Ok(supervisor) => {
            let record = EnvRecord {
                environment: environment.clone(),
                distro: distro.clone(),
                resources: spec.resource_names(),
                ports,
                started_at: now_rfc3339(),
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
fn await_ready(
    mut child: Child,
    ports: &[u16],
    timeout: Duration,
) -> Result<Supervisor> {
    let (sender, lines) = std::sync::mpsc::channel();
    let mut chatter = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        cli::drain_lines(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        let sink = sender.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut pipe = stderr;
            let _ = pipe.read_to_string(&mut text);
            for line in text.lines() {
                let _ = sink.send(format!("stderr: {line}"));
            }
        });
    }

    let deadline = Instant::now() + timeout;
    let mut supervisor = Supervisor {
        child,
        lines,
        ports: ports.to_vec(),
        chatter: Vec::new(),
    };
    loop {
        while let Ok(line) = supervisor.lines.try_recv() {
            match cli::parse_supervisor_line(&line) {
                SupervisorLine::Failed(port, reason) => {
                    return Err(BackendError::PortUnavailable {
                        port,
                        holder: format!("the distro could not bind 127.0.0.1:{port}: {reason}"),
                    });
                }
                SupervisorLine::Other(text) => chatter.push(text),
                SupervisorLine::Bound(_) | SupervisorLine::Ready => {}
            }
        }

        if !supervisor.alive() {
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
            supervisor.chatter = chatter;
            return Ok(supervisor);
        }

        if Instant::now() >= deadline {
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

    if inner.wsl.is_running(&distro)? {
        // Terminate only this distro: never `wsl --shutdown`, which would take
        // every other distro on the machine with it.
        let _ = inner.wsl.terminate(&distro);
    }

    // The leases are released when the distro dies; a probe from the host is
    // what proves it, and the daemon's port registry relies on it.
    let leased = ports.unwrap_or_else(|| {
        EnvRecord::read(config, environment)
            .map(|record| record.ports)
            .unwrap_or_default()
    });
    wait_until(config.stop_timeout, || {
        let running = inner.wsl.is_running(&distro).unwrap_or(false);
        !running && leased.iter().all(|port| !probe(*port))
    })?;

    // The disk and the record stay: `stop` is the preservation path.
    Ok(())
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
        assert!(
            !info.capabilities.resource_stats,
            "stats arrive with the exec and logs ticket"
        );
        assert!(!info.capabilities.mount);
        assert_eq!(unix_script("a
b\r\nc"), "a
b\nc");
    }

    #[tokio::test]
    async fn the_commands_this_ticket_does_not_serve_are_typed_unsupported() {
        let backend = WslBackend::new(WslConfig::new("C:/data"));
        let environment = "env-caps";

        match backend.exec(environment, None, &ExecSpec::default()).await {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "exec"),
            other => panic!("exec must be unsupported, got {other:?}"),
        }
        match backend.logs(environment, None, &LogOptions::default()).await {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "logs"),
            other => panic!("logs must be unsupported, got {:?}", other.map(|_| ())),
        }
        match backend.stats(environment).await {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "stats"),
            other => panic!("stats must be unsupported, got {other:?}"),
        }
        match backend.checkpoint(environment).await {
            Err(BackendError::Unsupported { capability, .. }) => {
                assert_eq!(capability, "checkpoint")
            }
            other => panic!("checkpoint must be unsupported, got {other:?}"),
        }
        let dir = std::env::temp_dir();
        match backend
            .copy_in(environment, None, &dir, "/tmp/x")
            .await
        {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "copyIn"),
            other => panic!("copyIn must be unsupported, got {other:?}"),
        }
        match backend
            .copy_out(environment, None, "/tmp/x", &dir)
            .await
        {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "copyOut"),
            other => panic!("copyOut must be unsupported, got {other:?}"),
        }
        match backend.events(environment).await {
            Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "events"),
            other => panic!("events must be unsupported, got {:?}", other.map(|_| ())),
        }
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
        };
        record.write(&config).expect("write");
        let read = EnvRecord::read(&config, "env-1").expect("read");
        assert_eq!(read.distro, "agentify-env-1");
        assert_eq!(read.resources, vec!["web", "db"]);
        EnvRecord::delete(&config, "env-1");
        assert!(EnvRecord::read(&config, "env-1").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}

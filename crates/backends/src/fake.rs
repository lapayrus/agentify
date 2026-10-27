//! A scripted, deterministic Backend.
//!
//! The fake is how the whole Environment lifecycle is exercised before any real
//! substrate exists. It behaves like a Backend and nothing more:
//!
//! * `start` binds a TCP listener on `127.0.0.1` for every leased host port, so
//!   the daemon's own health probe succeeds or fails for a real reason. `stop`
//!   drops them and the ports are released.
//! * state moves `absent` → `starting` → `running` → `stopped`, with a
//!   configurable delay so a caller can observe `starting`.
//! * failures and degradation are scriptable: `start` can fail with a typed
//!   error, and named Resources can be left unbound so the daemon reports
//!   `degraded`.
//! * `exec` returns stdout, stderr, and an exit code; logs support `tail`,
//!   `since`, and `follow`; stats return environment-level figures.
//!
//! Every figure it reports is a constant, so tests assert exact values.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentify_proto::Capabilities;

use crate::contract::{
    stream_of, Backend, BackendEvent, BackendInfo, BackendStatus, BoxFuture, CheckpointId,
    EventStream, ExecChunk, ExecSession, ExecSpec, LogLine, LogOptions, LogStream, ResourceStat,
    ResourceStatus, Stats, STATE_ABSENT, STATE_RUNNING, STATE_STARTING, STATE_STOPPED,
};
use crate::error::{BackendError, Result};
use crate::spec::EnvironmentSpec;

/// Adapter name reported by [`FakeBackend::describe`].
pub const BACKEND_NAME: &str = "fake";

/// Adapter version reported by [`FakeBackend::describe`].
pub const BACKEND_VERSION: &str = "0.1.0";

/// Platform string: the fake is substrate-free, so it says so.
pub const PLATFORM: &str = "scripted";

/// Environment-level CPU figure while running, in percent.
pub const CPU_PERCENT: f64 = 2.5;
/// Environment-level memory figure while running.
pub const MEMORY_BYTES: u64 = 96 * 1024 * 1024;
/// Environment-level disk figure while running.
pub const DISK_BYTES: u64 = 512 * 1024 * 1024;

/// A typed failure the script can inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFault {
    /// The substrate is not reachable.
    BackendUnavailable,
    /// The call never finishes in time.
    Timeout,
    /// The substrate version is not the expected one.
    VersionMismatch,
    /// A command cannot be run.
    ExecutionFailed,
}

impl FakeFault {
    fn error(self, operation: &str) -> BackendError {
        match self {
            FakeFault::BackendUnavailable => BackendError::BackendUnavailable {
                detail: format!("the fake Backend is unavailable during {operation}"),
            },
            FakeFault::Timeout => BackendError::Timeout {
                detail: format!("the fake Backend timed out during {operation}"),
            },
            FakeFault::VersionMismatch => BackendError::VersionMismatch {
                expected: BACKEND_VERSION.to_string(),
                found: "9.9.9".to_string(),
            },
            FakeFault::ExecutionFailed => BackendError::ExecutionFailed {
                code: 1,
                detail: format!("the fake Backend refused {operation}"),
            },
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "backend_unavailable" | "BackendUnavailable" => Some(FakeFault::BackendUnavailable),
            "timeout" | "Timeout" => Some(FakeFault::Timeout),
            "version_mismatch" | "VersionMismatch" => Some(FakeFault::VersionMismatch),
            "execution_failed" | "ExecutionFailed" => Some(FakeFault::ExecutionFailed),
            _ => None,
        }
    }
}

/// What the fake should do. The default is the well-behaved case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeScript {
    /// Pause inside `start`, so a caller can observe `starting`.
    pub start_delay_ms: u64,
    /// Pause inside `stop`.
    pub stop_delay_ms: u64,
    /// Make every `start` fail with this fault.
    pub start_fault: Option<FakeFault>,
    /// Make every `exec` fail with this fault.
    pub exec_fault: Option<FakeFault>,
    /// Resources whose leased ports are deliberately left unbound, so the
    /// daemon's health probe fails and it reports `degraded`.
    pub unbound_resources: Vec<String>,
    /// Pause between exec chunks.
    pub exec_delay_ms: u64,
    /// Write the [`EnvironmentSpec`] this Backend was handed to this path, as
    /// JSON. A test uses it to check what the daemon actually resolved — the
    /// values it injected, and the placeholders it left behind — without the
    /// production API ever exposing a spec.
    pub record_spec_path: Option<String>,
    /// Capabilities to advertise. Defaults to the fake's own set.
    pub capabilities: Capabilities,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            start_delay_ms: 0,
            stop_delay_ms: 0,
            start_fault: None,
            exec_fault: None,
            unbound_resources: Vec::new(),
            exec_delay_ms: 0,
            record_spec_path: None,
            capabilities: Capabilities {
                pty: false,
                mount: false,
                checkpoint: false,
                events: true,
                resource_stats: false,
                log_since: true,
            },
        }
    }
}

impl FakeScript {
    /// Every default, with `start` made to fail with `fault`.
    pub fn failing_start(fault: FakeFault) -> Self {
        Self {
            start_fault: Some(fault),
            ..Self::default()
        }
    }

    /// A script whose named Resources never bind their port.
    pub fn degraded(resources: &[&str]) -> Self {
        Self {
            unbound_resources: resources.iter().map(|name| name.to_string()).collect(),
            ..Self::default()
        }
    }

    /// Parse `AGENTIFY_FAKE_SCRIPT`, a flat JSON object. Unknown keys are an
    /// error so a typo in a test does not silently do nothing.
    pub fn from_json(raw: &str) -> std::result::Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| format!("not JSON: {err}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "the fake script must be a JSON object".to_string())?;
        let mut script = Self::default();
        for (key, value) in object {
            match key.as_str() {
                "start_delay_ms" => script.start_delay_ms = as_u64(value, key)?,
                "stop_delay_ms" => script.stop_delay_ms = as_u64(value, key)?,
                "exec_delay_ms" => script.exec_delay_ms = as_u64(value, key)?,
                "start_fault" => {
                    script.start_fault = Some(fault(value, key)?);
                }
                "exec_fault" => {
                    script.exec_fault = Some(fault(value, key)?);
                }
                "unbound_resources" => {
                    let names = value
                        .as_array()
                        .ok_or_else(|| format!("`{key}` must be an array of Resource names"))?;
                    for name in names {
                        let name = name
                            .as_str()
                            .ok_or_else(|| format!("`{key}` must hold strings"))?;
                        script.unbound_resources.push(name.to_string());
                    }
                }
                "record_spec_path" => {
                    script.record_spec_path = Some(
                        value
                            .as_str()
                            .ok_or_else(|| format!("`{key}` must be a path"))?
                            .to_string(),
                    );
                }
                other => return Err(format!("unknown fake script key: {other}")),
            }
        }
        Ok(script)
    }
}

fn as_u64(value: &serde_json::Value, key: &str) -> std::result::Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("`{key}` must be a whole number"))
}

fn fault(value: &serde_json::Value, key: &str) -> std::result::Result<FakeFault, String> {
    let raw = value
        .as_str()
        .ok_or_else(|| format!("`{key}` must be a string"))?;
    FakeFault::parse(raw).ok_or_else(|| {
        format!("`{key}` must be one of backend_unavailable, timeout, version_mismatch, execution_failed")
    })
}

/// One Environment's runtime state inside the fake.
struct FakeEnvironment {
    state: Mutex<String>,
    /// Held open while running: dropping a listener releases its port.
    listeners: Mutex<Vec<(u16, TcpListener)>>,
    /// Names of the Resources currently up.
    resources: Mutex<Vec<String>>,
    logs: Arc<Mutex<Vec<LogLine>>>,
    events: Arc<Mutex<Vec<BackendEvent>>>,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    seq: Arc<AtomicU64>,
}

impl FakeEnvironment {
    fn new(environment: &str) -> Self {
        Self {
            state: Mutex::new(STATE_ABSENT.to_string()),
            listeners: Mutex::new(Vec::new()),
            resources: Mutex::new(Vec::new()),
            logs: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(vec![BackendEvent {
                kind: "state_changed".to_string(),
                environment: environment.to_string(),
                resource: None,
                state: Some(STATE_ABSENT.to_string()),
                detail: None,
            }])),
            files: Mutex::new(BTreeMap::new()),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    fn state(&self) -> String {
        self.state.lock().expect("fake state lock").clone()
    }

    fn set_state(&self, environment: &str, state: &str) {
        *self.state.lock().expect("fake state lock") = state.to_string();
        self.events
            .lock()
            .expect("fake events lock")
            .push(BackendEvent {
                kind: "state_changed".to_string(),
                environment: environment.to_string(),
                resource: None,
                state: Some(state.to_string()),
                detail: None,
            });
    }

    fn push_log(&self, resource: &str, stream: &str, line: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.logs.lock().expect("fake logs lock").push(LogLine {
            seq,
            at: now_rfc3339_millis(),
            resource: resource.to_string(),
            stream: stream.to_string(),
            line: line.to_string(),
        });
    }
}

/// The scripted Backend the daemon uses until ticket 07 wires WSL.
pub struct FakeBackend {
    script: FakeScript,
    environments: Mutex<HashMap<String, Arc<FakeEnvironment>>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new(FakeScript::default())
    }
}

impl FakeBackend {
    pub fn new(script: FakeScript) -> Self {
        Self {
            script,
            environments: Mutex::new(HashMap::new()),
        }
    }

    /// A fake configured from the environment: `AGENTIFY_FAKE_SCRIPT` carries
    /// the script as JSON. Used when the daemon selects the fake.
    pub fn from_env() -> Self {
        let script = match std::env::var("AGENTIFY_FAKE_SCRIPT") {
            Ok(raw) if !raw.trim().is_empty() => FakeScript::from_json(&raw).unwrap_or_else(|err| {
                panic!("AGENTIFY_FAKE_SCRIPT is not a usable fake script: {err}")
            }),
            _ => FakeScript::default(),
        };
        Self::new(script)
    }

    pub fn script(&self) -> &FakeScript {
        &self.script
    }

    fn environment(&self, name: &str) -> Arc<FakeEnvironment> {
        let mut environments = self.environments.lock().expect("fake environments lock");
        environments
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(FakeEnvironment::new(name)))
            .clone()
    }

    fn existing(&self, name: &str) -> Option<Arc<FakeEnvironment>> {
        self.environments
            .lock()
            .expect("fake environments lock")
            .get(name)
            .cloned()
    }

    fn not_provisioned(environment: &str) -> BackendError {
        BackendError::NotProvisioned {
            detail: format!(
                "no runtime exists for Environment {environment}; start it first"
            ),
        }
    }
}

impl Backend for FakeBackend {
    fn describe(&self) -> BackendInfo {
        BackendInfo {
            name: BACKEND_NAME.to_string(),
            version: BACKEND_VERSION.to_string(),
            platform: PLATFORM.to_string(),
            capabilities: self.script.capabilities.clone(),
        }
    }

    fn start<'a>(&'a self, spec: &'a EnvironmentSpec) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(fault) = self.script.start_fault {
                return Err(fault.error("start"));
            }
            if let Some(path) = &self.script.record_spec_path {
                // What the daemon resolved, exactly as a real adapter would
                // receive it. A test reads this instead of asking the API for a
                // spec the API does not serve.
                let rendered = serde_json::to_vec_pretty(spec).map_err(|err| {
                    BackendError::BackendUnavailable {
                        detail: format!("cannot render the recorded spec: {err}"),
                    }
                })?;
                std::fs::write(path, rendered).map_err(|err| {
                    BackendError::BackendUnavailable {
                        detail: format!("cannot write the recorded spec to {path}: {err}"),
                    }
                })?;
            }
            if self.script.start_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.script.start_delay_ms)).await;
            }

            let environment = self.environment(&spec.environment_id);
            environment.set_state(&spec.environment_id, STATE_STARTING);

            // Idempotent and convergent: keep the listeners we already hold for
            // the ports we were given, bind the rest, drop everything else.
            let mut held: HashMap<u16, TcpListener> = environment
                .listeners
                .lock()
                .expect("fake listeners lock")
                .drain(..)
                .collect();

            let mut bound = Vec::new();
            for lease in &spec.ports {
                if self
                    .script
                    .unbound_resources
                    .iter()
                    .any(|name| name == &lease.resource)
                {
                    // Scripted degradation: the port stays closed, so the
                    // daemon's probe fails and it reports `degraded`.
                    continue;
                }
                let listener = match held.remove(&lease.host_port) {
                    Some(listener) => listener,
                    None => TcpListener::bind(("127.0.0.1", lease.host_port)).map_err(|err| {
                        BackendError::PortUnavailable {
                            port: lease.host_port,
                            holder: format!("the host refused the bind: {err}"),
                        }
                    })?,
                };
                listener
                    .set_nonblocking(true)
                    .map_err(|err| BackendError::BackendUnavailable {
                        detail: format!("cannot manage the leased listener: {err}"),
                    })?;
                bound.push((lease.host_port, listener));
            }

            *environment.listeners.lock().expect("fake listeners lock") = bound;
            *environment.resources.lock().expect("fake resources lock") =
                spec.resources.iter().map(|r| r.name.clone()).collect();
            environment.set_state(&spec.environment_id, STATE_RUNNING);

            for resource in &spec.resources {
                environment.push_log(&resource.name, "stdout", "fake: resource started");
                for lease in spec.leases_for(&resource.name) {
                    environment.push_log(
                        &resource.name,
                        "stdout",
                        &format!("fake: listening on 127.0.0.1:{}", lease.host_port),
                    );
                }
            }
            Ok(())
        })
    }

    fn stop<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.script.stop_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.script.stop_delay_ms)).await;
            }
            // Stopping an Environment that was never started is a no-op, not an
            // error: the caller asked for "stopped" and it is stopped.
            let Some(runtime) = self.existing(environment) else {
                return Ok(());
            };
            runtime
                .listeners
                .lock()
                .expect("fake listeners lock")
                .clear();
            runtime.set_state(environment, STATE_STOPPED);
            runtime
                .push_log("fake", "stdout", "fake: environment stopped");
            Ok(())
        })
    }

    fn remove<'a>(&'a self, environment: &'a str, keep_volumes: bool) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if keep_volumes {
                return Err(BackendError::unsupported("keepVolumes"));
            }
            let mut environments = self.environments.lock().expect("fake environments lock");
            environments.remove(environment);
            Ok(())
        })
    }

    fn status<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<BackendStatus>> {
        Box::pin(async move {
            let Some(runtime) = self.existing(environment) else {
                // Never provisioned is a snapshot, not a failure.
                return Ok(BackendStatus {
                    environment: environment.to_string(),
                    state: STATE_ABSENT.to_string(),
                    resources: Vec::new(),
                });
            };
            let names = runtime.resources.lock().expect("fake resources lock").clone();
            Ok(BackendStatus {
                environment: environment.to_string(),
                state: runtime.state(),
                resources: names
                    .into_iter()
                    .map(|name| ResourceStatus {
                        name,
                        state: STATE_RUNNING.to_string(),
                        message: None,
                    })
                    .collect(),
            })
        })
    }

    fn events<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<EventStream>> {
        Box::pin(async move {
            if !self.script.capabilities.events {
                return Err(BackendError::unsupported("events"));
            }
            let events = match self.existing(environment) {
                Some(runtime) => runtime.events.lock().expect("fake events lock").clone(),
                None => Vec::new(),
            };
            Ok(stream_of(events.into_iter().map(Ok).collect()))
        })
    }

    fn exec<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        spec: &'a ExecSpec,
    ) -> BoxFuture<'a, Result<ExecSession>> {
        Box::pin(async move {
            if let Some(fault) = self.script.exec_fault {
                return Err(fault.error("exec"));
            }
            let runtime = self
                .existing(environment)
                .ok_or_else(|| Self::not_provisioned(environment))?;
            if runtime.state() != STATE_RUNNING {
                return Err(BackendError::NotProvisioned {
                    detail: format!(
                        "Environment {environment} is {}; start it before running a command",
                        runtime.state()
                    ),
                });
            }
            if let Some(target) = target {
                let known = runtime.resources.lock().expect("fake resources lock");
                if !known.iter().any(|name| name == target) {
                    return Err(BackendError::NotProvisioned {
                        detail: format!(
                            "Environment {environment} has no Resource named {target:?}; it has: {}",
                            known.join(", ")
                        ),
                    });
                }
            }

            let plan = plan(&spec.command)?;
            let resource = target.unwrap_or("exec").to_string();

            // An exec's output is also a log line, so `read_logs` shows what an
            // Agent ran, exactly as a real substrate would.
            for (stream, text) in &plan.chunks {
                for line in text.lines() {
                    runtime.push_log(&resource, stream, line);
                }
            }

            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_flag = cancel.clone();
            let delay = Duration::from_millis(self.script.exec_delay_ms);

            let stream = futures_util::stream::unfold(
                (plan.chunks, 0usize, plan.exit, plan.pause_ms, true),
                move |(chunks, index, exit, pause_ms, live)| {
                    let cancel = cancel_flag.clone();
                    async move {
                        if !live {
                            return None;
                        }
                        if cancel.load(Ordering::SeqCst) {
                            return Some((
                                Ok(ExecChunk::Exit { code: 130 }),
                                (Vec::new(), index, 130, 0, false),
                            ));
                        }
                        if index < chunks.len() {
                            let (stream, text) = chunks[index].clone();
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                            let chunk = if stream == "stderr" {
                                ExecChunk::Stderr(text.into_bytes())
                            } else {
                                ExecChunk::Stdout(text.into_bytes())
                            };
                            return Some((
                                Ok(chunk),
                                (chunks, index + 1, exit, pause_ms, true),
                            ));
                        }
                        if pause_ms > 0 {
                            // Interruptible delay: a cancel ends the wait.
                            if !sleep_cancellable(pause_ms, &cancel).await {
                                return Some((
                                    Ok(ExecChunk::Exit { code: 130 }),
                                    (Vec::new(), index, 130, 0, false),
                                ));
                            }
                        }
                        Some((
                            Ok(ExecChunk::Exit { code: exit }),
                            (Vec::new(), index + 1, exit, 0, false),
                        ))
                    }
                },
            );

            Ok(ExecSession::new(stream, move || {
                cancel.store(true, Ordering::SeqCst);
                true
            }))
        })
    }

    fn logs<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        options: &'a LogOptions,
    ) -> BoxFuture<'a, Result<LogStream>> {
        Box::pin(async move {
            let Some(runtime) = self.existing(environment) else {
                return Err(Self::not_provisioned(environment));
            };
            if options.follow && !self.script.capabilities.log_since {
                return Err(BackendError::unsupported("log follow"));
            }

            let selected: Vec<LogLine> = {
                let logs = runtime.logs.lock().expect("fake logs lock");
                logs.iter()
                    .filter(|line| target.is_none_or(|name| line.resource == name))
                    .filter(|line| after_cursor(line, options.since.as_deref()))
                    .cloned()
                    .collect()
            };
            let lines = match options.tail {
                Some(tail) => {
                    let start = selected.len().saturating_sub(tail);
                    selected[start..].to_vec()
                }
                None => selected,
            };

            if !options.follow {
                return Ok(stream_of(lines.into_iter().map(Ok).collect()));
            }

            let cursor = lines.last().map(|line| line.seq).unwrap_or_else(|| {
                runtime
                    .logs
                    .lock()
                    .expect("fake logs lock")
                    .iter()
                    .map(|line| line.seq)
                    .max()
                    .unwrap_or(0)
            });
            let pending: VecDeque<LogLine> = lines.into();
            let logs = runtime.logs.clone();
            let target = target.map(str::to_string);

            let stream = futures_util::stream::unfold(
                (pending, cursor, logs, target),
                |(mut pending, mut cursor, logs, target)| async move {
                    loop {
                        if let Some(line) = pending.pop_front() {
                            cursor = cursor.max(line.seq);
                            return Some((Ok(line), (pending, cursor, logs, target)));
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let snapshot = logs.lock().expect("fake logs lock");
                        for line in snapshot.iter() {
                            if line.seq > cursor
                                && target.as_deref().is_none_or(|name| line.resource == name)
                            {
                                pending.push_back(line.clone());
                            }
                        }
                    }
                },
            );
            Ok(Box::pin(stream) as LogStream)
        })
    }

    fn stats<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<Stats>> {
        Box::pin(async move {
            let state = self
                .existing(environment)
                .map(|runtime| runtime.state())
                .unwrap_or_else(|| STATE_ABSENT.to_string());
            let running = state == STATE_RUNNING;
            Ok(Stats {
                environment: environment.to_string(),
                state,
                cpu_percent: if running { CPU_PERCENT } else { 0.0 },
                memory_bytes: if running { MEMORY_BYTES } else { 0 },
                disk_bytes: if running { DISK_BYTES } else { 0 },
                resources: if running {
                    vec![ResourceStat {
                        name: "environment".to_string(),
                        cpu_percent_milli: (CPU_PERCENT * 1000.0) as u32,
                        memory_bytes: MEMORY_BYTES,
                    }]
                } else {
                    Vec::new()
                },
            })
        })
    }

    fn copy_in<'a>(
        &'a self,
        environment: &'a str,
        _target: Option<&'a str>,
        host_path: &'a Path,
        guest_path: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let runtime = self
                .existing(environment)
                .ok_or_else(|| Self::not_provisioned(environment))?;
            let bytes = std::fs::read(host_path).map_err(|err| BackendError::ExecutionFailed {
                code: 1,
                detail: format!("cannot read {}: {err}", host_path.display()),
            })?;
            runtime
                .files
                .lock()
                .expect("fake files lock")
                .insert(guest_path.to_string(), bytes);
            Ok(())
        })
    }

    fn copy_out<'a>(
        &'a self,
        environment: &'a str,
        _target: Option<&'a str>,
        guest_path: &'a str,
        host_path: &'a Path,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let runtime = self
                .existing(environment)
                .ok_or_else(|| Self::not_provisioned(environment))?;
            let bytes = runtime
                .files
                .lock()
                .expect("fake files lock")
                .get(guest_path)
                .cloned()
                .ok_or_else(|| BackendError::NotProvisioned {
                    detail: format!("no file at {guest_path} in Environment {environment}"),
                })?;
            let mut file = std::fs::File::create(host_path).map_err(|err| {
                BackendError::ExecutionFailed {
                    code: 1,
                    detail: format!("cannot write {}: {err}", host_path.display()),
                }
            })?;
            file.write_all(&bytes).map_err(|err| BackendError::ExecutionFailed {
                code: 1,
                detail: format!("cannot write {}: {err}", host_path.display()),
            })?;
            Ok(())
        })
    }

    fn checkpoint<'a>(&'a self, _environment: &'a str) -> BoxFuture<'a, Result<CheckpointId>> {
        Box::pin(async move {
            if !self.script.capabilities.checkpoint {
                return Err(BackendError::unsupported("checkpoint"));
            }
            Ok("fake-checkpoint".to_string())
        })
    }

    fn restore<'a>(
        &'a self,
        _environment: &'a str,
        _checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.script.capabilities.checkpoint {
                return Err(BackendError::unsupported("restore"));
            }
            Ok(())
        })
    }

    fn delete_checkpoint<'a>(
        &'a self,
        _environment: &'a str,
        _checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.script.capabilities.checkpoint {
                return Err(BackendError::unsupported("deleteCheckpoint"));
            }
            Ok(())
        })
    }
}

/// The output an exec will produce.
struct Plan {
    chunks: Vec<(&'static str, String)>,
    exit: i32,
    /// A pause before the exit chunk, so cancellation has something to interrupt.
    pause_ms: u64,
}

/// Deterministic command semantics, so a test can assert exact output:
///
/// * `echo a b` → stdout `a b\n`, exit 0
/// * `fail 3` → stderr `fake: command failed\n`, exit 3 (default 1)
/// * `sleep 5000` → no output, waits, exit 0
/// * `env` → sorted `KEY=VALUE` lines, exit 0
/// * anything else → stdout `fake: <command line>\n`, exit 0
fn plan(command: &[String]) -> Result<Plan> {
    let Some(first) = command.first() else {
        return Err(BackendError::ExecutionFailed {
            code: 2,
            detail: "a command needs at least one argument".to_string(),
        });
    };
    let rest = &command[1..];
    Ok(match first.as_str() {
        "echo" => Plan {
            chunks: vec![("stdout", format!("{}\n", rest.join(" ")))],
            exit: 0,
            pause_ms: 0,
        },
        "fail" => Plan {
            chunks: vec![("stderr", "fake: command failed\n".to_string())],
            exit: rest
                .first()
                .and_then(|raw| raw.parse::<i32>().ok())
                .unwrap_or(1),
            pause_ms: 0,
        },
        "sleep" => Plan {
            chunks: Vec::new(),
            exit: 0,
            pause_ms: rest
                .first()
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(0),
        },
        "env" => Plan {
            chunks: vec![(
                "stdout",
                rest_to_lines(std::env::vars().collect::<BTreeMap<_, _>>()),
            )],
            exit: 0,
            pause_ms: 0,
        },
        other => Plan {
            chunks: vec![("stdout", format!("fake: {}\n", command.join(" ")))],
            exit: if other.is_empty() { 2 } else { 0 },
            pause_ms: 0,
        },
    })
}

fn rest_to_lines(vars: BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in vars {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// Sleep in small slices so a cancel ends the wait promptly. Returns `false`
/// when the wait was cancelled.
async fn sleep_cancellable(millis: u64, cancel: &AtomicBool) -> bool {
    let mut waited = 0;
    while waited < millis {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        let slice = (millis - waited).min(20);
        tokio::time::sleep(Duration::from_millis(slice)).await;
        waited += slice;
    }
    !cancel.load(Ordering::SeqCst)
}

/// `since` is either a sequence number (exact) or an RFC 3339 timestamp.
fn after_cursor(line: &LogLine, since: Option<&str>) -> bool {
    match since.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => true,
        Some(raw) => match raw.parse::<u64>() {
            Ok(seq) => line.seq > seq,
            Err(_) => line.at.as_str() >= raw,
        },
    }
}

/// RFC 3339, UTC, millisecond precision: comparable as a string.
pub fn now_rfc3339_millis() -> String {
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

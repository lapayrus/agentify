//! The Backend contract: environment-scoped, lifecycle-only, object-safe.
//!
//! It hides whether an Environment is one distro with several containers, a set
//! of per-container microVMs, or a VM. Lifecycle calls are idempotent; `start`
//! re-creates missing workloads; the Backend owns restart policy inside the
//! runtime; port leases are allocated by the daemon and passed in [`EnvironmentSpec`].
//!
//! Every method returns a boxed `Send` future so the daemon can hold an
//! `Arc<dyn Backend>` and swap substrates without touching a call site.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use agentify_proto::Capabilities;
use futures_util::{Stream, StreamExt};

use crate::error::Result;
use crate::spec::EnvironmentSpec;

/// A boxed, `Send` future. The trait's building block.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Environment states the contract recognises. The daemon owns the decision;
/// a Backend only reports what it knows.
pub const STATE_ABSENT: &str = "absent";
pub const STATE_PROVISIONING: &str = "provisioning";
pub const STATE_STOPPED: &str = "stopped";
pub const STATE_STARTING: &str = "starting";
pub const STATE_RUNNING: &str = "running";
pub const STATE_DEGRADED: &str = "degraded";
pub const STATE_FAILED: &str = "failed";

/// What a Backend is and what it can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    /// Stable adapter name: `fake`, `wsl`, `apple-container`, `lima`.
    pub name: String,
    /// Adapter version. The daemon refuses an unexpected major (`VersionMismatch`).
    pub version: String,
    /// Platform the adapter runs on, for diagnostics.
    pub platform: String,
    pub capabilities: Capabilities,
}

/// One waitable change a Backend can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendEvent {
    /// `state_changed`, `resource_exited`, `lease_lost`, `backend_unavailable`.
    pub kind: String,
    pub environment: String,
    pub resource: Option<String>,
    pub state: Option<String>,
    pub detail: Option<String>,
}

/// A Resource's own view of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceStatus {
    pub name: String,
    /// `stopped`, `starting`, `running`, `exited`, `failed`.
    pub state: String,
    pub message: Option<String>,
}

/// A snapshot of an Environment, as the Backend sees it. Health is *not* here:
/// the daemon decides that through the leased ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub environment: String,
    pub state: String,
    pub resources: Vec<ResourceStatus>,
}

/// What to run inside a Resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecSpec {
    /// `argv`: no shell, no interactive stdin.
    pub command: Vec<String>,
    /// Directory to run in, relative to the Resource's root.
    pub cwd: Option<String>,
    /// Extra environment variables; the Resource's own environment is inherited.
    pub env: BTreeMap<String, String>,
    /// Hard bound on the call.
    pub timeout: Option<std::time::Duration>,
    /// Bounded stdin, for a command that expects a body.
    pub stdin: Option<Vec<u8>>,
}

/// One item of a streamed exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command finished. Always the last item when the call ran to the end.
    Exit { code: i32 },
}

/// A running (or finished) exec. `Stream` yields output in order and ends after
/// [`ExecChunk::Exit`].
pub struct ExecSession {
    chunks: Pin<Box<dyn Stream<Item = Result<ExecChunk>> + Send>>,
    cancel: Box<dyn Fn() -> bool + Send + Sync>,
}

impl ExecSession {
    pub fn new(
        chunks: impl Stream<Item = Result<ExecChunk>> + Send + 'static,
        cancel: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            chunks: Box::pin(chunks),
            cancel: Box::new(cancel),
        }
    }

    /// An exec that has already produced everything. Cancellation is not
    /// supported: it is already over.
    pub fn finished(chunks: Vec<ExecChunk>) -> Self {
        Self::new(futures_util::stream::iter(chunks.into_iter().map(Ok)), || false)
    }

    /// Ask the Backend to stop the command. Returns `true` when the Backend
    /// supports cancellation and has been told; the stream then ends promptly.
    pub fn cancel(&self) -> bool {
        (self.cancel)()
    }

    /// The next chunk, or `None` once the stream is done.
    pub async fn next_chunk(&mut self) -> Option<Result<ExecChunk>> {
        self.next().await
    }

    /// Drain the session into `(stdout, stderr, exit_code)`.
    pub async fn collect(mut self) -> Result<(Vec<u8>, Vec<u8>, i32)> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut code = 0;
        while let Some(chunk) = self.next().await {
            match chunk? {
                ExecChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                ExecChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                ExecChunk::Exit { code: exit } => {
                    code = exit;
                    break;
                }
            }
        }
        Ok((stdout, stderr, code))
    }
}

impl Stream for ExecSession {
    type Item = Result<ExecChunk>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.chunks.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for ExecSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExecSession")
    }
}

/// One line of Resource output.
///
/// `Serialize`/`Deserialize` exist so an adapter can keep a log on disk in
/// exactly the shape it streams: the WSL adapter writes one JSON object per
/// line and reads the same struct back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    /// Monotonic per Environment. A `since` cursor that is a bare number is a
    /// sequence number, which makes tails exact even within one millisecond.
    pub seq: u64,
    /// RFC 3339 with millisecond precision, UTC.
    pub at: String,
    pub resource: String,
    /// `stdout` or `stderr`.
    pub stream: String,
    pub line: String,
}

/// How much log to read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogOptions {
    /// Keep only the last N lines.
    pub tail: Option<usize>,
    /// Keep only lines strictly after this cursor: a sequence number, or an
    /// RFC 3339 timestamp.
    pub since: Option<String>,
    /// Keep the stream open and yield new lines as they appear.
    pub follow: bool,
}

impl LogOptions {
    pub fn tail(n: usize) -> Self {
        Self {
            tail: Some(n),
            ..Self::default()
        }
    }

    pub fn following() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    pub fn since(mut self, cursor: impl Into<String>) -> Self {
        self.since = Some(cursor.into());
        self
    }
}

/// A Backend's environment-level figures. Per-Resource figures are optional:
/// not every substrate reports them.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub environment: String,
    pub state: String,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub resources: Vec<ResourceStat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceStat {
    pub name: String,
    pub cpu_percent_milli: u32,
    pub memory_bytes: u64,
}

/// A Backend identifier for a Checkpoint.
pub type CheckpointId = String;

/// A stream of log lines.
pub type LogStream = Pin<Box<dyn Stream<Item = Result<LogLine>> + Send>>;

/// A stream of Backend events.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<BackendEvent>> + Send>>;

/// The substitutable runtime substrate.
///
/// Object-safe: the daemon holds `Arc<dyn Backend>`; every method takes `&self`
/// and returns a boxed future.
pub trait Backend: Send + Sync {
    /// What this Backend is and what it can do.
    fn describe(&self) -> BackendInfo;

    /// Bring the Environment up, idempotently and convergently. Returns when
    /// every Resource is running.
    fn start<'a>(&'a self, spec: &'a EnvironmentSpec) -> BoxFuture<'a, Result<()>>;

    /// Stop the Environment's workloads, keeping the runtime and the data.
    fn stop<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Destroy the runtime. `keep_volumes` is `Unsupported` in v0.
    fn remove<'a>(&'a self, environment: &'a str, keep_volumes: bool) -> BoxFuture<'a, Result<()>>;

    /// Snapshot of the Environment, as the Backend sees it.
    fn status<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<BackendStatus>>;

    /// Changes the Backend can report. Optional capability.
    fn events<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<EventStream>>;

    /// Run a bounded command in a Resource and stream its output.
    fn exec<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        spec: &'a ExecSpec,
    ) -> BoxFuture<'a, Result<ExecSession>>;

    /// Read Resource logs, with `tail`, `since`, and `follow`.
    fn logs<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        options: &'a LogOptions,
    ) -> BoxFuture<'a, Result<LogStream>>;

    /// Environment-level figures; per-Resource figures when capable.
    fn stats<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<Stats>>;

    /// Copy a daemon-owned host path into the runtime. Optional capability.
    fn copy_in<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        host_path: &'a Path,
        guest_path: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    /// Copy a path out of the runtime into a daemon-owned host path.
    fn copy_out<'a>(
        &'a self,
        environment: &'a str,
        target: Option<&'a str>,
        guest_path: &'a str,
        host_path: &'a Path,
    ) -> BoxFuture<'a, Result<()>>;

    /// Create a Checkpoint. Optional capability.
    fn checkpoint<'a>(&'a self, environment: &'a str) -> BoxFuture<'a, Result<CheckpointId>>;

    /// Restore a Checkpoint. Optional capability, Approval-gated at the API.
    fn restore<'a>(
        &'a self,
        environment: &'a str,
        checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>>;

    /// Delete a Checkpoint. Optional capability.
    fn delete_checkpoint<'a>(
        &'a self,
        environment: &'a str,
        checkpoint: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
}

/// Helper for implementations: a stream of already-known items.
pub(crate) fn stream_of<T: Send + 'static>(items: Vec<T>) -> Pin<Box<dyn Stream<Item = T> + Send>> {
    Box::pin(futures_util::stream::iter(items))
}

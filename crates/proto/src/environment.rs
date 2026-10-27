//! Environment lifecycle payloads served under
//! `/v1/projects/{project}/environments/{env}`.
//!
//! State is the daemon's: a Backend reports what it sees, the daemon probes the
//! leased ports and decides `running` versus `degraded`. An Agent and the UI
//! read the same snapshot, so there is one truth.

use serde::{Deserialize, Serialize};

use crate::project::EnvironmentView;

/// State of an Environment that has never been started.
pub const STATE_ABSENT: &str = "absent";
/// The Backend is bringing Resources up.
pub const STATE_STARTING: &str = "starting";
/// Every Resource is up and every leased port answered a probe.
pub const STATE_RUNNING: &str = "running";
/// The Backend is up but at least one leased port did not answer.
pub const STATE_DEGRADED: &str = "degraded";
/// The last start or stop failed.
pub const STATE_FAILED: &str = "failed";

/// Health of an Environment or Resource. `unknown` is the honest answer before
/// a probe has run.
pub const HEALTH_HEALTHY: &str = "healthy";
pub const HEALTH_UNHEALTHY: &str = "unhealthy";
pub const HEALTH_UNKNOWN: &str = "unknown";

/// Which Backend is serving this daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRef {
    pub name: String,
    pub version: String,
    pub platform: String,
}

/// A host port the daemon leased for a guest port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeasedPortView {
    #[serde(rename = "guestPort")]
    pub guest_port: u16,
    #[serde(rename = "hostPort")]
    pub host_port: u16,
}

/// One Resource in a status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceStatusView {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub health: String,
    pub ports: Vec<LeasedPortView>,
}

/// `GET .../status`, and the payload `start` and `stop` return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    pub project: String,
    #[serde(rename = "projectId")]
    pub project_id: String,
    pub environment: EnvironmentView,
    /// `healthy`, `unhealthy`, or `unknown`.
    pub health: String,
    pub resources: Vec<ResourceStatusView>,
    pub backend: BackendRef,
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
    /// The operation this response came from. Present on a mutation.
    #[serde(rename = "operationId", skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// Why a state is what it is: the detail an Agent needs to act.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `POST .../exec`. A bounded command in one Resource; no interactive stdin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    /// `argv`, run without a shell.
    pub command: Vec<String>,
    /// Bound on the call, in milliseconds. The daemon caps it too.
    #[serde(default, rename = "timeoutMs", skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// What a command produced, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResponse {
    pub resource: String,
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "exitCode")]
    pub exit_code: i32,
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
}

/// One log line as the API serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLineView {
    /// Monotonic per Environment; pass it back as `since` for a live tail.
    pub seq: u64,
    pub at: String,
    pub resource: String,
    /// `stdout` or `stderr`.
    pub stream: String,
    pub line: String,
}

/// `GET .../logs` without following.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsResponse {
    pub lines: Vec<LogLineView>,
    /// Cursor to pass as `since` for the next read. `null` when there is nothing
    /// to continue from.
    #[serde(rename = "nextSince", skip_serializing_if = "Option::is_none")]
    pub next_since: Option<String>,
}

/// One Resource's figures, when the Backend reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceStatsView {
    pub name: String,
    #[serde(rename = "cpuPercent")]
    pub cpu_percent: String,
    pub memory: String,
}

/// `GET .../stats`: environment-level figures, formatted for a status bar and
/// also available as raw numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsResponse {
    pub state: String,
    #[serde(rename = "cpuPercent")]
    pub cpu_percent: f64,
    #[serde(rename = "memoryBytes")]
    pub memory_bytes: u64,
    #[serde(rename = "diskBytes")]
    pub disk_bytes: u64,
    pub cpu: String,
    pub memory: String,
    pub disk: String,
    pub resources: Vec<ResourceStatsView>,
}

/// `GET /v1/operations/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationView {
    pub id: String,
    /// `start`, `stop`.
    pub kind: String,
    /// `running`, `succeeded`, or `failed`.
    pub state: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "finishedAt", skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(rename = "errorDetail", skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    /// The payload the mutation produced when it finished on this side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

impl OperationView {
    /// True when nothing more will happen to this operation. An operation
    /// waiting for an Approval is not terminal: a human still has to decide,
    /// and the caller should keep waiting rather than retry.
    pub fn is_terminal(&self) -> bool {
        self.state != "running" && self.state != crate::OPERATION_WAITING_FOR_APPROVAL
    }
}

/// Header that makes a mutation replayable for 24 hours.
pub const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";

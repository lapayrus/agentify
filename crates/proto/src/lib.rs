//! Wire types and on-disk formats shared by every Agentify process.
//!
//! Nothing here performs I/O: the discovery file *format* and the API payloads
//! live in one place so the daemon, the MCP shim, the CLI, and the desktop
//! shell cannot drift apart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod paths;

pub mod client;

pub use paths::{resolve_data_root, APP_DIR_NAME, HOME_ENV};

/// Version of the loopback HTTP API. Every path served by the daemon lives
/// under `/v1`; a breaking change bumps this constant (ADR-0003).
pub const API_VERSION: &str = "v1";

/// File name of the discovery file inside the daemon's data root.
pub const DISCOVERY_FILE_NAME: &str = "discovery.json";

/// File name of the SQLite state database inside the daemon's data root.
pub const STATE_DB_FILE_NAME: &str = "state.db";

/// Coarse capability scope: read state.
pub const SCOPE_READ: &str = "read";

/// Coarse capability scope: start, stop, and mutate resources.
pub const SCOPE_OPERATE: &str = "operate";

/// Coarse capability scope: destructive operations. Never granted to the
/// token handed out through the discovery file.
pub const SCOPE_ADMIN: &str = "admin";

/// What a client reads out of the data root to talk to a running daemon.
///
/// The file always grants exactly one OS user access (the daemon's user) and
/// carries an Agent-scoped token, never a human-capable one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    /// PID of the daemon that wrote the file.
    pub pid: u32,
    /// Loopback port the daemon is listening on.
    pub port: u16,
    /// `http://127.0.0.1:<port>` — a convenience for clients.
    pub url: String,
    /// Agent-scoped bearer token. Only valid for this daemon run.
    pub token: String,
    /// Daemon version that wrote the file.
    pub daemon_version: String,
    /// API version served at `url`.
    pub api_version: String,
    /// RFC 3339 timestamp of when the daemon wrote the file.
    pub created_at: String,
}

/// Identity of the calling Agent, as resolved by the daemon from its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// Always `agent` in v0: the discovery token belongs to an Agent.
    pub kind: String,
    /// Stable identifier of the token that authenticated the request.
    pub id: String,
}

/// A Project reference. `None` in [`Whoami::project`] means the caller is not
/// bound to a Project (global context).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

/// What the daemon can actually do right now. Reported so an Agent does not
/// discover limits through failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Interactive terminals.
    pub pty: bool,
    /// Host-directory mounts.
    pub mount: bool,
    /// Checkpoint create/restore.
    pub checkpoint: bool,
    /// Server-sent events stream.
    #[serde(rename = "events")]
    pub events: bool,
    /// Per-Resource CPU/memory/disk figures.
    #[serde(rename = "resourceStats")]
    pub resource_stats: bool,
    /// Log tails honouring a `since` cursor.
    #[serde(rename = "logSince")]
    pub log_since: bool,
}

impl Capabilities {
    /// What the walking skeleton can do: an events stream and nothing else.
    pub fn v0_skeleton() -> Self {
        Self {
            pty: false,
            mount: false,
            checkpoint: false,
            events: true,
            resource_stats: false,
            log_since: false,
        }
    }
}

/// Per-run facts about the daemon serving the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// Minted once, on the first start against a data root, and then stable.
    /// Clients use it to tell "same installation" from "fresh data root".
    pub instance_id: String,
    /// RFC 3339 timestamp of this process start.
    pub started_at: String,
}

/// Payload of `GET /v1/whoami` and of the MCP `whoami` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Whoami {
    /// `null` when the caller is not bound to a Project.
    pub project: Option<ProjectRef>,
    /// Coarse scopes the presented token carries.
    pub scopes: Vec<String>,
    pub daemon_version: String,
    pub api_version: String,
    pub capabilities: Capabilities,
    pub actor: Actor,
    pub daemon: DaemonInfo,
}

/// `application/problem+json` body used for every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// URI identifying the problem type; `about:blank` while there is only one.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    /// Stable machine-readable code an Agent may branch on.
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    pub fn new(status: u16, code: &str, title: &str, detail: Option<String>) -> Self {
        Self {
            kind: "about:blank".to_string(),
            title: title.to_string(),
            status,
            code: code.to_string(),
            detail,
        }
    }
}

/// Path of the discovery file for a data root.
pub fn discovery_path(data_root: &Path) -> PathBuf {
    data_root.join(DISCOVERY_FILE_NAME)
}

/// Path of the state database for a data root.
pub fn state_db_path(data_root: &Path) -> PathBuf {
    data_root.join(STATE_DB_FILE_NAME)
}

/// Read and parse a discovery file.
pub fn read_discovery(data_root: &Path) -> std::io::Result<Discovery> {
    let path = discovery_path(data_root);
    let raw = std::fs::read_to_string(&path)?;
    serde_json::from_str(&raw).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a valid discovery file: {err}", path.display()),
        )
    })
}

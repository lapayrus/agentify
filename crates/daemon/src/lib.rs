//! Agentify control plane.
//!
//! The daemon owns all state and mutations and serves a loopback-only HTTP
//! API (ADR-0003). The desktop shell, the CLI, and the MCP shim are clients of
//! that API; none of them has a privileged path.

pub mod acl;
pub mod approval;
pub mod audit;
pub mod auth;
pub mod backends;
pub mod clock;
pub mod config;
pub mod consent;
pub mod disk;
pub mod discovery;
pub mod fixtures;
pub mod handshake;
pub mod health;
pub mod ingress;
pub mod lifecycle;
pub mod logfile;
pub mod login_item;
pub mod ports;
pub mod projects;
pub mod redact;
pub mod secrets;
pub mod server;
pub mod state;
pub mod uninstall;

pub use config::Config;
pub use lifecycle::Lifecycle;
pub use logfile::DaemonLog;
pub use server::{start_in_background, RunningDaemon, DAEMON_VERSION};

/// Write a line to the daemon's log file, creating it if needed.
///
/// Startup failures happen before a `DaemonLog` exists in the server, and a
/// release daemon has no console to print to, so this is the one fallback.
pub fn log_error(data_root: &std::path::Path, message: &str) {
    if let Ok(log) = DaemonLog::open(data_root) {
        log.write(message);
    }
}

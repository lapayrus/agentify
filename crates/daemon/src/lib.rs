//! Agentify control plane.
//!
//! The daemon owns all state and mutations and serves a loopback-only HTTP
//! API (ADR-0003). The desktop shell, the CLI, and the MCP shim are clients of
//! that API; none of them has a privileged path.

pub mod acl;
pub mod auth;
pub mod backends;
pub mod clock;
pub mod config;
pub mod discovery;
pub mod fixtures;
pub mod health;
pub mod lifecycle;
pub mod ports;
pub mod projects;
pub mod server;
pub mod state;

pub use config::Config;
pub use lifecycle::Lifecycle;
pub use server::{start_in_background, RunningDaemon, DAEMON_VERSION};

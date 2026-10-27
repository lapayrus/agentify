//! Local ingress: one Caddy instance the daemon owns.
//!
//! Caddy terminates TLS and routes `<resource>.<project>.localhost` to the host
//! port the daemon leased for that Resource. The daemon never edits routes by
//! hand: they are derived from leases on every start and stop, pushed to Caddy
//! through its admin API, and never exposed on the LAN.
//!
//! The pieces:
//!
//! * [`routes`] — the route model and the JSON Caddy is asked to serve. Pure
//!   functions, so the shape of a route is a testable fact.
//! * [`admin`] — a small admin client for `POST /load` and the granular
//!   `PUT`/`POST`/`DELETE` on one route inside one server.
//! * [`caddy`] — finding the binary and running it (no downloads, ever).
//! * [`trust`] — the `TrustInstaller` seam. Windows builds the per-user
//!   `certutil -addstore -user Root` command and the Firefox enterprise-roots
//!   policy command, but only the explicit opt-in path ever runs them.
//! * [`ports`] — probing the ingress ports before Caddy is started, so a
//!   conflict is a typed `PortUnavailable` instead of a mystery.
//! * [`manager`] — the `Ingress` the daemon calls: start, sync, remove.

pub mod admin;
pub mod caddy;
pub mod http;
pub mod manager;
pub mod ports;
pub mod routes;
pub mod trust;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use admin::{AdminClient, AdminError, HttpAdminClient};
pub use caddy::{CaddyBinary, CaddyError, CaddyInstance};
pub use manager::{AdminMode, Ingress, IngressError, IngressOptions, IngressStatus, RouteSnapshot};
pub use ports::{PortIssue, PortProbe};
pub use routes::{Route, RouteTarget, HttpMode, SERVER_HTTP, SERVER_TLS};
pub use trust::{
    Elevation, ManualTrustInstaller, TrustError, TrustInstaller, TrustPrepare, TrustReport,
    TrustState, TrustStep, WindowsTrustInstaller,
};

//! What the daemon hands a Backend when it starts an Environment.
//!
//! The daemon owns the ports: it leases a host port for every declared guest
//! port and passes the mapping here. A Backend binds what it is given and
//! invents nothing, which is what lets the daemon decide health the same way on
//! every substrate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One host port the daemon leased for a guest port of a Resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortLease {
    /// Resource the port belongs to.
    pub resource: String,
    /// The port the Resource listens on inside the runtime.
    pub guest_port: u16,
    /// The port the daemon leased on `127.0.0.1` and expects to be forwarded.
    pub host_port: u16,
}

/// A build context. Both paths are relative to the Project's workspace, which
/// the daemon owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpec {
    pub context: String,
    pub dockerfile: String,
}

/// A declared readiness probe. `healthcheck` defaults to a TCP probe on the
/// Resource's first port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthcheckSpec {
    pub port: u16,
    pub path: Option<String>,
    pub interval: String,
}

/// One Resource to bring up, as normalized from the Manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSpec {
    pub name: String,
    /// `app` or `database`.
    pub kind: String,
    pub image: Option<String>,
    pub build: Option<BuildSpec>,
    pub command: Vec<String>,
    /// Environment variables. Values are resolved by the daemon; secrets are
    /// never inlined into the Manifest.
    pub env: BTreeMap<String, String>,
    /// Guest ports this Resource listens on.
    pub ports: Vec<u16>,
    pub depends_on: Vec<String>,
    pub healthcheck: Option<HealthcheckSpec>,
    pub engine: Option<String>,
    pub version: Option<String>,
    pub volume: Option<String>,
}

/// Everything a Backend needs to bring one Environment up. Idempotent and
/// convergent: starting twice with the same spec leaves the same runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    /// Daemon-minted Environment id: the Backend's handle for this Environment.
    pub environment_id: String,
    /// Project name, used for human-readable names inside the runtime.
    pub project: String,
    /// The daemon-owned source directory. A Backend never reads a host path it
    /// was not given here.
    pub workspace: PathBuf,
    /// Every guest port the daemon leased.
    pub ports: Vec<PortLease>,
    pub resources: Vec<ResourceSpec>,
}

impl EnvironmentSpec {
    pub fn new(environment_id: impl Into<String>, project: impl Into<String>) -> Self {
        Self {
            environment_id: environment_id.into(),
            project: project.into(),
            workspace: PathBuf::new(),
            ports: Vec::new(),
            resources: Vec::new(),
        }
    }

    pub fn lease(&mut self, resource: impl Into<String>, guest_port: u16, host_port: u16) -> &mut Self {
        self.ports.push(PortLease {
            resource: resource.into(),
            guest_port,
            host_port,
        });
        self
    }

    pub fn with_resource(&mut self, resource: ResourceSpec) -> &mut Self {
        self.resources.push(resource);
        self
    }

    pub fn resource(&self, name: &str) -> Option<&ResourceSpec> {
        self.resources.iter().find(|resource| resource.name == name)
    }

    /// The leased host port for a resource's guest port.
    pub fn host_port(&self, resource: &str, guest_port: u16) -> Option<u16> {
        self.ports
            .iter()
            .find(|lease| lease.resource == resource && lease.guest_port == guest_port)
            .map(|lease| lease.host_port)
    }

    /// Every lease for one Resource.
    pub fn leases_for<'a>(&'a self, resource: &'a str) -> impl Iterator<Item = &'a PortLease> + 'a {
        self.ports.iter().filter(move |lease| lease.resource == resource)
    }

    /// Resource names, in dependency-friendly sorted order.
    pub fn resource_names(&self) -> Vec<String> {
        self.resources.iter().map(|r| r.name.clone()).collect()
    }
}

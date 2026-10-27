//! Environment orchestration.
//!
//! This is the daemon's half of the contract: it leases ports, records desired
//! and observed state, drives the Backend, probes the leased ports itself, and
//! reports one status snapshot that the API, MCP, and the UI all read.
//!
//! Rules that hold on every path:
//!
//! * the SQLite mutex is never held across an `await` — every store call is a
//!   short synchronous function that returns owned data;
//! * mutating calls are serialised per Environment by the caller
//!   ([`crate::server`] takes the lock before calling in), so two starts cannot
//!   interleave;
//! * `start` is idempotent and convergent: it always re-derives the spec, always
//!   re-acquires its leases, and always calls the Backend, whose `start` is
//!   itself idempotent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentify_backends::{Backend, BackendError, ExecChunk, ExecSpec, LogOptions, LogStream};
use agentify_ingress::{Ingress, IngressError};
use agentify_proto::{
    BackendRef, Cost, EnvironmentStatus, EnvironmentView, ExecRequest, ExecResponse, LeasedPortView,
    LogLineView, ResourceStatsView, ResourceStatusView, StatsResponse, HEALTH_HEALTHY,
    HEALTH_UNHEALTHY, HEALTH_UNKNOWN, STATE_ABSENT, STATE_DEGRADED, STATE_FAILED, STATE_RUNNING,
    STATE_STARTING, STATE_STOPPED,
};
use anyhow::Result as AnyResult;

use crate::clock;
use crate::health;
use crate::ingress as routes;
use crate::ports::{PortError, PortRegistry};
use crate::projects;
use crate::state::{EnvironmentRecord, LeaseRecord, ProjectRecord, State, StateUpdate};

/// How many parts of the lease range a start may try when something outside the
/// registry binds a port between the check and the bind.
const LEASE_ATTEMPTS: u32 = 3;

/// Why a lifecycle call failed.
#[derive(Debug)]
pub enum LifecycleError {
    /// No Project, Environment, or Resource by that reference.
    NotFound(String),
    /// The request itself is wrong; `path` names the field.
    Validation { path: String, detail: String },
    /// The Backend refused or failed.
    Backend(BackendError),
    /// No host port could be leased.
    Port { port: u16, holder: String },
    /// A secret the Manifest references has no value, or the store failed.
    Secret(String),
    /// Local ingress could not be configured: a port conflict, or no Caddy.
    Ingress(IngressError),
    /// The store failed.
    Store(anyhow::Error),
}

impl LifecycleError {
    /// Stable code for the `application/problem+json` body.
    pub fn code(&self) -> String {
        match self {
            LifecycleError::NotFound(_) => "NotFound".to_string(),
            LifecycleError::Validation { .. } => "ValidationFailed".to_string(),
            LifecycleError::Backend(err) => err.code().to_string(),
            LifecycleError::Port { .. } => "PortUnavailable".to_string(),
            LifecycleError::Secret(_) => "SecretSourceMissing".to_string(),
            LifecycleError::Ingress(err) => err.code().to_string(),
            LifecycleError::Store(_) => "Internal".to_string(),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            LifecycleError::NotFound(_) => 404,
            LifecycleError::Validation { .. } => 422,
            LifecycleError::Backend(err) => err.status(),
            LifecycleError::Port { .. } => 409,
            LifecycleError::Secret(_) => 409,
            LifecycleError::Ingress(err) => err.status(),
            LifecycleError::Store(_) => 500,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            LifecycleError::NotFound(detail) => detail.clone(),
            LifecycleError::Validation { detail, .. } => detail.clone(),
            LifecycleError::Backend(err) => err.to_string(),
            LifecycleError::Port { port, holder } => {
                format!("no host port could be leased: port {port} is held by {holder}")
            }
            LifecycleError::Secret(detail) => detail.clone(),
            LifecycleError::Ingress(err) => err.detail(),
            LifecycleError::Store(err) => format!("{err:#}"),
        }
    }

    /// The field a validation failure points at, for the problem body.
    pub fn path(&self) -> Option<&str> {
        match self {
            LifecycleError::Validation { path, .. } => Some(path),
            _ => None,
        }
    }
}

impl From<BackendError> for LifecycleError {
    fn from(err: BackendError) -> Self {
        LifecycleError::Backend(err)
    }
}

impl From<PortError> for LifecycleError {
    fn from(err: PortError) -> Self {
        match err {
            PortError::Unavailable { port, holder } => LifecycleError::Port { port, holder },
            PortError::Store(err) => LifecycleError::Store(err),
        }
    }
}

impl From<IngressError> for LifecycleError {
    fn from(err: IngressError) -> Self {
        LifecycleError::Ingress(err)
    }
}

impl From<anyhow::Error> for LifecycleError {
    fn from(err: anyhow::Error) -> Self {
        LifecycleError::Store(err)
    }
}

/// The daemon's Environment orchestration: one Backend, one port registry, one
/// set of knobs.
pub struct Lifecycle {
    pub backend: Arc<dyn Backend>,
    pub ports: PortRegistry,
    /// The Project's secret values, resolved into the spec before every start.
    pub secrets: Arc<crate::secrets::Secrets>,
    /// Local ingress, or `None` when the daemon runs without publishing
    /// anything. Routes are pushed here on every start and stop.
    pub ingress: Option<Arc<Ingress>>,
    /// How long the daemon waits for one leased port to answer.
    pub probe_timeout: Duration,
    /// Upper bound the daemon puts on one `exec`.
    pub exec_timeout: Duration,
    /// Set when the startup handshake refused this Backend: every call that
    /// would drive the substrate answers `VersionMismatch` instead.
    pub refusal: Option<String>,
}

impl Lifecycle {
    /// The gate the startup handshake puts in front of the Backend.
    ///
    /// A mismatched adapter is not driven at all: half-working lifecycle calls
    /// against a substrate this build does not understand are worse than a
    /// typed refusal that names the versions.
    fn gate(&self) -> Result<(), LifecycleError> {
        match &self.refusal {
            None => Ok(()),
            Some(refusal) => Err(LifecycleError::Backend(
                crate::handshake::refusal_error(refusal),
            )),
        }
    }

    pub fn backend_ref(&self) -> BackendRef {
        let info = self.backend.describe();
        BackendRef {
            name: info.name,
            version: info.version,
            platform: info.platform,
        }
    }

    /// Publish one Environment's routes, or keep them answering 503.
    fn publish(
        &self,
        record: &ProjectRecord,
        leases: &[LeaseRecord],
        running: bool,
    ) -> Result<(), LifecycleError> {
        let Some(ingress) = self.ingress.as_ref() else {
            return Ok(());
        };
        let routes = routes::environment_routes(record, leases, running)?;
        ingress
            .sync_environment(&record.environment.id, &routes)
            .map_err(LifecycleError::Ingress)
    }

    /// Bring an Environment up. Idempotent and convergent.
    pub async fn start(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<EnvironmentStatus, LifecycleError> {
        self.gate()?;
        let environment_id = record.environment.id.clone();
        state.set_environment_state(
            &environment_id,
            &StateUpdate {
                desired_state: STATE_RUNNING.to_string(),
                observed_state: STATE_STARTING.to_string(),
                health: HEALTH_UNKNOWN.to_string(),
            },
        )?;

        // Lease before the Backend runs: the port is what the Backend binds and
        // what the daemon probes. A bind race with another process on the
        // machine is retried on a fresh part of the range; a port held by
        // another Environment is a typed failure, not a retry.
        let base = projects::environment_spec(record, &self.secrets)?;
        let requests = projects::port_requests(record)?;
        let mut leases = Vec::new();
        let mut spec = base.clone();
        for attempt in 0..LEASE_ATTEMPTS {
            if attempt > 0 {
                // The previous attempt's ports are not usable; give them back.
                self.ports.release_all(state, &environment_id)?;
                spec = base.clone();
            }
            leases.clear();
            let mut port_failure = None;
            for (resource, guest_port) in &requests {
                match self
                    .ports
                    .allocate(state, &environment_id, resource, *guest_port, attempt)
                {
                    Ok(host_port) => {
                        spec.lease(resource.clone(), *guest_port, host_port);
                        leases.push(LeaseRecord {
                            environment_id: environment_id.clone(),
                            resource: resource.clone(),
                            guest_port: *guest_port,
                            host_port,
                            created_at: clock::now_rfc3339(),
                        });
                    }
                    Err(err) => {
                        port_failure = Some(err);
                        break;
                    }
                }
            }
            if let Some(err) = port_failure {
                self.mark(&state, &environment_id, STATE_RUNNING, STATE_FAILED, HEALTH_UNHEALTHY)?;
                return Err(err.into());
            }

            match self.backend.start(&spec).await {
                Ok(()) => break,
                Err(BackendError::PortUnavailable { .. }) if attempt + 1 < LEASE_ATTEMPTS => {
                    // Something outside the registry bound a port between the
                    // check and the bind. Try another part of the range.
                    continue;
                }
                Err(err) => {
                    self.mark(&state, &environment_id, STATE_RUNNING, STATE_FAILED, HEALTH_UNHEALTHY)?;
                    return Err(err.into());
                }
            }
        }

        let resources = self.probe(state, record, &leases).await?;

        // Publish before claiming health: an Environment whose URLs are not
        // answering is not "up", and a port conflict at this point is a typed
        // `PortUnavailable` rather than a start that quietly half-happened.
        if let Err(err) = self.publish(record, &leases, true) {
            self.mark(&state, &environment_id, STATE_RUNNING, STATE_DEGRADED, HEALTH_UNHEALTHY)?;
            return Err(err);
        }

        let (health, observed) = summarise(&resources, STATE_RUNNING);
        self.mark(&state, &environment_id, STATE_RUNNING, &observed, &health)?;
        Ok(self.view(record, &observed, &health, &resources))
    }

    /// Stop the workloads and release the leases. Idempotent.
    ///
    /// The routes stay, and switch to the daemon's 503: a bookmarked URL must
    /// keep answering after a stop, and must only disappear when the
    /// Environment does.
    pub async fn stop(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<EnvironmentStatus, LifecycleError> {
        self.gate()?;
        let environment_id = record.environment.id.clone();
        if let Err(err) = self.backend.stop(&environment_id).await {
            self.mark(&state, &environment_id, STATE_STOPPED, STATE_FAILED, HEALTH_UNHEALTHY)?;
            return Err(err.into());
        }
        // Publish the stopped page while the leases still exist, so the routes
        // are keyed off the same set of ports the Environment had.
        let leases = state.leases(&environment_id)?;
        let published = self.publish(record, &leases, false);
        self.ports.release_all(state, &environment_id)?;
        self.mark(&state, &environment_id, STATE_STOPPED, STATE_STOPPED, HEALTH_UNKNOWN)?;
        published?;
        let resources = stopped_resources(record);
        Ok(self.view(record, STATE_STOPPED, HEALTH_UNKNOWN, &resources))
    }

    /// Republish every Environment's routes from the database at startup.
    ///
    /// Routes are never read back from Caddy: they are recomputed from the
    /// Manifest plus the leases on every start and stop. A daemon that comes
    /// back after a restart therefore has to put them back itself, or a URL the
    /// machine was serving would stop answering because the daemon did.
    ///
    /// Nothing is published when there is nothing to publish, so an idle machine
    /// does not start an ingress it never asked for. A failure is the caller's
    /// to report: the API still serves, and the next start republishes.
    pub fn republish(&self, state: &State) -> Result<(), LifecycleError> {
        let Some(ingress) = self.ingress.as_ref() else {
            return Ok(());
        };
        let routes = routes::recovery_routes(state)?;
        if routes.is_empty() {
            return Ok(());
        }
        ingress.load_all(&routes).map_err(LifecycleError::Ingress)
    }

    /// Withdraw an Environment's routes for good, without touching its runtime.
    ///
    /// Deleting the Environment is the only thing that deletes a route, so a
    /// path that removes the rows itself (the Approval-driven delete) still has
    /// to call this: a stop only replaces the route with the 503.
    pub fn withdraw_routes(&self, environment_id: &str) -> Result<(), LifecycleError> {
        let Some(ingress) = self.ingress.as_ref() else {
            return Ok(());
        };
        ingress
            .remove_environment(environment_id)
            .map_err(LifecycleError::Ingress)
    }

    /// Remove an Environment for good: the runtime, its leases, and its routes.
    ///
    /// Deleting an Environment is the only thing that deletes a route.
    pub async fn remove(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<(), LifecycleError> {
        self.gate()?;
        let environment_id = record.environment.id.clone();
        self.backend
            .remove(&environment_id, false)
            .await
            .map_err(LifecycleError::Backend)?;
        self.withdraw_routes(&environment_id)?;
        self.ports.release_all(state, &environment_id)?;
        state
            .delete_project(&record.id)
            .map_err(LifecycleError::Store)?;
        let workspace = std::path::PathBuf::from(&record.workspace_path);
        if let Some(project_dir) = workspace.parent() {
            let _ = std::fs::remove_dir_all(project_dir);
        }
        Ok(())
    }

    /// Read the Environment's status, probing the leased ports. This persists
    /// what it observed, so the rail and the status bar cannot disagree.
    pub async fn status(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<EnvironmentStatus, LifecycleError> {
        self.gate()?;
        let stored = self.stored_environment(state, record)?;
        let leases = state.leases(&record.environment.id)?;

        if stored.observed_state == STATE_STOPPED || stored.observed_state == STATE_ABSENT {
            let resources = stopped_resources(record);
            return Ok(self.view(record, &stored.observed_state, HEALTH_UNKNOWN, &resources));
        }

        // A start in progress is `starting`, not `degraded`: the ports it will
        // bind are not up yet, and saying otherwise would be a lie about a
        // healthy start.
        if stored.observed_state == STATE_STARTING {
            let mut resources = self.probe(state, record, &leases).await?;
            for resource in &mut resources {
                resource.state = STATE_STARTING.to_string();
                resource.health = HEALTH_UNKNOWN.to_string();
            }
            return Ok(self.view(record, STATE_STARTING, HEALTH_UNKNOWN, &resources));
        }

        let resources = self.probe(state, record, &leases).await?;
        // A failed start stays failed until a start succeeds: a probe cannot
        // turn "the Backend refused to start it" into a milder verdict.
        let (health, observed) = if stored.observed_state == STATE_FAILED {
            (HEALTH_UNHEALTHY.to_string(), STATE_FAILED.to_string())
        } else {
            summarise(&resources, &stored.observed_state)
        };
        if observed != stored.observed_state || health != stored.health {
            // Only the observation moves: the desired state is the caller's.
            state.set_environment_state(
                &stored.id,
                &StateUpdate {
                    desired_state: stored.desired_state.clone(),
                    observed_state: observed.clone(),
                    health: health.clone(),
                },
            )?;
        }
        Ok(self.view(record, &observed, &health, &resources))
    }

    /// Run a bounded command in one Resource, returning stdout, stderr, and the
    /// exit code in order.
    pub async fn exec(
        &self,
        state: &State,
        record: &ProjectRecord,
        resource: &str,
        request: &ExecRequest,
    ) -> Result<ExecResponse, LifecycleError> {
        self.gate()?;
        if request.command.is_empty() {
            return Err(LifecycleError::Validation {
                path: "command".to_string(),
                detail: "`command` must carry at least one argument".to_string(),
            });
        }
        if !record.resources.iter().any(|known| known.name == resource) {
            return Err(LifecycleError::NotFound(format!(
                "Project {:?} has no Resource named {resource:?}; it has: {}",
                record.name,
                record
                    .resources
                    .iter()
                    .map(|known| known.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let stored = self.stored_environment(state, record)?;
        if stored.observed_state != STATE_RUNNING && stored.observed_state != STATE_DEGRADED {
            return Err(LifecycleError::Backend(BackendError::NotProvisioned {
                detail: format!(
                    "Environment {} is {}; start it before running a command",
                    stored.name, stored.observed_state
                ),
            }));
        }

        let bound = request
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.exec_timeout)
            .min(self.exec_timeout);
        let spec = ExecSpec {
            command: request.command.clone(),
            timeout: Some(bound),
            ..ExecSpec::default()
        };

        let started = Instant::now();
        let mut session = self.backend.exec(&stored.id, Some(resource), &spec).await?;
        let deadline = tokio::time::Instant::now() + bound;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = 0;
        loop {
            match tokio::time::timeout_at(deadline, session.next_chunk()).await {
                Ok(None) => break,
                Ok(Some(Ok(ExecChunk::Stdout(bytes)))) => stdout.extend_from_slice(&bytes),
                Ok(Some(Ok(ExecChunk::Stderr(bytes)))) => stderr.extend_from_slice(&bytes),
                Ok(Some(Ok(ExecChunk::Exit { code }))) => {
                    exit_code = code;
                    break;
                }
                Ok(Some(Err(err))) => return Err(err.into()),
                Err(_elapsed) => {
                    // Tell the Backend to stop, then report a typed timeout: a
                    // hung command must not wedge the caller.
                    session.cancel();
                    return Err(LifecycleError::Backend(BackendError::Timeout {
                        detail: format!(
                            "`{}` did not finish within {} ms in {resource}",
                            request.command.join(" "),
                            bound.as_millis()
                        ),
                    }));
                }
            }
        }

        Ok(ExecResponse {
            resource: resource.to_string(),
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Resource logs, bounded by `tail` and filtered by `since`.
    pub async fn logs(
        &self,
        state: &State,
        record: &ProjectRecord,
        resource: Option<&str>,
        options: &LogOptions,
    ) -> Result<LogStream, LifecycleError> {
        self.gate()?;
        if let Some(resource) = resource {
            if !record.resources.iter().any(|known| known.name == resource) {
                return Err(LifecycleError::NotFound(format!(
                    "Project {:?} has no Resource named {resource:?}",
                    record.name
                )));
            }
        }
        let stored = self.stored_environment(state, record)?;
        Ok(self
            .backend
            .logs(&stored.id, resource, options)
            .await
            .map_err(LifecycleError::Backend)?)
    }

    /// Environment-level figures, formatted for a status bar.
    pub async fn stats(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<StatsResponse, LifecycleError> {
        self.gate()?;
        let stored = self.stored_environment(state, record)?;
        let stats = self
            .backend
            .stats(&stored.id)
            .await
            .map_err(LifecycleError::Backend)?;
        Ok(StatsResponse {
            state: stored.observed_state,
            cpu_percent: stats.cpu_percent,
            memory_bytes: stats.memory_bytes,
            disk_bytes: stats.disk_bytes,
            cpu: format_cpu(stats.cpu_percent),
            memory: format_bytes(stats.memory_bytes),
            disk: format_bytes(stats.disk_bytes),
            resources: stats
                .resources
                .into_iter()
                .map(|resource| ResourceStatsView {
                    name: resource.name,
                    cpu_percent: format_cpu(f64::from(resource.cpu_percent_milli) / 1000.0),
                    memory: format_bytes(resource.memory_bytes),
                })
                .collect(),
        })
    }

    /// Reconcile once at startup: an Environment whose desired state is running
    /// but whose leased ports are dead did not survive the restart. It is marked
    /// degraded rather than quietly claimed to be running.
    pub async fn reconcile(&self, state: &State) -> AnyResult<Vec<(String, String)>> {
        let mut transitions = Vec::new();
        for environment in state.list_environments()? {
            if environment.desired_state != STATE_RUNNING || environment.observed_state == STATE_STOPPED {
                continue;
            }
            let leases = state.leases(&environment.id)?;
            let mut healthy = !leases.is_empty();
            for lease in &leases {
                if !health::tcp_probe(lease.host_port, self.probe_timeout).await {
                    healthy = false;
                }
            }
            let (observed, health_state) = if healthy {
                (STATE_RUNNING, HEALTH_HEALTHY)
            } else {
                (STATE_DEGRADED, HEALTH_UNHEALTHY)
            };
            state.set_environment_state(
                &environment.id,
                &StateUpdate {
                    desired_state: environment.desired_state.clone(),
                    observed_state: observed.to_string(),
                    health: health_state.to_string(),
                },
            )?;
            transitions.push((environment.id, observed.to_string()));
        }
        Ok(transitions)
    }

    fn stored_environment(
        &self,
        state: &State,
        record: &ProjectRecord,
    ) -> Result<EnvironmentRecord, LifecycleError> {
        Ok(state.environment(&record.environment.id)?.unwrap_or(
            EnvironmentRecord {
                id: record.environment.id.clone(),
                name: record.environment.name.clone(),
                desired_state: STATE_STOPPED.to_string(),
                observed_state: record.environment.observed_state.clone(),
                health: record.environment.health.clone(),
                updated_at: record.environment.updated_at.clone(),
            },
        ))
    }

    fn mark(
        &self,
        state: &State,
        environment_id: &str,
        desired: &str,
        observed: &str,
        health_state: &str,
    ) -> Result<(), LifecycleError> {
        state.set_environment_state(
            environment_id,
            &StateUpdate {
                desired_state: desired.to_string(),
                observed_state: observed.to_string(),
                health: health_state.to_string(),
            },
        )?;
        Ok(())
    }

    /// Probe every lease and build the per-Resource view.
    async fn probe(
        &self,
        state: &State,
        record: &ProjectRecord,
        leases: &[LeaseRecord],
    ) -> Result<Vec<ResourceStatusView>, LifecycleError> {
        // A fresh read, so a caller that just leased sees its own rows.
        let leases = if leases.is_empty() {
            state.leases(&record.environment.id)?
        } else {
            leases.to_vec()
        };

        let mut resources = Vec::new();
        for resource in record.resources.iter() {
            let own: Vec<&LeaseRecord> = leases
                .iter()
                .filter(|lease| lease.resource == resource.name)
                .collect();
            let mut ports = Vec::new();
            let mut all_healthy = true;
            let mut any_probe = false;
            for lease in own {
                ports.push(LeasedPortView {
                    guest_port: lease.guest_port,
                    host_port: lease.host_port,
                });
                any_probe = true;
                if !health::tcp_probe(lease.host_port, self.probe_timeout).await {
                    all_healthy = false;
                }
            }
            let (health_state, resource_state) = if !any_probe {
                (HEALTH_UNKNOWN.to_string(), STATE_RUNNING.to_string())
            } else if all_healthy {
                (HEALTH_HEALTHY.to_string(), STATE_RUNNING.to_string())
            } else {
                (HEALTH_UNHEALTHY.to_string(), STATE_DEGRADED.to_string())
            };
            resources.push(ResourceStatusView {
                name: resource.name.clone(),
                kind: resource.kind.clone(),
                state: resource_state,
                health: health_state,
                ports,
            });
        }
        Ok(resources)
    }

    fn view(
        &self,
        record: &ProjectRecord,
        observed: &str,
        health_state: &str,
        resources: &[ResourceStatusView],
    ) -> EnvironmentStatus {
        EnvironmentStatus {
            project: record.name.clone(),
            project_id: record.id.clone(),
            environment: EnvironmentView {
                id: record.environment.id.clone(),
                name: record.environment.name.clone(),
                state: observed.to_string(),
                cost: Cost::default(),
            },
            health: health_state.to_string(),
            resources: resources.to_vec(),
            backend: self.backend_ref(),
            checked_at: clock::now_rfc3339(),
            operation_id: None,
        }
    }
}

/// Collapse a Resource snapshot into `(health, state)`.
///
/// The state follows the probes, not the other way round: an Environment whose
/// ports answer is `running` again even if it was `degraded` a moment ago.
fn summarise(resources: &[ResourceStatusView], fallback: &str) -> (String, String) {
    if resources
        .iter()
        .any(|resource| resource.health == HEALTH_UNHEALTHY)
    {
        return (HEALTH_UNHEALTHY.to_string(), STATE_DEGRADED.to_string());
    }
    if resources
        .iter()
        .any(|resource| resource.health == HEALTH_HEALTHY)
    {
        return (HEALTH_HEALTHY.to_string(), STATE_RUNNING.to_string());
    }
    (HEALTH_UNKNOWN.to_string(), fallback.to_string())
}

fn stopped_resources(record: &ProjectRecord) -> Vec<ResourceStatusView> {
    record
        .resources
        .iter()
        .map(|resource| ResourceStatusView {
            name: resource.name.clone(),
            kind: resource.kind.clone(),
            state: STATE_STOPPED.to_string(),
            health: HEALTH_UNKNOWN.to_string(),
            ports: Vec::new(),
        })
        .collect()
}

/// `96 MB`, `1.5 GB`: one decimal, because a status bar does not need more.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// `2.5%`, `0%`.
pub fn format_cpu(percent: f64) -> String {
    if percent == 0.0 {
        "0%".to_string()
    } else {
        format!("{percent:.1}%")
    }
}

/// A log line as the wire type.
pub fn log_line_view(line: &agentify_backends::LogLine) -> LogLineView {
    LogLineView {
        seq: line.seq,
        at: line.at.clone(),
        resource: line.resource.clone(),
        stream: line.stream.clone(),
        line: line.line.clone(),
    }
}

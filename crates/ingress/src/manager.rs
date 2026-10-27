//! The ingress the daemon owns.
//!
//! One Caddy instance serves every Project. The daemon starts it with a full
//! config, then keeps it in step with one granular admin call per route change:
//! a route is appended when a hostname appears, replaced when its upstream moves
//! between a lease and the stopped page, and deleted only when the Environment
//! goes away.
//!
//! Two invariants hold on every path:
//!
//! * routes are derived from leases by the daemon, never edited by hand — this
//!   module is given the desired routes and makes Caddy match them;
//! * when trust is absent or refused, the plain-HTTP server serves the same
//!   routes instead of redirecting into a certificate the browser will reject.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::admin::{AdminClient, AdminError, HttpAdminClient};
use crate::caddy::{self, CaddyBinary, CaddyError, CaddyInstance};
use crate::ports::{PortIssue, PortProbe};
use crate::routes::{HttpMode, Route, SERVER_HTTP, SERVER_TLS};
use crate::trust::{TrustInstaller, TrustReport, TrustState};

/// Environment variable overriding the plain-HTTP port.
pub const HTTP_PORT_ENV: &str = "AGENTIFY_HTTP_PORT";

/// Environment variable overriding the TLS port.
pub const HTTPS_PORT_ENV: &str = "AGENTIFY_HTTPS_PORT";

/// Environment variable holding the admin endpoint of an already-running Caddy.
pub const ADMIN_ENV: &str = "AGENTIFY_CADDY_ADMIN";

/// The ports the spec names, and the only ones a user's bookmarks assume.
pub const DEFAULT_HTTP_PORT: u16 = 80;
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/// Where the trusted-root marker lives, under the ingress data directory.
const TRUST_MARKER: &str = "trust.json";

/// Whether the daemon runs Caddy or talks to one that already runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminMode {
    /// Start `caddy.exe` with a generated config on a random loopback port.
    Spawn,
    /// Use an admin endpoint that is already up. Tests use this with a fake
    /// admin server; it also lets a user point Agentify at their own Caddy.
    Attach(SocketAddr),
}

/// How ingress is configured.
#[derive(Debug, Clone)]
pub struct IngressOptions {
    pub http_port: u16,
    pub https_port: u16,
    /// Directory holding the generated config, Caddy's storage, its log, and the
    /// trust marker.
    pub data_dir: PathBuf,
    /// `AGENTIFY_CADDY`, when set.
    pub caddy: Option<PathBuf>,
    pub admin: AdminMode,
    /// The known trust state: what decides whether HTTP redirects or serves.
    pub trust: TrustState,
}

impl IngressOptions {
    /// Defaults for a data root: ports 80/443, a Caddy the daemon runs itself,
    /// and no trust assumed.
    pub fn new(data_root: &Path) -> Self {
        Self {
            http_port: DEFAULT_HTTP_PORT,
            https_port: DEFAULT_HTTPS_PORT,
            data_dir: data_root.join("ingress"),
            caddy: None,
            admin: AdminMode::Spawn,
            trust: stored_trust(&data_root.join("ingress")),
        }
    }

    pub fn with_ports(mut self, http: u16, https: u16) -> Self {
        self.http_port = http;
        self.https_port = https;
        self
    }

    pub fn with_admin(mut self, admin: AdminMode) -> Self {
        self.admin = admin;
        self
    }

    pub fn with_trust(mut self, trust: TrustState) -> Self {
        self.trust = trust;
        self
    }

    pub fn with_caddy(mut self, caddy: Option<PathBuf>) -> Self {
        self.caddy = caddy;
        self
    }

    /// Read the ports and the Caddy path from the environment.
    pub fn apply_env(&mut self) -> Result<(), IngressError> {
        if let Ok(raw) = std::env::var(HTTP_PORT_ENV) {
            self.http_port = parse_port(HTTP_PORT_ENV, &raw)?;
        }
        if let Ok(raw) = std::env::var(HTTPS_PORT_ENV) {
            self.https_port = parse_port(HTTPS_PORT_ENV, &raw)?;
        }
        if let Ok(raw) = std::env::var(caddy::CADDY_ENV) {
            if !raw.trim().is_empty() {
                self.caddy = Some(PathBuf::from(raw.trim()));
            }
        }
        if let Ok(raw) = std::env::var(ADMIN_ENV) {
            let raw = raw.trim();
            if !raw.is_empty() {
                let addr: SocketAddr = raw.parse().map_err(|_| {
                    IngressError::Config(format!(
                        "{ADMIN_ENV}={raw:?} is not an address like 127.0.0.1:2019"
                    ))
                })?;
                self.admin = AdminMode::Attach(addr);
            }
        }
        Ok(())
    }

    /// The path Caddy keeps its storage in: the local CA lives under it.
    pub fn storage_root(&self) -> PathBuf {
        self.data_dir.join("caddy")
    }

    /// The path of the local root certificate.
    pub fn root_cert(&self) -> PathBuf {
        self.storage_root()
            .join("pki")
            .join("authorities")
            .join("local")
            .join("root.crt")
    }

    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("caddy.json")
    }

    pub fn log_path(&self) -> PathBuf {
        self.data_dir.join("caddy.log")
    }

    /// Where the admin endpoint of the running Caddy is recorded, so a daemon
    /// that restarted after a crash can adopt it instead of fighting it for the
    /// ports.
    pub fn admin_record_path(&self) -> PathBuf {
        self.data_dir.join("admin.json")
    }
}

/// The admin endpoint recorded by a previous Caddy start, if it is still there.
///
/// A stale record is harmless: the caller tries to load the empty skeleton and
/// falls through to a fresh start if the endpoint does not answer.
fn recorded_admin(data_dir: &Path) -> Option<SocketAddr> {
    let raw = std::fs::read_to_string(data_dir.join("admin.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("admin")?.as_str()?.parse().ok()
}

fn parse_port(name: &str, raw: &str) -> Result<u16, IngressError> {
    raw.trim().parse().map_err(|_| {
        IngressError::Config(format!("{name}={raw:?} is not a port number"))
    })
}

/// Why ingress could not do its job.
#[derive(Debug)]
pub enum IngressError {
    /// A port Caddy needs is held by something else. Never worked around.
    Port(PortIssue),
    /// No Caddy, or Caddy would not start.
    Caddy(CaddyError),
    /// The admin API refused or broke.
    Admin(AdminError),
    /// Trust could not be installed or removed.
    Trust(crate::trust::TrustError),
    /// Configuration is wrong.
    Config(String),
    /// Ingress is deliberately not running.
    Unavailable(String),
}

impl IngressError {
    /// The stable `application/problem+json` code for the API.
    pub fn code(&self) -> &'static str {
        match self {
            IngressError::Port(_) => "PortUnavailable",
            _ => "BackendUnavailable",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            IngressError::Port(_) => 409,
            IngressError::Config(_) => 500,
            _ => 503,
        }
    }

    pub fn detail(&self) -> String {
        match self {
            IngressError::Port(issue) => issue.detail(),
            IngressError::Caddy(err) => {
                format!("local ingress needs Caddy: {err}")
            }
            IngressError::Admin(err) => format!("local ingress lost its Caddy config: {err}"),
            IngressError::Trust(err) => format!("{err}"),
            IngressError::Config(detail) => detail.clone(),
            IngressError::Unavailable(detail) => detail.clone(),
        }
    }
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for IngressError {}

impl From<PortIssue> for IngressError {
    fn from(issue: PortIssue) -> Self {
        IngressError::Port(issue)
    }
}

impl From<CaddyError> for IngressError {
    fn from(err: CaddyError) -> Self {
        IngressError::Caddy(err)
    }
}

impl From<AdminError> for IngressError {
    fn from(err: AdminError) -> Self {
        IngressError::Admin(err)
    }
}

impl From<crate::trust::TrustError> for IngressError {
    fn from(err: crate::trust::TrustError) -> Self {
        IngressError::Trust(err)
    }
}

/// One route as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSnapshot {
    pub hostname: String,
    pub url: String,
    /// The lease it proxies to, when it proxies at all.
    pub upstream: Option<u16>,
    /// `serving` or `stopped`.
    pub state: String,
    /// The Environment the route belongs to.
    pub environment: String,
}

/// What ingress is doing, for `GET /v1/ingress` and the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressStatus {
    /// `running`, `stopped`, or `unavailable`.
    pub state: String,
    pub http_port: u16,
    pub https_port: u16,
    pub trust: TrustState,
    pub caddy: Option<String>,
    pub routes: Vec<RouteSnapshot>,
    pub detail: Option<String>,
}

impl IngressStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state,
            "httpPort": self.http_port,
            "httpsPort": self.https_port,
            "trust": self.trust.as_str(),
            "https": self.trust.trusted(),
            "caddy": self.caddy,
            "detail": self.detail,
            "routes": self.routes.iter().map(|route| json!({
                "hostname": route.hostname,
                "url": route.url,
                "upstream": route.upstream,
                "state": route.state,
                "environment": route.environment,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    environment: String,
    route: Route,
}

struct Running {
    admin: Arc<dyn AdminClient>,
    instance: Option<CaddyInstance>,
    /// The routes Caddy currently serves, in the order it serves them.
    routes: Vec<Entry>,
    /// Set when a previous update failed: the next one reloads the whole config
    /// instead of trusting the route table.
    needs_load: bool,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    last_error: Option<String>,
    /// The trust state this process knows about. It only changes through
    /// [`Ingress::install_trust`] and [`Ingress::remove_trust`].
    trust: TrustState,
}

/// The daemon's handle on Caddy.
pub struct Ingress {
    options: IngressOptions,
    inner: Mutex<Inner>,
}

impl Ingress {
    pub fn new(options: IngressOptions) -> Self {
        let trust = options.trust;
        Self {
            options,
            inner: Mutex::new(Inner {
                trust,
                ..Inner::default()
            }),
        }
    }

    pub fn options(&self) -> &IngressOptions {
        &self.options
    }

    pub fn trust(&self) -> TrustState {
        self.inner.lock().expect("ingress state").trust
    }

    /// Whether Caddy is up and configurable right now.
    pub fn is_running(&self) -> bool {
        let mut inner = self.inner.lock().expect("ingress state");
        match inner.running.as_mut() {
            Some(running) => match running.instance.as_mut() {
                Some(instance) => instance.running(),
                None => true,
            },
            None => false,
        }
    }

    /// Start Caddy if it is not up yet: probe the ports, find the binary, write
    /// the config, run it, and load the empty skeleton.
    pub fn ensure_started(&self) -> Result<(), IngressError> {
        let mut inner = self.inner.lock().expect("ingress state");
        if let Some(running) = inner.running.as_mut() {
            let alive = match running.instance.as_mut() {
                Some(instance) => instance.running(),
                None => true,
            };
            if alive {
                return Ok(());
            }
            // The process died: drop it and start over.
            inner.running = None;
        }

        let started = self.start_locked(inner.trust.http_mode());
        match started {
            Ok(running) => {
                inner.running = Some(running);
                inner.last_error = None;
                Ok(())
            }
            Err(err) => {
                inner.last_error = Some(err.detail());
                Err(err)
            }
        }
    }

    fn start_locked(&self, mode: HttpMode) -> Result<Running, IngressError> {
        match &self.options.admin {
            AdminMode::Attach(addr) => {
                let admin = HttpAdminClient::new(*addr);
                let mut running = Running {
                    admin: Arc::new(admin),
                    instance: None,
                    routes: Vec::new(),
                    needs_load: false,
                };
                self.load_config(&mut running, &[], mode)?;
                Ok(running)
            }
            AdminMode::Spawn => {
                // A previous daemon may have left a Caddy of ours running (a
                // crash, a kill). Adopting it is what keeps the ports from
                // becoming a conflict the user has to resolve by hand.
                if let Some(admin) = recorded_admin(&self.options.data_dir) {
                    let mut running = Running {
                        admin: Arc::new(HttpAdminClient::new(admin)),
                        instance: None,
                        routes: Vec::new(),
                        needs_load: false,
                    };
                    if self.load_config(&mut running, &[], mode).is_ok() {
                        return Ok(running);
                    }
                }

                // Ports next: a conflict is the user's to fix, and it is not
                // worth starting a process to discover it.
                PortProbe::check(&[self.options.http_port, self.options.https_port])?;

                let binary = CaddyBinary::locate(
                    self.options.data_dir.parent().unwrap_or(&self.options.data_dir),
                    self.options.caddy.clone(),
                )?;
                std::fs::create_dir_all(&self.options.data_dir).map_err(|err| {
                    IngressError::Config(format!(
                        "creating the ingress directory {}: {err}",
                        self.options.data_dir.display()
                    ))
                })?;

                let admin_addr = free_loopback_port()?;
                let config = caddy::base_config(
                    self.options.http_port,
                    self.options.https_port,
                    admin_addr,
                    &self.options.storage_root(),
                );
                let config_path = self.options.config_path();
                write_json(&config_path, &config)?;

                let instance = CaddyInstance::spawn(
                    &binary.path,
                    &config_path,
                    &self.options.log_path(),
                    admin_addr,
                )?;
                // Record the endpoint *after* a successful start, so a failed
                // start cannot leave a stale record pointing at nothing.
                let _ = write_json(
                    &self.options.admin_record_path(),
                    &json!({ "admin": admin_addr.to_string() }),
                );

                Ok(Running {
                    admin: Arc::new(HttpAdminClient::new(admin_addr)),
                    instance: Some(instance),
                    routes: Vec::new(),
                    needs_load: false,
                })
            }
        }
    }

    /// Replace the whole config with one built from `entries`.
    fn load_config(
        &self,
        running: &mut Running,
        entries: &[Entry],
        mode: HttpMode,
    ) -> Result<(), IngressError> {
        let admin_addr = running.admin.addr();
        let mut config = caddy::base_config(
            self.options.http_port,
            self.options.https_port,
            admin_addr,
            &self.options.storage_root(),
        );
        let tls: Vec<Value> = entries.iter().map(|e| e.route.tls_json()).collect();
        let http: Vec<Value> = entries.iter().map(|e| e.route.http_json(mode)).collect();
        config["apps"]["http"]["servers"][SERVER_TLS]["routes"] = Value::Array(tls);
        config["apps"]["http"]["servers"][SERVER_HTTP]["routes"] = Value::Array(http);
        running.admin.load(&config)?;
        running.routes = entries.to_vec();
        running.needs_load = false;
        Ok(())
    }

    /// Load every route in one go: startup, recovery, and the repair after a
    /// failed granular update.
    pub fn load_all(&self, routes: &[(String, Route)]) -> Result<(), IngressError> {
        self.ensure_started()?;
        let entries: Vec<Entry> = routes
            .iter()
            .map(|(environment, route)| Entry {
                environment: environment.clone(),
                route: route.clone(),
            })
            .collect();
        let mut inner = self.inner.lock().expect("ingress state");
        let mode = inner.trust.http_mode();
        let running = inner
            .running
            .as_mut()
            .ok_or_else(|| IngressError::Unavailable("ingress is not running".to_string()))?;
        if !running.needs_load && running.routes == entries {
            // Startup already loaded exactly this; a second load would be noise.
            return Ok(());
        }
        let result = self.load_config(running, &entries, mode);
        if let Err(err) = &result {
            inner.last_error = Some(err.detail());
        }
        result
    }

    /// Make Caddy serve exactly `desired` for one Environment.
    pub fn sync_environment(
        &self,
        environment: &str,
        desired: &[Route],
    ) -> Result<(), IngressError> {
        self.apply(environment, desired)
    }

    /// Delete an Environment's routes. Nothing else removes a route.
    pub fn remove_environment(&self, environment: &str) -> Result<(), IngressError> {
        self.apply(environment, &[])
    }

    fn apply(&self, environment: &str, desired: &[Route]) -> Result<(), IngressError> {
        self.ensure_started()?;
        let mut inner = self.inner.lock().expect("ingress state");
        let mode = inner.trust.http_mode();
        let Some(running) = inner.running.as_mut() else {
            return Err(IngressError::Unavailable("ingress is not running".to_string()));
        };
        let result = self.apply_locked(running, environment, desired, mode);
        if let Err(err) = &result {
            if let Some(running) = inner.running.as_mut() {
                // Caddy and the table may now disagree; the next update reloads.
                running.needs_load = true;
            }
            inner.last_error = Some(err.detail());
        }
        result
    }

    /// Reach the desired routes, granularly if possible.
    ///
    /// Two kinds of change are one atomic config load rather than a sequence of
    /// granular writes:
    ///
    /// * the repair after a failed write, because the route table can no longer
    ///   be trusted;
    /// * **adding** a hostname, because Caddy starts certificate issuance on
    ///   every reload — two reloads a millisecond apart make two issuance jobs
    ///   race for the same name, and one of them loses. One load, one issuance,
    ///   and the browser's first handshake has a certificate to show.
    ///
    /// A route that only moves (running → stopped, lease → lease) or disappears
    /// is a single granular call, which is the frequent case.
    fn apply_locked(
        &self,
        running: &mut Running,
        environment: &str,
        desired: &[Route],
        mode: HttpMode,
    ) -> Result<(), IngressError> {
        let (next, ops) = plan(&running.routes, environment, desired);

        let adds_hostname = ops.iter().any(|op| matches!(op, Op::Append(_)));
        if running.needs_load || adds_hostname {
            return self.load_config(running, &next, mode);
        }

        match self.execute(running, &ops, mode) {
            Ok(()) => {
                running.routes = next;
                Ok(())
            }
            Err(err) => {
                self.load_config(running, &next, mode).map_err(|reload| {
                    IngressError::Admin(crate::admin::AdminError::Malformed(format!(
                        "a granular route write failed ({}), and reloading the config also \
                         failed: {}",
                        err.detail(),
                        reload.detail()
                    )))
                })
            }
        }
    }

    /// Run the planned granular writes. Puts and appends keep every existing
    /// index; deletes descend so each one sees the array it was planned against.
    ///
    /// A write Caddy refuses is repaired by the caller with a full load, so a
    /// Caddy that rejects one route can never leave the table ahead of reality.
    fn execute(
        &self,
        running: &mut Running,
        ops: &[Op],
        mode: HttpMode,
    ) -> Result<(), IngressError> {
        for op in ops {
            match op {
                Op::Put(index, route) => {
                    running.admin.put_route(SERVER_TLS, *index, &route.tls_json())?;
                    running.admin.put_route(SERVER_HTTP, *index, &route.http_json(mode))?;
                }
                Op::Append(route) => {
                    running.admin.append_route(SERVER_TLS, &route.tls_json())?;
                    running.admin.append_route(SERVER_HTTP, &route.http_json(mode))?;
                }
                Op::Delete(index) => {
                    running.admin.delete_route(SERVER_TLS, *index)?;
                    running.admin.delete_route(SERVER_HTTP, *index)?;
                }
            }
        }
        Ok(())
    }

    /// Every route Caddy is serving right now.
    pub fn routes(&self) -> Vec<RouteSnapshot> {
        let inner = self.inner.lock().expect("ingress state");
        let Some(running) = inner.running.as_ref() else {
            return Vec::new();
        };
        running
            .routes
            .iter()
            .map(|entry| RouteSnapshot {
                hostname: entry.route.hostname.clone(),
                url: format!("https://{}", entry.route.hostname),
                upstream: match entry.route.target {
                    crate::routes::RouteTarget::Upstream { port } => Some(port),
                    crate::routes::RouteTarget::Unavailable { .. } => None,
                },
                state: if entry.route.is_upstream() {
                    "serving".to_string()
                } else {
                    "stopped".to_string()
                },
                environment: entry.environment.clone(),
            })
            .collect()
    }

    /// A snapshot for the API, including why ingress is down when it is.
    pub fn status(&self) -> IngressStatus {
        let inner = self.inner.lock().expect("ingress state");
        let running = inner.running.is_some();
        let mut routes = Vec::new();
        if let Some(state) = inner.running.as_ref() {
            routes = state
                .routes
                .iter()
                .map(|entry| RouteSnapshot {
                    hostname: entry.route.hostname.clone(),
                    url: format!("https://{}", entry.route.hostname),
                    upstream: match entry.route.target {
                        crate::routes::RouteTarget::Upstream { port } => Some(port),
                        crate::routes::RouteTarget::Unavailable { .. } => None,
                    },
                    state: if entry.route.is_upstream() {
                        "serving".to_string()
                    } else {
                        "stopped".to_string()
                    },
                    environment: entry.environment.clone(),
                })
                .collect();
        }
        IngressStatus {
            state: if running {
                "running".to_string()
            } else if inner.last_error.is_some() {
                "unavailable".to_string()
            } else {
                "stopped".to_string()
            },
            http_port: self.options.http_port,
            https_port: self.options.https_port,
            trust: inner.trust,
            caddy: self
                .options
                .caddy
                .as_ref()
                .map(|path| path.display().to_string()),
            routes,
            detail: inner.last_error.clone(),
        }
    }

    /// Record a new trust state and re-point the plain-HTTP routes at it.
    pub fn set_trust(&self, trust: TrustState) -> Result<(), IngressError> {
        let mut inner = self.inner.lock().expect("ingress state");
        inner.trust = trust;
        let mode = trust.http_mode();
        let Some(running) = inner.running.as_mut() else {
            // Nothing is running: the next start uses the new state.
            return Ok(());
        };
        for (index, entry) in running.routes.iter().enumerate() {
            running
                .admin
                .put_route(SERVER_HTTP, index, &entry.route.http_json(mode))?;
        }
        Ok(())
    }

    /// Install the local root for the current user. Only ever called from an
    /// explicit opt-in path.
    pub fn install_trust(&self, installer: &dyn TrustInstaller) -> Result<TrustReport, IngressError> {
        let root = self.options.root_cert();
        let report = installer.install(&root)?;
        store_trust(&self.options.data_dir, report.state)?;
        self.set_trust(report.state)?;
        Ok(report)
    }

    /// Remove the local root, for uninstall.
    pub fn remove_trust(&self, installer: &dyn TrustInstaller) -> Result<TrustReport, IngressError> {
        let report = installer.remove()?;
        store_trust(&self.options.data_dir, TrustState::Absent)?;
        self.set_trust(TrustState::Absent)?;
        Ok(report)
    }

    /// Stop Caddy. The routes it served go with it.
    pub fn stop(&self) {
        let mut inner = self.inner.lock().expect("ingress state");
        if let Some(mut running) = inner.running.take() {
            if let Some(mut instance) = running.instance.take() {
                instance.stop();
                // Only our own process leaves a record behind; an adopted one
                // keeps its record so the next daemon can adopt it too.
                let _ = std::fs::remove_file(self.options.admin_record_path());
            }
        }
    }
}

impl Drop for Ingress {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The trust state a previous run recorded in the data directory.
pub fn stored_trust(data_dir: &Path) -> TrustState {
    let path = data_dir.join(TRUST_MARKER);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return TrustState::Absent;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return TrustState::Absent;
    };
    value
        .get("state")
        .and_then(Value::as_str)
        .and_then(TrustState::parse)
        .unwrap_or(TrustState::Absent)
}

/// Remember the trust state so a restart does not re-ask.
pub fn store_trust(data_dir: &Path, state: TrustState) -> Result<(), IngressError> {
    std::fs::create_dir_all(data_dir).map_err(|err| {
        IngressError::Config(format!("creating {}: {err}", data_dir.display()))
    })?;
    write_json(
        &data_dir.join(TRUST_MARKER),
        &json!({ "state": state.as_str() }),
    )
}

fn write_json(path: &Path, value: &Value) -> Result<(), IngressError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| IngressError::Config(format!("serialising {}: {err}", path.display())))?;
    std::fs::write(path, text)
        .map_err(|err| IngressError::Config(format!("writing {}: {err}", path.display())))
}

/// Grab a free loopback port for the admin API.
fn free_loopback_port() -> Result<SocketAddr, IngressError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|err| {
        IngressError::Config(format!("finding a free loopback port for the Caddy admin API: {err}"))
    })?;
    let addr = listener
        .local_addr()
        .map_err(|err| IngressError::Config(format!("reading the admin port: {err}")))?;
    drop(listener);
    Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port())))
}

/// One granular admin write.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// Replace the route at this index.
    Put(usize, Route),
    /// Add a route at the end.
    Append(Route),
    /// Remove the route at this index.
    Delete(usize),
}

/// Work out the new route table for one Environment and the smallest set of
/// calls that gets Caddy there.
///
/// Nothing else is touched: another Environment's routes keep their positions,
/// which is what makes a granular update safe.
fn plan(current: &[Entry], environment: &str, desired: &[Route]) -> (Vec<Entry>, Vec<Op>) {
    let mut next: Vec<Entry> = Vec::new();
    let mut puts: Vec<Op> = Vec::new();
    let mut deletes: Vec<usize> = Vec::new();

    for (index, entry) in current.iter().enumerate() {
        if entry.environment != environment {
            next.push(entry.clone());
            continue;
        }
        match desired.iter().find(|route| route.hostname == entry.route.hostname) {
            Some(route) if route.target != entry.route.target => {
                puts.push(Op::Put(index, route.clone()));
                next.push(Entry {
                    environment: entry.environment.clone(),
                    route: route.clone(),
                });
            }
            Some(_) => next.push(entry.clone()),
            None => deletes.push(index),
        }
    }

    let mut appends: Vec<Op> = Vec::new();
    for route in desired {
        let known = current
            .iter()
            .any(|entry| entry.route.hostname == route.hostname);
        if !known {
            appends.push(Op::Append(route.clone()));
            next.push(Entry {
                environment: environment.to_string(),
                route: route.clone(),
            });
        }
    }

    let mut ops = puts;
    ops.extend(appends);
    // Highest index first: each delete then sees the array it was planned on.
    deletes.sort_unstable();
    ops.extend(deletes.into_iter().rev().map(Op::Delete));
    (next, ops)
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn entry(environment: &str, hostname: &str, port: u16) -> Entry {
        Entry {
            environment: environment.to_string(),
            route: Route::upstream(hostname, port),
        }
    }

    #[test]
    fn a_new_hostname_is_one_append_and_leaves_the_rest_alone() {
        let current = vec![entry("env-1", "web.demo.localhost", 40001)];
        let desired = vec![
            Route::upstream("web.demo.localhost", 40001),
            Route::upstream("api.demo.localhost", 40002),
        ];
        let (next, ops) = plan(&current, "env-1", &desired);
        assert_eq!(next.len(), 2);
        assert_eq!(
            ops,
            vec![Op::Append(Route::upstream("api.demo.localhost", 40002))],
            "one call, and the existing route is untouched"
        );
    }

    #[test]
    fn a_moved_lease_is_one_put_at_its_own_index() {
        let current = vec![
            entry("env-1", "web.demo.localhost", 40001),
            entry("env-2", "api.other.localhost", 40009),
        ];
        let desired = vec![Route::upstream("web.demo.localhost", 40005)];
        let (next, ops) = plan(&current, "env-1", &desired);
        assert_eq!(
            ops,
            vec![Op::Put(0, Route::upstream("web.demo.localhost", 40005))]
        );
        assert_eq!(next[1], current[1], "the other Environment keeps its route");
    }

    #[test]
    fn removal_deletes_high_index_first_so_the_others_do_not_shift_under_it() {
        let current = vec![
            entry("env-1", "web.demo.localhost", 40001),
            entry("env-2", "api.other.localhost", 40009),
            entry("env-1", "admin.demo.localhost", 40002),
        ];
        let desired = vec![Route::upstream("web.demo.localhost", 40001)];
        let (next, ops) = plan(&current, "env-1", &desired);
        assert_eq!(ops, vec![Op::Delete(2)]);
        assert_eq!(next.len(), 2);

        let (next, ops) = plan(&current, "env-1", &[]);
        assert_eq!(ops, vec![Op::Delete(2), Op::Delete(0)], "{ops:?}");
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].environment, "env-2");
    }

    #[test]
    fn an_unchanged_environment_produces_no_calls_at_all() {
        let current = vec![entry("env-1", "web.demo.localhost", 40001)];
        let desired = vec![Route::upstream("web.demo.localhost", 40001)];
        let (_, ops) = plan(&current, "env-1", &desired);
        assert!(ops.is_empty(), "a start that changes nothing writes nothing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{summarise_routes, FakeAdmin, FakeTrustInstaller};
    use serde_json::json;

    const WEB: &str = "web.hello-pg.localhost";
    const API: &str = "api.hello-pg.localhost";

    /// Ingress attached to a fake admin endpoint: the real route code and the
    /// real admin client, without a Caddy process.
    struct Harness {
        ingress: Ingress,
        admin: FakeAdmin,
        _temp: tempfile::TempDir,
    }

    fn harness_with(trust: TrustState) -> Harness {
        let temp = tempfile::tempdir().expect("temp dir");
        let admin = FakeAdmin::start();
        let options = IngressOptions::new(temp.path())
            .with_admin(AdminMode::Attach(admin.addr()))
            .with_ports(18080, 18443)
            .with_trust(trust);
        let ingress = Ingress::new(options);
        Harness {
            ingress,
            admin,
            _temp: temp,
        }
    }

    fn harness() -> Harness {
        harness_with(TrustState::Absent)
    }

    /// A port nothing is listening on, for a test that must not collide with the
    /// machine's own services.
    fn free_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        listener.local_addr().expect("addr").port()
    }

    #[test]
    fn startup_loads_one_config_with_two_servers_and_no_routes() {
        let h = harness();
        h.ingress.load_all(&[]).expect("load");

        assert_eq!(h.admin.loads(), 1, "a full load, once");
        let config = h.admin.config();
        assert_eq!(config["admin"]["listen"], json!(h.admin.addr().to_string()));
        assert_eq!(
            config["apps"]["http"]["servers"][SERVER_TLS]["listen"][0],
            "127.0.0.1:18443"
        );
        assert_eq!(
            config["apps"]["http"]["servers"][SERVER_HTTP]["listen"][0],
            "127.0.0.1:18080"
        );
        assert!(h.admin.routes(SERVER_TLS).is_empty());
        assert!(h.admin.routes(SERVER_HTTP).is_empty());
        assert!(h.ingress.is_running());
    }

    #[test]
    fn a_running_environment_gets_a_route_that_points_at_its_lease() {
        let h = harness();
        h.ingress.load_all(&[]).expect("load");
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");

        assert_eq!(
            h.admin.loads(),
            2,
            "a new hostname is one atomic config load, so Caddy issues one certificate"
        );
        let routes = h.admin.routes(SERVER_TLS);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["@id"], "agentify:https:web.hello-pg.localhost");
        assert_eq!(routes[0]["handle"][0]["upstreams"][0]["dial"], "127.0.0.1:40017");

        let snapshots = h.ingress.routes();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].hostname, WEB);
        assert_eq!(snapshots[0].upstream, Some(40017));
        assert_eq!(snapshots[0].state, "serving");
        assert_eq!(snapshots[0].url, "https://web.hello-pg.localhost");
    }

    #[test]
    fn a_stopped_environment_keeps_its_route_and_answers_503() {
        let h = harness();
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync first");
        let loads = h.admin.loads();
        h.ingress
            .sync_environment(
                "env-1",
                &[Route::unavailable(
                    WEB,
                    "Project hello-pg is stopped. Start it to serve this address.",
                )],
            )
            .expect("sync stopped");

        let routes = h.admin.routes(SERVER_TLS);
        assert_eq!(routes.len(), 1, "the route survives a stop");
        assert_eq!(routes[0]["@id"], "agentify:https:web.hello-pg.localhost");
        assert_eq!(routes[0]["handle"][0]["handler"], "static_response");
        assert_eq!(routes[0]["handle"][0]["status_code"], 503);
        assert!(routes[0]["handle"][0]["body"]
            .as_str()
            .expect("body")
            .contains("stopped"));

        assert_eq!(
            h.admin.loads(),
            loads,
            "a stop is a granular update, not a reload: {:#?}",
            h.admin.requests()
        );
        assert_eq!(h.ingress.routes()[0].state, "stopped");
        assert_eq!(h.ingress.routes()[0].upstream, None);
    }

    #[test]
    fn removing_the_environment_is_the_only_thing_that_deletes_a_route() {
        let h = harness();
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");
        let loads = h.admin.loads();
        h.ingress.remove_environment("env-1").expect("remove");

        assert!(h.admin.routes(SERVER_TLS).is_empty());
        assert!(h.admin.routes(SERVER_HTTP).is_empty());
        assert!(h.ingress.routes().is_empty());
        assert_eq!(
            h.admin.loads(),
            loads,
            "deleting one route is granular too: {:#?}",
            h.admin.requests()
        );
    }

    #[test]
    fn one_environment_does_not_disturb_another_when_routes_come_and_go() {
        let h = harness();
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("env-1");
        h.ingress
            .sync_environment("env-2", &[Route::upstream(API, 40018)])
            .expect("env-2");
        assert_eq!(h.admin.routes(SERVER_TLS).len(), 2);

        h.ingress.remove_environment("env-1").expect("remove env-1");
        let routes = summarise_routes(&h.admin.routes(SERVER_TLS));
        assert_eq!(routes.len(), 1, "{routes:?}");
        assert_eq!(
            routes.get("agentify:https:api.hello-pg.localhost"),
            Some(&"proxy 127.0.0.1:40018".to_string()),
            "the surviving route still points at its own lease: {routes:?}"
        );

        // And the same hostname coming back for a new Environment lands again.
        h.ingress
            .sync_environment("env-3", &[Route::upstream(WEB, 40019)])
            .expect("env-3");
        let routes = summarise_routes(&h.admin.routes(SERVER_TLS));
        assert_eq!(
            routes.get("agentify:https:web.hello-pg.localhost"),
            Some(&"proxy 127.0.0.1:40019".to_string()),
            "{routes:?}"
        );
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn trust_refused_serves_the_app_over_http_instead_of_redirecting() {
        let h = harness_with(TrustState::Refused);
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");

        let https = h.admin.routes(SERVER_TLS);
        let http = h.admin.routes(SERVER_HTTP);
        assert_eq!(https[0]["handle"][0]["handler"], "reverse_proxy");
        assert_eq!(
            http[0]["handle"][0]["handler"], "reverse_proxy",
            "a refused root must not cost the user the app: HTTP serves it"
        );
        assert_eq!(http[0]["handle"][0]["upstreams"][0]["dial"], "127.0.0.1:40017");
        assert_eq!(h.ingress.status().trust, TrustState::Refused);
        assert_eq!(h.ingress.status().state, "running");
    }

    #[test]
    fn trust_installed_redirects_http_to_https() {
        let h = harness_with(TrustState::Installed);
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");

        let http = h.admin.routes(SERVER_HTTP);
        assert_eq!(http[0]["handle"][0]["status_code"], 308);
        assert_eq!(
            http[0]["handle"][0]["headers"]["Location"][0],
            "https://web.hello-pg.localhost{http.request.uri}"
        );
        assert!(h.ingress.status().trust.trusted());
    }

    #[test]
    fn installing_trust_rewrites_the_http_routes_and_is_remembered() {
        let h = harness_with(TrustState::Absent);
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");
        assert_eq!(
            h.admin.routes(SERVER_HTTP)[0]["handle"][0]["handler"],
            "reverse_proxy"
        );

        let installer = FakeTrustInstaller::trusting();
        let report = h.ingress.install_trust(&installer).expect("install");
        assert_eq!(report.state, TrustState::Installed);
        assert_eq!(installer.calls().len(), 1);
        assert!(installer.calls()[0].starts_with("install "));
        assert_eq!(h.ingress.trust(), TrustState::Installed);
        assert_eq!(
            h.admin.routes(SERVER_HTTP)[0]["handle"][0]["status_code"], 308,
            "the plain-HTTP routes follow the new trust state, in place"
        );
        assert_eq!(
            stored_trust(&h.ingress.options().data_dir),
            TrustState::Installed,
            "a restart must not re-ask, or silently lose HTTPS"
        );

        h.ingress.remove_trust(&installer).expect("remove");
        assert_eq!(h.ingress.trust(), TrustState::Absent);
        assert_eq!(
            h.admin.routes(SERVER_HTTP)[0]["handle"][0]["handler"],
            "reverse_proxy",
            "uninstall puts HTTP back to serving"
        );
        assert_eq!(installer.calls().last().map(String::as_str), Some("remove"));
    }

    #[test]
    fn a_refused_root_install_leaves_http_serving() {
        let h = harness_with(TrustState::Absent);
        h.ingress
            .sync_environment("env-1", &[Route::upstream(WEB, 40017)])
            .expect("sync");
        let installer = FakeTrustInstaller::refusing();
        let report = h.ingress.install_trust(&installer).expect("refusal is not an error");
        assert_eq!(report.state, TrustState::Refused);
        assert_eq!(h.ingress.trust(), TrustState::Refused);
        assert_eq!(
            h.admin.routes(SERVER_HTTP)[0]["handle"][0]["handler"],
            "reverse_proxy"
        );
    }

    #[test]
    fn a_port_conflict_is_typed_and_stops_before_anything_starts() {
        let temp = tempfile::tempdir().expect("temp dir");
        // Hold the HTTPS port, then ask ingress to use it.
        let squatter = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let https_port = squatter.local_addr().expect("addr").port();

        let ingress = Ingress::new(
            IngressOptions::new(temp.path())
                .with_ports(free_port(), https_port)
                .with_admin(AdminMode::Spawn),
        );
        let err = ingress.load_all(&[]).expect_err("the port is taken");
        assert_eq!(err.code(), "PortUnavailable", "{err:?}");
        assert_eq!(err.status(), 409);
        let detail = err.detail();
        assert!(detail.contains(&https_port.to_string()), "{detail}");
        assert!(detail.contains("will not move ingress"), "{detail}");
        assert!(!ingress.is_running(), "nothing was started");
        assert_eq!(ingress.status().state, "unavailable");
        assert!(ingress.status().detail.is_some());
    }

    #[test]
    fn a_missing_caddy_binary_is_a_typed_error_naming_what_it_looked_for() {
        let temp = tempfile::tempdir().expect("temp dir");
        let ingress = Ingress::new(
            IngressOptions::new(temp.path())
                .with_ports(free_port(), free_port())
                .with_caddy(Some(temp.path().join("nowhere").join("caddy.exe")))
                .with_admin(AdminMode::Spawn),
        );
        let err = ingress.load_all(&[]).expect_err("no binary");
        assert!(matches!(err, IngressError::Caddy(_)), "{err:?}");
        assert!(err.detail().contains("caddy.exe"), "{}", err.detail());
        assert_eq!(err.code(), "BackendUnavailable", "Caddy is not a port problem");
    }
}

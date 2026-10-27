//! The loopback HTTP API.
//!
//! Everything that talks to the daemon talks to this API: the desktop shell,
//! the CLI, and the MCP shim are all clients (ADR-0003). It binds to
//! `127.0.0.1` only and every route except the liveness probe requires the
//! Agent token published in the discovery file.
//!
//! Three rules shape the mutating routes:
//!
//! * each mutation runs as an **operation** with an id, and the HTTP call blocks
//!   for at most `AGENTIFY_OPERATION_TIMEOUT_MS`. A timeout answers `504` with
//!   the operation id, and `GET /v1/operations/{id}/wait` finishes the job.
//! * mutations are serialised per Environment, so a second `start` cannot race
//!   the first.
//! * a destructive mutation answers `202` with an Approval and an operation id
//!   before changing anything. The operation then waits for a human, who
//!   authorises it with an interactive OS gesture; an Agent holds the token but
//!   cannot produce that gesture.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentify_backends::LogOptions;
use agentify_proto::{
    ActivityPage, Actor, ApprovalAccepted, ApprovalDecision as ApprovalDecisionView,
    ApprovalDecisionRequest, ApprovalList, Capabilities, CreateProjectRequest, DaemonInfo,
    Discovery, DiskView, ExecRequest, ExecResponse, LogLineView, LogsResponse, OperationView,
    Problem, ProjectList,
    StatsResponse, Whoami, API_VERSION, APPROVAL_APPROVED, APPROVAL_DENIED, APPROVAL_EXPIRED,
    APPROVAL_PENDING, DEV_ENVIRONMENT, HEALTH_HEALTHY, OPERATION_WAITING_FOR_APPROVAL,
    STATE_RUNNING,
};
use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify, OwnedMutexGuard, Semaphore};
use tokio::time::Instant;

use crate::approval::{self, Destructive};
use crate::audit;
use crate::auth;
use crate::backends;
use crate::clock;
use crate::config::Config;
use crate::consent::{self, ConsentOutcome, ConsentVerifier};
use crate::disk::{DiskUsage, DISK_PRESSURE_BYTES};
use crate::discovery;
use crate::lifecycle::{self, Lifecycle, LifecycleError};
use crate::logfile::DaemonLog;
use crate::projects::{self, CreateError};
use crate::redact::Redactor;
use crate::secrets::{self, Secrets};
use crate::state::{
    ActorRecord, ApprovalRecord, IdempotencyRecord, OperationRecord, ProjectRecord, SecretRecord,
    State as StateStore,
};

/// Version of this daemon build.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a replayed mutation stays replayable.
pub const IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How many times startup tries to republish the routes, and how long it waits
/// between tries. A restart's own previous Caddy releases 80 and 443 a moment
/// after it is killed, and a route must not be lost to that race.
const REPUBLISH_ATTEMPTS: u32 = 20;
const REPUBLISH_PAUSE: Duration = Duration::from_millis(500);

/// The abuse cap from ADR-0003: at most two Environments may be starting at once.
const MAX_CONCURRENT_STARTS: usize = 2;

/// Identity the auth middleware attaches to an authenticated request.
#[derive(Debug, Clone)]
pub struct Caller {
    pub token_id: String,
    pub scopes: Vec<String>,
}

struct AppState {
    state: StateStore,
    /// The data root: the daemon owns every path under it, including each
    /// Project's workspace.
    data_root: PathBuf,
    instance_id: String,
    started_at: String,
    heartbeat: Duration,
    /// The Backend plus the daemon's own port registry and health decision.
    lifecycle: Arc<Lifecycle>,
    /// How long a mutating call blocks before answering `504`.
    operation_timeout: Duration,
    /// Default log tail.
    log_tail: usize,
    /// How long an Approval stays usable.
    approval_ttl: Duration,
    /// The human gate every Approval passes through.
    consent: Arc<dyn ConsentVerifier>,
    /// Where Project secret values live, and where `fromEnv` imports read from.
    secrets: Arc<Secrets>,
    /// One redactor per Project, built on first use and dropped when that
    /// Project's secrets change. Values are read from the store, so this cache
    /// holds plaintext in memory only for as long as the daemon runs — which is
    /// exactly as long as the injected values are live anyway.
    redactors: Mutex<HashMap<String, Arc<Redactor>>>,
    /// One async mutex per Environment: mutations are serialised, reads are not.
    env_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The startup handshake: what this build pins, what it found, and the
    /// sentence to show when the two disagree.
    handshake: crate::handshake::Handshake,
    /// Signals the graceful-shutdown future that an API caller asked the daemon
    /// to stop, so `POST /v1/system/shutdown` reuses the same exit path as Ctrl-C.
    shutdown: Arc<Notify>,
    /// Permits the two-concurrent-start cap: held only while `lifecycle.start`
    /// runs, so a third `start` is refused instead of queued.
    start_slots: Arc<Semaphore>,
    /// Measures free space on the data-root volume for the `whoami` warning.
    disk: Arc<dyn DiskUsage>,
    /// When the last request arrived, in milliseconds since the Unix epoch.
    /// The idle exit reads it; every route writes it.
    last_activity: Arc<AtomicU64>,
    /// Exit after this long without a request, when nothing is running.
    idle_exit: Option<Duration>,
    /// The bounded log file a console-less release build writes to.
    log: Arc<DaemonLog>,
}

impl AppState {
    /// The lock that serialises this Environment's mutations.
    fn env_lock(&self, environment_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.env_locks.lock().expect("environment lock table");
        locks
            .entry(environment_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn backend_capabilities(&self) -> Capabilities {
        self.lifecycle.backend.describe().capabilities
    }

    /// Record that a request arrived.
    fn touch(&self) {
        self.last_activity
            .store(now_millis(), Ordering::Relaxed);
    }

    /// Is this daemon unused enough to exit?
    ///
    /// A running Environment — desired or observed — and any Operation in flight
    /// hold it open. A state read that fails is not permission to exit.
    fn idle(&self) -> bool {
        let Some(timeout) = self.idle_exit else {
            return false;
        };
        let elapsed = now_millis().saturating_sub(self.last_activity.load(Ordering::Relaxed));
        if elapsed < timeout.as_millis() as u64 {
            return false;
        }
        if self.state.has_running_operations().unwrap_or(true) {
            return false;
        }
        match self.state.list_environments() {
            Ok(environments) => !environments.iter().any(|environment| {
                environment.observed_state == STATE_RUNNING
                    || environment.desired_state == STATE_RUNNING
            }),
            Err(_) => false,
        }
    }

    /// The Agent actor behind an authenticated request.
    fn actor(caller: &Caller) -> ActorRecord {
        ActorRecord {
            kind: "agent".to_string(),
            id: caller.token_id.clone(),
            token_id: Some(caller.token_id.clone()),
        }
    }

    /// Everything a Project's secrets currently contain, ready to be stripped
    /// out of whatever the daemon is about to emit. Cached until the Project's
    /// secrets change, because every response and every audit entry asks.
    fn redactor(&self, project_id: &str) -> Arc<Redactor> {
        if let Some(hit) = self
            .redactors
            .lock()
            .expect("the redactor cache")
            .get(project_id)
        {
            return hit.clone();
        }
        let redactor = match self.state.secrets(project_id) {
            Ok(secrets) => self
                .secrets
                .redactor(project_id, secrets.iter().map(|secret| secret.name.as_str())),
            // A store that cannot be read is not a reason to refuse the request:
            // the value is already out of reach, so there is nothing to redact.
            Err(_) => Redactor::empty(),
        };
        let redactor = Arc::new(redactor);
        self.redactors
            .lock()
            .expect("the redactor cache")
            .insert(project_id.to_string(), redactor.clone());
        redactor
    }

    /// Drop a cached redactor: the Project's secrets changed.
    fn forget_redaction(&self, project_id: &str) {
        self.redactors
            .lock()
            .expect("the redactor cache")
            .remove(project_id);
    }
}

/// A daemon running in a background thread.
pub struct RunningDaemon {
    handle: Option<std::thread::JoinHandle<Result<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
    /// What was published for clients.
    pub discovery: Discovery,
}

impl RunningDaemon {
    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.discovery.url
    }

    /// The Agent token this run published.
    pub fn token(&self) -> &str {
        &self.discovery.token
    }

    pub fn port(&self) -> u16 {
        self.discovery.port
    }

    /// Ask the daemon to stop and wait for it.
    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(());
        }
        self.join()
    }

    /// Wait for the daemon to finish: on `Ctrl-C`, or on a shutdown signal.
    pub fn wait(mut self) -> Result<()> {
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        match self.handle.take() {
            Some(handle) => match handle.join() {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("the daemon thread panicked")),
            },
            None => Ok(()),
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Start a daemon on a background thread and return once it has published its
/// discovery file and is accepting connections.
pub fn start_in_background(config: Config) -> Result<RunningDaemon> {
    let data_root = config.data_root.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<Discovery>>(1);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let handle = std::thread::Builder::new()
        .name("agentifyd".to_string())
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building the async runtime")?;
            runtime.block_on(serve(config, ready_tx, shutdown_rx))
        })
        .context("spawning the daemon thread")?;

    let discovery = match ready_rx.recv() {
        Ok(Ok(discovery)) => discovery,
        Ok(Err(err)) => {
            let _ = handle.join();
            return Err(err);
        }
        Err(_) => {
            let _ = handle.join();
            anyhow::bail!(
                "the daemon stopped before publishing a discovery file under {}",
                data_root.display()
            );
        }
    };

    Ok(RunningDaemon {
        handle: Some(handle),
        shutdown: Some(shutdown_tx),
        discovery,
    })
}

async fn serve(
    config: Config,
    ready: std::sync::mpsc::SyncSender<Result<Discovery>>,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let prepared = prepare(&config).await;
    let (state, listener, discovery) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            let _ = ready.send(Err(anyhow::anyhow!("{err:#}")));
            return Err(err);
        }
    };

    // Adoption: a running runtime either survived the restart or it did not.
    // Either way the daemon reports what it observed instead of claiming health.
    if let Err(err) = state.lifecycle.reconcile(&state.state).await {
        eprintln!("agentifyd: reconciling Environments failed: {err:#}");
    }

    // Then the routes: a restart must not change what this machine serves, so
    // every Environment's route is recomputed from the database and loaded into
    // Caddy again. The previous ingress may still be letting go of 80 and 443 —
    // a stopped daemon kills its Caddy, and the OS releases the sockets a moment
    // later — so this is allowed a few tries before it is reported.
    let mut republished = state.lifecycle.republish(&state.state);
    for _ in 1..REPUBLISH_ATTEMPTS {
        if republished.is_ok() {
            break;
        }
        tokio::time::sleep(REPUBLISH_PAUSE).await;
        republished = state.lifecycle.republish(&state.state);
    }
    if let Err(err) = republished {
        // Ingress is not the API: a machine whose 80 or 443 is held by somebody
        // else still gets a daemon that answers, and the next start republishes.
        eprintln!(
            "agentifyd: republishing the routes at startup failed: {} ({})",
            err.code(),
            err.detail()
        );
    }

    let published = discovery.clone();
    if ready.send(Ok(discovery)).is_err() {
        // Nobody is waiting for us any more; nothing to serve.
        discovery::remove(&config);
        return Ok(());
    }

    eprintln!(
        "agentifyd {DAEMON_VERSION} listening on {} (api {API_VERSION})",
        published.url
    );

    let shutdown_notify = state.shutdown.clone();

    let graceful = async move {
        let shutdown = shutdown;
        tokio::select! {
            _ = shutdown => {}
            _ = shutdown_notify.notified() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    };

    let app = Arc::new(state);
    // Adoption: what was asked to be running is brought back, in the
    // background, so the API answers while the workloads come up.
    spawn_adoption(app.clone());
    // An unowned daemon ends by itself once nobody asks it anything.
    spawn_idle_exit(app.clone());
    let log = app.log.clone();
    log.write(&format!("listening on {} (api {API_VERSION})", published.url));
    let served = axum::serve(listener, router(app))
        .with_graceful_shutdown(graceful)
        .await
        .context("serving the loopback API");

    log.write("stopped");
    discovery::remove(&config);
    served
}

/// Record that a request arrived, so the idle exit does not fire under a live
/// client. Health probes count: a probing caller is still a caller.
async fn track_activity(
    State(app): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    app.touch();
    next.run(request).await
}

/// Exit when nobody has asked the daemon anything for a while.
///
/// This is what lets an MCP-only daemon be on demand rather than resident: it
/// lives for the Agent's session and a little after. A running Environment or
/// an Operation in flight always holds it open.
fn spawn_idle_exit(app: Arc<AppState>) {
    let Some(timeout) = app.idle_exit else {
        return;
    };
    let tick = (timeout / 4).clamp(Duration::from_millis(25), Duration::from_secs(5));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            if !app.idle() {
                continue;
            }
            app.log
                .write("idle: no request, no running Environment, no Operation; exiting");
            app.shutdown.notify_one();
            return;
        }
    });
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Bring every Environment whose desired state is `running` back up.
///
/// A daemon restart is not a stop. On Windows the Backend's supervisor is a
/// foreground process the daemon holds open, so a daemon that dies takes the
/// workload and its leased ports with it — the data is still on disk, but the
/// URL behind the route answers 502 until something starts the Environment
/// again. Something is this: every Environment the user or an Agent asked to
/// keep running is adopted here.
///
/// It runs as one background task per Environment, under the same per-Environment
/// lock a mutation takes, so a start that arrives at the same moment waits for
/// this rather than racing it. The Backend's `start` is idempotent and
/// convergent, and the port registry hands back the same lease, so adopting a
/// runtime that did survive is a no-op.
fn spawn_adoption(app: Arc<AppState>) {
    tokio::spawn(async move {
        let projects = match app.state.list_projects() {
            Ok(projects) => projects,
            Err(err) => {
                app.log
                    .write(&format!("adoption could not read the Projects: {err:#}"));
                return;
            }
        };
        for record in projects {
            let desired = app
                .state
                .environment(&record.environment.id)
                .ok()
                .flatten()
                .map(|environment| environment.desired_state)
                .unwrap_or_default();
            if desired != STATE_RUNNING {
                continue;
            }
            let lock = app.env_lock(&record.environment.id);
            let guard = lock.lock_owned().await;
            match app.lifecycle.start(&app.state, &record).await {
                Ok(status) => app.log.write(&format!(
                    "adopted {}/{} ({})",
                    record.name, record.environment.name, status.health
                )),
                Err(err) => app.log.write(&format!(
                    "adopting {}/{} failed: {} ({})",
                    record.name,
                    record.environment.name,
                    err.code(),
                    err.detail()
                )),
            }
            drop(guard);
        }
    });
}

async fn prepare(config: &Config) -> Result<(AppState, TcpListener, Discovery)> {
    crate::config::ensure_data_root(&config.data_root)?;
    let log = Arc::new(DaemonLog::open(&config.data_root)?);
    log.write(&format!(
        "agentifyd {DAEMON_VERSION} starting with data root {}",
        config.data_root.display()
    ));

    let state = StateStore::open(&config.data_root)?;
    let instance = state.instance()?;
    let token = state.issue_agent_token()?;

    let backend = backends::select(
        &config.backend,
        config.backend_script.as_deref(),
        &config.data_root,
    )?;

    // The handshake is taken before anything drives the Backend: a substrate
    // this build did not pin is refused rather than half-used. A mismatch is not
    // fatal — the daemon still serves its API, so the UI can say which component
    // is wrong — but every lifecycle call answers `VersionMismatch`.
    let handshake = crate::handshake::check(config, &backend);
    if let Some(refusal) = &handshake.refusal {
        log.write(refusal);
    }

    let secrets = Arc::new(config.secrets());

    let ingress = config
        .ingress
        .clone()
        .map(|options| Arc::new(agentify_ingress::Ingress::new(options)));

    // Trust is never changed implicitly: an explicit startup action is the only
    // path that touches the user's certificate store.
    if let (Some(ingress), Some(action)) = (ingress.clone(), config.trust_action) {
        ingress
            .ensure_started()
            .map_err(|err| anyhow::anyhow!("starting ingress for the trust action: {err}"))?;
        let installer = agentify_ingress::WindowsTrustInstaller::new();
        match action {
            crate::config::TrustAction::Install => ingress
                .install_trust(&installer)
                .map_err(|err| anyhow::anyhow!("installing the local CA root: {err}"))?,
            crate::config::TrustAction::Remove => ingress
                .remove_trust(&installer)
                .map_err(|err| anyhow::anyhow!("removing the local CA root: {err}"))?,
        };
    }

    let lifecycle = Arc::new(Lifecycle {
        backend,
        ports: crate::ports::PortRegistry::new(config.lease_range),
        secrets: secrets.clone(),
        ingress,
        probe_timeout: config.probe_timeout,
        exec_timeout: config.exec_timeout,
        refusal: handshake.refusal.clone(),
    });

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;
    let port = listener.local_addr()?.port();

    let discovery = discovery::write(config, DAEMON_VERSION, &token.token, port)?;

    let state = AppState {
        state,
        data_root: config.data_root.clone(),
        instance_id: instance.id,
        started_at: clock::now_rfc3339(),
        heartbeat: config.heartbeat,
        lifecycle,
        operation_timeout: config.operation_timeout,
        log_tail: config.log_tail,
        approval_ttl: config.approval_ttl,
        consent: config.consent.verifier(),
        secrets,
        redactors: Mutex::new(HashMap::new()),
        env_locks: Mutex::new(HashMap::new()),
        handshake,
        shutdown: Arc::new(Notify::new()),
        start_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_STARTS)),
        disk: config.disk.clone(),
        last_activity: Arc::new(AtomicU64::new(now_millis())),
        idle_exit: config.idle_exit,
        log,
    };
    Ok((state, listener, discovery))
}

fn router(state: Arc<AppState>) -> Router {
    let environments = "/v1/projects/{project}/environments/{environment}";
    let protected = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/tokens/revoke", post(revoke_token))
        .route("/v1/handshake", get(handshake))
        .route("/v1/events", get(events))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{project}",
            get(get_project).delete(delete_project),
        )
        .route(
            "/v1/projects/{project}/secrets",
            get(list_secrets).post(create_secret),
        )
        .route(
            "/v1/projects/{project}/secrets/{name}",
            patch(update_secret).delete(delete_secret),
        )
        .route(
            "/v1/projects/{project}/secrets/{name}/reveal",
            post(reveal_secret),
        )
        .route(
            &format!("{environments}/start"),
            post(start_environment),
        )
        .route(&format!("{environments}/stop"), post(stop_environment))
        .route(&format!("{environments}/reset"), post(reset_environment))
        .route(&format!("{environments}/status"), get(environment_status))
        .route(
            &format!("{environments}/resources/{{resource}}/exec"),
            post(exec_in_resource),
        )
        .route(&format!("{environments}/logs"), get(environment_logs))
        .route(&format!("{environments}/stats"), get(environment_stats))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}/approve", post(approve_approval))
        .route("/v1/approvals/{id}/deny", post(deny_approval))
        .route("/v1/activity", get(activity))
        .route("/v1/operations/{id}", get(get_operation))
        .route("/v1/operations/{id}/wait", get(wait_operation))
        .route("/v1/system/update_readiness", get(update_readiness))
        .route("/v1/system/shutdown", post(shutdown_system))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_agent_token,
        ))
        // Outermost: no response body leaves the daemon before the Project's
        // known secret values are stripped out of it. The human reveal is the
        // single deliberate exception, and it says so in its own path.
        .route_layer(middleware::from_fn_with_state(state.clone(), redact_plaintext));

    Router::new()
        .route("/v1/healthz", get(healthz))
        .merge(protected)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), track_activity))
        .with_state(state)
}

async fn healthz(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "daemon_version": DAEMON_VERSION,
        "api_version": API_VERSION,
        // Unauthenticated on purpose: the shell shows a version banner before it
        // has done anything else, and the payload carries no token or state.
        "handshake": handshake_payload(&app),
    }))
}

/// `GET /v1/handshake`: the pinned versions, what was found, and the refusal.
async fn handshake(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(handshake_payload(&app))
}

fn handshake_payload(app: &AppState) -> serde_json::Value {
    let info = app.lifecycle.backend_ref();
    serde_json::json!({
        "ok": app.handshake.report.ok,
        "backend": {
            "name": info.name,
            "version": info.version,
            "platform": info.platform,
        },
        "components": app.handshake.report.components,
        "refusal": app.handshake.refusal,
    })
}

/// `GET /v1/system/update_readiness`: the two gates an update must clear before
/// it may apply — no mutation in flight, and every desired-running Environment
/// adopted and healthy.
async fn update_readiness(State(app): State<Arc<AppState>>) -> Response {
    let operations_in_flight = match app.state.has_running_operations() {
        Ok(value) => value,
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };

    let (projects, environments) = match (app.state.list_projects(), app.state.list_environments()) {
        (Ok(projects), Ok(environments)) => (projects, environments),
        (Err(err), _) | (_, Err(err)) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };

    let names: HashMap<String, String> = projects
        .iter()
        .map(|project| (project.environment.id.clone(), project.name.clone()))
        .collect();

    let mut degraded_projects = Vec::new();
    for environment in &environments {
        if environment.desired_state != STATE_RUNNING {
            continue;
        }
        let adopted = environment.observed_state == STATE_RUNNING
            && environment.health == HEALTH_HEALTHY;
        if !adopted {
            degraded_projects.push(
                names
                    .get(&environment.id)
                    .cloned()
                    .unwrap_or_else(|| environment.id.clone()),
            );
        }
    }

    Json(serde_json::json!({
        "operations_in_flight": operations_in_flight,
        "adopted": degraded_projects.is_empty(),
        "degraded_projects": degraded_projects,
    }))
    .into_response()
}

/// `POST /v1/system/shutdown`: the API path the updater and uninstaller use to
/// stop the daemon before swapping or deleting its files.
async fn shutdown_system(State(app): State<Arc<AppState>>) -> Response {
    app.shutdown.notify_one();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "shutting_down" })),
    )
        .into_response()
}

async fn whoami(State(app): State<Arc<AppState>>, Extension(caller): Extension<Caller>) -> Json<Whoami> {
    let backend = app.lifecycle.backend_ref();
    let disk = app.disk.free_bytes(&app.data_root).ok().map(|free_bytes| DiskView {
        free_bytes,
        warning: free_bytes < DISK_PRESSURE_BYTES,
    });
    Json(Whoami {
        // The walking skeleton has no Projects, so no caller is bound to one.
        project: None,
        scopes: caller.scopes,
        daemon_version: DAEMON_VERSION.to_string(),
        api_version: API_VERSION.to_string(),
        capabilities: app.backend_capabilities(),
        backend,
        actor: Actor {
            kind: "agent".to_string(),
            id: caller.token_id,
        },
        daemon: DaemonInfo {
            instance_id: app.instance_id.clone(),
            started_at: app.started_at.clone(),
        },
        disk,
    })
}

/// `POST /v1/tokens/revoke`: revoke the token that made this request.
async fn revoke_token(
    State(app): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    match app.state.revoke_token(&caller.token_id) {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({ "revoked": true })),
        )
            .into_response(),
        Ok(false) => problem(
            StatusCode::NOT_FOUND,
            "NotFound",
            Some("the presented token is already revoked or unknown".to_string()),
        ),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

async fn events(State(app): State<Arc<AppState>>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let heartbeat = app.heartbeat;
    let stream = futures_util::stream::unfold(0u64, move |seq| async move {
        if seq > 0 {
            tokio::time::sleep(heartbeat).await;
        }
        let payload = serde_json::json!({
            "seq": seq,
            "at": clock::now_rfc3339(),
            "daemon_version": DAEMON_VERSION,
        });
        let event = Event::default()
            .event("heartbeat")
            .id(seq.to_string())
            .data(payload.to_string());
        Some((Ok::<Event, Infallible>(event), seq + 1))
    });
    Sse::new(stream)
}

/// `POST /v1/projects`: create a Project from exactly one of a bundled fixture
/// or an inline Manifest. Nothing starts.
async fn create_project(
    State(app): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let request: CreateProjectRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some(format!(
                    "the request body is not a create_project payload: {}",
                    err
                )),
                Some(String::new()),
            )
        }
    };

    match projects::create(&app.state, &app.data_root, &app.secrets, request) {
        Ok(project) => {
            record_audit(
                &app,
                audit::Append {
                    project_id: Some(&project.id),
                    operation: "create_project",
                    target: Some(&project.name),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                    ..audit::Append::done(AppState::actor(&caller), "create_project", &request_id)
                },
            );
            (StatusCode::CREATED, Json(project)).into_response()
        }
        Err(CreateError::SecretSourceMissing(detail)) => {
            record_failure(
                &app,
                &caller,
                &request_id,
                "create_project",
                None,
                "SecretSourceMissing",
                started,
            );
            problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "SecretSourceMissing",
                Some(detail),
            )
        }
        Err(CreateError::Validation { path, detail }) => {
            record_failure(
                &app,
                &caller,
                &request_id,
                "create_project",
                None,
                "ValidationFailed",
                started,
            );
            problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some(detail),
                Some(path),
            )
        }
        Err(CreateError::SourceMissing(detail)) => {
            record_failure(
                &app,
                &caller,
                &request_id,
                "create_project",
                None,
                "SourceMissing",
                started,
            );
            problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "SourceMissing",
                Some(detail),
            )
        }
        Err(CreateError::AlreadyExists(detail)) => {
            record_failure(
                &app,
                &caller,
                &request_id,
                "create_project",
                None,
                "AlreadyExists",
                started,
            );
            problem(
                StatusCode::CONFLICT,
                "AlreadyExists",
                Some(detail),
            )
        }
        Err(CreateError::ProjectCapExceeded(detail)) => {
            record_failure(
                &app,
                &caller,
                &request_id,
                "create_project",
                None,
                "ProjectCapExceeded",
                started,
            );
            problem(StatusCode::CONFLICT, "ProjectCapExceeded", Some(detail))
        }
        Err(CreateError::Store(err)) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

/// `GET /v1/projects`.
async fn list_projects(State(app): State<Arc<AppState>>) -> Response {
    match projects::list(&app.state) {
        Ok(projects) => Json(ProjectList { projects }).into_response(),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

/// `GET /v1/projects/{project}`: by name or id.
async fn get_project(State(app): State<Arc<AppState>>, Path(reference): Path<String>) -> Response {
    match projects::get(&app.state, &reference) {
        Ok(Some(project)) => Json(project).into_response(),
        Ok(None) => problem(
            StatusCode::NOT_FOUND,
            "NotFound",
            Some(format!("no Project named or identified by {reference:?}")),
        ),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

// ---- Environment lifecycle -------------------------------------------------

/// What a mutation produced, so it can be replayed verbatim.
#[derive(Debug, Clone)]
struct Outcome {
    status: StatusCode,
    body: String,
}

impl Outcome {
    fn json(status: StatusCode, value: &impl serde::Serialize) -> Self {
        Self {
            status,
            body: serde_json::to_string(value).unwrap_or_default(),
        }
    }

    fn problem(status: StatusCode, code: &str, detail: String, operation_id: Option<&str>) -> Self {
        let title = status.canonical_reason().unwrap_or("Error").to_string();
        let mut problem = Problem::new(status.as_u16(), code, &title, Some(detail));
        problem.operation_id = operation_id.map(str::to_string);
        Self {
            status,
            body: serde_json::to_string(&problem).unwrap_or_default(),
        }
    }

    fn lifecycle(err: LifecycleError) -> Self {
        let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self::problem(status, &err.code(), err.detail(), None)
    }

    fn succeeded(&self) -> bool {
        self.status.is_success()
    }

    fn into_response(self) -> Response {
        let content_type = if self.status.is_success() {
            "application/json"
        } else {
            "application/problem+json"
        };
        (self.status, [(header::CONTENT_TYPE, content_type)], self.body).into_response()
    }
}

/// The kinds of mutation that run as an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    Start,
    Stop,
}

impl Mutation {
    fn as_str(&self) -> &'static str {
        match self {
            Mutation::Start => "start",
            Mutation::Stop => "stop",
        }
    }
}

async fn start_environment(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    mutate(app, project, environment, Mutation::Start, caller, headers).await
}

async fn stop_environment(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    mutate(app, project, environment, Mutation::Stop, caller, headers).await
}

/// Run a lifecycle mutation as an operation with an id.
async fn mutate(
    app: Arc<AppState>,
    project: String,
    environment: String,
    mutation: Mutation,
    caller: Caller,
    headers: HeaderMap,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let path = format!(
        "/v1/projects/{project}/environments/{environment}/{}",
        mutation.as_str()
    );
    let fingerprint = fingerprint("POST", &path, &[]);

    // A replay must not run the mutation again.
    match replay(&app, &headers, "POST", &path, &fingerprint) {
        Replay::None => {}
        Replay::Response(outcome) => return outcome.into_response(),
        Replay::Conflict => {
            return Outcome::problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                "this Idempotency-Key was already used for a different request".to_string(),
                None,
            )
            .into_response()
        }
    }

    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };

    // The two-concurrent-start cap: a third start is refused before it even
    // creates an operation, rather than queued behind the other two.
    let start_slot = if mutation == Mutation::Start {
        match app.start_slots.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return Outcome::lifecycle(LifecycleError::StartCapExceeded).into_response(),
        }
    } else {
        None
    };

    let operation_kind = match mutation {
        Mutation::Start => "start_environment",
        Mutation::Stop => "stop_environment",
    };
    let target = format!("{}/{}", record.name, record.environment.name);

    let now = clock::now_rfc3339();
    let operation = OperationRecord {
        id: uuid::Uuid::new_v4().to_string(),
        kind: mutation.as_str().to_string(),
        project_id: record.id.clone(),
        environment_id: record.environment.id.clone(),
        state: "running".to_string(),
        created_at: now.clone(),
        updated_at: now,
        finished_at: None,
        error_code: None,
        error_detail: None,
        result_json: None,
    };
    if let Err(err) = app.state.create_operation(&operation) {
        return Outcome::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            format!("{err:#}"),
            None,
        )
        .into_response();
    }
    let operation_id = operation.id.clone();

    let (tx, rx) = oneshot::channel::<Outcome>();
    let worker = app.clone();
    let worker_operation = operation_id.clone();
    let worker_record = record.clone();
    let environment_id = record.environment.id.clone();
    let worker_project = record.id.clone();
    let worker_request = request_id.clone();
    let worker_actor = AppState::actor(&caller);
    tokio::spawn(async move {
        let lock = worker.env_lock(&environment_id);
        let guard: OwnedMutexGuard<()> = lock.lock_owned().await;
        let result = match mutation {
            Mutation::Start => worker.lifecycle.start(&worker.state, &worker_record).await,
            Mutation::Stop => worker.lifecycle.stop(&worker.state, &worker_record).await,
        };
        drop(guard);
        drop(start_slot);

        let outcome = match result {
            Ok(mut status) => {
                status.operation_id = Some(worker_operation.clone());
                let outcome = Outcome::json(StatusCode::OK, &status);
                let _ = worker.state.finish_operation(
                    &worker_operation,
                    "succeeded",
                    None,
                    None,
                    Some(&outcome.body),
                );
                record_audit(
                    &worker,
                    audit::Append {
                        project_id: Some(&worker_project),
                        operation: operation_kind,
                        target: Some(&target),
                        duration_ms: Some(started.elapsed().as_millis() as u64),
                        ..audit::Append::done(worker_actor, operation_kind, &worker_request)
                    },
                );
                outcome
            }
            Err(err) => {
                let code = err.code();
                let detail = err.detail();
                let outcome = Outcome::lifecycle(err);
                let _ = worker.state.finish_operation(
                    &worker_operation,
                    "failed",
                    Some(&code),
                    Some(&detail),
                    None,
                );
                record_audit(
                    &worker,
                    audit::Append {
                        project_id: Some(&worker_project),
                        operation: operation_kind,
                        target: Some(&target),
                        result: "failed",
                        error_code: Some(&code),
                        duration_ms: Some(started.elapsed().as_millis() as u64),
                        ..audit::Append::done(worker_actor, operation_kind, &worker_request)
                    },
                );
                outcome
            }
        };
        let _ = tx.send(outcome);
    });

    let outcome = match tokio::time::timeout(app.operation_timeout, rx).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => Outcome::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "the operation ended without a result".to_string(),
            Some(&operation_id),
        ),
        Err(_elapsed) => Outcome::problem(
            StatusCode::GATEWAY_TIMEOUT,
            "Timeout",
            format!(
                "{} did not finish within {} ms; wait on the operation to see the outcome",
                mutation.as_str(),
                app.operation_timeout.as_millis()
            ),
            Some(&operation_id),
        ),
    };

    if outcome.succeeded() {
        remember(&app, &headers, "POST", &path, &fingerprint, &outcome);
    }
    outcome.into_response()
}

/// `GET .../status`.
async fn environment_status(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
) -> Response {
    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };
    match app.lifecycle.status(&app.state, &record).await {
        Ok(status) => Json(status).into_response(),
        Err(err) => Outcome::lifecycle(err).into_response(),
    }
}

/// `POST .../resources/{resource}/exec`.
async fn exec_in_resource(
    State(app): State<Arc<AppState>>,
    Path((project, environment, resource)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let request: ExecRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return Outcome::problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                format!("the request body is not an exec payload: {err}"),
                None,
            )
            .into_response()
        }
    };
    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };
    let target = format!("{}/{}/{}", record.name, record.environment.name, resource);
    match app
        .lifecycle
        .exec(&app.state, &record, &resource, &request)
        .await
    {
        Ok(response) => {
            // The command's output is a value: the audit records that it ran,
            // in which Resource, and for how long — never what it printed.
            record_audit(
                &app,
                audit::Append {
                    project_id: Some(&record.id),
                    operation: "exec",
                    target: Some(&target),
                    duration_ms: Some(response.duration_ms),
                    ..audit::Append::done(AppState::actor(&caller), "exec", &request_id)
                },
            );
            Json::<ExecResponse>(response).into_response()
        }
        Err(err) => {
            let code = err.code();
            record_audit(
                &app,
                audit::Append {
                    project_id: Some(&record.id),
                    operation: "exec",
                    target: Some(&target),
                    result: "failed",
                    error_code: Some(&code),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                    ..audit::Append::done(AppState::actor(&caller), "exec", &request_id)
                },
            );
            Outcome::lifecycle(err).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    /// Keep only the last N lines.
    tail: Option<usize>,
    /// A sequence number or an RFC 3339 timestamp.
    since: Option<String>,
    /// `1` or `true` streams new lines over SSE.
    follow: Option<String>,
    /// Restrict to one Resource.
    resource: Option<String>,
}

/// `GET .../logs`: a bounded tail, or a live SSE stream with `follow=1`.
async fn environment_logs(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };
    let follow = matches!(
        query.follow.as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes")
    );
    let options = LogOptions {
        tail: Some(query.tail.unwrap_or(app.log_tail)),
        since: query.since.clone(),
        follow,
    };

    let stream = match app
        .lifecycle
        .logs(&app.state, &record, query.resource.as_deref(), &options)
        .await
    {
        Ok(stream) => stream,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };

    if !follow {
        let collected: Vec<_> = stream.collect::<Vec<_>>().await;
        let mut lines = Vec::new();
        let mut next_since = None;
        for item in collected {
            match item {
                Ok(line) => {
                    next_since = Some(line.seq.to_string());
                    lines.push(lifecycle::log_line_view(&line));
                }
                Err(err) => {
                    return Outcome::problem(
                        StatusCode::from_u16(err.status()).unwrap_or(StatusCode::BAD_GATEWAY),
                        err.code(),
                        err.to_string(),
                        None,
                    )
                    .into_response()
                }
            }
        }
        return Json(LogsResponse { lines, next_since }).into_response();
    }

    // A stream is not buffered through the response redactor, so its lines are
    // redacted here, where each one is made.
    let redactor = app.redactor(&record.id);
    let events = stream.map(move |item| {
        let event = match item {
            Ok(line) => Event::default()
                .event("log")
                .id(line.seq.to_string())
                .data(
                    serde_json::to_string(&redact_log_line(&redactor, &line))
                        .unwrap_or_default(),
                ),
            Err(err) => Event::default().event("error").data(
                serde_json::json!({
                    "code": err.code(),
                    "detail": redactor.redact(&err.to_string()),
                })
                .to_string(),
            ),
        };
        Ok::<Event, Infallible>(event)
    });
    Sse::new(events).into_response()
}

/// One log line with the Project's known secret values removed.
fn redact_log_line(
    redactor: &Redactor,
    line: &agentify_backends::LogLine,
) -> LogLineView {
    let mut view = lifecycle::log_line_view(line);
    view.line = redactor.redact(&view.line);
    view
}

/// `GET .../stats`.
async fn environment_stats(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
) -> Response {
    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };
    match app.lifecycle.stats(&app.state, &record).await {
        Ok(stats) => Json::<StatsResponse>(stats).into_response(),
        Err(err) => Outcome::lifecycle(err).into_response(),
    }
}

// ---- destructive operations: 202, an Approval, and an operation ------------

/// The `202` a destructive call answers with, before changing anything.
struct Accepted {
    approval_id: String,
    operation_id: String,
    body: String,
}

impl Accepted {
    fn into_response(self) -> Response {
        let Accepted {
            approval_id,
            operation_id,
            body,
        } = self;
        (
            StatusCode::ACCEPTED,
            [
                ("content-type", "application/json"),
                ("x-approval-id", approval_id.as_str()),
                ("x-operation-id", operation_id.as_str()),
            ],
            body,
        )
            .into_response()
    }
}

/// `DELETE /v1/projects/{project}`: ask a human first.
async fn delete_project(
    State(app): State<Arc<AppState>>,
    Path(project): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    let record = match app.state.find_project(&project) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                "NotFound",
                Some(format!("no Project named or identified by {project:?}")),
            )
        }
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };
    let destructive = Destructive {
        operation: approval::DELETE_PROJECT,
        target: record.name.clone(),
        parameters: [
            ("project".to_string(), record.name.clone()),
            ("projectId".to_string(), record.id.clone()),
        ]
        .into_iter()
        .collect(),
    };
    request_approval(app, record, destructive, caller, headers, "DELETE").await
}

/// `POST .../environments/{environment}/reset`: ask a human first.
async fn reset_environment(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    let record = match resolve(&app.state, &project, &environment) {
        Ok(record) => record,
        Err(err) => return Outcome::lifecycle(err).into_response(),
    };
    let destructive = Destructive {
        operation: approval::RESET_ENVIRONMENT,
        target: format!("{}/{}", record.name, record.environment.name),
        parameters: [
            ("project".to_string(), record.name.clone()),
            (
                "environment".to_string(),
                record.environment.name.clone(),
            ),
            ("environmentId".to_string(), record.environment.id.clone()),
        ]
        .into_iter()
        .collect(),
    };
    request_approval(app, record, destructive, caller, headers, "POST").await
}

/// Record an operation that waits for a human, the Approval that releases it,
/// and answer `202` with both ids. Nothing about the target changes here.
async fn request_approval(
    app: Arc<AppState>,
    record: ProjectRecord,
    destructive: Destructive,
    caller: Caller,
    headers: HeaderMap,
    method: &str,
) -> Response {
    let request_id = request_id(&headers);
    let path = match destructive.operation {
        approval::DELETE_PROJECT => format!("/v1/projects/{}", record.name),
        _ => format!(
            "/v1/projects/{}/environments/{}/reset",
            record.name, record.environment.name
        ),
    };
    let fingerprint = fingerprint(method, &path, &[]);
    match replay(&app, &headers, method, &path, &fingerprint) {
        Replay::None => {}
        Replay::Response(outcome) => return outcome.into_response(),
        Replay::Conflict => {
            return Outcome::problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                "this Idempotency-Key was already used for a different request".to_string(),
                None,
            )
            .into_response()
        }
    }

    let now = clock::now_rfc3339();
    let expires_at = clock::rfc3339_in_millis(app.approval_ttl.as_millis() as i64);
    let params_hash = destructive.params_hash();
    let operation = OperationRecord {
        id: uuid::Uuid::new_v4().to_string(),
        kind: destructive.operation.to_string(),
        project_id: record.id.clone(),
        environment_id: record.environment.id.clone(),
        state: OPERATION_WAITING_FOR_APPROVAL.to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        finished_at: None,
        error_code: None,
        error_detail: None,
        result_json: None,
    };
    if let Err(err) = app.state.create_operation(&operation) {
        return Outcome::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            format!("{err:#}"),
            None,
        )
        .into_response();
    }

    let approval = ApprovalRecord {
        id: uuid::Uuid::new_v4().to_string(),
        operation_id: operation.id.clone(),
        operation: destructive.operation.to_string(),
        project_id: Some(record.id.clone()),
        project_name: record.name.clone(),
        environment_id: Some(record.environment.id.clone()),
        target: destructive.target.clone(),
        params_json: destructive.params_json(),
        params_hash: params_hash.clone(),
        requester: AppState::actor(&caller),
        state: APPROVAL_PENDING.to_string(),
        created_at: now,
        expires_at: expires_at.clone(),
        decided_at: None,
        decided_by: None,
    };
    if let Err(err) = app.state.create_approval(&approval) {
        let _ = app.state.finish_operation(
            &operation.id,
            "failed",
            Some("Internal"),
            Some("the Approval could not be recorded"),
            None,
        );
        return Outcome::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            format!("{err:#}"),
            Some(&operation.id),
        )
        .into_response();
    }

    record_audit(
        &app,
        audit::Append {
            project_id: Some(&record.id),
            operation: destructive.operation,
            target: Some(&destructive.target),
            params_hash: Some(&params_hash),
            approval_id: Some(&approval.id),
            result: "requested",
            ..audit::Append::done(
                AppState::actor(&caller),
                destructive.operation,
                &request_id,
            )
        },
    );

    // Release whoever is waiting when nobody decides in time.
    spawn_expiry(&app, approval.id.clone(), expires_at.clone());

    let body = serde_json::to_string(&ApprovalAccepted {
        approval_id: approval.id.clone(),
        operation_id: operation.id.clone(),
        operation: destructive.operation.to_string(),
        target: destructive.target,
        params_hash,
        expires_at,
    })
    .unwrap_or_default();

    let accepted = Accepted {
        approval_id: approval.id.clone(),
        operation_id: operation.id.clone(),
        body,
    };
    let outcome = Outcome {
        status: StatusCode::ACCEPTED,
        body: accepted.body.clone(),
    };
    remember(&app, &headers, method, &path, &fingerprint, &outcome);
    accepted.into_response()
}

/// Spawn the timer that expires one Approval, and releases its operation.
fn spawn_expiry(app: &Arc<AppState>, approval_id: String, expires_at: String) {
    let worker = app.clone();
    let wait = clock::millis_until(&expires_at)
        .unwrap_or(0)
        .saturating_add(20);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(wait)).await;
        expire(&worker, &approval_id);
    });
}

/// Expire a pending Approval: the operation fails with `ApprovalExpired` so a
/// waiter is released instead of hanging.
fn expire(app: &AppState, approval_id: &str) {
    let Ok(Some(record)) = app.state.approval(approval_id) else {
        return;
    };
    if record.state != APPROVAL_PENDING {
        return;
    }
    let decided = app.state.decide_approval(
        approval_id,
        &approval::decision(APPROVAL_EXPIRED, record.requester.clone()),
    );
    if !matches!(decided, Ok(true)) {
        return;
    }
    let _ = app.state.finish_operation(
        &record.operation_id,
        "failed",
        Some("ApprovalExpired"),
        Some("nobody approved the operation before its Approval expired"),
        None,
    );
    record_audit(
        app,
        audit::Append {
            project_id: record.project_id.as_deref(),
            operation: &record.operation,
            target: Some(&record.target),
            params_hash: Some(&record.params_hash),
            approval_id: Some(&record.id),
            result: "expired",
            error_code: Some("ApprovalExpired"),
            ..audit::Append::done(record.requester.clone(), &record.operation, "approval-timer")
        },
    );
}

// ---- the Approval inbox ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApprovalQuery {
    /// `pending` by default; `all` shows decided ones too.
    state: Option<String>,
    limit: Option<usize>,
}

/// `GET /v1/approvals`: the inbox, pending first.
async fn list_approvals(
    State(app): State<Arc<AppState>>,
    Query(query): Query<ApprovalQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let state = query.state.as_deref().unwrap_or(APPROVAL_PENDING);
    let filter = match state {
        "all" | "*" => None,
        APPROVAL_PENDING | APPROVAL_APPROVED | APPROVAL_DENIED | APPROVAL_EXPIRED => Some(state),
        other => {
            return problem(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some(format!(
                    "`state` must be one of pending, approved, denied, expired, all; got {other:?}"
                )),
            )
        }
    };
    // Anything past its deadline is not pending any more, whatever the row says.
    if filter.is_none() || filter == Some(APPROVAL_PENDING) {
        let now = clock::now_rfc3339();
        match app.state.expire_due_approvals(&now) {
            Ok(due) => {
                let timer_request = "approval-timer".to_string();
                for record in due {
                    let _ = app.state.finish_operation(
                        &record.operation_id,
                        "failed",
                        Some("ApprovalExpired"),
                        Some("nobody approved the operation before its Approval expired"),
                        None,
                    );
                    record_audit(
                        &app,
                        audit::Append {
                            project_id: record.project_id.as_deref(),
                            operation: &record.operation,
                            target: Some(&record.target),
                            params_hash: Some(&record.params_hash),
                            approval_id: Some(&record.id),
                            result: "expired",
                            error_code: Some("ApprovalExpired"),
                            ..audit::Append::done(
                                record.requester.clone(),
                                &record.operation,
                                &timer_request,
                            )
                        },
                    );
                }
            }
            Err(err) => {
                eprintln!("agentifyd: expiring approvals failed: {err:#}");
            }
        }
    }

    match app.state.approvals(filter, limit) {
        Ok(records) => Json(ApprovalList {
            approvals: records.iter().map(approval::view).collect(),
        })
        .into_response(),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

/// `POST /v1/approvals/{id}/approve`: the OS gesture is the gate.
async fn approve_approval(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    decide(app, id, caller, headers, body, true).await
}

/// `POST /v1/approvals/{id}/deny`: the target is left exactly as it was.
async fn deny_approval(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    decide(app, id, caller, headers, body, false).await
}

/// Decide one Approval: check it is still usable, ask the OS when approving,
/// then record the decision. Only the decision is atomic; the work it releases
/// runs afterwards, under the Environment's lock.
async fn decide(
    app: Arc<AppState>,
    id: String,
    caller: Caller,
    headers: HeaderMap,
    body: Bytes,
    approve: bool,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let request: ApprovalDecisionRequest = if body.is_empty() {
        ApprovalDecisionRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(err) => {
                return problem(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ValidationFailed",
                    Some(format!("the request body is not an approval decision: {err}")),
                )
            }
        }
    };

    let record = match app.state.approval(&id) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                "NotFound",
                Some(format!("no Approval with id {id:?}")),
            )
        }
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };

    // An Approval past its deadline cannot be spent, whatever the row says.
    if record.state == APPROVAL_PENDING && clock::is_expired(&record.expires_at) {
        expire(&app, &id);
    }

    let refusal = |code: &str, status: StatusCode, detail: String| -> Response {
        record_audit(
            &app,
            audit::Append {
                project_id: record.project_id.as_deref(),
                operation: &record.operation,
                target: Some(&record.target),
                params_hash: Some(&record.params_hash),
                approval_id: Some(&record.id),
                result: "failed",
                error_code: Some(code),
                duration_ms: Some(started.elapsed().as_millis() as u64),
                ..audit::Append::done(AppState::actor(&caller), &record.operation, &request_id)
            },
        );
        let mut problem = Problem::new(status.as_u16(), code, status.canonical_reason().unwrap_or("Error"), Some(detail));
        problem.approval_id = Some(record.id.clone());
        problem.operation_id = Some(record.operation_id.clone());
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            serde_json::to_string(&problem).unwrap_or_default(),
        )
            .into_response()
    };

    // A yes covers exactly what the human was shown: a decision that names a
    // different operation or a different parameter hash is refused.
    if let Some(operation_id) = request.operation_id.as_deref() {
        if operation_id != record.operation_id {
            return refusal(
                "ApprovalMismatch",
                StatusCode::CONFLICT,
                format!(
                    "this Approval covers operation {:?}, not {operation_id:?}",
                    record.operation_id
                ),
            );
        }
    }
    if let Some(params_hash) = request.params_hash.as_deref() {
        if params_hash != record.params_hash {
            return refusal(
                "ApprovalMismatch",
                StatusCode::CONFLICT,
                "the parameters the caller named are not the ones this Approval covers"
                    .to_string(),
            );
        }
    }

    // Already decided: say which way, so a replay is unambiguous.
    if record.state != APPROVAL_PENDING {
        let (code, status) = match record.state.as_str() {
            APPROVAL_EXPIRED => ("ApprovalExpired", StatusCode::GONE),
            APPROVAL_DENIED => ("ApprovalDenied", StatusCode::CONFLICT),
            _ => ("ApprovalConsumed", StatusCode::CONFLICT),
        };
        return refusal(
            code,
            status,
            format!(
                "this Approval is {}; it can be used once, within its lifetime",
                record.state
            ),
        );
    }

    let by = if approve {
        // The gate: only an interactive OS gesture lets a destructive operation
        // run. An Agent holds the token but cannot produce the gesture.
        match ask_for_consent(&app, &record).await {
            ConsentOutcome::Verified => ActorRecord {
                kind: "human".to_string(),
                id: consent::os_account(),
                token_id: Some(caller.token_id.clone()),
            },
            ConsentOutcome::Declined(detail) => {
                return refusal("ApprovalRequired", StatusCode::FORBIDDEN, detail)
            }
            ConsentOutcome::Unavailable(detail) => {
                return refusal("ConsentUnavailable", StatusCode::SERVICE_UNAVAILABLE, detail)
            }
        }
    } else {
        ActorRecord {
            kind: "human".to_string(),
            id: consent::os_account(),
            token_id: Some(caller.token_id.clone()),
        }
    };

    let state = if approve {
        APPROVAL_APPROVED
    } else {
        APPROVAL_DENIED
    };
    let decision = approval::decision(state, by.clone());
    match app.state.decide_approval(&id, &decision) {
        Ok(true) => {}
        Ok(false) => {
            return refusal(
                "ApprovalConsumed",
                StatusCode::CONFLICT,
                "another decision landed first; an Approval is single use".to_string(),
            )
        }
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    }

    record_audit(
        &app,
        audit::Append {
            project_id: record.project_id.as_deref(),
            operation: &record.operation,
            target: Some(&record.target),
            params_hash: Some(&record.params_hash),
            approval_id: Some(&record.id),
            result: if approve { "approved" } else { "denied" },
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..audit::Append::done(by.clone(), &record.operation, &request_id)
        },
    );

    if approve {
        // The operation becomes live again before the work starts, so a reader
        // never sees a decided Approval beside a waiting operation.
        let _ = app
            .state
            .start_operation(&record.operation_id)
            .map_err(|err| eprintln!("agentifyd: resuming the operation failed: {err:#}"));
        spawn_execution(app.clone(), record.clone(), by, request_id.clone());
    } else {
        let _ = app.state.finish_operation(
            &record.operation_id,
            "failed",
            Some("ApprovalDenied"),
            Some(if record.operation == approval::DELETE_PROJECT {
                "a human denied the delete; the Project is unchanged".to_string()
            } else {
                "a human denied the reset; the data is unchanged".to_string()
            }
            .as_str()),
            None,
        );
    }

    let decided = match app.state.approval(&id) {
        Ok(Some(decided)) => decided,
        _ => record,
    };
    let operation = match app.state.operation(&decided.operation_id) {
        Ok(Some(operation)) => operation_view(&operation),
        _ => operation_view(&OperationRecord {
            id: decided.operation_id.clone(),
            kind: decided.operation.clone(),
            project_id: String::new(),
            environment_id: String::new(),
            state: "failed".to_string(),
            created_at: clock::now_rfc3339(),
            updated_at: clock::now_rfc3339(),
            finished_at: None,
            error_code: Some("Internal".to_string()),
            error_detail: None,
            result_json: None,
        }),
    };
    Json(ApprovalDecisionView {
        approval: approval::view(&decided),
        operation,
    })
    .into_response()
}

/// Ask the injected verifier for the interactive gesture, off the async workers.
async fn ask_for_consent(app: &AppState, record: &ApprovalRecord) -> ConsentOutcome {
    let verifier = app.consent.clone();
    let message = approval::prompt(record);
    match tokio::task::spawn_blocking(move || verifier.verify(&message)).await {
        Ok(outcome) => outcome,
        Err(err) => ConsentOutcome::Unavailable(format!("the consent prompt failed: {err}")),
    }
}

/// Run the work an Approval released. The operation carries the outcome, so a
/// caller waiting on it sees success or a typed failure.
fn spawn_execution(
    app: Arc<AppState>,
    approval: ApprovalRecord,
    decided_by: ActorRecord,
    request_id: String,
) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let environment_id = approval.environment_id.clone().unwrap_or_default();
        let lock = app.env_lock(&environment_id);
        let guard: OwnedMutexGuard<()> = lock.lock_owned().await;
        let result = match approval.operation.as_str() {
            approval::DELETE_PROJECT => run_delete(&app, &approval).await,
            approval::RESET_ENVIRONMENT => run_reset(&app, &approval).await,
            other => Err(LifecycleError::NotFound(format!(
                "no operation named {other:?} can be released by an Approval"
            ))),
        };
        drop(guard);

        let duration = started.elapsed().as_millis() as u64;
        match result {
            Ok(payload) => {
                let body = payload.to_string();
                let _ = app.state.finish_operation(
                    &approval.operation_id,
                    "succeeded",
                    None,
                    None,
                    Some(&body),
                );
                record_audit(
                    &app,
                    audit::Append {
                        project_id: approval.project_id.as_deref(),
                        operation: &approval.operation,
                        target: Some(&approval.target),
                        params_hash: Some(&approval.params_hash),
                        approval_id: Some(&approval.id),
                        duration_ms: Some(duration),
                        ..audit::Append::done(decided_by, &approval.operation, &request_id)
                    },
                );
            }
            Err(err) => {
                let code = err.code();
                let detail = err.detail();
                let _ = app.state.finish_operation(
                    &approval.operation_id,
                    "failed",
                    Some(&code),
                    Some(&detail),
                    None,
                );
                record_audit(
                    &app,
                    audit::Append {
                        project_id: approval.project_id.as_deref(),
                        operation: &approval.operation,
                        target: Some(&approval.target),
                        params_hash: Some(&approval.params_hash),
                        approval_id: Some(&approval.id),
                        result: "failed",
                        error_code: Some(&code),
                        duration_ms: Some(duration),
                        ..audit::Append::done(decided_by, &approval.operation, &request_id)
                    },
                );
            }
        }
    });
}

/// Delete a Project: stop it, destroy its runtime and volumes, delete its rows,
/// and say what was found gone. The report is the point: "deleted" has to mean
/// nothing is left.
async fn run_delete(
    app: &AppState,
    approval: &ApprovalRecord,
) -> std::result::Result<serde_json::Value, LifecycleError> {
    let environment_id = approval.environment_id.clone().unwrap_or_default();
    let project_id = approval.project_id.clone().unwrap_or_default();
    let project = app.state.find_project(&project_id)?;
    let mut notes: Vec<String> = Vec::new();

    if let Some(record) = &project {
        if let Err(err) = app.lifecycle.stop(&app.state, record).await {
            // A runtime that cannot be stopped is reported, not hidden; the
            // removal below still has to make it gone.
            notes.push(format!("stop: {} {}", err.code(), err.detail()));
        }
        match app.lifecycle.backend.remove(&environment_id, false).await {
            Ok(()) => {}
            Err(agentify_backends::BackendError::NotProvisioned { .. }) => {}
            Err(err) => return Err(LifecycleError::Backend(err)),
        }
    }

    app.state.release_leases(&environment_id)?;

    // A deleted Project takes its secret values with it: leaving them in the
    // credential manager would keep credentials alive for a Project that no
    // longer exists. The names are read before the rows cascade away.
    let secret_names: Vec<String> = app
        .state
        .secrets(&project_id)?
        .into_iter()
        .map(|secret| secret.name)
        .collect();

    app.state.delete_project(&project_id)?;
    // Deleting the Environment is the only thing that deletes its routes; the
    // stop above replaced them with the 503, so they are withdrawn here.
    app.lifecycle.withdraw_routes(&environment_id)?;

    for name in &secret_names {
        if let Err(err) = app.secrets.delete(&project_id, name) {
            // Reported, not hidden: the Project is gone either way, and a note is
            // what a person needs to clean up by hand.
            notes.push(format!("secret {name:?}: {err}"));
        }
    }
    app.forget_redaction(&project_id);

    let workspace = match &project {
        Some(record) => PathBuf::from(&record.workspace_path),
        None => crate::projects::workspace_path(&app.data_root, &project_id),
    };
    let project_dir = match workspace.parent() {
        Some(parent) => parent.to_path_buf(),
        None => workspace.clone(),
    };
    let workspace_removed = match std::fs::remove_dir_all(&project_dir) {
        Ok(()) => true,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                notes.push(format!("workspace: {err}"));
            }
            !workspace.exists()
        }
    };

    // Verify rather than assume.
    let rows_gone = app.state.find_project(&project_id)?.is_none();
    let leases_gone = app.state.leases(&environment_id)?.is_empty();
    let runtime = app.lifecycle.backend.status(&environment_id).await?;
    let runtime_gone = runtime.state == agentify_backends::STATE_ABSENT;

    let verified = rows_gone && leases_gone && runtime_gone && workspace_removed;
    if !verified {
        return Err(LifecycleError::Backend(
            agentify_backends::BackendError::BackendUnavailable {
                detail: format!(
                    "the delete could not be verified: rows_gone={rows_gone} \
                     leases_gone={leases_gone} runtime_gone={runtime_gone} \
                     workspace_gone={workspace_removed}"
                ),
            },
        ));
    }

    Ok(serde_json::json!({
        "project": approval.project_name,
        "projectId": project_id,
        "operation": approval.operation,
        "removed": {
            "rows": rows_gone,
            "runtime": runtime_gone,
            "volumes": runtime_gone,
            "leases": leases_gone,
            "workspace": workspace_removed,
        },
        "verified": verified,
        "notes": notes,
    }))
}

/// Reset an Environment: clear its data and restart it, keeping the Project.
///
/// The fake has no data model yet, so the Environment's runtime is dropped and
/// built again — which is what clears its in-memory data. Ticket 09/10 replace
/// this with real data semantics; the API and the Approval stay the same.
async fn run_reset(
    app: &AppState,
    approval: &ApprovalRecord,
) -> std::result::Result<serde_json::Value, LifecycleError> {
    let environment_id = approval.environment_id.clone().unwrap_or_default();
    let project_id = approval.project_id.clone().unwrap_or_default();
    let record = app
        .state
        .find_project(&project_id)?
        .ok_or_else(|| LifecycleError::NotFound(format!(
            "no Project with id {project_id:?}; the reset cannot run"
        )))?;

    app.lifecycle.stop(&app.state, &record).await?;
    match app.lifecycle.backend.remove(&environment_id, false).await {
        Ok(()) => {}
        Err(agentify_backends::BackendError::NotProvisioned { .. }) => {}
        Err(err) => return Err(LifecycleError::Backend(err)),
    }
    app.state.release_leases(&environment_id)?;
    let status = app.lifecycle.start(&app.state, &record).await?;
    let mut payload = serde_json::to_value(&status).unwrap_or(serde_json::Value::Null);
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "operation".to_string(),
            serde_json::Value::String(approval.operation.clone()),
        );
        object.insert("reset".to_string(), serde_json::Value::Bool(true));
    }
    Ok(payload)
}

// ---- the activity feed -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct ActivityQuery {
    limit: Option<usize>,
    /// Narrow to one Project.
    project: Option<String>,
}

/// `GET /v1/activity`: the audit chain, newest first, with the chain's verdict.
async fn activity(
    State(app): State<Arc<AppState>>,
    Query(query): Query<ActivityQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let project_id = match query.project.as_deref() {
        Some(reference) => match app.state.find_project(reference) {
            Ok(Some(record)) => Some(record.id),
            // An unknown Project is an empty feed, not an error: the feed is a
            // read over what happened, including things that no longer exist.
            Ok(None) => Some(reference.to_string()),
            Err(err) => {
                return problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal",
                    Some(format!("{err:#}")),
                )
            }
        },
        None => None,
    };
    match audit::page(&app.state, limit, project_id.as_deref()) {
        Ok(page) => Json::<ActivityPage>(page).into_response(),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

// ---- secrets ---------------------------------------------------------------

/// A secret as the API reports it. Metadata only, by construction: there is no
/// field a value could be put in, so no read path can leak one.
#[derive(Debug, serde::Serialize)]
struct SecretView {
    name: String,
    /// `generate`, `fromEnv`, or `literal`.
    source: String,
    /// The host variable a `fromEnv` import came from.
    #[serde(rename = "sourceRef", skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    /// Whether the Manifest still references it, which is what decides whether
    /// it can be deleted.
    referenced: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

#[derive(Debug, serde::Serialize)]
struct SecretList {
    secrets: Vec<SecretView>,
}

fn secret_view(record: &ProjectRecord, secret: &SecretRecord) -> SecretView {
    SecretView {
        name: secret.name.clone(),
        source: secret.source_kind.clone(),
        source_ref: secret.source_ref.clone(),
        referenced: projects::is_referenced(record, &secret.name),
        created_at: secret.created_at.clone(),
        updated_at: secret.updated_at.clone(),
    }
}

/// The Project a secrets route names, or a typed `NotFound`.
fn project_of(app: &AppState, reference: &str) -> std::result::Result<ProjectRecord, Response> {
    match app.state.find_project(reference) {
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(problem(
            StatusCode::NOT_FOUND,
            "NotFound",
            Some(format!("no Project named or identified by {reference:?}")),
        )),
        Err(err) => Err(problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        )),
    }
}

/// `GET /v1/projects/{project}/secrets`: names and provenance, never values.
async fn list_secrets(
    State(app): State<Arc<AppState>>,
    Path(project): Path<String>,
) -> Response {
    let record = match project_of(&app, &project) {
        Ok(record) => record,
        Err(response) => return response,
    };
    match app.state.secrets(&record.id) {
        Ok(secrets) => Json(SecretList {
            secrets: secrets
                .iter()
                .map(|secret| secret_view(&record, secret))
                .collect(),
        })
        .into_response(),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct CreateSecretRequest {
    name: Option<String>,
    generate: Option<String>,
    #[serde(rename = "fromEnv")]
    from_env: Option<String>,
    value: Option<String>,
}

/// `POST /v1/projects/{project}/secrets`: an Agent supplies a value, or asks the
/// daemon to mint one. The response is metadata; the value is never echoed.
async fn create_secret(
    State(app): State<Arc<AppState>>,
    Path(project): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let record = match project_of(&app, &project) {
        Ok(record) => record,
        Err(response) => return response,
    };
    let request: CreateSecretRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some(format!("the request body is not a create_secret payload: {err}")),
                Some(String::new()),
            )
        }
    };

    let name = match request.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some("a secret needs a `name`".to_string()),
                Some("name".to_string()),
            )
        }
    };
    if let Err(detail) = secrets::validate_name(&name) {
        return problem_at(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ValidationFailed",
            Some(detail),
            Some("name".to_string()),
        );
    }

    let (source_kind, value) = match (&request.generate, &request.from_env, &request.value) {
        (Some(kind), None, None) => match kind.trim() {
            "password" | "token" => match secrets::generate() {
                Ok(value) => (secrets::SOURCE_GENERATE, value),
                Err(err) => {
                    return problem(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SecretSourceMissing",
                        Some(format!("{err}")),
                    )
                }
            },
            other => {
                return problem_at(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ValidationFailed",
                    Some(format!(
                        "unsupported secret kind {other:?}; use `password` or `token`"
                    )),
                    Some("generate".to_string()),
                )
            }
        },
        (None, None, Some(value)) if !value.is_empty() => (secrets::SOURCE_LITERAL, value.clone()),
        (None, Some(_), None) => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some(
                    "a `fromEnv` secret is imported once, when the Project is created; \
                     declare it in the Manifest instead"
                        .to_string(),
                ),
                Some("fromEnv".to_string()),
            )
        }
        _ => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some("a secret needs exactly one of `generate` or `value`".to_string()),
                Some("generate".to_string()),
            )
        }
    };

    let now = clock::now_rfc3339();
    let secret = SecretRecord {
        project_id: record.id.clone(),
        name: name.clone(),
        source_kind: source_kind.to_string(),
        source_ref: None,
        created_at: now.clone(),
        updated_at: now,
    };
    match app.state.insert_secret(&secret) {
        Ok(true) => {}
        Ok(false) => {
            return problem(
                StatusCode::CONFLICT,
                "AlreadyExists",
                Some(format!(
                    "Project {:?} already has a secret named {name:?}",
                    record.name
                )),
            )
        }
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    }
    if let Err(err) = app.secrets.put(&record.id, &name, &value) {
        let _ = app.state.delete_secret(&record.id, &name);
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "SecretStoreUnavailable",
            Some(format!("{err}")),
        );
    }
    app.forget_redaction(&record.id);

    record_audit(
        &app,
        audit::Append {
            project_id: Some(&record.id),
            operation: "create_secret",
            target: Some(&format!("{}/{name}", record.name)),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..audit::Append::done(AppState::actor(&caller), "create_secret", &request_id)
        },
    );
    (StatusCode::CREATED, Json(secret_view(&record, &secret))).into_response()
}

#[derive(Debug, Deserialize)]
struct UpdateSecretRequest {
    generate: Option<String>,
    value: Option<String>,
}

/// `PATCH /v1/projects/{project}/secrets/{name}`: replace a value. A `fromEnv`
/// import is final — it was imported once, from the host, and rewriting it would
/// make the Project's provenance a lie.
async fn update_secret(
    State(app): State<Arc<AppState>>,
    Path((project, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let record = match project_of(&app, &project) {
        Ok(record) => record,
        Err(response) => return response,
    };
    let existing = match app.state.find_secret(&record.id, &name) {
        Ok(Some(secret)) => secret,
        Ok(None) => return unknown_secret(&record, &name),
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };
    let request: UpdateSecretRequest = if body.is_empty() {
        UpdateSecretRequest {
            generate: None,
            value: None,
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(err) => {
                return problem_at(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ValidationFailed",
                    Some(format!(
                        "the request body is not an update_secret payload: {err}"
                    )),
                    Some(String::new()),
                )
            }
        }
    };

    if existing.source_kind == secrets::SOURCE_FROM_ENV {
        return problem(
            StatusCode::CONFLICT,
            "SecretImmutable",
            Some(format!(
                "the secret {name:?} was imported from the environment variable {} when the \
                 Project was created; an import happens once",
                existing.source_ref.as_deref().unwrap_or("(unknown)")
            )),
        );
    }

    let (source_kind, value) = match (&request.generate, &request.value) {
        (Some(kind), None) => match kind.trim() {
            "password" | "token" => match secrets::generate() {
                Ok(value) => (secrets::SOURCE_GENERATE, value),
                Err(err) => {
                    return problem(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SecretSourceMissing",
                        Some(format!("{err}")),
                    )
                }
            },
            other => {
                return problem_at(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ValidationFailed",
                    Some(format!(
                        "unsupported secret kind {other:?}; use `password` or `token`"
                    )),
                    Some("generate".to_string()),
                )
            }
        },
        (None, Some(value)) if !value.is_empty() => (secrets::SOURCE_LITERAL, value.clone()),
        _ => {
            return problem_at(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ValidationFailed",
                Some("a secret update needs exactly one of `generate` or `value`".to_string()),
                Some("value".to_string()),
            )
        }
    };

    if let Err(err) = app.secrets.put(&record.id, &name, &value) {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "SecretStoreUnavailable",
            Some(format!("{err}")),
        );
    }
    let updated = SecretRecord {
        source_kind: source_kind.to_string(),
        source_ref: None,
        updated_at: clock::now_rfc3339(),
        ..existing
    };
    if let Err(err) = app.state.update_secret(&updated) {
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        );
    }
    app.forget_redaction(&record.id);

    record_audit(
        &app,
        audit::Append {
            project_id: Some(&record.id),
            operation: "update_secret",
            target: Some(&format!("{}/{name}", record.name)),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..audit::Append::done(AppState::actor(&caller), "update_secret", &request_id)
        },
    );
    Json(secret_view(&record, &updated)).into_response()
}

/// `DELETE /v1/projects/{project}/secrets/{name}`: only a secret the Manifest no
/// longer references. Deleting a referenced one would silently break the next
/// start, so it is refused.
async fn delete_secret(
    State(app): State<Arc<AppState>>,
    Path((project, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let record = match project_of(&app, &project) {
        Ok(record) => record,
        Err(response) => return response,
    };
    match app.state.find_secret(&record.id, &name) {
        Ok(Some(_)) => {}
        Ok(None) => return unknown_secret(&record, &name),
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    }
    if projects::is_referenced(&record, &name) {
        return problem(
            StatusCode::CONFLICT,
            "SecretInUse",
            Some(format!(
                "the Manifest of Project {:?} references the secret {name:?}; \
                 remove the reference and create the Project again before deleting it",
                record.name
            )),
        );
    }

    // Metadata first: a value with no row would be unreachable and unforgettable.
    if let Err(err) = app.state.delete_secret(&record.id, &name) {
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        );
    }
    if let Err(err) = app.secrets.delete(&record.id, &name) {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "SecretStoreUnavailable",
            Some(format!("{err}")),
        );
    }
    app.forget_redaction(&record.id);

    record_audit(
        &app,
        audit::Append {
            project_id: Some(&record.id),
            operation: "delete_secret",
            target: Some(&format!("{}/{name}", record.name)),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..audit::Append::done(AppState::actor(&caller), "delete_secret", &request_id)
        },
    );
    Json(serde_json::json!({ "name": name, "deleted": true })).into_response()
}

/// `POST /v1/projects/{project}/secrets/{name}/reveal`: the one path that returns
/// a value, and only to a person who passes the OS gesture. Audited without it.
async fn reveal_secret(
    State(app): State<Arc<AppState>>,
    Path((project, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = request_id(&headers);
    let record = match project_of(&app, &project) {
        Ok(record) => record,
        Err(response) => return response,
    };
    match app.state.find_secret(&record.id, &name) {
        Ok(Some(_)) => {}
        Ok(None) => return unknown_secret(&record, &name),
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    }

    let message = format!(
        "Agentify: reveal the secret \"{name}\" of Project \"{}\"?",
        record.name
    );
    let outcome = ask_for_gesture(&app, message).await;
    let audit = |result: &'static str, code: Option<&'static str>, by: ActorRecord| {
        record_audit(
            &app,
            audit::Append {
                project_id: Some(&record.id),
                operation: "reveal_secret",
                target: Some(&format!("{}/{name}", record.name)),
                result,
                error_code: code,
                duration_ms: Some(started.elapsed().as_millis() as u64),
                ..audit::Append::done(by, "reveal_secret", &request_id)
            },
        );
    };

    let by = match outcome {
        ConsentOutcome::Verified => ActorRecord {
            kind: "human".to_string(),
            id: consent::os_account(),
            token_id: Some(caller.token_id.clone()),
        },
        ConsentOutcome::Declined(detail) => {
            audit(
                "failed",
                Some("ApprovalRequired"),
                AppState::actor(&caller),
            );
            return problem(StatusCode::FORBIDDEN, "ApprovalRequired", Some(detail));
        }
        ConsentOutcome::Unavailable(detail) => {
            audit(
                "failed",
                Some("ConsentUnavailable"),
                AppState::actor(&caller),
            );
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "ConsentUnavailable",
                Some(detail),
            );
        }
    };

    match app.secrets.get(&record.id, &name) {
        Ok(Some(value)) => {
            // The audit records the reveal, who made it, and which secret — never
            // the value.
            audit("succeeded", None, by.clone());
            Json(serde_json::json!({
                "project": record.name,
                "name": name,
                "value": value,
                "revealedAt": clock::now_rfc3339(),
                "actor": { "kind": by.kind, "id": by.id },
            }))
            .into_response()
        }
        Ok(None) => {
            audit("failed", Some("SecretSourceMissing"), by);
            problem(
                StatusCode::CONFLICT,
                "SecretSourceMissing",
                Some(format!(
                    "the secret {name:?} has no value in the credential manager"
                )),
            )
        }
        Err(err) => {
            audit("failed", Some("SecretStoreUnavailable"), by);
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "SecretStoreUnavailable",
                Some(format!("{err}")),
            )
        }
    }
}

fn unknown_secret(record: &ProjectRecord, name: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "NotFound",
        Some(format!(
            "Project {:?} has no secret named {name:?}",
            record.name
        )),
    )
}

/// Ask the injected verifier for the interactive gesture, off the async workers.
async fn ask_for_gesture(app: &AppState, message: String) -> ConsentOutcome {
    let verifier = app.consent.clone();
    match tokio::task::spawn_blocking(move || verifier.verify(&message)).await {
        Ok(outcome) => outcome,
        Err(err) => ConsentOutcome::Unavailable(format!("the consent prompt failed: {err}")),
    }
}

// ---- redaction -------------------------------------------------------------

/// The Project a path names, if it names one: `/v1/projects/{project}/...`.
fn project_in_path(path: &str) -> Option<String> {
    let mut segments = path.split('/');
    if segments.next() != Some("") || segments.next() != Some("v1") {
        return None;
    }
    if segments.next() != Some("projects") {
        return None;
    }
    let project = segments.next()?;
    if project.is_empty() {
        None
    } else {
        Some(project.to_string())
    }
}

/// Strip a Project's known secret values — and their percent-encoded forms — out
/// of every JSON response body before it leaves the daemon.
///
/// One seam rather than a rule every handler has to remember: status, errors,
/// exec output, activity, and anything added later all pass through here. The
/// human reveal is the single exception, and it is named: a body is only left
/// alone when the path ends in `/reveal`, where a value is the point.
async fn redact_plaintext(
    State(app): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let reveal = path.ends_with("/reveal");
    let reference = project_in_path(&path);
    let response = next.run(request).await;
    if reveal {
        return response;
    }
    let Some(reference) = reference else {
        return response;
    };
    let Ok(Some(record)) = app.state.find_project(&reference) else {
        return response;
    };
    let redactor = app.redactor(&record.id);
    if redactor.is_empty() {
        return response;
    }
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json") || value.starts_with("application/problem+json"))
        .unwrap_or(false);
    // An SSE stream is not buffered: its lines are redacted where they are made.
    if !json {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("agentifyd: reading a response body to redact it failed: {err}");
            return Response::from_parts(parts, Body::empty());
        }
    };
    let text = String::from_utf8_lossy(&bytes).to_string();
    let redacted = redactor.redact(&text);
    let mut response = Response::from_parts(parts, Body::from(redacted));
    response.headers_mut().remove(header::CONTENT_LENGTH);
    response
}

// ---- audit helpers ---------------------------------------------------------

/// The id a mutation is recorded under, so a support question can be traced
/// from one HTTP call to one audit entry.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Append one audit entry, with the Project's known secret values stripped out.
///
/// The audit never records a value by design; this is the guarantee, not the
/// hope: an operation whose target or error text happened to contain one is
/// masked before it is hashed and stored.
fn record_audit(app: &AppState, entry: audit::Append<'_>) {
    let redactor = entry.project_id.map(|project_id| app.redactor(project_id));
    let target;
    let operation;
    let entry = match &redactor {
        Some(redactor) if !redactor.is_empty() => {
            target = redactor.redact_opt(entry.target);
            operation = redactor.redact(entry.operation);
            audit::Append {
                target: target.as_deref(),
                operation: &operation,
                ..entry
            }
        }
        _ => entry,
    };
    if let Err(err) = audit::append(&app.state, entry) {
        eprintln!("agentifyd: recording an audit entry failed: {err:#}");
    }
}

/// Record a mutation that was refused before it changed anything.
fn record_failure(
    app: &AppState,
    caller: &Caller,
    request_id: &str,
    operation: &str,
    target: Option<&str>,
    code: &str,
    started: std::time::Instant,
) {
    record_audit(
        app,
        audit::Append {
            project_id: None,
            operation,
            target,
            params_hash: None,
            approval_id: None,
            result: "failed",
            error_code: Some(code),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            request_id,
            actor: AppState::actor(caller),
        },
    );
}

/// The Project and Environment a route named, or a typed `NotFound`.
fn resolve(
    state: &StateStore,
    project: &str,
    environment: &str,
) -> Result<ProjectRecord, LifecycleError> {
    let record = state
        .find_project(project)
        .map_err(LifecycleError::Store)?
        .ok_or_else(|| {
            LifecycleError::NotFound(format!("no Project named or identified by {project:?}"))
        })?;
    if environment != DEV_ENVIRONMENT {
        return Err(LifecycleError::NotFound(format!(
            "Project {:?} has no Environment named {environment:?}; v0 recognises only {DEV_ENVIRONMENT:?}",
            record.name
        )));
    }
    Ok(record)
}

// ---- operations ------------------------------------------------------------

/// `GET /v1/operations/{id}`.
async fn get_operation(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match app.state.operation(&id) {
        Ok(Some(record)) => Json(operation_view(&record)).into_response(),
        Ok(None) => problem(
            StatusCode::NOT_FOUND,
            "NotFound",
            Some(format!("no operation with id {id:?}")),
        ),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            Some(format!("{err:#}")),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct WaitQuery {
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
}

/// `GET /v1/operations/{id}/wait`: block until the operation finishes, or until
/// the wait itself is over. The returned operation says which happened.
async fn wait_operation(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Response {
    let wait = query
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(app.operation_timeout);
    let deadline = Instant::now() + wait;
    loop {
        let record = match app.state.operation(&id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return problem(
                    StatusCode::NOT_FOUND,
                    "NotFound",
                    Some(format!("no operation with id {id:?}")),
                )
            }
            Err(err) => {
                return problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal",
                    Some(format!("{err:#}")),
                )
            }
        };
        let view = operation_view(&record);
        if view.is_terminal() {
            return Json(view).into_response();
        }
        if Instant::now() >= deadline {
            return Json(view).into_response();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn operation_view(record: &OperationRecord) -> OperationView {
    OperationView {
        id: record.id.clone(),
        kind: record.kind.clone(),
        state: record.state.clone(),
        created_at: record.created_at.clone(),
        finished_at: record.finished_at.clone(),
        duration_ms: record
            .finished_at
            .as_deref()
            .and_then(|finished| clock::millis_between(&record.created_at, finished)),
        error_code: record.error_code.clone(),
        error_detail: record.error_detail.clone(),
        result: record
            .result_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
    }
}

// ---- idempotency -----------------------------------------------------------

enum Replay {
    /// No usable record: run the mutation.
    None,
    /// A live record: answer with exactly the original response.
    Response(Outcome),
    /// The key was used for a different request: refuse rather than guess.
    Conflict,
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get(agentify_proto::IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A stable hash of the request a key was used for.
fn fingerprint(method: &str, path: &str, body: &[u8]) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(method.as_bytes());
    material.push(b'\n');
    material.extend_from_slice(path.as_bytes());
    material.push(b'\n');
    material.extend_from_slice(body);
    auth::sha256_hex(&String::from_utf8_lossy(&material))
}

fn replay(
    app: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    fingerprint: &str,
) -> Replay {
    let Some(key) = idempotency_key(headers) else {
        return Replay::None;
    };
    let record = match app.state.idempotency(&key) {
        Ok(Some(record)) => record,
        Ok(None) => return Replay::None,
        Err(_) => return Replay::None,
    };
    if record.request_hash != fingerprint || record.method != method || record.path != path {
        return Replay::Conflict;
    }
    if clock::is_older_than(&record.created_at, IDEMPOTENCY_TTL) {
        return Replay::None;
    }
    Replay::Response(Outcome {
        status: StatusCode::from_u16(record.status).unwrap_or(StatusCode::OK),
        body: record.body,
    })
}

fn remember(
    app: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    fingerprint: &str,
    outcome: &Outcome,
) {
    let Some(key) = idempotency_key(headers) else {
        return;
    };
    let record = IdempotencyRecord {
        key,
        method: method.to_string(),
        path: path.to_string(),
        request_hash: fingerprint.to_string(),
        status: outcome.status.as_u16(),
        body: outcome.body.clone(),
        created_at: clock::now_rfc3339(),
    };
    if let Err(err) = app.state.put_idempotency(&record) {
        eprintln!("agentifyd: remembering an idempotency key failed: {err:#}");
    }
    // Keep the table from growing without bound.
    let cutoff = clock::rfc3339_in_days(-1);
    let _ = app.state.purge_idempotency_before(&cutoff);
}

// ---- auth ------------------------------------------------------------------

async fn require_agent_token(
    State(app): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let Some(presented) = presented else {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            Some("callers must present the Agent token from the discovery file".to_string()),
        );
    };

    let hash = auth::sha256_hex(presented);
    let record = match app.state.find_active_token(&hash) {
        Ok(record) => record,
        Err(err) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                Some(format!("{err:#}")),
            )
        }
    };

    let Some(record) = record else {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            Some("the presented token is unknown or revoked".to_string()),
        );
    };

    if clock::is_expired(&record.expires_at) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            Some("the presented token has expired".to_string()),
        );
    }

    request.extensions_mut().insert(Caller {
        token_id: record.id,
        scopes: record.scopes,
    });
    next.run(request).await
}

async fn not_found() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "NotFound",
        Some("no such route".to_string()),
    )
}

fn problem(status: StatusCode, code: &str, detail: Option<String>) -> Response {
    problem_at(status, code, detail, None)
}

fn problem_at(
    status: StatusCode,
    code: &str,
    detail: Option<String>,
    path: Option<String>,
) -> Response {
    let title = status.canonical_reason().unwrap_or("Error").to_string();
    let mut body = Problem::new(status.as_u16(), code, &title, detail);
    body.path = path;
    let body = serde_json::to_vec(&body).unwrap_or_default();
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        body,
    )
        .into_response()
}

//! The loopback HTTP API.
//!
//! Everything that talks to the daemon talks to this API: the desktop shell,
//! the CLI, and the MCP shim are all clients (ADR-0003). It binds to
//! `127.0.0.1` only and every route except the liveness probe requires the
//! Agent token published in the discovery file.
//!
//! Two rules shape the mutating routes:
//!
//! * each mutation runs as an **operation** with an id, and the HTTP call blocks
//!   for at most `AGENTIFY_OPERATION_TIMEOUT_MS`. A timeout answers `504` with
//!   the operation id, and `GET /v1/operations/{id}/wait` finishes the job.
//! * mutations are serialised per Environment, so a second `start` cannot race
//!   the first.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentify_backends::LogOptions;
use agentify_proto::{
    Actor, Capabilities, CreateProjectRequest, DaemonInfo, Discovery, ExecRequest, ExecResponse,
    LogsResponse, OperationView, Problem, ProjectList, StatsResponse, Whoami, API_VERSION,
    DEV_ENVIRONMENT,
};
use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, OwnedMutexGuard};
use tokio::time::Instant;

use crate::auth;
use crate::backends;
use crate::clock;
use crate::config::Config;
use crate::discovery;
use crate::lifecycle::{self, Lifecycle, LifecycleError};
use crate::projects::{self, CreateError};
use crate::state::{IdempotencyRecord, OperationRecord, ProjectRecord, State as StateStore};

/// Version of this daemon build.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a replayed mutation stays replayable.
pub const IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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
    /// One async mutex per Environment: mutations are serialised, reads are not.
    env_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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

    let app = router(Arc::new(state));
    let graceful = async move {
        let shutdown = shutdown;
        tokio::select! {
            _ = shutdown => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    };

    let served = axum::serve(listener, app)
        .with_graceful_shutdown(graceful)
        .await
        .context("serving the loopback API");

    discovery::remove(&config);
    served
}

async fn prepare(config: &Config) -> Result<(AppState, TcpListener, Discovery)> {
    crate::config::ensure_data_root(&config.data_root)?;

    let state = StateStore::open(&config.data_root)?;
    let instance = state.instance()?;
    let token = state.issue_agent_token()?;

    let backend = backends::select(
        &config.backend,
        config.backend_script.as_deref(),
        &config.data_root,
    )?;

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
        ingress,
        probe_timeout: config.probe_timeout,
        exec_timeout: config.exec_timeout,
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
        env_locks: Mutex::new(HashMap::new()),
    };
    Ok((state, listener, discovery))
}

fn router(state: Arc<AppState>) -> Router {
    let environments = "/v1/projects/{project}/environments/{environment}";
    let protected = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/events", get(events))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{project}", get(get_project))
        .route(
            &format!("{environments}/start"),
            post(start_environment),
        )
        .route(&format!("{environments}/stop"), post(stop_environment))
        .route(&format!("{environments}/status"), get(environment_status))
        .route(
            &format!("{environments}/resources/{{resource}}/exec"),
            post(exec_in_resource),
        )
        .route(&format!("{environments}/logs"), get(environment_logs))
        .route(&format!("{environments}/stats"), get(environment_stats))
        .route("/v1/operations/{id}", get(get_operation))
        .route("/v1/operations/{id}/wait", get(wait_operation))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_agent_token,
        ));

    Router::new()
        .route("/v1/healthz", get(healthz))
        .merge(protected)
        .fallback(not_found)
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "daemon_version": DAEMON_VERSION,
        "api_version": API_VERSION,
    }))
}

async fn whoami(State(app): State<Arc<AppState>>, Extension(caller): Extension<Caller>) -> Json<Whoami> {
    let backend = app.lifecycle.backend_ref();
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
    })
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
async fn create_project(State(app): State<Arc<AppState>>, body: Bytes) -> Response {
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

    match projects::create(&app.state, &app.data_root, request) {
        Ok(project) => (StatusCode::CREATED, Json(project)).into_response(),
        Err(CreateError::Validation { path, detail }) => problem_at(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ValidationFailed",
            Some(detail),
            Some(path),
        ),
        Err(CreateError::SourceMissing(detail)) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SourceMissing",
            Some(detail),
        ),
        Err(CreateError::AlreadyExists(detail)) => problem(
            StatusCode::CONFLICT,
            "AlreadyExists",
            Some(detail),
        ),
        Err(CreateError::Store(err)) => {
            problem(StatusCode::INTERNAL_SERVER_ERROR, "Internal", Some(format!("{err:#}")))
        }
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
    headers: HeaderMap,
) -> Response {
    mutate(app, project, environment, Mutation::Start, headers).await
}

async fn stop_environment(
    State(app): State<Arc<AppState>>,
    Path((project, environment)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    mutate(app, project, environment, Mutation::Stop, headers).await
}

/// Run a lifecycle mutation as an operation with an id.
async fn mutate(
    app: Arc<AppState>,
    project: String,
    environment: String,
    mutation: Mutation,
    headers: HeaderMap,
) -> Response {
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
    tokio::spawn(async move {
        let lock = worker.env_lock(&environment_id);
        let guard: OwnedMutexGuard<()> = lock.lock_owned().await;
        let result = match mutation {
            Mutation::Start => worker.lifecycle.start(&worker.state, &worker_record).await,
            Mutation::Stop => worker.lifecycle.stop(&worker.state, &worker_record).await,
        };
        drop(guard);

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
    body: Bytes,
) -> Response {
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
    match app
        .lifecycle
        .exec(&app.state, &record, &resource, &request)
        .await
    {
        Ok(response) => Json::<ExecResponse>(response).into_response(),
        Err(err) => Outcome::lifecycle(err).into_response(),
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

    let events = stream.map(|item| {
        let event = match item {
            Ok(line) => Event::default()
                .event("log")
                .id(line.seq.to_string())
                .data(
                    serde_json::to_string(&lifecycle::log_line_view(&line)).unwrap_or_default(),
                ),
            Err(err) => Event::default().event("error").data(
                serde_json::json!({ "code": err.code(), "detail": err.to_string() }).to_string(),
            ),
        };
        Ok::<Event, Infallible>(event)
    });
    Sse::new(events).into_response()
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

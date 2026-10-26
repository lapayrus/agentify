//! The loopback HTTP API.
//!
//! Everything that talks to the daemon talks to this API: the desktop shell,
//! the CLI, and the MCP shim are all clients (ADR-0003). It binds to
//! `127.0.0.1` only and every route except the liveness probe requires the
//! Agent token published in the discovery file.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use agentify_proto::{Actor, Capabilities, DaemonInfo, Discovery, Problem, Whoami, API_VERSION};
use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::auth;
use crate::clock;
use crate::config::Config;
use crate::discovery;
use crate::state::State as StateStore;

/// Version of this daemon build.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identity the auth middleware attaches to an authenticated request.
#[derive(Debug, Clone)]
pub struct Caller {
    pub token_id: String,
    pub scopes: Vec<String>,
}

struct AppState {
    state: StateStore,
    instance_id: String,
    started_at: String,
    heartbeat: Duration,
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

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding {}", config.bind_addr))?;
    let port = listener.local_addr()?.port();

    let discovery = discovery::write(config, DAEMON_VERSION, &token.token, port)?;

    let state = AppState {
        state,
        instance_id: instance.id,
        started_at: clock::now_rfc3339(),
        heartbeat: config.heartbeat,
    };
    Ok((state, listener, discovery))
}

fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/events", get(events))
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
    Json(Whoami {
        // The walking skeleton has no Projects, so no caller is bound to one.
        project: None,
        scopes: caller.scopes,
        daemon_version: DAEMON_VERSION.to_string(),
        api_version: API_VERSION.to_string(),
        capabilities: Capabilities::v0_skeleton(),
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
    let title = status.canonical_reason().unwrap_or("Error").to_string();
    let body = Problem::new(status.as_u16(), code, &title, detail);
    let body = serde_json::to_vec(&body).unwrap_or_default();
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        body,
    )
        .into_response()
}

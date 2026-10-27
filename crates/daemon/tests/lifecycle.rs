//! The Environment lifecycle, at the public API seam.
//!
//! Every test boots the real daemon against a temporary data root and talks to
//! it the way an Agent or the UI does: read the discovery file, then speak HTTP
//! or open an SSE stream. Nothing here reaches into daemon internals, and the
//! only substitute is the scripted fake Backend — the seam the spec agreed on.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentify_daemon::ports::PortRange;
use agentify_daemon::Config;
use serde_json::{json, Value};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The value the bundled fixture imports for `api-token`.
const FIXTURE_API_TOKEN: &str = "fixture-api-token-from-the-host";

/// A daemon booted by a test, shut down on drop.
struct Daemon {
    inner: Option<agentify_daemon::RunningDaemon>,
}

impl Daemon {
    fn boot(home: &Path) -> Daemon {
        Daemon::boot_with(Config::new(home))
    }

    fn boot_with(config: Config) -> Daemon {
        // The bundled fixture imports one secret from the host environment. A
        // test supplies it explicitly, so the suite does not depend on how the
        // machine's shell is set up.
        let config = config.with_secret_env([("HELLO_API_TOKEN", FIXTURE_API_TOKEN)]);
        Daemon {
            inner: Some(agentify_daemon::start_in_background(config).expect("boot daemon")),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.as_ref().expect("running").base_url())
    }

    fn token(&self) -> String {
        self.inner.as_ref().expect("running").token().to_string()
    }

    fn get(&self, path: &str) -> (u16, Value) {
        request("GET", &self.url(path), &self.token(), None, None)
    }

    fn post(&self, path: &str) -> (u16, Value) {
        request("POST", &self.url(path), &self.token(), Some("{}"), None)
    }

    fn post_with(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> (u16, Value) {
        request(
            "POST",
            &self.url(path),
            &self.token(),
            Some(&body.to_string()),
            Some(headers),
        )
    }

    fn shutdown(&mut self) {
        if let Some(mut daemon) = self.inner.take() {
            daemon.shutdown().expect("stop daemon");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn request(
    method: &str,
    url: &str,
    token: &str,
    body: Option<&str>,
    headers: Option<&[(&str, &str)]>,
) -> (u16, Value) {
    let (status, text) = raw(method, url, token, body, headers);
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

fn raw(
    method: &str,
    url: &str,
    token: &str,
    body: Option<&str>,
    headers: Option<&[(&str, &str)]>,
) -> (u16, String) {
    let mut call = if method == "GET" {
        ureq::get(url)
    } else {
        ureq::post(url)
    };
    call = call
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(20));
    if body.is_some() {
        call = call.set("Content-Type", "application/json");
    }
    for (name, value) in headers.unwrap_or(&[]) {
        call = call.set(name, value);
    }
    let result = match body {
        Some(body) => call.send_string(body),
        None => call.call(),
    };
    match result {
        Ok(response) => (
            response.status(),
            response.into_string().unwrap_or_default(),
        ),
        Err(ureq::Error::Status(status, response)) => {
            (status, response.into_string().unwrap_or_default())
        }
        Err(err) => panic!("transport error for {url}: {err}"),
    }
}

/// Create the bundled fixture Project. The Manifest declares `web` and `db`.
fn create_fixture(daemon: &Daemon) {
    let (status, body) = daemon.post_with(
        "/v1/projects",
        &json!({ "fixture": "hello-pg" }),
        &[],
    );
    assert_eq!(status, 201, "create the fixture Project: {body}");
}

fn start(daemon: &Daemon, project: &str) -> (u16, Value) {
    daemon.post(&format!("/v1/projects/{project}/environments/dev/start"))
}

fn stop(daemon: &Daemon, project: &str) -> (u16, Value) {
    daemon.post(&format!("/v1/projects/{project}/environments/dev/stop"))
}

fn status(daemon: &Daemon, project: &str) -> (u16, Value) {
    daemon.get(&format!("/v1/projects/{project}/environments/dev/status"))
}

/// The host ports a status snapshot reports.
fn leased_ports(status: &Value) -> Vec<u16> {
    let mut ports: Vec<u16> = status["resources"]
        .as_array()
        .map(|resources| {
            resources
                .iter()
                .flat_map(|resource| {
                    resource["ports"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|port| port["hostPort"].as_u64())
                .map(|port| port as u16)
                .collect()
        })
        .unwrap_or_default();
    ports.sort_unstable();
    ports
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    listener.local_addr().expect("addr").port()
}

#[test]
fn whoami_reports_the_active_backend_and_its_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, whoami) = daemon.get("/v1/whoami");
    assert_eq!(status, 200);
    assert_eq!(whoami["backend"]["name"], "fake", "{whoami}");
    assert_eq!(whoami["backend"]["platform"], "scripted");
    assert_eq!(
        whoami["capabilities"]["logSince"], true,
        "capabilities now describe the active Backend: {whoami}"
    );
    assert_eq!(
        whoami["capabilities"]["checkpoint"], false,
        "the fake cannot checkpoint, and says so: {whoami}"
    );
    assert_eq!(whoami["capabilities"]["events"], true);
}

#[test]
fn start_status_and_stop_report_running_then_stopped() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    // Nothing has run yet.
    let (status_code, before) = status(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "{before}");
    assert_eq!(before["environment"]["state"], "stopped");
    assert_eq!(before["health"], "unknown");
    assert_eq!(before["resources"].as_array().expect("resources").len(), 2);

    // Start blocks until every Resource is up.
    let (status_code, started) = start(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "{started}");
    assert_eq!(started["environment"]["state"], "running");
    assert_eq!(started["health"], "healthy");
    assert!(started["operationId"].is_string(), "a mutation has an id: {started}");
    let web = started["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|resource| resource["name"] == "web")
        .expect("web");
    assert_eq!(web["state"], "running");
    assert_eq!(web["health"], "healthy");
    assert_eq!(web["ports"][0]["guestPort"], 3000, "{web}");
    assert_eq!(web["ports"][0]["hostPort"].as_u64().is_some(), true);

    // The Project payload the rail reads agrees.
    let (_, project) = daemon.get("/v1/projects/hello-pg");
    assert_eq!(project["environment"]["state"], "running", "{project}");

    // The leased ports really are bound: that is what health means.
    for port in leased_ports(&started) {
        assert!(port_open(port), "leased port {port} answers a probe");
    }

    let (status_code, stopped) = stop(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "{stopped}");
    assert_eq!(stopped["environment"]["state"], "stopped");
    assert_eq!(stopped["health"], "unknown");

    let (_, after) = status(&daemon, "hello-pg");
    assert_eq!(after["environment"]["state"], "stopped");
    for port in leased_ports(&started) {
        assert!(!port_open(port), "stop released port {port}");
    }

    // Stopping again is a no-op, not an error.
    let (again, _) = stop(&daemon, "hello-pg");
    assert_eq!(again, 200, "stop is idempotent");
}

#[test]
fn start_is_idempotent_and_reuses_the_same_leases() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    let (_, first) = start(&daemon, "hello-pg");
    let ports = leased_ports(&first);
    assert_eq!(ports.len(), 1, "web:3000 is leased: {first}");

    let (status_code, second) = start(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "start is idempotent: {second}");
    assert_eq!(second["environment"]["state"], "running");
    assert_eq!(
        leased_ports(&second),
        ports,
        "a second start converges on the same leases instead of moving URLs"
    );
    assert_eq!(
        second["resources"].as_array().expect("resources").len(),
        2,
        "no duplicate Resources"
    );
}

#[test]
fn a_lease_is_released_on_stop_and_reacquired_on_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    let (_, started) = start(&daemon, "hello-pg");
    let ports = leased_ports(&started);
    assert!(!ports.is_empty());

    stop(&daemon, "hello-pg");
    for port in &ports {
        assert!(!port_open(*port), "port {port} is released on stop");
    }

    let (_, restarted) = start(&daemon, "hello-pg");
    assert_eq!(restarted["environment"]["state"], "running");
    assert_eq!(
        leased_ports(&restarted),
        ports,
        "start reacquires the same host ports"
    );
}

#[test]
fn a_port_conflict_is_a_typed_port_unavailable() {
    let temp = tempfile::tempdir().expect("temp dir");
    // One leaseable port for the whole machine: the second Project cannot have
    // one, and must be told so instead of failing obscurely.
    let port = free_port();
    let config = Config::new(temp.path()).with_lease_range(PortRange::new(port, port));
    let daemon = Daemon::boot_with(config);

    let one_port = |project: &str| {
        format!(
            "version: 1\nproject: {project}\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports:\n      - {{ port: 3000 }}\n"
        )
    };

    let (created, first) = daemon.post_with(
        "/v1/projects",
        &json!({ "manifest": one_port("first") }),
        &[],
    );
    assert_eq!(created, 201, "{first}");
    let holder = first["environment"]["id"]
        .as_str()
        .expect("the first Environment id")
        .to_string();

    let (status_code, started) = start(&daemon, "first");
    assert_eq!(status_code, 200, "{started}");
    assert_eq!(leased_ports(&started), vec![port]);

    let (created, _) = daemon.post_with(
        "/v1/projects",
        &json!({ "manifest": one_port("second") }),
        &[],
    );
    assert_eq!(created, 201);

    let (status_code, failure) = start(&daemon, "second");
    assert_eq!(status_code, 409, "a lease conflict is a 409: {failure}");
    assert_eq!(failure["code"], "PortUnavailable", "{failure}");
    assert!(
        failure["detail"]
            .as_str()
            .unwrap_or_default()
            .contains(&holder),
        "the failure names the Environment holding the port ({holder}): {failure}"
    );

    // The Environment that failed is marked failed, not left claiming to start.
    let (_, after) = status(&daemon, "second");
    assert_eq!(after["environment"]["state"], "failed", "{after}");
}

#[test]
fn health_is_decided_by_the_daemon_and_reports_degraded() {
    let temp = tempfile::tempdir().expect("temp dir");
    // The fake never binds `web`'s port, so the daemon's own probe fails.
    let config = Config::new(temp.path()).with_backend_script(r#"{"unbound_resources": ["web"]}"#);
    let daemon = Daemon::boot_with(config);
    create_fixture(&daemon);

    let (status_code, started) = start(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "{started}");
    assert_eq!(
        started["environment"]["state"], "degraded",
        "a bound port and a closed port is degraded: {started}"
    );
    assert_eq!(started["health"], "unhealthy");

    let resources = started["resources"].as_array().expect("resources");
    let web = resources.iter().find(|r| r["name"] == "web").expect("web");
    let db = resources.iter().find(|r| r["name"] == "db").expect("db");
    assert_eq!(web["health"], "unhealthy", "{web}");
    assert_eq!(
        db["health"], "unknown",
        "a database has no host port to probe: {db}"
    );
    assert_eq!(db["state"], "running", "{db}");

    // The same answer on a read, because health is observed, not remembered.
    let (_, again) = status(&daemon, "hello-pg");
    assert_eq!(again["environment"]["state"], "degraded");
}

#[test]
fn exec_returns_stdout_stderr_and_an_exit_code() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");

    let (status_code, echo) = daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/resources/web/exec",
        &json!({ "command": ["echo", "hello", "agent"] }),
        &[],
    );
    assert_eq!(status_code, 200, "{echo}");
    assert_eq!(echo["stdout"], "hello agent\n");
    assert_eq!(echo["stderr"], "");
    assert_eq!(echo["exitCode"], 0);
    assert_eq!(echo["resource"], "web");
    assert!(echo["durationMs"].as_u64().is_some());

    // A failing command is a successful call that reports the exit code.
    let (status_code, failed) = daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/resources/web/exec",
        &json!({ "command": ["fail", "3"] }),
        &[],
    );
    assert_eq!(status_code, 200, "{failed}");
    assert_eq!(failed["exitCode"], 3);
    assert!(
        failed["stderr"].as_str().unwrap_or_default().contains("failed"),
        "{failed}"
    );

    // An unknown Resource is a typed 404, not a 500.
    let (status_code, missing) = daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/resources/nope/exec",
        &json!({ "command": ["echo", "hi"] }),
        &[],
    );
    assert_eq!(status_code, 404, "{missing}");
    assert_eq!(missing["code"], "NotFound");

    // A stopped Environment cannot run a command, and says why.
    stop(&daemon, "hello-pg");
    let (status_code, stopped) = daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/resources/web/exec",
        &json!({ "command": ["echo", "hi"] }),
        &[],
    );
    assert_eq!(status_code, 409, "{stopped}");
    assert_eq!(stopped["code"], "NotProvisioned");
}

#[test]
fn logs_support_a_bounded_tail_since_and_following() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");

    let (status_code, logs) = daemon.get("/v1/projects/hello-pg/environments/dev/logs?tail=500");
    assert_eq!(status_code, 200, "{logs}");
    let lines = logs["lines"].as_array().expect("lines");
    assert!(!lines.is_empty(), "starting a Project produces logs: {logs}");
    assert!(
        lines.iter().any(|line| line["line"]
            .as_str()
            .unwrap_or_default()
            .contains("listening on 127.0.0.1")),
        "the fake reports the port it bound: {logs}"
    );
    let next = logs["nextSince"].as_str().expect("a cursor").to_string();

    // `since` continues exactly where the cursor was.
    let (_, following) = daemon.get(&format!(
        "/v1/projects/hello-pg/environments/dev/logs?since={next}"
    ));
    assert_eq!(
        following["lines"].as_array().expect("lines").len(),
        0,
        "nothing new yet: {following}"
    );

    // A bounded tail keeps only the newest lines.
    let (_, tail) = daemon.get("/v1/projects/hello-pg/environments/dev/logs?tail=1");
    assert_eq!(tail["lines"].as_array().expect("lines").len(), 1, "{tail}");

    // Filtering by Resource works.
    let (_, by_resource) =
        daemon.get("/v1/projects/hello-pg/environments/dev/logs?tail=500&resource=web");
    for line in by_resource["lines"].as_array().expect("lines") {
        assert_eq!(line["resource"], "web", "{by_resource}");
    }

    // `follow=1` is an SSE stream.
    let response = ureq::get(&daemon.url(
        "/v1/projects/hello-pg/environments/dev/logs?follow=1&tail=1",
    ))
    .set("Authorization", &format!("Bearer {}", daemon.token()))
    .timeout(Duration::from_secs(10))
    .call()
    .expect("open the log stream");
    assert!(
        response
            .header("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "follow is SSE, got {:?}",
        response.header("content-type")
    );
    let mut reader = BufReader::new(response.into_reader());
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.contains("event: log") {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read the log stream");
        assert!(read > 0, "the stream closed before a log line: {seen:?}");
        assert!(Instant::now() < deadline, "no log line within 10s");
        seen.push_str(&line);
    }
    drop(reader);

    // A new exec appears on a following stream: prove it with a fresh read.
    daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/resources/web/exec",
        &json!({ "command": ["echo", "after-follow"] }),
        &[],
    );
    let (_, after) = daemon.get("/v1/projects/hello-pg/environments/dev/logs?tail=50");
    assert!(
        after["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .any(|line| line["line"].as_str().unwrap_or_default().contains("after-follow")),
        "exec output is logged: {after}"
    );
}

#[test]
fn stats_report_environment_level_figures() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    let (status_code, stopped) = daemon.get("/v1/projects/hello-pg/environments/dev/stats");
    assert_eq!(status_code, 200, "{stopped}");
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["memoryBytes"], 0);
    assert_eq!(stopped["cpu"], "0%");

    start(&daemon, "hello-pg");
    let (status_code, running) = daemon.get("/v1/projects/hello-pg/environments/dev/stats");
    assert_eq!(status_code, 200, "{running}");
    assert_eq!(running["state"], "running");
    assert!(running["memoryBytes"].as_u64().unwrap_or(0) > 0, "{running}");
    assert!(running["diskBytes"].as_u64().unwrap_or(0) > 0, "{running}");
    assert_eq!(running["memory"], "96 MB", "{running}");
    assert_eq!(running["cpu"], "2.5%", "{running}");
    assert!(!running["resources"].as_array().expect("resources").is_empty());
}

#[test]
fn an_idempotency_key_replays_the_original_response_without_running_again() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    let key = [("Idempotency-Key", "start-once")];
    let path = "/v1/projects/hello-pg/environments/dev/start";
    let (status_code, original) = daemon.post_with(path, &json!({}), &key);
    assert_eq!(status_code, 200, "{original}");
    assert_eq!(original["environment"]["state"], "running");
    let operation_id = original["operationId"].clone();

    // Change the world: stop the Environment.
    stop(&daemon, "hello-pg");
    let (_, after_stop) = status(&daemon, "hello-pg");
    assert_eq!(after_stop["environment"]["state"], "stopped");

    // The same key replays the first response verbatim — it does not start.
    let (status_code, replayed) = daemon.post_with(path, &json!({}), &key);
    assert_eq!(status_code, 200);
    assert_eq!(replayed, original, "the response is byte-for-byte the original");
    assert_eq!(replayed["operationId"], operation_id);
    let (_, still_stopped) = status(&daemon, "hello-pg");
    assert_eq!(
        still_stopped["environment"]["state"], "stopped",
        "a replay must not run the mutation again"
    );

    // Reusing the key for a different request is refused, not replayed.
    let (status_code, conflict) = daemon.post_with(
        "/v1/projects/hello-pg/environments/dev/stop",
        &json!({}),
        &key,
    );
    assert_eq!(status_code, 422, "{conflict}");
    assert_eq!(conflict["code"], "ValidationFailed");
}

#[test]
fn a_mutation_that_exceeds_its_timeout_returns_an_operation_id() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config::new(temp.path())
        .with_operation_timeout(Duration::from_millis(50))
        .with_backend_script(r#"{"start_delay_ms": 600}"#);
    let daemon = Daemon::boot_with(config);
    create_fixture(&daemon);

    let (status_code, timeout) =
        daemon.post("/v1/projects/hello-pg/environments/dev/start");
    assert_eq!(status_code, 504, "{timeout}");
    assert_eq!(timeout["code"], "Timeout");
    let operation_id = timeout["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("the timeout carries the operation id: {timeout}"))
        .to_string();

    // While it runs, the Environment is honestly reported as starting.
    let (_, in_flight) = status(&daemon, "hello-pg");
    assert_eq!(
        in_flight["environment"]["state"], "starting",
        "a start in progress is starting, not degraded: {in_flight}"
    );

    // The operation is visible and still running.
    let (status_code, operation) = daemon.get(&format!("/v1/operations/{operation_id}"));
    assert_eq!(status_code, 200, "{operation}");
    assert_eq!(operation["kind"], "start");
    assert!(
        operation["state"] == "running" || operation["state"] == "succeeded",
        "{operation}"
    );

    // ...and wait finishes it.
    let (status_code, finished) =
        daemon.get(&format!("/v1/operations/{operation_id}/wait?timeoutMs=10000"));
    assert_eq!(status_code, 200, "{finished}");
    assert_eq!(finished["state"], "succeeded", "{finished}");
    assert_eq!(
        finished["result"]["environment"]["state"], "running",
        "the waiter receives the result: {finished}"
    );

    let (_, after) = status(&daemon, "hello-pg");
    assert_eq!(after["environment"]["state"], "running");
}

#[test]
fn unknown_projects_environments_and_operations_are_typed_404s() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    create_fixture(&daemon);

    let (status_code, missing_project) = start(&daemon, "nope");
    assert_eq!(status_code, 404, "{missing_project}");
    assert_eq!(missing_project["code"], "NotFound");

    let (status_code, missing_environment) = daemon.post(
        "/v1/projects/hello-pg/environments/staging/start",
    );
    assert_eq!(status_code, 404, "v0 recognises only dev: {missing_environment}");
    assert_eq!(missing_environment["code"], "NotFound");

    let (status_code, missing_operation) = daemon.get("/v1/operations/nope");
    assert_eq!(status_code, 404);
    assert_eq!(missing_operation["code"], "NotFound");
}

#[test]
fn a_scripted_backend_failure_is_reported_and_the_environment_failed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config::new(temp.path()).with_backend_script(r#"{"start_fault": "backend_unavailable"}"#);
    let daemon = Daemon::boot_with(config);
    create_fixture(&daemon);

    let (status_code, failure) = start(&daemon, "hello-pg");
    assert_eq!(status_code, 503, "{failure}");
    assert_eq!(failure["code"], "BackendUnavailable");

    let (_, after) = status(&daemon, "hello-pg");
    assert_eq!(after["environment"]["state"], "failed", "{after}");
    assert_eq!(after["health"], "unhealthy");

    // Retrying after the fault clears is convergent: a new daemon can start it.
    drop(daemon);
    let daemon = Daemon::boot(temp.path());
    let (status_code, recovered) = start(&daemon, "hello-pg");
    assert_eq!(status_code, 200, "{recovered}");
    assert_eq!(recovered["environment"]["state"], "running");
}

// ---------------------------------------------------------------------------
// Restart persistence, driven through a real process so the restart is a real
// one: the first daemon's listeners and in-memory Backend are gone.
// ---------------------------------------------------------------------------

struct Process {
    child: Child,
    _home: PathBuf,
    discovery: agentify_proto::Discovery,
}

impl Process {
    fn boot(home: &Path) -> Process {
        std::fs::create_dir_all(home).expect("data root");
        let child = Command::new(env!("CARGO_BIN_EXE_agentifyd"))
            .env("AGENTIFY_HOME", home)
            .env("AGENTIFY_BACKEND", "fake")
            // The fixture imports one secret from the host environment.
            .env("HELLO_API_TOKEN", FIXTURE_API_TOKEN)
            .env("AGENTIFY_SECRET_STORE", "memory")
            // This test is about restart persistence, not ingress: without the
            // switch the binary would try to start Caddy and `start` would be a
            // 503 on a machine that has none.
            .env("AGENTIFY_INGRESS", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentifyd");
        let mut process = Process {
            child,
            _home: home.to_path_buf(),
            discovery: agentify_proto::Discovery {
                pid: 0,
                port: 0,
                url: String::new(),
                token: String::new(),
                daemon_version: String::new(),
                api_version: String::new(),
                created_at: String::new(),
            },
        };
        process.discovery = await_discovery(home, &mut process.child);
        process
    }

    fn call(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        request(
            method,
            &format!("{}{path}", self.discovery.url),
            &self.discovery.token,
            body,
            None,
        )
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn await_discovery(home: &Path, child: &mut Child) -> agentify_proto::Discovery {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll daemon") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("daemon exited before publishing discovery ({status}): {stderr}");
        }
        if let Ok(discovery) = agentify_proto::read_discovery(home) {
            if !discovery.token.is_empty()
                && matches!(try_healthz(&format!("{}/v1/healthz", discovery.url)), Some(200))
            {
                return discovery;
            }
        }
        assert!(Instant::now() < deadline, "no working discovery file within {BOOT_TIMEOUT:?}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `GET /v1/healthz`, or `None` when nothing is serving the port yet.
fn try_healthz(url: &str) -> Option<u16> {
    match ureq::get(url).timeout(Duration::from_millis(500)).call() {
        Ok(response) => Some(response.status()),
        Err(ureq::Error::Status(status, _)) => Some(status),
        Err(_) => None,
    }
}

#[test]
fn state_survives_a_daemon_restart_and_start_converges_again() {
    let temp = tempfile::tempdir().expect("temp dir");

    let first = Process::boot(temp.path());
    let (code, _) = first.call("POST", "/v1/projects", Some(r#"{"fixture":"hello-pg"}"#));
    assert_eq!(code, 201);
    let (code, started) = first.call("POST", "/v1/projects/hello-pg/environments/dev/start", Some("{}"));
    assert_eq!(code, 200, "{started}");
    let ports = leased_ports(&started);
    assert_eq!(ports.len(), 1, "only the app is published: {started}");
    first.kill();

    // A new daemon: same data root, new process, no runtime.
    let second = Process::boot(temp.path());
    let (code, project) = second.call("GET", "/v1/projects/hello-pg", None);
    assert_eq!(code, 200, "the Project survived the restart: {project}");
    assert_eq!(project["name"], "hello-pg");

    // Adoption brings a running Environment back in the background. A daemon
    // restart is not a stop: poll until it converges on its persisted leases.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let after_restart = loop {
        let (code, status) = second.call("GET", "/v1/projects/hello-pg/environments/dev/status", None);
        assert_eq!(code, 200, "{status}");
        if status["environment"]["state"] == "running" {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "adoption did not bring the Environment back: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(after_restart["health"], "healthy", "{after_restart}");
    assert_eq!(
        leased_ports(&after_restart),
        ports,
        "adoption reuses the persisted leases"
    );

    // Starting again converges, and reacquires the same host ports.
    let (code, restarted) = second.call("POST", "/v1/projects/hello-pg/environments/dev/start", Some("{}"));
    assert_eq!(code, 200, "{restarted}");
    assert_eq!(restarted["environment"]["state"], "running");
    assert_eq!(
        leased_ports(&restarted),
        ports,
        "leases are persisted, so a restart does not move URLs"
    );
    for port in leased_ports(&restarted) {
        assert!(port_open(port), "port {port} is bound again");
    }

    second.kill();
}

#[test]
fn creating_a_twenty_first_project_is_refused_with_a_typed_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    for index in 0..20 {
        let manifest = format!(
            "version: 1\nproject: p{index}\nresources:\n  web: {{ kind: app, image: node:22 }}\n"
        );
        let (status, body) = daemon.post_with(
            "/v1/projects",
            &json!({ "manifest": manifest }),
            &[],
        );
        assert_eq!(status, 201, "project {index} created: {body}");
    }

    let manifest = "version: 1\nproject: overflow\nresources:\n  web: { kind: app, image: node:22 }\n";
    let (status, body) = daemon.post_with(
        "/v1/projects",
        &json!({ "manifest": manifest }),
        &[],
    );
    assert_eq!(status, 409, "the 21st project is refused: {body}");
    assert_eq!(body["code"], "ProjectCapExceeded");
}

#[test]
fn a_third_concurrent_start_is_refused_with_a_typed_cap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config::new(temp.path()).with_backend_script(r#"{"start_delay_ms": 2000}"#);
    let daemon = Daemon::boot_with(config);

    for name in ["a", "b", "c"] {
        let manifest = format!(
            "version: 1\nproject: {name}\nresources:\n  web: {{ kind: app, image: node:22 }}\n"
        );
        let (status, body) = daemon.post_with(
            "/v1/projects",
            &json!({ "manifest": manifest }),
            &[],
        );
        assert_eq!(status, 201, "project {name} created: {body}");
    }

    // Fire all three starts at once so they overlap: two take the two slots, the
    // third is refused before it creates an operation.
    let base = daemon.url("");
    let token = daemon.token();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for name in ["a", "b", "c"] {
        let base = base.clone();
        let token = token.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            raw(
                "POST",
                &format!("{base}/v1/projects/{name}/environments/dev/start"),
                &token,
                Some("{}"),
                None,
            )
        }));
    }

    let results: Vec<(u16, String)> = handles
        .into_iter()
        .map(|handle| handle.join().expect("start thread"))
        .collect();
    let statuses: Vec<u16> = results.iter().map(|(status, _)| *status).collect();
    let started = statuses.iter().filter(|status| **status == 200).count();
    let capped = statuses.iter().filter(|status| **status == 409).count();
    assert_eq!(started, 2, "exactly two starts run: {statuses:?}");
    assert_eq!(capped, 1, "exactly one start is capped: {statuses:?}");
}

#[test]
fn whoami_reports_disk_pressure_from_the_injected_seam() {
    #[derive(Debug)]
    struct FakeDisk {
        free: u64,
    }
    impl agentify_daemon::disk::DiskUsage for FakeDisk {
        fn free_bytes(&self, _path: &Path) -> anyhow::Result<u64> {
            Ok(self.free)
        }
    }

    let low = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot_with(
        Config::new(low.path()).with_disk_usage(std::sync::Arc::new(FakeDisk { free: 1 })),
    );
    let (status, body) = daemon.get("/v1/whoami");
    assert_eq!(status, 200);
    assert_eq!(body["disk"]["free_bytes"].as_u64(), Some(1));
    assert_eq!(body["disk"]["warning"].as_bool(), Some(true));

    let high = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot_with(
        Config::new(high.path()).with_disk_usage(std::sync::Arc::new(FakeDisk {
            free: u64::MAX,
        })),
    );
    let (_, body) = daemon.get("/v1/whoami");
    assert_eq!(body["disk"]["warning"].as_bool(), Some(false));
}

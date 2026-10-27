//! Local ingress at the public API seam.
//!
//! Every test boots the real daemon against a temporary data root and talks to it
//! the way the desktop shell or an Agent does: read the discovery file, speak
//! HTTP. The only substitutions are the ones the ingress crate publishes for
//! tests — a Caddy admin endpoint that speaks the real admin API over loopback,
//! and a trust installer that never reaches the certificate store. What is
//! asserted is the route Caddy was told to serve, which is the wire contract,
//! never a daemon internal.

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use agentify_daemon::consent::ConsentMode;
use agentify_daemon::Config;
use agentify_ingress::testing::{summarise_routes, FakeAdmin, FakeTrustInstaller};
use agentify_ingress::{
    AdminMode, IngressOptions, TrustInstaller, TrustState, SERVER_HTTP, SERVER_TLS,
};
use serde_json::{json, Value};

/// The bundled fixture Project: one `web` app on guest port 3000.
const PROJECT: &str = "hello-pg";

/// The hostname the daemon derives for it, without a hosts-file edit.
const WEB_ID_HTTPS: &str = "agentify:https:web.hello-pg.localhost";
const WEB_ID_HTTP: &str = "agentify:http:web.hello-pg.localhost";

/// A daemon booted by a test, shut down on drop.
struct Daemon {
    inner: Option<agentify_daemon::RunningDaemon>,
}

impl Daemon {
    fn boot(config: Config) -> Daemon {
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
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("POST", path, Some(&body.to_string()))
    }

    fn delete(&self, path: &str) -> (u16, Value) {
        self.request("DELETE", path, None)
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let url = self.url(path);
        let mut call = match method {
            "GET" => ureq::get(&url),
            "DELETE" => ureq::request("DELETE", &url),
            _ => ureq::post(&url),
        };
        call = call
            .set("Authorization", &format!("Bearer {}", self.token()))
            .timeout(Duration::from_secs(20));
        if body.is_some() {
            call = call.set("Content-Type", "application/json");
        }
        let result = match body {
            Some(body) => call.send_string(body),
            None => call.call(),
        };
        let (status, text) = match result {
            Ok(response) => (
                response.status(),
                response.into_string().unwrap_or_default(),
            ),
            Err(ureq::Error::Status(status, response)) => {
                (status, response.into_string().unwrap_or_default())
            }
            Err(err) => panic!("transport error for {url}: {err}"),
        };
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Wait for an approval-driven operation to finish.
    fn wait(&self, operation_id: &str) -> Value {
        let (status, operation) =
            self.get(&format!("/v1/operations/{operation_id}/wait?timeoutMs=15000"));
        assert_eq!(status, 200, "waiting for {operation_id}: {operation}");
        operation
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

/// The trust state the daemon boots with, decided by the fake installer.
///
/// No test can reach the machine's certificate store: the state that usually
/// follows an install comes from the double, and is handed to ingress exactly
/// as a real report would be.
fn trust_from(installer: &FakeTrustInstaller, data_root: &Path) -> TrustState {
    let root = IngressOptions::new(data_root).root_cert();
    let report = installer.install(&root).expect("the fake installer never fails");
    assert_eq!(installer.calls().len(), 1, "the double was the one asked");
    report.state
}

/// A daemon whose ingress talks to a fake Caddy, on loopback only.
fn daemon_with(fake: &FakeAdmin, home: &Path, trust: TrustState) -> Daemon {
    let ingress = IngressOptions::new(home)
        .with_admin(AdminMode::Attach(fake.addr()))
        .with_ports(free_port(), free_port())
        .with_trust(trust);
    Daemon::boot(
        Config::new(home)
            .with_consent(ConsentMode::Granted)
            .with_ingress(ingress),
    )
}

fn create_fixture(daemon: &Daemon) {
    let (status, body) = daemon.post("/v1/projects", &json!({ "fixture": PROJECT }));
    assert_eq!(status, 201, "create the fixture Project: {body}");
}

fn start(daemon: &Daemon) -> (u16, Value) {
    daemon.post(
        &format!("/v1/projects/{PROJECT}/environments/dev/start"),
        &json!({}),
    )
}

fn stop(daemon: &Daemon) -> (u16, Value) {
    daemon.post(
        &format!("/v1/projects/{PROJECT}/environments/dev/stop"),
        &json!({}),
    )
}

fn environment_status(daemon: &Daemon) -> Value {
    let (status, body) = daemon.get(&format!(
        "/v1/projects/{PROJECT}/environments/dev/status"
    ));
    assert_eq!(status, 200, "status: {body}");
    body
}

/// The host port the daemon leased for `web`: guest 3000.
fn leased_web_port(daemon: &Daemon) -> u16 {
    let body = environment_status(daemon);
    let port = body["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|resource| resource["name"] == "web")
        .and_then(|resource| resource["ports"].as_array())
        .and_then(|ports| {
            ports
                .iter()
                .find(|port| port["guestPort"] == 3000)
                .and_then(|port| port["hostPort"].as_u64())
        })
        .unwrap_or_else(|| panic!("no host port leased for web: {body}"));
    port as u16
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    listener.local_addr().expect("addr").port()
}

/// What one of Caddy's servers currently says, route id to what it does.
fn routes(fake: &FakeAdmin, server: &str) -> std::collections::BTreeMap<String, String> {
    summarise_routes(&fake.routes(server))
}

// ---------------------------------------------------------------------------
// start: a route whose upstream is the leased host port.
// ---------------------------------------------------------------------------

#[test]
fn start_publishes_a_route_whose_upstream_is_the_leased_host_port() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fake = FakeAdmin::start();
    let installer = FakeTrustInstaller::trusting();
    let trust = trust_from(&installer, temp.path());
    assert_eq!(trust, TrustState::Installed, "the double installed the root");
    let daemon = daemon_with(&fake, temp.path(), trust);

    create_fixture(&daemon);
    let (status, started) = start(&daemon);
    assert_eq!(status, 200, "start: {started}");
    assert_eq!(started["environment"]["state"], "running", "{started}");

    let port = leased_web_port(&daemon);
    assert_eq!(
        routes(&fake, SERVER_TLS).get(WEB_ID_HTTPS),
        Some(&format!("proxy 127.0.0.1:{port}")),
        "Caddy was told to proxy {WEB_ID_HTTPS} to the lease: {:#?}",
        fake.routes(SERVER_TLS)
    );
    assert!(
        fake.loads() >= 1,
        "the daemon configured Caddy through the admin API"
    );
}

// ---------------------------------------------------------------------------
// stop: the route moves to the daemon's 503, and the lease goes back.
// ---------------------------------------------------------------------------

#[test]
fn stop_replaces_the_route_with_the_daemons_503_and_releases_the_lease() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fake = FakeAdmin::start();
    let daemon = daemon_with(&fake, temp.path(), TrustState::Installed);
    create_fixture(&daemon);
    let (status, started) = start(&daemon);
    assert_eq!(status, 200, "{started}");
    let port = leased_web_port(&daemon);
    assert!(port_open(port), "the fake Backend is serving on {port}");
    let loads_before = fake.loads();

    let (status, stopped) = stop(&daemon);
    assert_eq!(status, 200, "stop: {stopped}");
    assert_eq!(stopped["environment"]["state"], "stopped", "{stopped}");

    // The hostname survives, and answers a page the daemon wrote itself.
    let tls = fake.routes(SERVER_TLS);
    assert_eq!(tls.len(), 1, "a stop never deletes the route: {tls:#?}");
    let handler = &tls[0]["handle"][0];
    assert_eq!(handler["handler"], "static_response", "{handler}");
    assert_eq!(handler["status_code"], 503, "{handler}");
    let body = handler["body"].as_str().expect("body");
    assert!(body.contains(PROJECT), "the 503 names the Project: {body}");
    assert!(
        body.contains("stopped"),
        "the 503 says what happened: {body}"
    );
    assert_eq!(
        fake.loads(),
        loads_before,
        "a stop is one granular route write, not a reload: {:#?}",
        fake.requests()
    );

    // The lease is back in the pool, and nothing answers on it.
    assert!(
        environment_status(&daemon)["resources"]
            .as_array()
            .expect("resources")
            .iter()
            .all(|resource| resource["ports"]
                .as_array()
                .map(|ports| ports.iter().all(|port| port["hostPort"].is_null()))
                .unwrap_or(true)),
        "a stopped Environment holds no leases"
    );
    assert!(!port_open(port), "the lease on {port} was released");
}

// ---------------------------------------------------------------------------
// delete: the only thing that deletes the route.
// ---------------------------------------------------------------------------

#[test]
fn deleting_the_project_deletes_the_route_and_a_stop_never_does() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fake = FakeAdmin::start();
    let daemon = daemon_with(&fake, temp.path(), TrustState::Installed);
    create_fixture(&daemon);
    let (status, started) = start(&daemon);
    assert_eq!(status, 200, "{started}");
    assert!(routes(&fake, SERVER_TLS).contains_key(WEB_ID_HTTPS));

    let (status, stopped) = stop(&daemon);
    assert_eq!(status, 200, "{stopped}");
    assert!(
        routes(&fake, SERVER_TLS).contains_key(WEB_ID_HTTPS),
        "a stop keeps the route: {:#?}",
        fake.routes(SERVER_TLS)
    );

    // Deleting needs an Approval: the request is accepted, the decision runs it.
    let (status, accepted) = daemon.delete(&format!("/v1/projects/{PROJECT}"));
    assert_eq!(status, 202, "delete: {accepted}");
    let approval_id = accepted["approvalId"].as_str().expect("approvalId");
    let operation_id = accepted["operationId"].as_str().expect("operationId").to_string();
    assert!(
        routes(&fake, SERVER_TLS).contains_key(WEB_ID_HTTPS),
        "asking to delete changes nothing yet"
    );

    let (status, decided) = daemon.post(&format!("/v1/approvals/{approval_id}/approve"), &json!({}));
    assert_eq!(status, 200, "{decided}");
    let operation = daemon.wait(&operation_id);
    assert_eq!(operation["state"], "succeeded", "{operation}");

    assert!(
        fake.routes(SERVER_TLS).is_empty(),
        "removing the Environment deletes its routes: {:#?}",
        fake.routes(SERVER_TLS)
    );
    assert!(
        fake.routes(SERVER_HTTP).is_empty(),
        "{:#?}",
        fake.routes(SERVER_HTTP)
    );
    let (status, missing) = daemon.get(&format!("/v1/projects/{PROJECT}"));
    assert_eq!(status, 404, "{missing}");
}

// ---------------------------------------------------------------------------
// trust: absent or refused means HTTP serves, and never redirects.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_or_refused_root_serves_the_same_route_over_http() {
    // Two daemons: one that was never offered trust, and one where the user
    // declined it. Both must serve the app, not a certificate the browser will
    // reject.
    let declining = FakeTrustInstaller::refusing();
    let probe = tempfile::tempdir().expect("temp dir");
    let refused = trust_from(&declining, probe.path());
    assert_eq!(
        refused,
        TrustState::Refused,
        "the user said no; that is a state, not an error"
    );

    for (label, trust) in [("absent", TrustState::Absent), ("refused", refused)] {
        let temp = tempfile::tempdir().expect("temp dir");
        let fake = FakeAdmin::start();
        let mut daemon = daemon_with(&fake, temp.path(), trust);
        create_fixture(&daemon);
        let (status, started) = start(&daemon);
        assert_eq!(status, 200, "{label}: start: {started}");
        let port = leased_web_port(&daemon);

        let http = fake.routes(SERVER_HTTP);
        assert_eq!(http.len(), 1, "{label}: one route on the plain port: {http:#?}");
        assert_eq!(
            http[0]["handle"][0]["handler"], "reverse_proxy",
            "{label}: HTTP serves the app instead of redirecting: {http:#?}"
        );
        assert_eq!(
            http[0]["handle"][0]["upstreams"][0]["dial"],
            format!("127.0.0.1:{port}"),
            "{label}: the same upstream as HTTPS"
        );
        assert_eq!(
            http[0]["@id"], WEB_ID_HTTP,
            "{label}: the same route, on the other server"
        );
        assert!(
            http[0]["handle"][0].get("status_code").is_none(),
            "{label}: no 308 redirect into an untrusted certificate"
        );

        daemon.shutdown();
    }
}

// ---------------------------------------------------------------------------
// a port conflict: typed, named, and nothing started.
// ---------------------------------------------------------------------------

#[test]
fn an_ingress_port_already_held_is_a_409_naming_the_holder_and_nothing_starts() {
    let temp = tempfile::tempdir().expect("temp dir");
    // Hold the HTTPS port, as another server on the machine would.
    let squatter = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let https_port = squatter.local_addr().expect("addr").port();
    let http_port = free_port();

    let ingress = IngressOptions::new(temp.path())
        .with_ports(http_port, https_port)
        .with_admin(AdminMode::Spawn)
        .with_caddy(Some(temp.path().join("not-a-caddy.exe")));
    let daemon = Daemon::boot(Config::new(temp.path()).with_ingress(ingress));
    create_fixture(&daemon);

    let (status, failure) = start(&daemon);
    assert_eq!(status, 409, "a held ingress port is a 409: {failure}");
    assert_eq!(failure["code"], "PortUnavailable", "{failure}");
    let detail = failure["detail"].as_str().expect("detail");
    assert!(
        detail.contains(&format!("port {https_port} is already in use by")),
        "the failure names the port: {detail}"
    );
    let holder = detail
        .split("is already in use by ")
        .nth(1)
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    assert!(
        holder.contains("PID") || holder.contains("process"),
        "the failure names the holder, not just the port: {detail}"
    );
    assert!(
        detail.contains("will not move ingress"),
        "there is no silent port change: {detail}"
    );

    // Nothing started: Caddy was never launched, and no config was written for
    // it, so the ports it would have bound are untouched.
    assert!(
        !port_open(http_port),
        "Caddy was never started on {http_port}"
    );
    assert!(
        !temp.path().join("ingress").join("caddy.json").exists(),
        "a port conflict must stop before a config is written"
    );
    drop(squatter);
}

// ---------------------------------------------------------------------------
// the origin is loopback only.
// ---------------------------------------------------------------------------

#[test]
fn every_published_server_listens_on_loopback_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fake = FakeAdmin::start();
    let daemon = daemon_with(&fake, temp.path(), TrustState::Installed);
    create_fixture(&daemon);
    let (status, started) = start(&daemon);
    assert_eq!(status, 200, "{started}");

    let config = fake.config();
    for server in [SERVER_TLS, SERVER_HTTP] {
        let listen = config["apps"]["http"]["servers"][server]["listen"]
            .as_array()
            .unwrap_or_else(|| panic!("no listen array for {server}: {config}"));
        for address in listen {
            let address = address.as_str().unwrap_or_default();
            assert!(
                address.starts_with("127.0.0.1:"),
                "{server} listens on {address}, which is not loopback"
            );
        }
    }
}

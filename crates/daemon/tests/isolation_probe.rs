//! Cross-Project isolation, exercised on the scripted fake Backend.
//!
//! The WSL slice proves literal volume and database isolation against a real
//! distro; this probe proves the same boundaries on the fake, so CI always runs
//! the security acceptance suite without WSL.

use std::net::TcpListener;
use std::time::Duration;

use agentify_daemon::consent::ConsentMode;
use agentify_daemon::Config;
use agentify_ingress::testing::{summarise_routes, FakeAdmin};
use agentify_ingress::{AdminMode, IngressOptions, SERVER_HTTP};
use serde_json::{json, Value};

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

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("POST", path, Some(&body.to_string()))
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let url = self.url(path);
        let mut call = if method == "GET" {
            ureq::get(&url)
        } else {
            ureq::post(&url)
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

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    listener.local_addr().expect("addr").port()
}

fn routes(fake: &FakeAdmin, server: &str) -> std::collections::BTreeMap<String, String> {
    summarise_routes(&fake.routes(server))
}

fn create(daemon: &Daemon, name: &str) -> Value {
    let manifest = format!(
        "version: 1\nproject: {name}\nsecrets:\n  pg: {{ generate: password }}\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports:\n      - {{ port: 3000, hostname: web }}\n  db:\n    kind: database\n    engine: postgres\n    credentials: {{ username: {name}, password: \"${{secret:pg}}\" }}\n"
    );
    let (status, body) = daemon.post("/v1/projects", &json!({ "manifest": manifest }));
    assert_eq!(status, 201, "create {name}: {body}");
    body
}

fn start(daemon: &Daemon, name: &str) {
    let (status, body) =
        daemon.post(&format!("/v1/projects/{name}/environments/dev/start"), &json!({}));
    assert_eq!(status, 200, "start {name}: {body}");
}

fn db_password(spec: &Value) -> String {
    spec["resources"]
        .as_array()
        .expect("recorded resources")
        .iter()
        .find(|resource| resource["name"] == "db")
        .and_then(|resource| resource["env"]["POSTGRES_PASSWORD"].as_str())
        .map(str::to_string)
        .expect("the db Resource carries a resolved password")
}

#[test]
fn cross_project_probes_fail_on_the_fake_backend() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fake = FakeAdmin::start();
    let ingress = IngressOptions::new(temp.path())
        .with_admin(AdminMode::Attach(fake.addr()))
        .with_ports(free_port(), free_port());

    let spec_path = temp.path().join("recorded-spec.json");
    let script = json!({ "record_spec_path": spec_path.to_string_lossy() }).to_string();
    let daemon = Daemon::boot(
        Config::new(temp.path())
            .with_consent(ConsentMode::Granted)
            .with_ingress(ingress)
            .with_backend_script(script),
    );

    let a = create(&daemon, "hello");
    let b = create(&daemon, "world");

    // Two Projects never share a workspace.
    let workspace_a = a["workspace"].as_str().expect("workspace A");
    let workspace_b = b["workspace"].as_str().expect("workspace B");
    assert_ne!(workspace_a, workspace_b, "each Project owns its workspace");
    assert!(
        !workspace_a.starts_with(workspace_b) && !workspace_b.starts_with(workspace_a),
        "workspaces are siblings, not nested: {workspace_a} vs {workspace_b}"
    );

    // Two Projects never share a URL, and each URL names its own Project.
    let url_a = a["routes"][0]["url"].as_str().expect("url A");
    let url_b = b["routes"][0]["url"].as_str().expect("url B");
    assert_ne!(url_a, url_b, "Project URLs are distinct");
    assert!(url_a.contains("hello"), "A's URL names its Project: {url_a}");
    assert!(url_b.contains("world"), "B's URL names its Project: {url_b}");

    // Start A first and record its resolved database credential, then B.
    start(&daemon, "hello");
    let spec_a: Value = serde_json::from_str(
        &std::fs::read_to_string(&spec_path).expect("recorded spec after A"),
    )
    .expect("spec JSON");
    let password_a = db_password(&spec_a);

    start(&daemon, "world");
    let spec_b: Value = serde_json::from_str(
        &std::fs::read_to_string(&spec_path).expect("recorded spec after B"),
    )
    .expect("spec JSON");
    let password_b = db_password(&spec_b);

    assert_ne!(
        password_a, password_b,
        "each Project gets its own database credentials"
    );

    // Each route's upstream is its own lease: A never dials B's host port.
    let published = routes(&fake, SERVER_HTTP);
    let upstream_a = published
        .get("agentify:http:web.hello.localhost")
        .unwrap_or_else(|| panic!("A's route is published: {published:?}"));
    let upstream_b = published
        .get("agentify:http:web.world.localhost")
        .unwrap_or_else(|| panic!("B's route is published: {published:?}"));
    assert_ne!(
        upstream_a, upstream_b,
        "each hostname points at its own leased port: {published:?}"
    );
}

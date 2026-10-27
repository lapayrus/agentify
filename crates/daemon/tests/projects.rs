//! Acceptance tests at the public API seam: Projects created from the bundled
//! fixture or an inline Manifest, then inspected.
//!
//! Every test boots the real `agentifyd` against a temporary data root and
//! speaks HTTP, the way the MCP shim, the CLI, and the shell do. Nothing here
//! reaches into daemon internals.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentify_proto::Discovery;
use serde_json::{json, Value};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    home: PathBuf,
    discovery: Discovery,
}

impl Daemon {
    fn boot(home: &Path) -> Daemon {
        std::fs::create_dir_all(home).expect("create data root");
        let child = Command::new(env!("CARGO_BIN_EXE_agentifyd"))
            .env("AGENTIFY_HOME", home)
            // The fixture imports one secret from the host environment, and no
            // test may touch this machine's credential manager.
            .env("HELLO_API_TOKEN", "fixture-api-token-from-the-host")
            .env("AGENTIFY_SECRET_STORE", "memory")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentifyd");

        let mut daemon = Daemon {
            child,
            home: home.to_path_buf(),
            discovery: Discovery {
                pid: 0,
                port: 0,
                url: String::new(),
                token: String::new(),
                daemon_version: String::new(),
                api_version: String::new(),
                created_at: String::new(),
            },
        };
        daemon.discovery = await_discovery(home, &mut daemon.child);
        daemon
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.discovery.url)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request(ureq::post(&self.url(path)), body)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.request(ureq::get(&self.url(path)), &Value::Null)
    }

    fn request(&self, request: ureq::Request, body: &Value) -> (u16, Value) {
        let request = request.set(
            "Authorization",
            &format!("Bearer {}", self.discovery.token),
        );
        let result = if body.is_null() {
            request.call()
        } else {
            request
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
        };
        to_pair(result)
    }

    /// Create a Project from the bundled fixture and return the body.
    fn create_fixture(&self, name: &str) -> Value {
        let (status, body) = self.post("/v1/projects", &json!({ "fixture": name }));
        assert_eq!(status, 201, "creating the {name} fixture failed: {body}");
        body
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn to_pair(result: Result<ureq::Response, ureq::Error>) -> (u16, Value) {
    let response = match result {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(transport) => panic!("transport error: {transport}"),
    };
    let status = response.status();
    let raw = response.into_string().unwrap_or_default();
    let body = if raw.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("not JSON ({err}): {raw}"))
    };
    (status, body)
}

fn await_discovery(home: &Path, child: &mut Child) -> Discovery {
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
            if !discovery.token.is_empty() && discovery.port != 0 {
                if let Ok((200, _)) =
                    try_health(&format!("{}/v1/healthz", discovery.url))
                {
                    return discovery;
                }
            }
        }
        if Instant::now() > deadline {
            panic!("daemon did not publish a working discovery file within {BOOT_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn try_health(url: &str) -> Result<(u16, String), ureq::Error> {
    match ureq::get(url).call() {
        Ok(response) => Ok((response.status(), String::new())),
        Err(ureq::Error::Status(status, _)) => Ok((status, String::new())),
        Err(transport) => Err(transport),
    }
}

/// The inline Manifest used by the acceptance tests: one app from an image,
/// one Postgres database, a generated secret, and a port with a defaulted
/// hostname.
const INLINE_MANIFEST: &str = r#"
version: 1
project: inline
secrets:
  pg-password: { generate: password }
resources:
  web:
    kind: app
    image: node:22-alpine
    command: [node, server.js]
    env:
      DATABASE_URL: postgres://demo:${secret:pg-password}@db:5432/demo
    ports:
      - port: 3000
    dependsOn: [db]
  db:
    kind: database
    engine: postgres
    credentials:
      username: demo
      password: ${secret:pg-password}
"#;

#[test]
fn a_project_created_from_the_bundled_fixture_has_one_dev_environment_and_two_resources() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, project) = daemon.post("/v1/projects", &json!({ "fixture": "hello-pg" }));
    assert_eq!(status, 201, "creating from the fixture failed: {project}");

    assert_eq!(project["name"], "hello-pg");
    assert_eq!(project["source"], "fixture");
    assert_eq!(project["sourceName"], "hello-pg");
    assert!(
        !project["id"].as_str().unwrap_or_default().is_empty(),
        "the daemon mints the Project id: {project}"
    );

    let environment = &project["environment"];
    assert_eq!(environment["name"], "dev");
    assert_eq!(
        environment["state"], "stopped",
        "nothing starts in this ticket; the state is a placeholder"
    );
    assert!(
        environment["cost"]["cpu"].is_null() && environment["cost"]["memory"].is_null(),
        "cost is a placeholder until stats exist: {}",
        environment["cost"]
    );

    let resources = project["resources"].as_array().expect("resources list");
    assert_eq!(resources.len(), 2, "one app and one database: {project}");
    let web = resources
        .iter()
        .find(|resource| resource["name"] == "web")
        .expect("the web resource");
    assert_eq!(web["kind"], "app");
    assert_eq!(web["image"], Value::Null);
    assert_eq!(web["build"]["context"], ".");
    assert_eq!(web["build"]["dockerfile"], "Dockerfile");
    assert_eq!(web["url"], "https://web.hello-pg.localhost");
    assert_eq!(web["ports"][0]["port"], 3000);
    assert_eq!(web["ports"][0]["hostname"], "web");
    assert_eq!(web["dependsOn"], json!(["db"]));

    let db = resources
        .iter()
        .find(|resource| resource["name"] == "db")
        .expect("the db resource");
    assert_eq!(db["kind"], "database");
    assert_eq!(db["engine"], "postgres");
    assert_eq!(db["version"], "16");
    assert_eq!(db["volume"], "hello-pg-db-data");
    assert_eq!(db["username"], "hello");

    assert_eq!(
        project["routes"],
        json!([{
            "resource": "web",
            "hostname": "web",
            "url": "https://web.hello-pg.localhost"
        }]),
        "the derived URL is https://<hostname>.<project>.localhost"
    );

    // The fixture's source files are copied into the daemon-owned workspace,
    // so a `build` Resource has a build context without touching the repo.
    let workspace = project["workspace"].as_str().expect("a workspace path");
    assert!(
        workspace.starts_with(&temp.path().display().to_string()),
        "the workspace is daemon-owned inside the data root: {workspace}"
    );
    let workspace = PathBuf::from(workspace);
    assert!(workspace.join("Dockerfile").exists(), "the Dockerfile is copied");
    assert!(workspace.join("server.js").exists(), "the server file is copied");
    assert!(
        workspace.join("agentify.yaml").exists(),
        "the normalized source Manifest travels with the Project"
    );

    // Listing and reading by name or id agree.
    let (list_status, listed) = daemon.get("/v1/projects");
    assert_eq!(list_status, 200);
    let listed = listed["projects"].as_array().expect("a projects list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], project["id"]);

    let (by_name_status, by_name) = daemon.get("/v1/projects/hello-pg");
    assert_eq!(by_name_status, 200);
    assert_eq!(by_name, project);

    let id = project["id"].as_str().expect("id");
    let (by_id_status, by_id) = daemon.get(&format!("/v1/projects/{id}"));
    assert_eq!(by_id_status, 200, "get accepts an id too: {by_id}");
    assert_eq!(by_id, project);
}

#[test]
fn a_project_created_from_an_inline_image_manifest_is_persisted_and_listed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, project) = daemon.post(
        "/v1/projects",
        &json!({ "manifest": INLINE_MANIFEST }),
    );
    assert_eq!(status, 201, "creating from an inline manifest failed: {project}");
    assert_eq!(project["name"], "inline");
    assert_eq!(project["source"], "manifest");
    assert!(project["sourceName"].is_null());
    assert_eq!(project["resources"].as_array().expect("resources").len(), 2);
    assert_eq!(
        project["environment"]["state"],
        "stopped",
        "creating a Project creates its dev Environment and nothing runs yet"
    );

    let web = project["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .find(|resource| resource["name"] == "web")
        .cloned()
        .expect("web");
    assert_eq!(web["image"], "node:22-alpine");
    assert_eq!(
        web["url"], "https://web.inline.localhost",
        "a single port defaults its hostname to the Resource name"
    );

    let (list_status, listed) = daemon.get("/v1/projects");
    assert_eq!(list_status, 200);
    assert_eq!(listed["projects"].as_array().expect("list").len(), 1);
    assert_eq!(listed["projects"][0]["name"], "inline");
}

#[test]
fn two_projects_with_different_names_coexist() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    daemon.create_fixture("hello-pg");
    let (status, second) = daemon.post(
        "/v1/projects",
        &json!({ "manifest": INLINE_MANIFEST }),
    );
    assert_eq!(status, 201, "{second}");

    let (_, listed) = daemon.get("/v1/projects");
    let names: Vec<String> = listed["projects"]
        .as_array()
        .expect("list")
        .iter()
        .map(|project| project["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["hello-pg".to_string(), "inline".to_string()]);
}

#[test]
fn a_duplicate_project_name_is_already_exists() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());
    daemon.create_fixture("hello-pg");

    let (status, problem) = daemon.post("/v1/projects", &json!({ "fixture": "hello-pg" }));
    assert_eq!(status, 409, "{problem}");
    assert_eq!(problem["code"], "AlreadyExists");
    assert_eq!(problem["status"], 409);

    // The same name from an inline Manifest collides too: the name is the
    // Project's identity.
    let (status, problem) = daemon.post(
        "/v1/projects",
        &json!({ "manifest": INLINE_MANIFEST.replace("project: inline", "project: hello-pg") }),
    );
    assert_eq!(status, 409, "{problem}");
    assert_eq!(problem["code"], "AlreadyExists");
}

#[test]
fn an_unknown_project_is_not_found() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, problem) = daemon.get("/v1/projects/nope");
    assert_eq!(status, 404);
    assert_eq!(problem["code"], "NotFound");

    let (list_status, listed) = daemon.get("/v1/projects");
    assert_eq!(list_status, 200);
    assert_eq!(listed["projects"], json!([]));
}

#[test]
fn creating_a_project_requires_exactly_one_source() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, problem) = daemon.post("/v1/projects", &json!({}));
    assert_eq!(status, 422, "{problem}");
    assert_eq!(problem["code"], "ValidationFailed");

    let (status, problem) =
        daemon.post("/v1/projects", &json!({ "fixture": "hello-pg", "manifest": INLINE_MANIFEST }));
    assert_eq!(status, 422, "{problem}");
    assert_eq!(problem["code"], "ValidationFailed");
    assert!(
        problem["detail"].as_str().unwrap_or_default().contains("exactly one"),
        "the detail explains the rule: {problem}"
    );

    let (status, problem) = daemon.post("/v1/projects", &json!({ "fixture": "hello-pg", "extra": 1 }));
    assert_eq!(status, 422, "{problem}");
    assert_eq!(problem["code"], "ValidationFailed");
}

#[test]
fn an_unknown_fixture_is_source_missing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, problem) = daemon.post("/v1/projects", &json!({ "fixture": "nope" }));
    assert_eq!(status, 422, "{problem}");
    assert_eq!(problem["code"], "SourceMissing");
}

#[test]
fn a_manifest_with_a_build_and_no_fixture_source_is_source_missing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let manifest = r#"
version: 1
project: needing-source
resources:
  web:
    kind: app
    build: { context: . }
"#;
    let (status, problem) = daemon.post("/v1/projects", &json!({ "manifest": manifest }));
    assert_eq!(status, 422, "{problem}");
    assert_eq!(problem["code"], "SourceMissing");
    assert!(
        problem["detail"].as_str().unwrap_or_default().contains("build"),
        "the detail names the build Resource: {problem}"
    );

    let (_, listed) = daemon.get("/v1/projects");
    assert_eq!(
        listed["projects"],
        json!([]),
        "a rejected Project leaves no state behind"
    );
}

#[test]
fn malformed_manifests_are_validation_failures_with_a_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let cases: [(&str, &str); 6] = [
        // Unknown field inside a Resource.
        (
            "version: 1\nproject: a\nresources:\n  web: { kind: app, image: node:22, replicas: 2 }\n",
            "resources.web.replicas",
        ),
        // Missing required key.
        ("version: 1\nproject: a\n", "resources"),
        // image and build together.
        (
            "version: 1\nproject: a\nresources:\n  web:\n    kind: app\n    image: node:22\n    build: { context: . }\n",
            "resources.web",
        ),
        // Undefined secret reference.
        (
            "version: 1\nproject: a\nresources:\n  web:\n    kind: app\n    image: node:22\n    env: { TOKEN: \"${secret:nope}\" }\n",
            "resources.web.env.TOKEN",
        ),
        // Duplicate hostnames.
        (
            "version: 1\nproject: a\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports: [{ port: 3000, hostname: app }]\n  api:\n    kind: app\n    image: node:22\n    ports: [{ port: 3000, hostname: app }]\n",
            "resources.web.ports[0].hostname",
        ),
        // Dependency cycle.
        (
            "version: 1\nproject: a\nresources:\n  web:\n    kind: app\n    image: node:22\n    dependsOn: [api]\n  api:\n    kind: app\n    image: node:22\n    dependsOn: [web]\n",
            "resources.",
        ),
    ];

    for (manifest, expected_path) in cases {
        let (status, problem) = daemon.post("/v1/projects", &json!({ "manifest": manifest }));
        assert_eq!(status, 422, "manifest was not rejected: {manifest} -> {problem}");
        assert_eq!(problem["code"], "ValidationFailed", "{problem}");
        let path = problem["path"].as_str().unwrap_or_default();
        assert!(
            path.starts_with(expected_path),
            "expected path {expected_path:?}, got {path:?} for {manifest}"
        );
    }

    let (status, problem) = daemon.post(
        "/v1/projects",
        &json!({ "manifest": "version: 1\nproject: Bad_Name\nresources:\n  web: { kind: app, image: node:22 }\n" }),
    );
    assert_eq!(status, 422);
    assert_eq!(problem["path"], "project", "an invalid name names its field");

    let (_, listed) = daemon.get("/v1/projects");
    assert_eq!(
        listed["projects"],
        json!([]),
        "rejected manifests create nothing: {listed}"
    );
}

#[test]
fn project_endpoints_require_the_agent_token() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let response = ureq::get(&daemon.url("/v1/projects")).call();
    let status = match response {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(transport) => panic!("transport error: {transport}"),
    };
    assert_eq!(status, 401);

    let response = ureq::post(&daemon.url("/v1/projects"))
        .set("Content-Type", "application/json")
        .send_string(&json!({ "fixture": "hello-pg" }).to_string());
    let status = match response {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(transport) => panic!("transport error: {transport}"),
    };
    assert_eq!(status, 401);
    assert!(
        !daemon.home.join("projects").exists()
            || std::fs::read_dir(daemon.home.join("projects"))
                .map(|entries| entries.count() == 0)
                .unwrap_or(true),
        "an unauthenticated request creates no workspace"
    );
}

#[test]
fn projects_survive_a_daemon_restart() {
    let temp = tempfile::tempdir().expect("temp dir");

    let (id, created) = {
        let daemon = Daemon::boot(temp.path());
        let project = daemon.create_fixture("hello-pg");
        (project["id"].to_string(), project)
    };

    let daemon = Daemon::boot(temp.path());
    let (status, again) = daemon.get("/v1/projects/hello-pg");
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["id"].to_string(), id);
    assert_eq!(again["resources"].as_array().expect("resources").len(), 2);
    assert_eq!(again["created_at"], created["created_at"]);

    let (_, listed) = daemon.get("/v1/projects");
    assert_eq!(
        listed["projects"].as_array().expect("list").len(),
        1,
        "one Project, no duplicate dev Environments or Resources"
    );
    assert_eq!(listed["projects"][0]["environment"]["name"], "dev");
}

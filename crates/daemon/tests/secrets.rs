//! Project-scoped secrets, at the public API seam.
//!
//! Every test boots the real `agentifyd` process against a temporary data root
//! with a file-backed fake secret store (`AGENTIFY_SECRET_STORE=memory`), so no
//! test can read or write the developer's real OS credentials. Everything is
//! observed the way a client observes it: HTTP, an audit entry, or a file the
//! scripted fake Backend recorded.
//!
//! The only substitutes are the two the spec agreed on: the scripted fake
//! Backend, and the secret store the daemon is configured with.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The value the daemon imports through `fromEnv` in these tests.
const HOST_TOKEN: &str = "tok-from-the-host-environment-42";

/// A value with characters that percent-encode, so both the exact value and its
/// encoded forms have to be redacted.
const AWKWARD_TOKEN: &str = "s3cr3t p@ss/word+&";

/// A daemon process booted by a test, killed on drop.
struct Daemon {
    child: Child,
    home: PathBuf,
    discovery: agentify_proto::Discovery,
}

impl Daemon {
    /// Boot a daemon with a temporary data root, an in-memory secret store, and
    /// whatever a test needs on top.
    fn boot(home: &Path, extra_env: &[(&str, &str)]) -> Daemon {
        std::fs::create_dir_all(home).expect("create data root");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentifyd"));
        command
            .env("AGENTIFY_HOME", home)
            // Never the developer's real credential manager.
            .env("AGENTIFY_SECRET_STORE", "memory")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn agentifyd");

        let mut daemon = Daemon {
            child,
            home: home.to_path_buf(),
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
        daemon.discovery = await_discovery(home, &mut daemon.child);
        daemon
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("POST", path, Some(body))
    }

    fn patch(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("PATCH", path, Some(body))
    }

    fn delete(&self, path: &str) -> (u16, Value) {
        self.request("DELETE", path, None)
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let (status, text) = self.raw(method, path, body);
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, value)
    }

    fn raw(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
        let url = format!("{}{path}", self.discovery.url);
        let mut call = match method {
            "GET" => ureq::get(&url),
            "DELETE" => ureq::request("DELETE", &url),
            "PATCH" => ureq::request("PATCH", &url),
            _ => ureq::post(&url),
        };
        call = call
            .set("Authorization", &format!("Bearer {}", self.discovery.token))
            .timeout(Duration::from_secs(20));
        if body.is_some() {
            call = call.set("Content-Type", "application/json");
        }
        let result = match body {
            Some(body) => call.send_string(&body.to_string()),
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

    /// `POST /v1/projects` from an inline Manifest.
    fn create(&self, manifest: &str) -> (u16, Value) {
        self.post("/v1/projects", &json!({ "manifest": manifest }))
    }

    fn reveal(&self, project: &str, name: &str) -> (u16, Value) {
        self.post(
            &format!("/v1/projects/{project}/secrets/{name}/reveal"),
            &json!({}),
        )
    }

    fn secrets(&self, project: &str) -> Vec<Value> {
        let (status, body) = self.get(&format!("/v1/projects/{project}/secrets"));
        assert_eq!(status, 200, "list secrets: {body}");
        body["secrets"].as_array().cloned().unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = &self.home;
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
            if !discovery.token.is_empty() && discovery.port != 0 {
                let health = ureq::get(&format!("{}/v1/healthz", discovery.url))
                    .timeout(Duration::from_secs(2))
                    .call();
                if matches!(health, Ok(_)) {
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

/// The Manifest these tests create: one generated password, one value imported
/// from the host environment, and both referenced from a Resource's env and from
/// a database's credentials.
fn manifest(project: &str) -> String {
    format!(
        r#"
version: 1
project: {project}
secrets:
  db-password:
    generate: password
  api-token:
    fromEnv: AGENTIFY_TEST_API_TOKEN
resources:
  web:
    kind: app
    image: nginx:alpine
    env:
      DATABASE_URL: postgres://app:${{secret:db-password}}@db:5432/app
      API_TOKEN: ${{secret:api-token}}
      GREETING: hello
    ports:
      - port: 3000
        hostname: web
    dependsOn: [db]
  db:
    kind: database
    engine: postgres
    credentials:
      username: app
      password: ${{secret:db-password}}
"#
    )
}

/// A Manifest whose only secret is imported from a variable nobody set.
fn manifest_with_a_missing_source(project: &str) -> String {
    format!(
        r#"
version: 1
project: {project}
secrets:
  api-token:
    fromEnv: AGENTIFY_TEST_ABSENT_VARIABLE
resources:
  web:
    kind: app
    image: nginx:alpine
    env:
      API_TOKEN: ${{secret:api-token}}
    ports:
      - port: 3000
        hostname: web
"#
    )
}

/// Percent-encode everything that is not unreserved, upper-case hex: the shape a
/// value takes when it is pasted into a URL.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn is_url_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

// ---------------------------------------------------------------------------
// Creation and import
// ---------------------------------------------------------------------------

#[test]
fn a_generated_password_is_stored_and_never_returned() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43100-43119"),
        ],
    );

    let (status, created) = daemon.create(&manifest("secret-gen"));
    assert_eq!(status, 201, "create: {created}");

    let listed = daemon.secrets("secret-gen");
    assert_eq!(listed.len(), 2, "two declared secrets: {listed:?}");
    let password = listed
        .iter()
        .find(|secret| secret["name"] == "db-password")
        .unwrap_or_else(|| panic!("db-password is listed: {listed:?}"));
    assert_eq!(password["source"], "generate", "{password}");
    assert_eq!(password["referenced"], true, "{password}");
    assert!(
        password.get("value").is_none(),
        "a listed secret carries metadata only: {password}"
    );
    assert!(
        password["createdAt"].is_string() && password["updatedAt"].is_string(),
        "timestamps are metadata: {password}"
    );

    // The generated value is a strong, URL-safe secret, and the only way to see
    // it is the human reveal.
    let (status, revealed) = daemon.reveal("secret-gen", "db-password");
    assert_eq!(status, 200, "reveal: {revealed}");
    let value = revealed["value"]
        .as_str()
        .unwrap_or_else(|| panic!("the reveal carries the value: {revealed}"))
        .to_string();
    assert!(
        value.len() >= 32,
        "at least 32 bytes of randomness: {value:?} is {} chars",
        value.len()
    );
    assert!(
        is_url_safe(&value),
        "the generated value is URL-safe: {value:?}"
    );
    assert_ne!(
        value, HOST_TOKEN,
        "each secret gets its own value, not the imported one"
    );

    // Nothing that was returned before the reveal can have carried the value.
    assert!(
        !created.to_string().contains(&value),
        "the create response carries no plaintext"
    );
    assert!(
        !listed.iter().any(|secret| secret.to_string().contains(&value)),
        "the list carries no plaintext"
    );
}

#[test]
fn from_env_is_imported_once_and_a_missing_source_leaves_nothing_behind() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43120-43139"),
        ],
    );

    let (status, created) = daemon.create(&manifest("secret-env"));
    assert_eq!(status, 201, "create: {created}");
    assert!(
        !created.to_string().contains(HOST_TOKEN),
        "the create response carries no plaintext"
    );

    let listed = daemon.secrets("secret-env");
    let imported = listed
        .iter()
        .find(|secret| secret["name"] == "api-token")
        .unwrap_or_else(|| panic!("api-token is listed: {listed:?}"));
    assert_eq!(imported["source"], "fromEnv", "{imported}");
    assert!(
        !imported.to_string().contains(HOST_TOKEN),
        "the import's provenance is metadata, not the value: {imported}"
    );

    let (status, revealed) = daemon.reveal("secret-env", "api-token");
    assert_eq!(status, 200, "reveal: {revealed}");
    assert_eq!(
        revealed["value"], HOST_TOKEN,
        "the imported value is the host environment's value"
    );

    // An absent source is a typed refusal, and it leaves no Project behind.
    let (status, problem) = daemon.create(&manifest_with_a_missing_source("secret-absent"));
    assert_eq!(status, 422, "a missing source is a refusal: {problem}");
    assert_eq!(problem["code"], "SecretSourceMissing", "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("AGENTIFY_TEST_ABSENT_VARIABLE"),
        "the refusal names the variable a person has to set: {problem}"
    );

    let (status, projects) = daemon.get("/v1/projects");
    assert_eq!(status, 200, "{projects}");
    let names: Vec<&str> = projects["projects"]
        .as_array()
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| project["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        names,
        vec!["secret-env"],
        "the refused Project left no row: {projects}"
    );

    let (status, missing) = daemon.get("/v1/projects/secret-absent");
    assert_eq!(status, 404, "{missing}");

    // Nothing was written for it on disk either: exactly one Project directory.
    let dirs = std::fs::read_dir(temp.path().join("projects"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(dirs, 1, "the refused Project left no workspace");
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// What the scripted Backend was handed at `start`, as the fake recorded it.
fn recorded_spec(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("the fake Backend recorded no spec at {}: {err}", path.display())
    });
    serde_json::from_str(&raw).expect("the recorded spec is JSON")
}

fn resource<'a>(spec: &'a Value, name: &str) -> &'a Value {
    spec["resources"]
        .as_array()
        .and_then(|resources| {
            resources
                .iter()
                .find(|resource| resource["name"] == name)
        })
        .unwrap_or_else(|| panic!("the spec has a {name} Resource: {spec}"))
}

#[test]
fn references_resolve_into_the_spec_the_backend_receives() {
    let temp = tempfile::tempdir().expect("temp dir");
    let spec_path = temp.path().join("recorded-spec.json");
    let script = json!({ "record_spec_path": spec_path.display().to_string() }).to_string();
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_FAKE_SCRIPT", &script),
            ("AGENTIFY_LEASE_RANGE", "43140-43159"),
        ],
    );

    let (status, created) = daemon.create(&manifest("secret-spec"));
    assert_eq!(status, 201, "create: {created}");

    let (status, started) = daemon.post("/v1/projects/secret-spec/environments/dev/start", &json!({}));
    assert_eq!(status, 200, "start: {started}");
    assert!(
        !started.to_string().contains(HOST_TOKEN),
        "the start's status carries no plaintext: {started}"
    );

    let (status, revealed) = daemon.reveal("secret-spec", "db-password");
    assert_eq!(status, 200, "reveal: {revealed}");
    let password = revealed["value"].as_str().expect("the value").to_string();

    let spec = recorded_spec(&spec_path);
    let rendered = spec.to_string();
    assert!(
        !rendered.contains("${secret:"),
        "every reference is resolved before the Backend sees it: {rendered}"
    );

    let web = resource(&spec, "web");
    assert_eq!(
        web["env"]["API_TOKEN"], HOST_TOKEN,
        "an imported secret reaches the Resource's env: {web}"
    );
    assert_eq!(
        web["env"]["DATABASE_URL"],
        format!("postgres://app:{password}@db:5432/app"),
        "a reference inside a larger value is resolved in place: {web}"
    );
    assert_eq!(
        web["env"]["GREETING"], "hello",
        "a literal stays a literal: {web}"
    );

    let db = resource(&spec, "db");
    assert_eq!(
        db["env"]["POSTGRES_PASSWORD"],
        serde_json::Value::String(password.clone()),
        "a database's credentials are resolved into its env: {db}"
    );
    assert_eq!(
        db["env"]["POSTGRES_USER"], "app",
        "the username is not a secret and is passed through: {db}"
    );
}

// ---------------------------------------------------------------------------
// No plaintext, anywhere
// ---------------------------------------------------------------------------

#[test]
fn no_api_response_carries_a_plaintext_value() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", AWKWARD_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43160-43179"),
        ],
    );

    let (status, created) = daemon.create(&manifest("secret-plain"));
    assert_eq!(status, 201, "create: {created}");
    let (status, started) = daemon.post("/v1/projects/secret-plain/environments/dev/start", &json!({}));
    assert_eq!(status, 200, "start: {started}");
    let (status, _) = daemon.post(
        "/v1/projects/secret-plain/environments/dev/resources/web/exec",
        &json!({ "command": ["echo", AWKWARD_TOKEN] }),
    );
    assert_eq!(status, 200, "exec");

    let (_, revealed) = daemon.reveal("secret-plain", "db-password");
    let generated = revealed["value"].as_str().expect("the value").to_string();

    let paths = [
        "/v1/projects".to_string(),
        "/v1/projects/secret-plain".to_string(),
        "/v1/projects/secret-plain/secrets".to_string(),
        "/v1/projects/secret-plain/environments/dev/status".to_string(),
        "/v1/projects/secret-plain/environments/dev/logs?tail=500".to_string(),
        "/v1/projects/secret-plain/environments/dev/stats".to_string(),
        "/v1/activity?limit=200".to_string(),
    ];
    for path in paths {
        let (status, body) = daemon.raw("GET", &path, None);
        assert_eq!(status, 200, "{path}: {body}");
        for value in [HOST_TOKEN, AWKWARD_TOKEN, generated.as_str()] {
            assert!(
                !body.contains(value),
                "{path} carries no plaintext ({value:?}): {body}"
            );
        }
        assert!(
            !body.contains(&percent_encode(AWKWARD_TOKEN)),
            "{path} carries no percent-encoded plaintext: {body}"
        );
    }
}

#[test]
fn logs_and_exec_output_are_redacted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", AWKWARD_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43180-43199"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-redact"));
    assert_eq!(status, 201, "create: {created}");
    let (status, started) = daemon.post("/v1/projects/secret-redact/environments/dev/start", &json!({}));
    assert_eq!(status, 200, "start: {started}");

    // A command whose output is a known secret, and one whose output is the
    // value as a URL would encode it.
    for argument in [AWKWARD_TOKEN.to_string(), percent_encode(AWKWARD_TOKEN)] {
        let (status, body) = daemon.post(
            "/v1/projects/secret-redact/environments/dev/resources/web/exec",
            &json!({ "command": ["echo", argument] }),
        );
        assert_eq!(status, 200, "exec: {body}");
        let stdout = body["stdout"].as_str().unwrap_or_default().to_string();
        assert!(
            !stdout.contains(AWKWARD_TOKEN) && !stdout.contains(&percent_encode(AWKWARD_TOKEN)),
            "exec output is redacted: {stdout:?}"
        );
        assert!(
            stdout.contains("[REDACTED]"),
            "the redactor marks what it removed: {stdout:?}"
        );
    }

    // The fake Backend turns exec output into log lines, as a real substrate
    // would, so the logs carry the same redaction.
    let (status, logs) = daemon.get(
        "/v1/projects/secret-redact/environments/dev/logs?tail=500",
    );
    assert_eq!(status, 200, "logs: {logs}");
    let lines: Vec<String> = logs["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| line["line"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(!lines.is_empty(), "there are log lines: {logs}");
    for line in &lines {
        assert!(
            !line.contains(AWKWARD_TOKEN) && !line.contains(&percent_encode(AWKWARD_TOKEN)),
            "every log line is redacted: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Updates: an import is final, a supplied value is not
// ---------------------------------------------------------------------------

#[test]
fn an_imported_secret_is_final_and_a_literal_one_can_be_replaced() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43280-43299"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-update"));
    assert_eq!(status, 201, "create: {created}");

    // A `fromEnv` import happened once, when the Project was created. Rewriting it
    // would make the Project's provenance a lie, so it is refused.
    let (status, problem) = daemon.patch(
        "/v1/projects/secret-update/secrets/api-token",
        &json!({ "value": "a-replacement-the-agent-picked" }),
    );
    assert_eq!(status, 409, "an import is immutable: {problem}");
    assert_eq!(problem["code"], "SecretImmutable", "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("AGENTIFY_TEST_API_TOKEN"),
        "the refusal says where the value came from: {problem}"
    );
    assert!(
        !problem.to_string().contains(HOST_TOKEN),
        "the refusal carries no plaintext: {problem}"
    );
    let (_, revealed) = daemon.reveal("secret-update", "api-token");
    assert_eq!(revealed["value"], HOST_TOKEN, "the import is untouched");

    // A value an Agent supplied is the Agent's to replace.
    let value = "agent-chosen-value-71c";
    let (status, made) = daemon.post(
        "/v1/projects/secret-update/secrets",
        &json!({ "name": "scratch-token", "value": value }),
    );
    assert_eq!(status, 201, "create a literal secret: {made}");
    assert_eq!(made["source"], "literal", "{made}");
    assert!(!made.to_string().contains(value), "the value is never echoed");

    let replacement = "agent-replacement-value-9ab";
    let (status, updated) = daemon.patch(
        "/v1/projects/secret-update/secrets/scratch-token",
        &json!({ "value": replacement }),
    );
    assert_eq!(status, 200, "replace a literal secret: {updated}");
    assert!(
        !updated.to_string().contains(replacement),
        "an update never echoes the value: {updated}"
    );
    let (status, revealed) = daemon.reveal("secret-update", "scratch-token");
    assert_eq!(status, 200, "{revealed}");
    assert_eq!(revealed["value"], replacement, "the new value is the one stored");

    // A name the Project does not have is a typed 404, not a silent write.
    let (status, missing) = daemon.patch(
        "/v1/projects/secret-update/secrets/nope",
        &json!({ "value": "whatever" }),
    );
    assert_eq!(status, 404, "{missing}");
    assert_eq!(missing["code"], "NotFound", "{missing}");
}

// ---------------------------------------------------------------------------
// Persistence: metadata in SQLite, values only in the store
// ---------------------------------------------------------------------------

/// Every file the state database is made of: the database, its write-ahead log,
/// and the shared-memory index.
fn state_files(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.starts_with("state.db"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The store the daemon in these tests writes to. Its file is the one place a
/// value lands on disk, and only a test-configured daemon has it at all.
fn store_file(home: &Path) -> String {
    std::fs::read_to_string(home.join(agentify_daemon::secrets::TEST_STORE_FILE))
        .expect("the test secret store's file")
}

#[test]
fn the_state_database_holds_metadata_and_never_a_value() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43300-43319"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-metadata"));
    assert_eq!(status, 201, "create: {created}");
    let (status, revealed) = daemon.reveal("secret-metadata", "db-password");
    assert_eq!(status, 200, "{revealed}");
    let generated = revealed["value"].as_str().expect("the value").to_string();

    // The Project row and its spec hold declarations, never values, so a copy of
    // the state database is safe to hand to someone else.
    let files = state_files(temp.path());
    assert!(!files.is_empty(), "the daemon created a state database");
    let mut text = String::new();
    for path in &files {
        let bytes = std::fs::read(path).expect("read the state database");
        let part = String::from_utf8_lossy(&bytes).to_string();
        for value in [HOST_TOKEN, generated.as_str()] {
            assert!(
                !part.contains(value),
                "{} carries no secret value ({value:?})",
                path.display()
            );
        }
        text.push_str(&part);
    }
    // ...and it does hold the metadata: the migration-7 table is there, and the
    // names are in it. A value would need a column to live in, and there is none.
    assert!(
        text.contains("secrets_metadata"),
        "the state database records secret metadata"
    );
    assert!(
        text.contains("api-token"),
        "the metadata names the secret: {}",
        created["id"]
    );

    // The values really were stored: the store has both of them.
    let store = store_file(temp.path());
    assert!(store.contains(&generated), "the generated value is stored");
    assert!(store.contains(HOST_TOKEN), "the imported value is stored");
}

#[test]
fn deleting_a_project_takes_its_secret_values_with_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43320-43339"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-purge"));
    assert_eq!(status, 201, "create: {created}");
    let (_, revealed) = daemon.reveal("secret-purge", "db-password");
    let generated = revealed["value"].as_str().expect("the value").to_string();
    assert!(store_file(temp.path()).contains(&generated));

    // Deleting a Project needs a human, as every destructive operation does.
    let (status, accepted) = daemon.delete("/v1/projects/secret-purge");
    assert_eq!(status, 202, "{accepted}");
    let approval = accepted["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("the 202 carries an approvalId: {accepted}"))
        .to_string();
    let operation = accepted["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("the 202 carries an operationId: {accepted}"))
        .to_string();
    let (status, decided) = daemon.post(&format!("/v1/approvals/{approval}/approve"), &json!({}));
    assert_eq!(status, 200, "approve: {decided}");
    let (status, finished) =
        daemon.get(&format!("/v1/operations/{operation}/wait?timeoutMs=20000"));
    assert_eq!(status, 200, "{finished}");
    assert_eq!(finished["state"], "succeeded", "{finished}");

    let (status, missing) = daemon.get("/v1/projects/secret-purge");
    assert_eq!(status, 404, "the Project is gone: {missing}");

    // Nothing left behind: the values went with the Project.
    let store = store_file(temp.path());
    assert!(
        !store.contains(&generated),
        "the generated value is gone: {store}"
    );
    assert!(
        !store.contains(HOST_TOKEN),
        "the imported value is gone: {store}"
    );
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn a_problem_body_is_redacted_like_every_other_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43340-43359"),
        ],
    );
    // A value shaped like a name, so it can be typed into a path — which is what
    // an Agent guessing at a value would do. The daemon must mask it in a failure
    // as readily as in a success, or the error channel becomes an oracle.
    let guessable = "guessed-value-4f2b";
    let (status, created) = daemon.create(&manifest("secret-problem"));
    assert_eq!(status, 201, "create: {created}");
    let (status, made) = daemon.post(
        "/v1/projects/secret-problem/secrets",
        &json!({ "name": "guessable", "value": guessable }),
    );
    assert_eq!(status, 201, "{made}");

    let (status, problem) =
        daemon.delete(&format!("/v1/projects/secret-problem/secrets/{guessable}"));
    assert_eq!(status, 404, "no secret has that name: {problem}");
    assert_eq!(problem["code"], "NotFound", "{problem}");
    assert!(
        !problem.to_string().contains(guessable),
        "the problem body carries no plaintext: {problem}"
    );
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("[REDACTED]"),
        "the value is masked where it would have appeared: {problem}"
    );
}

// ---------------------------------------------------------------------------
// Agents: create, update, delete
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_referenced_secret_is_rejected_and_an_unreferenced_one_is_allowed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43200-43219"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-delete"));
    assert_eq!(status, 201, "create: {created}");

    // The Manifest references it, so removing the value would break the next
    // start: refused.
    let (status, problem) = daemon.delete("/v1/projects/secret-delete/secrets/api-token");
    assert_eq!(status, 409, "a referenced secret is refused: {problem}");
    assert_eq!(problem["code"], "SecretInUse", "{problem}");
    assert!(
        !problem.to_string().contains(HOST_TOKEN),
        "the refusal carries no plaintext: {problem}"
    );

    // A secret nothing references can go.
    let (status, created_secret) = daemon.post(
        "/v1/projects/secret-delete/secrets",
        &json!({ "name": "scratch", "value": "a-literal-only-the-agent-knows" }),
    );
    assert_eq!(status, 201, "create a secret: {created_secret}");
    assert_eq!(created_secret["name"], "scratch");
    assert!(
        created_secret.get("value").is_none(),
        "creating a secret never echoes its value: {created_secret}"
    );

    let (status, deleted) = daemon.delete("/v1/projects/secret-delete/secrets/scratch");
    assert_eq!(status, 200, "an unreferenced secret is deletable: {deleted}");
    let names: Vec<String> = daemon
        .secrets("secret-delete")
        .iter()
        .filter_map(|secret| secret["name"].as_str().map(str::to_string))
        .collect();
    assert!(!names.contains(&"scratch".to_string()), "{names:?}");
    assert!(names.contains(&"api-token".to_string()), "{names:?}");
}

// ---------------------------------------------------------------------------
// Reveal
// ---------------------------------------------------------------------------

#[test]
fn reveal_requires_consent_and_is_audited_without_the_value() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43220-43239"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-reveal"));
    assert_eq!(status, 201, "create: {created}");

    let (status, revealed) = daemon.reveal("secret-reveal", "api-token");
    assert_eq!(status, 200, "reveal: {revealed}");
    assert_eq!(revealed["value"], HOST_TOKEN);
    assert!(revealed["revealedAt"].is_string(), "{revealed}");

    let (status, activity) = daemon.get("/v1/activity?limit=200");
    assert_eq!(status, 200, "{activity}");
    assert_eq!(
        activity["integrity"]["verified"], true,
        "the chain still verifies: {activity}"
    );
    let entry = activity["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["operation"] == "reveal_secret")
        })
        .unwrap_or_else(|| panic!("the reveal is audited: {activity}"));
    assert_eq!(entry["result"], "succeeded", "{entry}");
    assert_eq!(entry["actor"]["kind"], "human", "a human revealed it: {entry}");
    assert_eq!(entry["projectId"], created["id"], "{entry}");
    assert!(
        entry["target"]
            .as_str()
            .unwrap_or_default()
            .contains("api-token"),
        "the audit names the secret, not its value: {entry}"
    );
    assert!(
        !activity.to_string().contains(HOST_TOKEN),
        "the audit carries no plaintext: {activity}"
    );
}

#[test]
fn an_agent_token_cannot_read_a_value() {
    let temp = tempfile::tempdir().expect("temp dir");
    // Nobody at this machine passes the gesture, so the human path is closed.
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "declined"),
            ("AGENTIFY_LEASE_RANGE", "43240-43259"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-noreveal"));
    assert_eq!(status, 201, "create: {created}");

    let (status, problem) = daemon.reveal("secret-noreveal", "api-token");
    assert_eq!(status, 403, "reveal without the gesture: {problem}");
    assert_eq!(problem["code"], "ApprovalRequired", "{problem}");
    assert!(
        !problem.to_string().contains(HOST_TOKEN),
        "a refused reveal carries no plaintext: {problem}"
    );

    // The metadata is still readable, so an Agent knows the name exists.
    let names: Vec<String> = daemon
        .secrets("secret-noreveal")
        .iter()
        .filter_map(|secret| secret["name"].as_str().map(str::to_string))
        .collect();
    assert!(names.contains(&"api-token".to_string()), "{names:?}");
}

#[test]
fn secret_mutations_land_in_the_audit_chain() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        temp.path(),
        &[
            ("AGENTIFY_TEST_API_TOKEN", HOST_TOKEN),
            ("AGENTIFY_CONSENT", "granted"),
            ("AGENTIFY_LEASE_RANGE", "43260-43279"),
        ],
    );
    let (status, created) = daemon.create(&manifest("secret-audit"));
    assert_eq!(status, 201, "create: {created}");
    let project_id = created["id"].as_str().expect("the Project id").to_string();

    let (status, body) = daemon.post(
        "/v1/projects/secret-audit/secrets",
        &json!({ "name": "agent-made", "generate": "token" }),
    );
    assert_eq!(status, 201, "create a secret: {body}");

    let (status, body) = daemon.patch(
        "/v1/projects/secret-audit/secrets/agent-made",
        &json!({ "value": "a-replacement-value-the-agent-chose" }),
    );
    assert_eq!(status, 200, "update a secret: {body}");
    assert!(
        !body.to_string().contains("a-replacement-value-the-agent-chose"),
        "an update never echoes the value: {body}"
    );

    let (status, body) = daemon.delete("/v1/projects/secret-audit/secrets/agent-made");
    assert_eq!(status, 200, "delete a secret: {body}");

    let (status, activity) = daemon.get("/v1/activity?limit=200");
    assert_eq!(status, 200, "{activity}");
    assert_eq!(
        activity["integrity"]["verified"], true,
        "the chain still verifies: {activity}"
    );
    for operation in ["create_secret", "update_secret", "delete_secret"] {
        let entry = activity["entries"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["operation"] == operation)
            })
            .unwrap_or_else(|| panic!("{operation} is audited: {activity}"));
        assert_eq!(entry["result"], "succeeded", "{entry}");
        assert_eq!(entry["projectId"], project_id.as_str(), "{entry}");
        assert_eq!(entry["actor"]["kind"], "agent", "{entry}");
    }
    assert!(
        !activity.to_string().contains("a-replacement-value-the-agent-chose"),
        "the audit carries no value: {activity}"
    );
    assert!(
        !activity.to_string().contains(HOST_TOKEN),
        "the audit carries no value: {activity}"
    );
}

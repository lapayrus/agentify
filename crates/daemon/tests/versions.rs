//! The version handshake, at the public API seam.
//!
//! A build pins the API, the daemon, the MCP shim, Caddy, each Backend adapter's
//! major, and the rootfs digest. A machine whose adapter is a different major
//! must be refused with `VersionMismatch` rather than driven, and the shell must
//! be able to show why — so both the refusal and the report go through the API a
//! client already speaks.
//!
//! The mismatch is produced by the fake Backend's script (`{"version": …}`),
//! which is the sanctioned substitution point: no test-only switch was added to
//! the daemon for this.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentify_proto::Discovery;
use serde_json::{json, Value};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    discovery: Discovery,
}

impl Daemon {
    fn boot(home: &Path) -> Daemon {
        Daemon::boot_with(home, &[])
    }

    fn boot_with(home: &Path, extra_env: &[(&str, &str)]) -> Daemon {
        std::fs::create_dir_all(home).expect("create data root");
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentifyd"));
        command
            .env("AGENTIFY_HOME", home)
            .env("AGENTIFY_BACKEND", "fake")
            .env("AGENTIFY_SECRET_STORE", "memory")
            .env("HELLO_API_TOKEN", "fixture-api-token-from-the-host")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn agentifyd");
        let mut daemon = Daemon {
            child,
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

    fn get(&self, path: &str) -> (u16, Value) {
        to_pair(
            ureq::get(&self.url(path))
                .set("Authorization", &format!("Bearer {}", self.discovery.token))
                .call(),
        )
    }

    fn get_anonymous(&self, path: &str) -> (u16, Value) {
        to_pair(ureq::get(&self.url(path)).call())
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        to_pair(
            ureq::post(&self.url(path))
                .set("Authorization", &format!("Bearer {}", self.discovery.token))
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
        )
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
                if try_health(&format!("{}/v1/healthz", discovery.url)) == Some(200) {
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

fn try_health(url: &str) -> Option<u16> {
    match ureq::get(url).call() {
        Ok(response) => Some(response.status()),
        Err(ureq::Error::Status(status, _)) => Some(status),
        // Not listening yet: the daemon has not bound, so keep waiting.
        Err(_) => None,
    }
}

/// The data root, so a test can assert where state really landed.
fn home(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[test]
fn the_handshake_reports_every_pinned_version_and_passes_on_a_shipped_build() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, report) = daemon.get("/v1/handshake");
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["ok"], true, "a shipped build matches its own pins: {report}");
    assert!(report["refusal"].is_null());

    let components: Vec<&str> = report["components"]
        .as_array()
        .expect("components is an array")
        .iter()
        .map(|component| component["component"].as_str().unwrap_or_default())
        .collect();
    for expected in ["api", "daemon", "mcp", "caddy", "adapter:fake", "rootfs"] {
        assert!(
            components.contains(&expected),
            "the handshake reports {expected}: {components:?}"
        );
    }

    let api = report["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["component"] == "api")
        .expect("an api row");
    assert_eq!(api["expected"], "v1");
    assert_eq!(api["found"], "v1");
    assert_eq!(api["rule"], "exact");

    assert_eq!(report["backend"]["name"], "fake");
}

#[test]
fn healthz_publishes_the_handshake_without_a_token() {
    // The shell draws its version banner before it has done anything else, so
    // the pin verdict is on the unauthenticated liveness probe and carries no
    // token or Project state.
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (status, health) = daemon.get_anonymous("/v1/healthz");
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["handshake"]["ok"], true, "{health}");
    assert!(
        health.get("token").is_none() && health["handshake"].get("token").is_none(),
        "the liveness probe must not leak the token: {health}"
    );
}

#[test]
fn a_mismatched_adapter_is_refused_with_versionmismatch() {
    let temp = tempfile::tempdir().expect("temp dir");
    // A different major from the pinned adapter version. The fake reads its
    // script from configuration, so no production seam was added for this.
    let daemon = Daemon::boot_with(
        temp.path(),
        &[("AGENTIFY_FAKE_SCRIPT", r#"{"version":"9.0.0"}"#)],
    );

    let (status, report) = daemon.get("/v1/handshake");
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["ok"], false, "the mismatch is reported: {report}");
    let refusal = report["refusal"].as_str().unwrap_or_default();
    assert!(
        refusal.contains("refused") && refusal.contains("adapter:fake"),
        "the refusal names the component: {refusal}"
    );
    assert!(
        refusal.contains("9.0.0"),
        "the refusal names what it found: {refusal}"
    );

    let (create_status, project) = daemon.post("/v1/projects", &json!({ "fixture": "hello-pg" }));
    assert_eq!(create_status, 201, "{project}");
    assert_eq!(
        project["environment"]["state"], "stopped",
        "a refused Backend does not change what creating means"
    );

    let (start_status, problem) = daemon.post(
        "/v1/projects/hello-pg/environments/dev/start",
        &Value::Null,
    );
    assert_eq!(start_status, 409, "the start is refused: {problem}");
    assert_eq!(problem["code"], "VersionMismatch", "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("VersionMismatch"),
        "the problem explains itself: {problem}"
    );

    // Reads that would drive the substrate are refused the same way.
    let (status_status, problem) = daemon.get("/v1/projects/hello-pg/environments/dev/status");
    assert_eq!(status_status, 409, "{problem}");
    assert_eq!(problem["code"], "VersionMismatch");
}

#[test]
fn state_and_the_discovery_file_live_under_the_data_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = home(temp.path());
    let daemon = Daemon::boot(&root);
    let _ = daemon.get_anonymous("/v1/healthz");

    // Everything durable is under the root, in the documented subdirectories —
    // never beside the installed program.
    for directory in [
        root.join("logs"),
        root.join("checkpoints"),
        root.join("projects"),
        root.join("wsl").join("rootfs"),
        root.join("wsl").join("env"),
        root.join("ingress"),
        root.join("tools"),
    ] {
        assert!(
            directory.is_dir(),
            "{} is part of the data root layout",
            directory.display()
        );
    }
    assert!(agentify_proto::state_db_path(&root).is_file());
    assert!(agentify_proto::discovery_path(&root).is_file());
}

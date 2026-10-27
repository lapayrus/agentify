//! The update and shutdown primitives, at the public API seam.
//!
//! An update asks two questions of the daemon before it applies: is a mutation
//! in flight, and has every desired-running Environment been adopted and found
//! healthy? This suite proves both answers and the shutdown path the updater and
//! uninstaller use to stop the daemon before swapping or deleting its files.

use std::time::Duration;

use agentify_daemon::Config;
use serde_json::{json, Value};

/// A daemon booted in-process, shut down on drop.
struct Daemon {
    inner: Option<agentify_daemon::RunningDaemon>,
}

impl Daemon {
    fn boot_with(config: Config) -> Daemon {
        let config = config.with_secret_env([("HELLO_API_TOKEN", "fixture-api-token")]);
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
        request("GET", &self.url(path), &self.token(), None)
    }

    fn post_with(&self, path: &str, body: &Value) -> (u16, Value) {
        request("POST", &self.url(path), &self.token(), Some(&body.to_string()))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.inner.take() {
            let _ = daemon.shutdown();
        }
    }
}

fn request(method: &str, url: &str, token: &str, body: Option<&str>) -> (u16, Value) {
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
    let result = match body {
        Some(body) => call.send_string(body),
        None => call.call(),
    };
    let (status, text) = match result {
        Ok(response) => (response.status(), response.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(status, response)) => {
            (status, response.into_string().unwrap_or_default())
        }
        Err(err) => panic!("transport error for {url}: {err}"),
    };
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn create_fixture(daemon: &Daemon) {
    let (status, body) = daemon.post_with("/v1/projects", &json!({ "fixture": "hello-pg" }));
    assert_eq!(status, 201, "create the fixture Project: {body}");
}

fn readiness(daemon: &Daemon) -> (u16, Value) {
    daemon.get("/v1/system/update_readiness")
}

#[test]
fn update_readiness_is_idle_when_nothing_is_running() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot_with(Config::new(temp.path()));

    let (status, body) = readiness(&daemon);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operations_in_flight"], false, "{body}");
    assert_eq!(body["adopted"], true, "{body}");
    assert_eq!(body["degraded_projects"], json!([]), "{body}");
}

#[test]
fn update_readiness_reports_an_operation_waiting_for_approval() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot_with(Config::new(temp.path()));
    create_fixture(&daemon);

    // A destructive mutation becomes an operation in `waiting_for_approval`, which
    // is still "in flight": an update must refuse while a human is deciding.
    let (reset_status, reset_body) =
        daemon.post_with("/v1/projects/hello-pg/environments/dev/reset", &json!({}));
    assert_eq!(reset_status, 202, "{reset_body}");

    let (status, body) = readiness(&daemon);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operations_in_flight"], true, "{body}");
}

#[test]
fn update_readiness_reports_a_degraded_adoption() {
    let temp = tempfile::tempdir().expect("temp dir");
    // The fake never binds `web`'s port, so the daemon's own probe finds the
    // Environment degraded after start.
    let config = Config::new(temp.path()).with_backend_script(r#"{"unbound_resources": ["web"]}"#);
    let daemon = Daemon::boot_with(config);
    create_fixture(&daemon);

    let (start_status, started) =
        daemon.post_with("/v1/projects/hello-pg/environments/dev/start", &json!({}));
    assert_eq!(start_status, 200, "{started}");
    assert_eq!(started["environment"]["state"], "degraded", "{started}");

    let (status, body) = readiness(&daemon);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operations_in_flight"], false, "{body}");
    assert_eq!(body["adopted"], false, "{body}");
    assert_eq!(body["degraded_projects"], json!(["hello-pg"]), "{body}");
}

#[test]
fn shutdown_stops_the_daemon_and_removes_its_discovery() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let running = agentify_daemon::start_in_background(
        Config::new(home).with_secret_env([("HELLO_API_TOKEN", "fixture-api-token")]),
    )
    .expect("boot daemon");

    let url = format!("{}/v1/system/shutdown", running.base_url());
    let token = running.token().to_string();
    let response = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(20))
        .send_string("{}");
    assert_eq!(response.expect("shutdown response").status(), 202);

    running.wait().expect("the daemon exits cleanly");
    assert!(
        !agentify_proto::discovery_path(home).exists(),
        "a shut-down daemon removes its discovery file"
    );
}

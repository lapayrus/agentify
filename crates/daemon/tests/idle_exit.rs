//! The idle exit, at the public API seam.
//!
//! A daemon nobody owns must not stay resident: an MCP shim starts one for its
//! session, and it ends when the Agent stops asking. Work holds it open — a
//! running Environment or an Operation in flight — because exiting under either
//! would be a silent failure.

use std::path::Path;
use std::time::{Duration, Instant};

use agentify_daemon::Config;
use serde_json::{json, Value};

const SHORT: Duration = Duration::from_millis(300);

fn boot(home: &Path, idle: Duration) -> agentify_daemon::RunningDaemon {
    agentify_daemon::start_in_background(
        Config::new(home)
            .with_idle_exit(idle)
            .with_secret_env([("HELLO_API_TOKEN", "fixture-api-token")]),
    )
    .expect("boot daemon")
}

fn post(url: &str, token: &str, body: &Value) -> u16 {
    let response = ureq::post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(20))
        .send_string(&body.to_string());
    match response {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(err) => panic!("transport error for {url}: {err}"),
    }
}

fn healthy(url: &str) -> bool {
    matches!(ureq::get(&format!("{url}/v1/healthz")).call(), Ok(response) if response.status() == 200)
}

fn await_gone(home: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !agentify_proto::discovery_path(home).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn an_idle_daemon_exits_and_removes_its_discovery_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let mut daemon = boot(home, SHORT);

    assert!(
        await_gone(home, Duration::from_secs(5)),
        "the daemon did not idle out within five seconds"
    );
    daemon.shutdown().expect("the daemon exits cleanly");
}

#[test]
fn a_running_environment_holds_the_daemon_open() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let mut daemon = boot(home, SHORT);
    let url = daemon.base_url().to_string();
    let token = daemon.token().to_string();

    assert_eq!(
        post(
            &format!("{url}/v1/projects"),
            &token,
            &json!({ "fixture": "hello-pg" })
        ),
        201
    );
    assert_eq!(
        post(
            &format!("{url}/v1/projects/hello-pg/environments/dev/start"),
            &token,
            &json!({})
        ),
        200
    );

    // Longer than the idle window, and still answering: the Environment is the
    // reason, not a heartbeat.
    std::thread::sleep(Duration::from_millis(900));
    assert!(
        healthy(&url),
        "a running Environment must keep the daemon open"
    );

    assert_eq!(
        post(
            &format!("{url}/v1/projects/hello-pg/environments/dev/stop"),
            &token,
            &json!({})
        ),
        200
    );
    assert!(
        await_gone(home, Duration::from_secs(5)),
        "once nothing runs, the daemon idles out"
    );
    daemon.shutdown().expect("the daemon exits cleanly");
}

#[test]
fn an_operation_in_flight_holds_the_daemon_open() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let mut daemon = boot(home, SHORT);
    let url = daemon.base_url().to_string();
    let token = daemon.token().to_string();

    assert_eq!(
        post(
            &format!("{url}/v1/projects"),
            &token,
            &json!({ "fixture": "hello-pg" })
        ),
        201
    );
    // A reset waits for an Approval, which is an Operation in flight.
    assert_eq!(
        post(
            &format!("{url}/v1/projects/hello-pg/environments/dev/reset"),
            &token,
            &json!({})
        ),
        202
    );

    std::thread::sleep(Duration::from_millis(900));
    assert!(
        healthy(&url),
        "an Operation in flight must keep the daemon open"
    );
    daemon.shutdown().expect("the daemon exits cleanly");
}

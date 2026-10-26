//! Acceptance tests for the CLI, at the same seam as everything else: boot a
//! daemon, run the real `agentify` binary, read its output.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn agentify(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentify"))
        .args(args)
        .env("AGENTIFY_HOME", home)
        .output()
        .expect("run agentify")
}

#[test]
fn whoami_prints_the_daemon_payload_as_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");

    let output = agentify(temp.path(), &["whoami"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value =
        serde_json::from_slice(&output.stdout).expect("whoami prints JSON on stdout");
    assert_eq!(payload["api_version"], "v1");
    assert!(payload["project"].is_null());
    assert_eq!(payload["scopes"], serde_json::json!(["read", "operate"]));

    daemon.shutdown().expect("stop daemon");
}

#[test]
fn without_a_daemon_the_cli_fails_loudly_and_names_the_data_root() {
    let temp = tempfile::tempdir().expect("temp dir");

    let output = agentify(temp.path(), &["whoami"]);
    assert!(!output.status.success(), "a missing daemon is an error");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon unreachable"),
        "the message names the failure: {stderr}"
    );
    assert!(
        stderr.contains("AGENTIFY_HOME"),
        "the message names the knob that points at the data root: {stderr}"
    );
}

#[test]
fn status_reports_a_connected_daemon() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");

    let output = agentify(temp.path(), &["status"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("daemon connected"), "got: {stdout}");
    assert!(stdout.contains(daemon.base_url()), "got: {stdout}");

    daemon.shutdown().expect("stop daemon");
}

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

#[test]
fn bootstrap_without_a_flag_only_reports_and_changes_nothing() {
    // A read: no elevation, no RunOnce entry, no install. On a machine that
    // already has WSL 2 this is the "Ready and do nothing" path; on a machine
    // without it, it is the "someone has to agree" path. Either way the exit
    // code says which, and the registry value is exactly as it was.
    let temp = tempfile::tempdir().expect("temp dir");
    let before = pending_resume();
    let output = agentify(temp.path(), &["bootstrap"]);
    let after = pending_resume();
    assert_eq!(
        before, after,
        "reporting the bootstrap state must not write a resume entry"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        // Ready: nothing to do, and nothing was asked for.
        Some(0) => {
            assert!(stdout.contains("bootstrap: ready"), "got: {stdout}");
            assert!(
                !stdout.contains("elevated step"),
                "a ready machine is asked for no elevated step: {stdout}"
            );
            assert!(
                !stdout.to_ascii_lowercase().contains("administrator prompt"),
                "nothing is proposed on a ready machine: {stdout}"
            );
        }
        // A human action is still needed: the output says which, and offers no
        // install without consent.
        Some(2) => {
            assert!(
                stdout.contains("consent_required")
                    || stdout.contains("reboot_required")
                    || stdout.contains("missing"),
                "got: {stdout} stderr: {stderr}"
            );
            assert!(
                stdout.contains("agentify bootstrap --consent")
                    || stdout.contains("elevated step"),
                "the output names the human step: {stdout}"
            );
        }
        other => panic!(
            "unexpected exit {other:?}; stdout: {stdout} stderr: {stderr}"
        ),
    }
}

#[test]
fn bootstrap_rejects_an_unknown_action_without_touching_anything() {
    let temp = tempfile::tempdir().expect("temp dir");
    let output = agentify(temp.path(), &["bootstrap", "--install-silently"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown bootstrap action"), "{stderr}");
    assert!(stderr.contains("--consent"), "it says what to use: {stderr}");
}

/// The shipped resume value, read without elevation: `None` on a machine that is
/// not mid-install, which is what a test run must leave behind.
fn pending_resume() -> Option<String> {
    use agentify_bootstrap::{ResumeStore, RunOnce, RESUME_VALUE_NAME};
    RunOnce.pending(RESUME_VALUE_NAME).expect("read RunOnce")
}

//! The shell's only real logic, tested against a real daemon: what the window
//! shows comes from the discovery file and `GET /v1/whoami`.

use agentify_shell::{status_for, DaemonStatus};

#[test]
fn a_running_daemon_is_reported_as_connected_with_the_whoami_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");

    match status_for(temp.path()) {
        DaemonStatus::Connected { whoami, data_root } => {
            assert_eq!(whoami["api_version"], "v1");
            assert!(whoami["project"].is_null());
            assert_eq!(whoami["scopes"], serde_json::json!(["read", "operate"]));
            assert_eq!(data_root, temp.path().display().to_string());
        }
        other => panic!("expected a connected shell, got {other:?}"),
    }

    daemon.shutdown().expect("stop daemon");
}

#[test]
fn a_missing_daemon_is_reported_as_unreachable_with_a_reason() {
    let temp = tempfile::tempdir().expect("temp dir");

    match status_for(temp.path()) {
        DaemonStatus::Unreachable { reason, .. } => {
            assert!(
                reason.contains("daemon unreachable"),
                "the shell explains itself: {reason}"
            );
        }
        other => panic!("expected an unreachable shell, got {other:?}"),
    }
}

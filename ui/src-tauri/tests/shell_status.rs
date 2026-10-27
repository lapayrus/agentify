//! The shell's only real logic, tested against a real daemon: what the window
//! shows comes from the discovery file and `GET /v1/whoami` / `GET /v1/projects`.

use agentify_shell::{status_for, DaemonStatus};

#[test]
fn a_running_daemon_is_reported_as_connected_with_the_whoami_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");

    match status_for(temp.path()) {
        DaemonStatus::Connected {
            whoami,
            projects,
            projects_error,
            data_root,
        } => {
            assert_eq!(whoami["api_version"], "v1");
            assert!(whoami["project"].is_null());
            assert_eq!(whoami["scopes"], serde_json::json!(["read", "operate"]));
            assert_eq!(data_root, temp.path().display().to_string());
            assert!(projects.is_empty(), "no Projects exist yet");
            assert!(projects_error.is_none());
        }
        other => panic!("expected a connected shell, got {other:?}"),
    }

    daemon.shutdown().expect("stop daemon");
}

#[test]
fn a_created_project_reaches_the_shell_with_its_environment_resources_and_urls() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");

    ureq::post(&format!("{}/v1/projects", daemon.base_url()))
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({ "fixture": "hello-pg" }).to_string())
        .expect("create the fixture Project");

    match status_for(temp.path()) {
        DaemonStatus::Connected {
            projects,
            projects_error,
            ..
        } => {
            assert!(projects_error.is_none(), "{projects_error:?}");
            assert_eq!(projects.len(), 1, "the rail lists the Project: {projects:?}");
            let project = &projects[0];
            assert_eq!(project["name"], "hello-pg");
            assert_eq!(project["environment"]["name"], "dev");
            assert_eq!(
                project["routes"][0]["url"],
                "https://web.hello-pg.localhost"
            );
            assert_eq!(project["resources"].as_array().expect("resources").len(), 2);
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

#[test]
fn start_and_stop_from_the_shell_go_through_the_same_api() {
    use agentify_shell::{
        environment_action_for, environment_logs_for, environment_stats_for, environment_status_for,
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
        .expect("boot daemon");
    ureq::post(&format!("{}/v1/projects", daemon.base_url()))
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({ "fixture": "hello-pg" }).to_string())
        .expect("create the fixture Project");

    let before = environment_status_for(temp.path(), "hello-pg").expect("status");
    assert_eq!(before["environment"]["state"], "stopped");
    assert_eq!(before["backend"]["name"], "fake");

    let started = environment_action_for(temp.path(), "hello-pg", "start");
    assert!(started.ok, "start failed: {started:?}");
    assert_eq!(started.action, "start");
    assert_eq!(
        started.status.as_ref().expect("status")["environment"]["state"],
        "running"
    );

    let stats = environment_stats_for(temp.path(), "hello-pg").expect("stats");
    assert_eq!(stats["state"], "running");
    assert!(stats["memoryBytes"].as_u64().unwrap_or(0) > 0, "{stats}");
    assert_eq!(stats["memory"], "96 MB");

    let logs = environment_logs_for(temp.path(), "hello-pg", Some(20), None).expect("logs");
    let lines = logs["lines"].as_array().expect("lines");
    assert!(!lines.is_empty(), "the Logs tab has something to show: {logs}");
    assert!(logs["nextSince"].is_string(), "the cursor drives following: {logs}");

    let stopped = environment_action_for(temp.path(), "hello-pg", "stop");
    assert!(stopped.ok, "stop failed: {stopped:?}");
    assert_eq!(
        stopped.status.as_ref().expect("status")["environment"]["state"],
        "stopped"
    );

    // An unknown action never reaches the daemon.
    let unknown = environment_action_for(temp.path(), "hello-pg", "reset");
    assert!(!unknown.ok);
    assert_eq!(unknown.code.as_deref(), Some("ValidationFailed"));

    // A typed daemon failure reaches the shell with its code.
    let missing = environment_action_for(temp.path(), "nope", "start");
    assert!(!missing.ok);
    assert_eq!(missing.code.as_deref(), Some("NotFound"));

    daemon.shutdown().expect("stop daemon");
}

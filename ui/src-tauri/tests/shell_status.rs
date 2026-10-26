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

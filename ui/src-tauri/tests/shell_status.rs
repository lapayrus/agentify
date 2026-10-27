//! The shell's only real logic, tested against a real daemon: what the window
//! shows comes from the discovery file and `GET /v1/whoami` / `GET /v1/projects`.

use agentify_shell::{status_for, DaemonStatus};

/// The value the bundled fixture imports for its `api-token`. A test supplies it
/// explicitly, so the shell's suite depends on no shell variable on this machine.
const FIXTURE_API_TOKEN: &str = "fixture-api-token-from-the-host";

/// A daemon for these tests: the in-memory secret store, and the fixture's
/// imported value, so no test touches this machine's credential manager.
fn config(home: &std::path::Path) -> agentify_daemon::Config {
    agentify_daemon::Config::new(home).with_secret_env([("HELLO_API_TOKEN", FIXTURE_API_TOKEN)])
}

#[test]
fn a_running_daemon_is_reported_as_connected_with_the_whoami_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
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
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
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
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
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

#[test]
fn the_shell_shows_the_approval_inbox_and_decides_through_the_same_api() {
    use agentify_shell::{activity_for, approvals_for, decide_approval_for};

    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
        .expect("boot daemon");
    ureq::post(&format!("{}/v1/projects", daemon.base_url()))
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({ "fixture": "hello-pg" }).to_string())
        .expect("create the fixture Project");

    // An Agent asks for a destructive call: it lands in the inbox, nothing more.
    let accepted: serde_json::Value = serde_json::from_str(
        &ureq::request(
            "DELETE",
            &format!("{}/v1/projects/hello-pg", daemon.base_url()),
        )
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .call()
        .expect("DELETE the Project")
        .into_string()
        .expect("body"),
    )
    .expect("202 JSON");
    let approval = accepted["approvalId"].as_str().expect("approval id").to_string();
    let operation = accepted["operationId"].as_str().expect("operation id").to_string();
    let params = accepted["paramsHash"].as_str().expect("params hash").to_string();

    // The card the shell renders comes straight from `GET /v1/approvals`.
    let inbox = approvals_for(temp.path(), None, None).expect("the inbox");
    let cards = inbox["approvals"].as_array().expect("approvals");
    assert_eq!(cards.len(), 1, "{inbox}");
    assert_eq!(cards[0]["operation"], "delete_project");
    assert_eq!(cards[0]["target"], "hello-pg");
    assert_eq!(cards[0]["state"], "pending");
    assert_eq!(cards[0]["requester"]["kind"], "agent");
    assert_eq!(cards[0]["paramsHash"], params.as_str());
    assert!(!cards[0]["parameters"].as_array().expect("parameters").is_empty());
    assert!(cards[0]["expiresAt"].is_string());
    assert_eq!(
        approvals_for(temp.path(), None, Some(1))
            .expect("a bounded inbox")["approvals"]
            .as_array()
            .expect("approvals")
            .len(),
        1,
        "the limit is honoured"
    );

    // The feed the shell renders is the audit chain, and it verifies.
    let feed = activity_for(temp.path(), Some(50)).expect("the feed");
    assert_eq!(feed["integrity"]["verified"], true, "{feed}");
    let entries = feed["entries"].as_array().expect("entries");
    assert!(
        entries.iter().any(|entry| entry["operation"] == "delete_project"
            && entry["result"] == "requested"
            && entry["actor"]["kind"] == "agent"
            && entry["approvalId"] == approval.as_str()),
        "the request is in the feed: {feed}"
    );

    // Approving without the OS gesture is refused, and the shell reports why.
    let refused = decide_approval_for(temp.path(), &approval, true, None, None);
    assert!(!refused.ok, "an Agent cannot produce the gesture: {refused:?}");
    assert_eq!(refused.code.as_deref(), Some("ApprovalRequired"));
    assert!(refused.detail.is_some());

    // Denying needs no gesture, and leaves the Project exactly as it was.
    let denied = decide_approval_for(
        temp.path(),
        &approval,
        false,
        Some(&operation),
        Some(&params),
    );
    assert!(denied.ok, "deny failed: {denied:?}");
    assert_eq!(denied.approval.as_ref().expect("approval")["state"], "denied");
    assert_eq!(
        denied.approval.as_ref().expect("approval")["decidedBy"]["kind"],
        "human"
    );
    assert_eq!(
        denied.operation.as_ref().expect("operation")["errorCode"],
        "ApprovalDenied"
    );
    assert!(
        matches!(
            status_for(temp.path()),
            DaemonStatus::Connected { ref projects, .. } if projects.len() == 1
        ),
        "a denial leaves the Project alone"
    );
    assert!(
        approvals_for(temp.path(), None, None)
            .expect("the inbox")
            ["approvals"]
            .as_array()
            .expect("approvals")
            .is_empty(),
        "the inbox holds pending work only"
    );
    let decided = approvals_for(temp.path(), Some("all"), None).expect("all Approvals");
    assert_eq!(decided["approvals"][0]["state"], "denied");

    // A decision on an Approval that does not exist is a typed failure.
    let missing = decide_approval_for(temp.path(), "nope", false, None, None);
    assert!(!missing.ok);
    assert_eq!(missing.code.as_deref(), Some("NotFound"));

    daemon.shutdown().expect("stop daemon");
}

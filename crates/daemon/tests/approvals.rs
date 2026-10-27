//! Approvals, the audit chain, and the activity feed, at the public API seam.
//!
//! Every test boots the real daemon against a temporary data root and talks to
//! it the way an Agent or the desktop shell does: read the discovery file, then
//! speak HTTP. Nothing here reaches into daemon internals, and no test can open
//! a real OS prompt: the consent gesture is always a scripted verifier.

use std::path::Path;
use std::time::Duration;

use agentify_daemon::consent::ConsentMode;
use agentify_daemon::ports::PortRange;
use agentify_daemon::Config;
use serde_json::{json, Value};

/// A daemon booted by a test, shut down on drop.
struct Daemon {
    inner: Option<agentify_daemon::RunningDaemon>,
}

impl Daemon {
    /// A daemon where a human passes the OS gesture when asked.
    fn granting(home: &Path) -> Daemon {
        Daemon::boot(Config::new(home).with_consent(ConsentMode::Granted))
    }

    /// A daemon with nobody at the console: the gesture cannot be passed.
    fn refusing(home: &Path) -> Daemon {
        Daemon::boot(Config::new(home).with_consent(ConsentMode::Declined))
    }

    fn boot(config: Config) -> Daemon {
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
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("POST", path, Some(&body.to_string()))
    }

    fn delete(&self, path: &str) -> (u16, Value) {
        self.request("DELETE", path, None)
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let url = self.url(path);
        let mut call = match method {
            "GET" => ureq::get(&url),
            "DELETE" => ureq::request("DELETE", &url),
            _ => ureq::post(&url),
        };
        call = call
            .set("Authorization", &format!("Bearer {}", self.token()))
            .timeout(Duration::from_secs(20));
        if body.is_some() {
            call = call.set("Content-Type", "application/json");
        }
        let result = match body {
            Some(body) => call.send_string(body),
            None => call.call(),
        };
        let (status, text) = match result {
            Ok(response) => (
                response.status(),
                response.into_string().unwrap_or_default(),
            ),
            Err(ureq::Error::Status(status, response)) => {
                (status, response.into_string().unwrap_or_default())
            }
            Err(err) => panic!("transport error for {url}: {err}"),
        };
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Wait for an operation to finish, and return its view.
    fn wait(&self, operation_id: &str) -> Value {
        let (status, operation) = self.get(&format!(
            "/v1/operations/{operation_id}/wait?timeoutMs=15000"
        ));
        assert_eq!(status, 200, "waiting for {operation_id}: {operation}");
        operation
    }

    fn shutdown(&mut self) {
        if let Some(mut daemon) = self.inner.take() {
            daemon.shutdown().expect("stop daemon");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `create_project` from the bundled fixture, and check it landed.
fn create_fixture(daemon: &Daemon) {
    let (status, body) = daemon.post("/v1/projects", &json!({ "fixture": "hello-pg" }));
    assert_eq!(status, 201, "create the fixture Project: {body}");
}

fn project(daemon: &Daemon, name: &str) -> Value {
    let (status, body) = daemon.get(&format!("/v1/projects/{name}"));
    assert_eq!(status, 200, "read Project {name}: {body}");
    body
}

fn start(daemon: &Daemon, name: &str) {
    let (status, body) = daemon.post(
        &format!("/v1/projects/{name}/environments/dev/start"),
        &json!({}),
    );
    assert_eq!(status, 200, "start {name}: {body}");
}

fn exec(daemon: &Daemon, name: &str, command: &[&str]) {
    let (status, body) = daemon.post(
        &format!("/v1/projects/{name}/environments/dev/resources/web/exec"),
        &json!({ "command": command }),
    );
    assert_eq!(status, 200, "exec {command:?}: {body}");
}

/// Every log line the daemon reports for the Environment.
fn log_lines(daemon: &Daemon, name: &str) -> Vec<String> {
    let (status, body) = daemon.get(&format!(
        "/v1/projects/{name}/environments/dev/logs?tail=500"
    ));
    assert_eq!(status, 200, "logs: {body}");
    body["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| line["line"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn leased_ports(daemon: &Daemon, name: &str) -> Vec<u16> {
    let (status, body) = daemon.get(&format!(
        "/v1/projects/{name}/environments/dev/status"
    ));
    assert_eq!(status, 200, "status: {body}");
    let mut ports: Vec<u16> = body["resources"]
        .as_array()
        .map(|resources| {
            resources
                .iter()
                .flat_map(|resource| resource["ports"].as_array().cloned().unwrap_or_default())
                .filter_map(|port| port["hostPort"].as_u64())
                .map(|port| port as u16)
                .collect()
        })
        .unwrap_or_default();
    ports.sort_unstable();
    ports
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    listener.local_addr().expect("addr").port()
}

fn approval_id(accepted: &Value) -> String {
    accepted["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("no approvalId in {accepted}"))
        .to_string()
}

fn operation_id(accepted: &Value) -> String {
    accepted["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("no operationId in {accepted}"))
        .to_string()
}

/// The pending approvals the inbox shows.
fn pending(daemon: &Daemon) -> Vec<Value> {
    let (status, body) = daemon.get("/v1/approvals");
    assert_eq!(status, 200, "{body}");
    body["approvals"].as_array().cloned().unwrap_or_default()
}

fn approve(daemon: &Daemon, id: &str) -> (u16, Value) {
    daemon.post(&format!("/v1/approvals/{id}/approve"), &json!({}))
}

fn deny(daemon: &Daemon, id: &str) -> (u16, Value) {
    daemon.post(&format!("/v1/approvals/{id}/deny"), &json!({}))
}

fn activity(daemon: &Daemon) -> Value {
    let (status, body) = daemon.get("/v1/activity?limit=200");
    assert_eq!(status, 200, "{body}");
    body
}

// ---------------------------------------------------------------------------
// 202 with ids, and nothing changed yet.
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_project_returns_202_with_ids_and_changes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    let ports = leased_ports(&daemon, "hello-pg");
    assert!(!ports.is_empty());
    let project_id = project(&daemon, "hello-pg")["id"]
        .as_str()
        .expect("project id")
        .to_string();

    let (status, accepted) = daemon.delete("/v1/projects/hello-pg");
    assert_eq!(status, 202, "a destructive call asks first: {accepted}");
    assert_eq!(accepted["operation"], "delete_project");
    assert_eq!(accepted["target"], "hello-pg");
    assert!(!approval_id(&accepted).is_empty());
    assert!(!operation_id(&accepted).is_empty());
    assert!(
        accepted["expiresAt"].as_str().unwrap_or_default().contains('T'),
        "the Approval says when it dies: {accepted}"
    );

    // Nothing has changed: the Project, its Environment, its ports, its files.
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    assert_eq!(leased_ports(&daemon, "hello-pg"), ports, "the leases stand");
    assert!(temp
        .path()
        .join("projects")
        .join(&project_id)
        .join("workspace")
        .is_dir());

    // The operation is not finished: it is waiting for a human.
    let (_, operation) = daemon.get(&format!("/v1/operations/{}", operation_id(&accepted)));
    assert_eq!(
        operation["state"], "waiting_for_approval",
        "the operation waits instead of running: {operation}"
    );
}

#[test]
fn resetting_an_environment_returns_202_with_ids_and_changes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    exec(&daemon, "hello-pg", &["echo", "before-reset"]);
    let ports = leased_ports(&daemon, "hello-pg");
    assert!(log_lines(&daemon, "hello-pg")
        .iter()
        .any(|line| line.contains("before-reset")));

    let (status, accepted) = daemon.post(
        "/v1/projects/hello-pg/environments/dev/reset",
        &json!({}),
    );
    assert_eq!(status, 202, "reset asks first: {accepted}");
    assert_eq!(accepted["operation"], "reset_environment");
    assert_eq!(accepted["target"], "hello-pg/dev");
    assert!(!approval_id(&accepted).is_empty());
    assert!(!operation_id(&accepted).is_empty());

    // The data and the runtime are untouched while the Approval is pending.
    assert!(log_lines(&daemon, "hello-pg")
        .iter()
        .any(|line| line.contains("before-reset")));
    assert_eq!(leased_ports(&daemon, "hello-pg"), ports);
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
}

// ---------------------------------------------------------------------------
// The inbox.
// ---------------------------------------------------------------------------

#[test]
fn the_inbox_shows_the_pending_approval_with_everything_a_human_needs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    let token_id = {
        let (_, whoami) = daemon.get("/v1/whoami");
        whoami["actor"]["id"].as_str().expect("token id").to_string()
    };

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let approvals = pending(&daemon);
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    let card = &approvals[0];
    assert_eq!(card["id"], approval_id(&accepted));
    assert_eq!(card["operation"], "delete_project");
    assert_eq!(card["target"], "hello-pg");
    assert_eq!(card["state"], "pending");
    assert_eq!(card["operationId"], operation_id(&accepted));
    assert_eq!(
        card["requester"]["kind"], "agent",
        "the card names who asked: {card}"
    );
    assert_eq!(card["requester"]["id"], token_id);
    assert_eq!(
        card["paramsHash"].as_str().unwrap_or_default().len(),
        64,
        "the Approval is bound to a hash of the operation and its parameters: {card}"
    );
    let parameters = card["parameters"].as_array().expect("a parameter summary");
    assert!(
        parameters
            .iter()
            .any(|parameter| parameter["name"] == "project" && parameter["value"] == "hello-pg"),
        "the summary says what would be deleted: {card}"
    );
    assert!(card["expiresAt"].as_str().is_some(), "{card}");

    // Approving empties the inbox, and the decided Approval is still readable.
    let (status, decided) = approve(&daemon, &approval_id(&accepted));
    assert_eq!(status, 200, "{decided}");
    assert!(pending(&daemon).is_empty(), "the inbox holds pending work only");
    let (status, all) = daemon.get("/v1/approvals?state=all");
    assert_eq!(status, 200);
    assert_eq!(all["approvals"][0]["state"], "approved", "{all}");
    assert_eq!(all["approvals"][0]["decidedBy"]["kind"], "human");
}

// ---------------------------------------------------------------------------
// The gate: an Agent cannot pass the gesture.
// ---------------------------------------------------------------------------

#[test]
fn an_agent_token_without_the_gesture_cannot_complete_an_approval() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::refusing(temp.path());
    create_fixture(&daemon);

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let id = approval_id(&accepted);

    let (status, refused) = approve(&daemon, &id);
    assert_eq!(
        status, 403,
        "the Agent holds the token but not the gesture: {refused}"
    );
    assert_eq!(refused["code"], "ApprovalRequired");
    assert_eq!(refused["approvalId"], id, "the refusal names the Approval");

    // Still pending, still nothing changed.
    assert_eq!(pending(&daemon).len(), 1);
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    let (_, operation) = daemon.get(&format!("/v1/operations/{}", operation_id(&accepted)));
    assert_eq!(operation["state"], "waiting_for_approval", "{operation}");

    // A denied gesture is audited, with the Agent as the actor.
    let entries = activity(&daemon);
    assert!(
        entries["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["errorCode"] == "ApprovalRequired"
                && entry["actor"]["kind"] == "agent"),
        "the failed attempt is recorded: {entries}"
    );
}

// ---------------------------------------------------------------------------
// Approve: the operation runs, and the result is verified.
// ---------------------------------------------------------------------------

#[test]
fn approving_deletes_the_project_and_the_result_is_verified() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    let ports = leased_ports(&daemon, "hello-pg");
    let project_id = project(&daemon, "hello-pg")["id"]
        .as_str()
        .expect("project id")
        .to_string();

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let (status, decided) = approve(&daemon, &approval_id(&accepted));
    assert_eq!(status, 200, "{decided}");
    assert_eq!(decided["approval"]["state"], "approved");
    assert_eq!(decided["approval"]["decidedBy"]["kind"], "human");

    let operation = daemon.wait(&operation_id(&accepted));
    assert_eq!(operation["state"], "succeeded", "{operation}");
    assert_eq!(operation["kind"], "delete_project");
    assert!(!operation["result"].is_null(), "the waiter gets a result");

    // The Project rows, the runtime, the volumes, the leases, and the source.
    let (status, missing) = daemon.get("/v1/projects/hello-pg");
    assert_eq!(status, 404, "{missing}");
    assert_eq!(missing["code"], "NotFound");
    let (_, listed) = daemon.get("/v1/projects");
    assert_eq!(listed["projects"].as_array().expect("projects").len(), 0);
    for port in ports {
        assert!(!port_open(port), "port {port} was released");
    }
    assert!(
        !temp.path().join("projects").join(&project_id).exists(),
        "the Project's workspace is gone"
    );

    // Deleting it again is a typed 404, not a second Approval.
    let (status, again) = daemon.delete("/v1/projects/hello-pg");
    assert_eq!(status, 404, "{again}");
    assert_eq!(again["code"], "NotFound");
}

#[test]
fn approving_resets_the_data_and_restarts_the_environment() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    exec(&daemon, "hello-pg", &["echo", "before-reset"]);
    let ports = leased_ports(&daemon, "hello-pg");

    let (_, accepted) = daemon.post(
        "/v1/projects/hello-pg/environments/dev/reset",
        &json!({}),
    );
    let (status, decided) = approve(&daemon, &approval_id(&accepted));
    assert_eq!(status, 200, "{decided}");

    let operation = daemon.wait(&operation_id(&accepted));
    assert_eq!(operation["state"], "succeeded", "{operation}");
    assert_eq!(operation["kind"], "reset_environment");

    // The Project is still there, restarted, on the same leases...
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    let (_, status_snapshot) = daemon.get("/v1/projects/hello-pg/environments/dev/status");
    assert_eq!(
        status_snapshot["environment"]["state"], "running",
        "reset restarts what it cleared: {status_snapshot}"
    );
    assert_eq!(leased_ports(&daemon, "hello-pg"), ports);
    for port in &ports {
        assert!(port_open(*port), "port {port} is bound again");
    }

    // ...and its data is gone: the fake's in-memory lines do not survive.
    let lines = log_lines(&daemon, "hello-pg");
    assert!(
        !lines.iter().any(|line| line.contains("before-reset")),
        "reset cleared the Environment's data: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("resource started")),
        "the Environment came back up: {lines:?}"
    );
}

// ---------------------------------------------------------------------------
// Single use, expiry, and parameter binding.
// ---------------------------------------------------------------------------

#[test]
fn an_approval_is_single_use() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let id = approval_id(&accepted);
    let (status, _) = approve(&daemon, &id);
    assert_eq!(status, 200);

    let (status, replay) = approve(&daemon, &id);
    assert_eq!(status, 409, "a yes cannot be spent twice: {replay}");
    assert_eq!(replay["code"], "ApprovalConsumed");
    assert_eq!(replay["approvalId"], id);

    // Denying a spent Approval is refused too.
    let (status, refused) = deny(&daemon, &id);
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["code"], "ApprovalConsumed");

    daemon.wait(&operation_id(&accepted));
}

#[test]
fn an_expired_approval_is_rejected_and_its_operation_ends() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(
        Config::new(temp.path())
            .with_consent(ConsentMode::Granted)
            .with_approval_ttl(Duration::from_millis(200)),
    );
    create_fixture(&daemon);

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let id = approval_id(&accepted);

    // Nobody decides before the deadline.
    std::thread::sleep(Duration::from_millis(400));
    let (status, expired) = approve(&daemon, &id);
    assert_eq!(
        status, 410,
        "an Approval past its deadline cannot be spent: {expired}"
    );
    assert_eq!(expired["code"], "ApprovalExpired", "{expired}");
    assert_eq!(expired["approvalId"], id);

    // The waiter is released instead of hanging forever.
    let operation = daemon.wait(&operation_id(&accepted));
    assert_eq!(operation["state"], "failed", "{operation}");
    assert_eq!(operation["errorCode"], "ApprovalExpired", "{operation}");

    // The target is untouched.
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    let (_, all) = daemon.get("/v1/approvals?state=all");
    assert_eq!(all["approvals"][0]["state"], "expired", "{all}");
}

#[test]
fn an_approval_is_bound_to_its_operation_and_parameters() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    let one_port = |project: &str| {
        format!(
            "version: 1\nproject: {project}\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports:\n      - {{ port: 3000 }}\n"
        )
    };
    let (status, _) = daemon.post("/v1/projects", &json!({ "manifest": one_port("other") }));
    assert_eq!(status, 201);

    let (_, first) = daemon.delete("/v1/projects/hello-pg");
    let (_, second) = daemon.delete("/v1/projects/other");
    assert_ne!(approval_id(&first), approval_id(&second));
    assert_ne!(
        first["paramsHash"], second["paramsHash"],
        "two different operations never share a parameter hash"
    );

    // Approving the first while claiming it covers the second is refused.
    let (status, mismatched) = daemon.post(
        &format!("/v1/approvals/{}/approve", approval_id(&first)),
        &json!({ "operationId": operation_id(&second) }),
    );
    assert_eq!(status, 409, "{mismatched}");
    assert_eq!(mismatched["code"], "ApprovalMismatch");

    let (status, mismatched) = daemon.post(
        &format!("/v1/approvals/{}/approve", approval_id(&first)),
        &json!({ "paramsHash": second["paramsHash"] }),
    );
    assert_eq!(status, 409, "{mismatched}");
    assert_eq!(mismatched["code"], "ApprovalMismatch");

    // Nothing ran.
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    assert_eq!(project(&daemon, "other")["name"], "other");
    assert_eq!(pending(&daemon).len(), 2);

    // Approving what the human was actually shown works.
    let (status, decided) = daemon.post(
        &format!("/v1/approvals/{}/approve", approval_id(&first)),
        &json!({ "operationId": operation_id(&first), "paramsHash": first["paramsHash"] }),
    );
    assert_eq!(status, 200, "{decided}");
    daemon.wait(&operation_id(&first));
    let (status, _) = daemon.get("/v1/projects/hello-pg");
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Deny.
// ---------------------------------------------------------------------------

#[test]
fn denying_leaves_the_target_unchanged_and_is_audited() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    let ports = leased_ports(&daemon, "hello-pg");

    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    let (status, decided) = deny(&daemon, &approval_id(&accepted));
    assert_eq!(status, 200, "{decided}");
    assert_eq!(decided["approval"]["state"], "denied");
    assert_eq!(decided["approval"]["decidedBy"]["kind"], "human");

    // The Project is untouched: still there, still running, still leased.
    assert_eq!(project(&daemon, "hello-pg")["name"], "hello-pg");
    assert_eq!(leased_ports(&daemon, "hello-pg"), ports);
    for port in ports {
        assert!(port_open(port), "port {port} was never released");
    }

    // The agent waiting on the operation is told, instead of hanging.
    let operation = daemon.wait(&operation_id(&accepted));
    assert_eq!(operation["state"], "failed", "{operation}");
    assert_eq!(operation["errorCode"], "ApprovalDenied", "{operation}");

    // The denial is on the record, with the human who made it.
    let entries = activity(&daemon);
    assert!(
        entries["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["operation"] == "delete_project"
                && entry["result"] == "denied"
                && entry["actor"]["kind"] == "human"
                && entry["approvalId"] == approval_id(&accepted)),
        "the denial is audited with its actor: {entries}"
    );

    // A denied Approval cannot be approved afterwards.
    let (status, refused) = approve(&daemon, &approval_id(&accepted));
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["code"], "ApprovalDenied");
}

// ---------------------------------------------------------------------------
// The audit chain and the activity feed.
// ---------------------------------------------------------------------------

#[test]
fn the_activity_feed_is_hash_chained_and_covers_every_mutation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());

    // A fresh daemon has an empty, verified chain.
    let empty = activity(&daemon);
    assert_eq!(empty["entries"].as_array().expect("entries").len(), 0, "{empty}");
    assert_eq!(empty["integrity"]["verified"], true, "{empty}");
    assert_eq!(empty["integrity"]["entries"], 0);

    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    exec(&daemon, "hello-pg", &["echo", "audited"]);
    let (_, accepted) = daemon.delete("/v1/projects/hello-pg");
    approve(&daemon, &approval_id(&accepted));
    daemon.wait(&operation_id(&accepted));

    let page = activity(&daemon);
    assert_eq!(page["integrity"]["verified"], true, "the chain verifies: {page}");
    let entries = page["entries"].as_array().expect("entries");
    assert_eq!(
        page["integrity"]["entries"].as_i64().unwrap_or(0),
        entries.len() as i64,
        "the walk covers every entry it returns"
    );
    assert!(entries.len() >= 5, "every mutation is recorded: {page}");

    // Entries link: each one's prevHash is the hash of the entry before it.
    for pair in entries.windows(2) {
        assert_eq!(
            pair[1]["hash"], pair[0]["prevHash"],
            "the chain links newest to oldest: {page}"
        );
    }

    // The chain covers Agent and human actions, with identity, operation,
    // target, result, and a request id.
    let kinds: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["actor"]["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"agent"), "Agent actions are audited: {page}");
    assert!(kinds.contains(&"human"), "human decisions are audited: {page}");
    let operations: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["operation"].as_str())
        .collect();
    for expected in ["create_project", "start_environment", "exec", "delete_project"] {
        assert!(
            operations.contains(&expected),
            "{expected} is in the feed: {page}"
        );
    }
    for entry in entries {
        assert!(!entry["requestId"].as_str().unwrap_or_default().is_empty(), "{entry}");
        assert_eq!(entry["hash"].as_str().unwrap_or_default().len(), 64, "{entry}");
        assert_eq!(entry["prevHash"].as_str().unwrap_or_default().len(), 64, "{entry}");
        assert!(entry["at"].as_str().unwrap_or_default().contains('T'), "{entry}");
        assert!(!entry["result"].as_str().unwrap_or_default().is_empty(), "{entry}");
    }

    // A mutation records what it was bound to, and never a value.
    let deletion = entries
        .iter()
        .find(|entry| entry["operation"] == "delete_project" && entry["result"] == "succeeded")
        .unwrap_or_else(|| panic!("the delete result is audited: {page}"));
    assert_eq!(deletion["target"], "hello-pg");
    assert_eq!(
        deletion["paramsHash"], accepted["paramsHash"],
        "the audit records the parameter hash, not the parameters: {deletion}"
    );
    assert_eq!(deletion["approvalId"], approval_id(&accepted));
    assert!(deletion["durationMs"].as_u64().is_some());
    assert!(
        !page.to_string().contains("audited"),
        "the exec's output is a value, and values never reach the audit: {page}"
    );
}

#[test]
fn the_activity_feed_can_be_read_per_project_and_with_a_bound() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    create_fixture(&daemon);
    start(&daemon, "hello-pg");
    let project_id = project(&daemon, "hello-pg")["id"]
        .as_str()
        .expect("project id")
        .to_string();

    let (status, page) = daemon.get(&format!("/v1/activity?project={project_id}&limit=2"));
    assert_eq!(status, 200, "{page}");
    let entries = page["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "the bound is honoured: {page}");
    for entry in entries {
        assert_eq!(entry["projectId"], project_id, "{entry}");
    }
    assert_eq!(page["integrity"]["verified"], true);

    let (status, _) = daemon.get("/v1/activity?project=nope");
    assert_eq!(status, 200, "an unknown Project is an empty feed, not an error");
}

#[test]
fn approvals_and_activity_require_the_agent_token() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::granting(temp.path());
    for path in ["/v1/approvals", "/v1/activity"] {
        let url = daemon.url(path);
        let status = match ureq::get(&url).call() {
            Ok(response) => response.status(),
            Err(ureq::Error::Status(status, _)) => status,
            Err(err) => panic!("transport error for {url}: {err}"),
        };
        assert_eq!(status, 401, "{path} needs the token");
    }
    // Deciding on an Approval needs it too.
    let url = daemon.url("/v1/approvals/nope/approve");
    let status = match ureq::post(&url).send_string("{}") {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(err) => panic!("transport error for {url}: {err}"),
    };
    assert_eq!(status, 401, "a decision needs the token as well");
}

#[test]
fn a_reset_and_a_delete_share_the_port_lease_registry() {
    // Deleting a Project must give its host ports back to the machine, so the
    // next Project can lease them.
    let temp = tempfile::tempdir().expect("temp dir");
    let port = free_port();
    let daemon = Daemon::boot(
        Config::new(temp.path())
            .with_consent(ConsentMode::Granted)
            .with_lease_range(PortRange::new(port, port)),
    );
    let one_port = |project: &str| {
        format!(
            "version: 1\nproject: {project}\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports:\n      - {{ port: 3000 }}\n"
        )
    };
    let (status, _) = daemon.post("/v1/projects", &json!({ "manifest": one_port("first") }));
    assert_eq!(status, 201);
    start(&daemon, "first");
    assert_eq!(leased_ports(&daemon, "first"), vec![port]);

    let (_, accepted) = daemon.delete("/v1/projects/first");
    approve(&daemon, &approval_id(&accepted));
    daemon.wait(&operation_id(&accepted));
    assert!(!port_open(port), "the port came back to the machine");

    let (status, _) = daemon.post("/v1/projects", &json!({ "manifest": one_port("after-delete") }));
    assert_eq!(status, 201);
    start(&daemon, "after-delete");
    assert_eq!(leased_ports(&daemon, "after-delete"), vec![port]);
}

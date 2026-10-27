//! Acceptance tests for the MCP stdio shim.
//!
//! The shim is a client of the daemon API like any other, so these tests boot a
//! real daemon and speak line-delimited JSON-RPC to the real `agentify-mcp`
//! process.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

struct Shim {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Shim {
    fn spawn(home: &Path) -> Shim {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentify-mcp"))
            .env("AGENTIFY_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentify-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        // Never let a wedged shim hang the suite.
        let killer = std::thread::spawn({
            let pid = child.id();
            move || {
                std::thread::sleep(Duration::from_secs(30));
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        });
        std::mem::forget(killer);

        Shim {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, request: Value) {
        writeln!(self.stdin, "{request}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "the shim closed stdout");
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("not JSON ({err}): {line}"))
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The value the bundled fixture imports for its `api-token`. A test supplies it
/// explicitly, so the suite does not depend on how the machine's shell is set up.
const FIXTURE_API_TOKEN: &str = "fixture-api-token-from-the-host";

/// A daemon for these tests: the memory secret store, and the fixture's imported
/// value, so no test touches this machine's credential manager.
fn config(home: &Path) -> agentify_daemon::Config {
    agentify_daemon::Config::new(home).with_secret_env([("HELLO_API_TOKEN", FIXTURE_API_TOKEN)])
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "agentify-tests", "version": "0.0.0" }
        }
    })
}

#[test]
fn initialize_lists_tools_and_whoami_returns_the_http_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
        .expect("boot daemon");

    let whoami_url = format!("{}/v1/whoami", daemon.base_url());
    let http: Value = serde_json::from_str(
        &ureq::get(&whoami_url)
            .set(
                "Authorization",
                &format!("Bearer {}", daemon.token()),
            )
            .call()
            .expect("whoami over HTTP")
            .into_string()
            .expect("body"),
    )
    .expect("whoami JSON");

    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    let initialized = shim.read();
    assert_eq!(initialized["jsonrpc"], "2.0");
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert!(
        initialized["result"]["capabilities"]["tools"].is_object(),
        "the shim advertises tools: {initialized}"
    );
    assert_eq!(initialized["result"]["serverInfo"]["name"], "agentify");

    // Notifications must not be answered.
    shim.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    shim.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = shim.read();
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list returned no tools: {listed}"));
    let whoami_tool = tools
        .iter()
        .find(|tool| tool["name"] == "whoami")
        .unwrap_or_else(|| panic!("no whoami tool in {listed}"));
    assert_eq!(whoami_tool["inputSchema"]["type"], "object");

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "whoami", "arguments": {} }
    }));
    let called = shim.read();
    assert_eq!(called["id"], 3);
    assert_eq!(called["result"]["isError"], false);

    let text = called["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {called}"));
    let over_mcp: Value = serde_json::from_str(text).expect("whoami text is JSON");
    assert_eq!(
        over_mcp, http,
        "the MCP whoami payload must be identical to GET /v1/whoami"
    );
    assert_eq!(called["result"]["structuredContent"], http);

    daemon.shutdown().expect("stop daemon");
}

#[test]
fn project_tools_create_list_and_get_through_the_same_api() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
        .expect("boot daemon");
    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    shim.read();

    shim.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = shim.read();
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for name in ["create_project", "list_projects", "get_project", "whoami"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no {name} tool in {listed}"));
        assert_eq!(tool["inputSchema"]["type"], "object", "{name} needs a schema");
        assert!(
            tool["description"].as_str().unwrap_or_default().len() > 20,
            "{name} needs a description an Agent can act on"
        );
    }
    let create = tools
        .iter()
        .find(|tool| tool["name"] == "create_project")
        .expect("create_project");
    assert_eq!(
        create["inputSchema"]["properties"]["fixture"]["type"], "string",
        "create_project takes a fixture name: {create}"
    );
    assert_eq!(
        create["inputSchema"]["properties"]["manifest"]["type"], "string",
        "create_project takes an inline Manifest: {create}"
    );

    // Empty list first.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "list_projects", "arguments": {} }
    }));
    let empty = shim.read();
    assert_eq!(empty["result"]["isError"], false);
    assert_eq!(empty["result"]["structuredContent"]["projects"], json!([]));

    // Create from the bundled fixture.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": { "name": "create_project", "arguments": { "fixture": "hello-pg" } }
    }));
    let created = shim.read();
    assert_eq!(created["result"]["isError"], false, "{created}");
    assert_eq!(created["result"]["structuredContent"]["name"], "hello-pg");
    assert_eq!(
        created["result"]["structuredContent"]["environment"]["name"],
        "dev"
    );
    assert_eq!(
        created["result"]["structuredContent"]["resources"]
            .as_array()
            .expect("resources")
            .len(),
        2
    );

    // The same Project over plain HTTP, to prove the shim is only a mapping.
    let http: Value = serde_json::from_str(
        &ureq::get(&format!("{}/v1/projects/hello-pg", daemon.base_url()))
            .set("Authorization", &format!("Bearer {}", daemon.token()))
            .call()
            .expect("get over HTTP")
            .into_string()
            .expect("body"),
    )
    .expect("project JSON");
    assert_eq!(created["result"]["structuredContent"], http);

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": { "name": "list_projects", "arguments": {} }
    }));
    let listed = shim.read();
    assert_eq!(
        listed["result"]["structuredContent"]["projects"]
            .as_array()
            .expect("projects")
            .len(),
        1
    );

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": { "name": "get_project", "arguments": { "project": "hello-pg" } }
    }));
    let fetched = shim.read();
    assert_eq!(fetched["result"]["structuredContent"]["name"], "hello-pg");

    // A duplicate name is a typed error, not prose.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": "create_project", "arguments": { "fixture": "hello-pg" } }
    }));
    let duplicate = shim.read();
    assert_eq!(duplicate["error"]["data"]["code"], "AlreadyExists", "{duplicate}");

    // Validation failures carry the path of the offending field.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "create_project",
            "arguments": {
                "manifest": "version: 1\nproject: bad\nresources:\n  web:\n    kind: app\n    image: node:22\n    env: { TOKEN: \"${secret:nope}\" }\n"
            }
        }
    }));
    let invalid = shim.read();
    assert_eq!(invalid["error"]["data"]["code"], "ValidationFailed", "{invalid}");
    assert_eq!(invalid["error"]["data"]["path"], "resources.web.env.TOKEN");

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": { "name": "get_project", "arguments": { "project": "nope" } }
    }));
    let missing = shim.read();
    assert_eq!(missing["error"]["data"]["code"], "NotFound", "{missing}");

    daemon.shutdown().expect("stop daemon");
}

#[test]
fn whoami_reports_a_clear_error_when_the_daemon_is_not_running() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    shim.read();

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": "whoami", "arguments": {} }
    }));
    let called = shim.read();
    assert_eq!(called["id"], 7);
    let message = called["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error response: {called}"));
    assert!(
        message.contains("daemon unreachable"),
        "the shim names the failure: {message}"
    );
}

#[test]
fn unknown_methods_and_tools_are_json_rpc_errors() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut shim = Shim::spawn(temp.path());

    shim.send(json!({"jsonrpc": "2.0", "id": 1, "method": "nope"}));
    let unknown_method = shim.read();
    assert_eq!(unknown_method["error"]["code"], -32601);

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "create_checkpoint", "arguments": {} }
    }));
    let unknown_tool = shim.read();
    assert_eq!(unknown_tool["error"]["code"], -32602);
    assert!(unknown_tool["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("create_checkpoint"));
}

#[test]
fn environment_tools_drive_the_whole_lifecycle_over_the_same_api() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path()))
        .expect("boot daemon");
    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    shim.read();

    // The new tools are advertised with schemas an Agent can act on.
    shim.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = shim.read();
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for name in [
        "start_environment",
        "stop_environment",
        "get_status",
        "run_command",
        "read_logs",
        "get_stats",
        "wait_for_operation",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no {name} tool in {listed}"));
        assert_eq!(tool["inputSchema"]["type"], "object", "{name} needs a schema");
        assert!(
            tool["description"].as_str().unwrap_or_default().len() > 40,
            "{name} needs a description an Agent can act on"
        );
    }
    // Environment tools are addressed by Project; the idempotency waiter by
    // operation.
    for name in [
        "start_environment",
        "stop_environment",
        "get_status",
        "run_command",
        "read_logs",
        "get_stats",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .expect("the tool exists");
        assert_eq!(
            tool["inputSchema"]["properties"]["project"]["type"], "string",
            "{name} takes a project: {tool}"
        );
    }
    let wait = tools
        .iter()
        .find(|tool| tool["name"] == "wait_for_operation")
        .expect("wait_for_operation");
    assert_eq!(
        wait["inputSchema"]["properties"]["operationId"]["type"], "string",
        "wait_for_operation takes an operation id: {wait}"
    );
    assert_eq!(wait["inputSchema"]["required"], json!(["operationId"]));
    let run_command = tools
        .iter()
        .find(|tool| tool["name"] == "run_command")
        .expect("run_command");
    assert_eq!(
        run_command["inputSchema"]["properties"]["command"]["type"], "array",
        "run_command takes an argv list: {run_command}"
    );
    assert_eq!(
        run_command["inputSchema"]["required"],
        json!(["project", "resource", "command"])
    );

    // A Project to drive.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "create_project", "arguments": { "fixture": "hello-pg" } }
    }));
    let created = shim.read();
    assert_eq!(created["result"]["isError"], false, "{created}");

    // Status before anything runs.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": { "name": "get_status", "arguments": { "project": "hello-pg" } }
    }));
    let before = shim.read();
    assert_eq!(before["result"]["isError"], false, "{before}");
    assert_eq!(
        before["result"]["structuredContent"]["environment"]["state"],
        "stopped"
    );
    assert_eq!(
        before["result"]["structuredContent"]["backend"]["name"],
        "fake"
    );

    // Start, and compare against the HTTP payload to prove the shim is a mapping.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": { "name": "start_environment", "arguments": { "project": "hello-pg" } }
    }));
    let started = shim.read();
    assert_eq!(started["result"]["isError"], false, "{started}");
    let status_payload = &started["result"]["structuredContent"];
    assert_eq!(status_payload["environment"]["state"], "running", "{started}");
    let operation_id = status_payload["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("start returns an operation id: {started}"))
        .to_string();

    let http: Value = serde_json::from_str(
        &ureq::get(&format!(
            "{}/v1/projects/hello-pg/environments/dev/status",
            daemon.base_url()
        ))
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .call()
        .expect("status over HTTP")
        .into_string()
        .expect("body"),
    )
    .expect("status JSON");
    assert_eq!(
        status_payload["environment"]["state"], http["environment"]["state"],
        "MCP reports the same state as HTTP"
    );

    // The operation that start produced can be waited on.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": { "name": "wait_for_operation", "arguments": { "operationId": operation_id } }
    }));
    let waited = shim.read();
    assert_eq!(waited["result"]["isError"], false, "{waited}");
    assert_eq!(
        waited["result"]["structuredContent"]["state"], "succeeded",
        "{waited}"
    );
    assert_eq!(
        waited["result"]["structuredContent"]["result"]["environment"]["state"], "running",
        "the waiter receives the mutation's result: {waited}"
    );

    // run_command returns stdout, stderr, and the exit code.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "project": "hello-pg", "resource": "web", "command": ["echo", "from-mcp"] }
        }
    }));
    let ran = shim.read();
    assert_eq!(ran["result"]["isError"], false, "{ran}");
    assert_eq!(ran["result"]["structuredContent"]["stdout"], "from-mcp\n");
    assert_eq!(ran["result"]["structuredContent"]["exitCode"], 0);

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "project": "hello-pg", "resource": "web", "command": ["fail", "7"] }
        }
    }));
    let failed = shim.read();
    assert_eq!(
        failed["result"]["structuredContent"]["exitCode"], 7,
        "a non-zero exit is a result, not a tool error: {failed}"
    );

    // read_logs returns a bounded tail and a cursor.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": { "name": "read_logs", "arguments": { "project": "hello-pg", "tail": 5 } }
    }));
    let logs = shim.read();
    assert_eq!(logs["result"]["isError"], false, "{logs}");
    let lines = logs["result"]["structuredContent"]["lines"]
        .as_array()
        .expect("lines");
    assert!(!lines.is_empty(), "{logs}");
    assert!(lines.len() <= 5, "the tail is bounded: {logs}");
    assert!(
        logs["result"]["structuredContent"]["nextSince"].is_string(),
        "a cursor comes back for the next read: {logs}"
    );

    // get_stats reports environment-level figures.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": { "name": "get_stats", "arguments": { "project": "hello-pg" } }
    }));
    let stats = shim.read();
    assert_eq!(stats["result"]["isError"], false, "{stats}");
    assert_eq!(stats["result"]["structuredContent"]["state"], "running");
    assert!(
        stats["result"]["structuredContent"]["memoryBytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );

    // stop_environment is idempotent and reports the stopped state.
    for (id, label) in [(11, "first"), (12, "second")] {
        shim.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "stop_environment", "arguments": { "project": "hello-pg" } }
        }));
        let stopped = shim.read();
        assert_eq!(stopped["result"]["isError"], false, "{label}: {stopped}");
        assert_eq!(
            stopped["result"]["structuredContent"]["environment"]["state"], "stopped",
            "{label}: {stopped}"
        );
    }

    // A typed failure keeps its code across MCP.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "project": "hello-pg", "resource": "web", "command": ["echo", "hi"] }
        }
    }));
    let refused = shim.read();
    assert_eq!(refused["error"]["data"]["code"], "NotProvisioned", "{refused}");

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 14,
        "method": "tools/call",
        "params": { "name": "get_status", "arguments": { "project": "nope" } }
    }));
    let missing = shim.read();
    assert_eq!(missing["error"]["data"]["code"], "NotFound", "{missing}");

    // A malformed argument is a validation failure, not a crash.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 15,
        "method": "tools/call",
        "params": { "name": "run_command", "arguments": { "project": "hello-pg" } }
    }));
    let invalid = shim.read();
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    assert_eq!(invalid["error"]["data"]["code"], "ValidationFailed");

    daemon.shutdown().expect("stop daemon");
}

/// The tools that need a human: they answer 202 with ids and change nothing.
#[test]
fn destructive_tools_answer_202_and_wait_for_an_approval() {
    use agentify_daemon::consent::ConsentMode;

    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(
        config(temp.path()).with_consent(ConsentMode::Granted),
    )
    .expect("boot daemon");
    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    shim.read();

    // Both tools are advertised, with schemas an Agent can act on.
    shim.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = shim.read();
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for name in ["delete_project", "reset_environment"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no {name} tool in {listed}"));
        assert_eq!(tool["inputSchema"]["type"], "object", "{name} needs a schema");
        assert_eq!(
            tool["inputSchema"]["properties"]["project"]["type"], "string",
            "{name} takes a project: {tool}"
        );
        assert_eq!(tool["inputSchema"]["required"], json!(["project"]));
        assert!(
            tool["description"]
                .as_str()
                .unwrap_or_default()
                .contains("202"),
            "{name} says it asks a human first: {tool}"
        );
    }
    let reset = tools
        .iter()
        .find(|tool| tool["name"] == "reset_environment")
        .expect("reset_environment");
    assert_eq!(
        reset["inputSchema"]["properties"]["environment"]["type"], "string",
        "reset_environment takes an Environment: {reset}"
    );

    // A Project to destroy.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "create_project", "arguments": { "fixture": "hello-pg" } }
    }));
    let created = shim.read();
    assert_eq!(created["result"]["isError"], false, "{created}");

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": { "name": "start_environment", "arguments": { "project": "hello-pg" } }
    }));
    let started = shim.read();
    assert_eq!(started["result"]["isError"], false, "{started}");

    // reset_environment asks first: ids come back, nothing is reset.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": { "name": "reset_environment", "arguments": { "project": "hello-pg" } }
    }));
    let reset_accepted = shim.read();
    assert_eq!(reset_accepted["result"]["isError"], false, "{reset_accepted}");
    let reset_payload = &reset_accepted["result"]["structuredContent"];
    assert_eq!(reset_payload["operation"], "reset_environment", "{reset_accepted}");
    assert_eq!(reset_payload["target"], "hello-pg/dev", "{reset_accepted}");
    assert!(
        reset_payload["approvalId"].as_str().unwrap_or_default().len() > 8,
        "reset names its Approval: {reset_accepted}"
    );
    let reset_operation = reset_payload["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("reset names its operation: {reset_accepted}"))
        .to_string();

    // Its operation is waiting for a human, not running and not finished.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "wait_for_operation",
            "arguments": { "operationId": reset_operation, "timeoutMs": 200 }
        }
    }));
    let reset_waiting = shim.read();
    assert_eq!(reset_waiting["result"]["isError"], false, "{reset_waiting}");
    assert_eq!(
        reset_waiting["result"]["structuredContent"]["state"], "waiting_for_approval",
        "{reset_waiting}"
    );

    // delete_project asks first too.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": { "name": "delete_project", "arguments": { "project": "hello-pg" } }
    }));
    let delete_accepted = shim.read();
    assert_eq!(delete_accepted["result"]["isError"], false, "{delete_accepted}");
    let delete_payload = &delete_accepted["result"]["structuredContent"];
    assert_eq!(delete_payload["operation"], "delete_project", "{delete_accepted}");
    assert_eq!(delete_payload["target"], "hello-pg", "{delete_accepted}");
    assert!(
        delete_payload["expiresAt"]
            .as_str()
            .unwrap_or_default()
            .contains('T'),
        "the Approval says when it dies: {delete_accepted}"
    );
    let approval_id = delete_payload["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("delete names its Approval: {delete_accepted}"))
        .to_string();
    let operation_id = delete_payload["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("delete names its operation: {delete_accepted}"))
        .to_string();

    // The 202 body over MCP is the 202 body over HTTP: same fields, no drift.
    let over_http: Value = serde_json::from_str(
        &ureq::request(
            "DELETE",
            &format!("{}/v1/projects/hello-pg", daemon.base_url()),
        )
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .call()
        .expect("DELETE over HTTP")
        .into_string()
        .expect("body"),
    )
    .expect("202 JSON");
    let mut over_mcp: Vec<&String> = delete_payload
        .as_object()
        .expect("an object")
        .keys()
        .collect();
    let mut over_http_keys: Vec<&String> = over_http.as_object().expect("an object").keys().collect();
    over_mcp.sort();
    over_http_keys.sort();
    assert_eq!(over_mcp, over_http_keys, "MCP and HTTP agree on the 202 shape");

    // Nothing has changed yet.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": "get_project", "arguments": { "project": "hello-pg" } }
    }));
    let still_there = shim.read();
    assert_eq!(still_there["result"]["isError"], false, "{still_there}");
    assert_eq!(still_there["result"]["structuredContent"]["name"], "hello-pg");

    // wait_for_operation still works, and reports a wait that ended before the
    // operation did rather than pretending the work is done.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "wait_for_operation",
            "arguments": { "operationId": operation_id, "timeoutMs": 400 }
        }
    }));
    let waiting = shim.read();
    assert_eq!(waiting["result"]["isError"], false, "{waiting}");
    assert_eq!(
        waiting["result"]["structuredContent"]["state"], "waiting_for_approval",
        "the operation waits for a human, so a waiter keeps waiting: {waiting}"
    );
    assert_eq!(
        waiting["result"]["structuredContent"]["kind"], "delete_project",
        "{waiting}"
    );

    // A human at the console approves; the Agent's waiter then sees the result.
    let decided: Value = serde_json::from_str(
        &ureq::post(&format!(
            "{}/v1/approvals/{approval_id}/approve",
            daemon.base_url()
        ))
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .set("Content-Type", "application/json")
        .send_string("{}")
        .expect("approve over HTTP")
        .into_string()
        .expect("body"),
    )
    .expect("decision JSON");
    assert_eq!(decided["approval"]["state"], "approved", "{decided}");

    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "wait_for_operation",
            "arguments": { "operationId": operation_id, "timeoutMs": 15000 }
        }
    }));
    let finished = shim.read();
    assert_eq!(finished["result"]["isError"], false, "{finished}");
    assert_eq!(
        finished["result"]["structuredContent"]["state"], "succeeded",
        "an approved delete runs and reports its result: {finished}"
    );
    assert_eq!(
        finished["result"]["structuredContent"]["result"]["verified"], true,
        "the result says the teardown was verified: {finished}"
    );

    // The reset's Approval was never decided, so its data is untouched; the
    // deleted Project is gone, as a typed error rather than prose.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": { "name": "get_project", "arguments": { "project": "hello-pg" } }
    }));
    let gone = shim.read();
    assert_eq!(gone["error"]["data"]["code"], "NotFound", "{gone}");

    // An operation id that does not exist is a typed error too.
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": { "name": "wait_for_operation", "arguments": { "operationId": "nope" } }
    }));
    let missing = shim.read();
    assert_eq!(missing["error"]["data"]["code"], "NotFound", "{missing}");

    daemon.shutdown().expect("stop daemon");
}

/// Call one tool and return its JSON-RPC response.
fn call(shim: &mut Shim, id: u64, tool: &str, arguments: Value) -> Value {
    shim.send(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    }));
    shim.read()
}

/// The four secret tools: metadata in, values never out.
#[test]
fn secret_tools_list_create_update_and_delete_through_the_same_api() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(config(temp.path())).expect("boot daemon");
    let mut shim = Shim::spawn(temp.path());

    shim.send(initialize());
    shim.read();

    // All four are advertised, addressed by Project and name.
    shim.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = shim.read();
    let tools = listed["result"]["tools"].as_array().expect("tools");
    for name in [
        "list_secrets",
        "create_secret",
        "update_secret",
        "delete_secret",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no {name} tool in {listed}"));
        assert_eq!(tool["inputSchema"]["type"], "object", "{name} needs a schema");
        assert_eq!(
            tool["inputSchema"]["properties"]["project"]["type"], "string",
            "{name} takes a project: {tool}"
        );
        assert!(
            tool["description"].as_str().unwrap_or_default().len() > 40,
            "{name} needs a description an Agent can act on"
        );
    }
    let create = tools
        .iter()
        .find(|tool| tool["name"] == "create_secret")
        .expect("create_secret");
    for field in ["name", "generate", "value"] {
        assert_eq!(
            create["inputSchema"]["properties"][field]["type"], "string",
            "create_secret takes {field}: {create}"
        );
    }

    // A Project with two declared secrets.
    let created = call(&mut shim, 3, "create_project", json!({ "fixture": "hello-pg" }));
    assert_eq!(created["result"]["isError"], false, "{created}");

    let listed_secrets = call(&mut shim, 4, "list_secrets", json!({ "project": "hello-pg" }));
    assert_eq!(
        listed_secrets["result"]["isError"], false,
        "{listed_secrets}"
    );
    let secrets = listed_secrets["result"]["structuredContent"]["secrets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names: Vec<&str> = secrets
        .iter()
        .filter_map(|secret| secret["name"].as_str())
        .collect();
    assert_eq!(names, vec!["api-token", "pg-password"], "{listed_secrets}");
    for secret in &secrets {
        assert!(
            secret.get("value").is_none(),
            "a listed secret is metadata only: {secret}"
        );
        assert_eq!(secret["referenced"], true, "{secret}");
    }
    assert!(
        !listed_secrets.to_string().contains(FIXTURE_API_TOKEN),
        "the imported value never travels: {listed_secrets}"
    );

    // An Agent mints a secret, then replaces it with one it chose. Neither call
    // echoes a value back.
    let minted = call(
        &mut shim,
        5,
        "create_secret",
        json!({ "project": "hello-pg", "name": "session-key", "generate": "token" }),
    );
    assert_eq!(minted["result"]["isError"], false, "{minted}");
    assert_eq!(minted["result"]["structuredContent"]["name"], "session-key");
    assert_eq!(minted["result"]["structuredContent"]["source"], "generate");
    let supplied = "value-from-the-agent-over-mcp";
    let updated = call(
        &mut shim,
        6,
        "update_secret",
        json!({ "project": "hello-pg", "name": "session-key", "value": supplied }),
    );
    assert_eq!(updated["result"]["isError"], false, "{updated}");
    assert_eq!(updated["result"]["structuredContent"]["source"], "literal");
    assert!(
        !updated.to_string().contains(supplied),
        "an update never echoes the value: {updated}"
    );

    // A secret the Manifest still references cannot be deleted: the next start
    // would have nothing to inject.
    let refused = call(
        &mut shim,
        7,
        "delete_secret",
        json!({ "project": "hello-pg", "name": "api-token" }),
    );
    assert_eq!(refused["error"]["data"]["code"], "SecretInUse", "{refused}");
    assert!(
        !refused.to_string().contains(FIXTURE_API_TOKEN),
        "the refusal carries no plaintext: {refused}"
    );

    // The one the Agent made is unreferenced, so it goes.
    let deleted = call(
        &mut shim,
        8,
        "delete_secret",
        json!({ "project": "hello-pg", "name": "session-key" }),
    );
    assert_eq!(deleted["result"]["isError"], false, "{deleted}");
    assert_eq!(deleted["result"]["structuredContent"]["deleted"], true);

    let after = call(&mut shim, 9, "list_secrets", json!({ "project": "hello-pg" }));
    let remaining: Vec<String> = after["result"]["structuredContent"]["secrets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|secret| secret["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(remaining, vec!["api-token", "pg-password"], "{after}");

    // A bad payload is a typed validation failure, not a panic or a leaked value.
    let invalid = call(
        &mut shim,
        10,
        "create_secret",
        json!({ "project": "hello-pg", "name": "two-sources", "generate": "token", "value": "x" }),
    );
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    assert_eq!(invalid["error"]["data"]["code"], "ValidationFailed");

    daemon.shutdown().expect("stop daemon");
}

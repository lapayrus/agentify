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
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
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
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
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
        "params": { "name": "delete_project", "arguments": {} }
    }));
    let unknown_tool = shim.read();
    assert_eq!(unknown_tool["error"]["code"], -32602);
    assert!(unknown_tool["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("delete_project"));
}

#[test]
fn environment_tools_drive_the_whole_lifecycle_over_the_same_api() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut daemon = agentify_daemon::start_in_background(agentify_daemon::Config::new(temp.path()))
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

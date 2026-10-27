//! Line-delimited JSON-RPC 2.0 over stdio.
//!
//! Only what the walking skeleton needs: `initialize`, `tools/list`, and
//! `tools/call` for `whoami`. Every tool is a thin mapping onto the daemon API,
//! so the MCP surface cannot drift from HTTP.

use serde_json::{json, Value};

use agentify_proto::client::DaemonClient;

/// MCP protocol revision the shim speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Name reported in `serverInfo`.
pub const SERVER_NAME: &str = "agentify";

const INSTRUCTIONS: &str = "Agentify exposes local Project environments to agents. Start with `list_projects` to \
see what exists, `create_project` to create one from a bundled fixture or an inline Manifest, and \
`get_project` to inspect its Environment, Resources, and URLs. Then `start_environment` brings the dev \
Environment up (idempotent: retrying is safe), `run_command` runs a bounded command in a Resource, \
`read_logs` and `get_stats` observe it, and `stop_environment` stops it while keeping its data. If a \
mutation takes too long the error carries an operation id: finish it with `wait_for_operation` instead \
of retrying blindly. `whoami` reports scopes, versions, the active Backend, and its capabilities.";

/// Handle one line of input. Returns the response to write, or `None` for
/// notifications (which must not be answered).
pub fn handle_line(line: &str, client: &DaemonClient) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(err) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("invalid JSON: {err}"),
                None,
            ))
        }
    };
    handle(request, client)
}

/// Handle one parsed JSON-RPC request.
pub fn handle(request: Value, client: &DaemonClient) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let Some(method) = method else {
        return Some(error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "a JSON-RPC request needs a method",
            None,
        ));
    };

    // Notifications (no id) never get a response.
    let Some(id) = id else {
        return None;
    };

    Some(match method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "title": "Agentify",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools() })),
        "tools/call" => call_tool(id, params, client),
        other => error_response(id, -32601, &format!("unknown method: {other}"), None),
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "whoami",
            "title": "Who am I",
            "description": "Report the Agent's Project context (null when global), scopes, daemon and \
API versions, and capabilities. Returns exactly the payload of GET /v1/whoami.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "list_projects",
            "title": "List Projects",
            "description": "List every Project on this machine with its Environment, Resources, derived \
URLs, and state and cost placeholders. Returns exactly the payload of GET /v1/projects.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "get_project",
            "title": "Get a Project",
            "description": "Get one Project by name or id, with its Environment, Resources, and derived \
URLs. Returns exactly the payload of GET /v1/projects/{project}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name or id."
                    }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "create_project",
            "title": "Create a Project",
            "description": "Create a Project from exactly one of a bundled fixture or an inline Manifest \
(the agentify.yaml text). The daemon validates the Manifest strictly, creates the Project's dev \
Environment and Resources, and copies the fixture source into a workspace it owns. Nothing starts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "fixture": {
                        "type": "string",
                        "description": "Name of a bundled fixture. `hello-pg` is the vertical slice."
                    },
                    "manifest": {
                        "type": "string",
                        "description": "An inline Manifest (agentify.yaml) as YAML text."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "start_environment",
            "title": "Start an Environment",
            "description": "Bring a Project's dev Environment up and return its status once every Resource \
is running. Idempotent and convergent: retrying after a timeout is safe, and ports already leased are \
reused. Returns exactly the payload of POST /v1/projects/{project}/environments/{environment}/start. If \
the call exceeds the daemon's timeout it answers 504 with an `operationId`; finish it with \
`wait_for_operation`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": {
                        "type": "string",
                        "description": "Environment name. v0 recognises only `dev`, which is the default."
                    }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "stop_environment",
            "title": "Stop an Environment",
            "description": "Stop a Project's dev Environment: its Resources stop, its host-port leases are \
released, and its runtime and data are kept, so starting again is fast. Idempotent. Returns exactly the \
payload of POST /v1/projects/{project}/environments/{environment}/stop.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": { "type": "string", "description": "Environment name; defaults to `dev`." }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_status",
            "title": "Get Environment status",
            "description": "The one truth about an Environment: state (starting, running, degraded, stopped, \
failed), health, each Resource with its health and leased ports, the active Backend, and when it was \
checked. Health is decided by the daemon probing the leased host ports. Returns exactly the payload of \
GET /v1/projects/{project}/environments/{environment}/status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": { "type": "string", "description": "Environment name; defaults to `dev`." }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "run_command",
            "title": "Run a command in a Resource",
            "description": "Run a bounded command in one Resource and return its stdout, stderr, and exit \
code. There is no interactive stdin. The command is an argv list, run without a shell, and inherits the \
Resource's environment. A non-zero exit code is a successful call that reports the code, not an error. \
Returns exactly the payload of POST .../resources/{resource}/exec.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": { "type": "string", "description": "Environment name; defaults to `dev`." },
                    "resource": { "type": "string", "description": "Resource to run in, e.g. `web` or `db`." },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "argv, e.g. [\"node\", \"--version\"]."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Bound on the call in milliseconds; the daemon caps it too."
                    }
                },
                "required": ["project", "resource", "command"],
                "additionalProperties": false
            }
        },
        {
            "name": "read_logs",
            "title": "Read Resource logs",
            "description": "Read an Environment's logs, bounded by default so an Agent's context is not \
flooded. `tail` keeps the newest N lines, `since` continues from a previous `nextSince` cursor, and \
`resource` narrows to one Resource. Returns exactly the payload of GET .../logs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": { "type": "string", "description": "Environment name; defaults to `dev`." },
                    "resource": { "type": "string", "description": "Only this Resource's lines." },
                    "tail": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Keep only the newest N lines. The daemon caps it."
                    },
                    "since": {
                        "type": "string",
                        "description": "Cursor from a previous response's `nextSince`; only newer lines are returned."
                    }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "get_stats",
            "title": "Get Environment stats",
            "description": "CPU, memory, and disk for an Environment, as formatted strings for a status bar \
and as raw numbers. Per-Resource figures appear when the Backend reports them. Returns exactly the payload \
of GET .../stats.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id." },
                    "environment": { "type": "string", "description": "Environment name; defaults to `dev`." }
                },
                "required": ["project"],
                "additionalProperties": false
            }
        },
        {
            "name": "wait_for_operation",
            "title": "Wait for an operation",
            "description": "Wait for a long mutation to finish and return the operation, including the \
result it produced. Use it with the `operationId` a timed-out `start_environment` or `stop_environment` \
returned, instead of retrying the mutation. Returns exactly the payload of GET /v1/operations/{id}/wait; \
the operation's `state` says whether the wait ended or the operation did.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "operationId": {
                        "type": "string",
                        "description": "The id a timed-out mutation returned."
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How long to wait, in milliseconds."
                    }
                },
                "required": ["operationId"],
                "additionalProperties": false
            }
        }
    ])
}

fn call_tool(id: Value, params: Value, client: &DaemonClient) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    let result = match name {
        "whoami" => client.whoami(),
        "list_projects" => client.list_projects(),
        "get_project" => match required_string(&arguments, "project") {
            Ok(project) => client.get_project(&project),
            Err(message) => return invalid_arguments(id, &message),
        },
        "create_project" => match create_arguments(&arguments) {
            Ok((fixture, manifest)) => client.create_project(fixture.as_deref(), manifest.as_deref()),
            Err(message) => return invalid_arguments(id, &message),
        },
        "start_environment" => match environment_arguments(&arguments) {
            Ok((project, environment)) => client.start_environment(&project, &environment),
            Err(message) => return invalid_arguments(id, &message),
        },
        "stop_environment" => match environment_arguments(&arguments) {
            Ok((project, environment)) => client.stop_environment(&project, &environment),
            Err(message) => return invalid_arguments(id, &message),
        },
        "get_status" => match environment_arguments(&arguments) {
            Ok((project, environment)) => client.environment_status(&project, &environment),
            Err(message) => return invalid_arguments(id, &message),
        },
        "run_command" => match run_command_arguments(&arguments) {
            Ok((project, environment, resource, command, timeout_ms)) => {
                client.exec(&project, &environment, &resource, &command, timeout_ms)
            }
            Err(message) => return invalid_arguments(id, &message),
        },
        "read_logs" => match read_logs_arguments(&arguments) {
            Ok((project, environment, resource, tail, since)) => client.logs(
                &project,
                &environment,
                resource.as_deref(),
                tail,
                since.as_deref(),
            ),
            Err(message) => return invalid_arguments(id, &message),
        },
        "get_stats" => match environment_arguments(&arguments) {
            Ok((project, environment)) => client.stats(&project, &environment),
            Err(message) => return invalid_arguments(id, &message),
        },
        "wait_for_operation" => match wait_arguments(&arguments) {
            Ok((operation_id, timeout_ms)) => client.wait_for_operation(&operation_id, timeout_ms),
            Err(message) => return invalid_arguments(id, &message),
        },
        other => {
            return error_response(
                id,
                -32602,
                &format!("unknown tool: {other}"),
                Some(json!({ "code": "NotFound" })),
            )
        }
    };

    match result {
        Ok(payload) => ok(
            id,
            json!({
                "content": [{ "type": "text", "text": payload.to_string() }],
                "structuredContent": payload,
                "isError": false,
            }),
        ),
        Err(err) => {
            // Forward the daemon's stable code (and a validation path or the
            // operation id, when there is one) so an Agent branches on the same
            // values over MCP as over HTTP.
            let mut data = json!({ "code": err.code() });
            if let Some(problem) = err.problem() {
                for field in ["path", "detail", "status", "operationId"] {
                    if let Some(value) = problem.get(field) {
                        data[field] = value.clone();
                    }
                }
            }
            error_response(id, -32603, &err.to_string(), Some(data))
        }
    }
}

fn required_string(arguments: &Value, field: &str) -> std::result::Result<String, String> {
    match arguments.get(field).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        _ => Err(format!("`{field}` is required and must be a non-empty string")),
    }
}

/// An optional non-empty string.
fn optional_string(arguments: &Value, field: &str) -> std::result::Result<Option<String>, String> {
    match arguments.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{field}` must be a non-empty string")),
    }
}

/// An optional positive whole number.
fn optional_count(arguments: &Value, field: &str) -> std::result::Result<Option<u64>, String> {
    match arguments.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if value > 0 => Ok(Some(value)),
            _ => Err(format!("`{field}` must be a positive whole number")),
        },
        Some(_) => Err(format!("`{field}` must be a positive whole number")),
    }
}

/// `(project, environment)`: the Environment defaults to `dev`, the only one v0
/// recognises.
fn environment_arguments(arguments: &Value) -> std::result::Result<(String, String), String> {
    let project = required_string(arguments, "project")?;
    let environment =
        optional_string(arguments, "environment")?.unwrap_or_else(|| "dev".to_string());
    Ok((project, environment))
}

#[allow(clippy::type_complexity)]
fn run_command_arguments(
    arguments: &Value,
) -> std::result::Result<(String, String, String, Vec<String>, Option<u64>), String> {
    let (project, environment) = environment_arguments(arguments)?;
    let resource = required_string(arguments, "resource")?;
    let command = match arguments.get("command") {
        Some(Value::Array(items)) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(part) if !part.is_empty() => parts.push(part.to_string()),
                    _ => return Err("`command` must be an array of non-empty strings".to_string()),
                }
            }
            if parts.is_empty() {
                return Err("`command` must carry at least one argument".to_string());
            }
            parts
        }
        _ => return Err("`command` is required and must be an array of strings".to_string()),
    };
    let timeout_ms = optional_count(arguments, "timeoutMs")?;
    Ok((project, environment, resource, command, timeout_ms))
}

#[allow(clippy::type_complexity)]
fn read_logs_arguments(
    arguments: &Value,
) -> std::result::Result<(String, String, Option<String>, Option<usize>, Option<String>), String> {
    let (project, environment) = environment_arguments(arguments)?;
    let resource = optional_string(arguments, "resource")?;
    let tail = optional_count(arguments, "tail")?.map(|tail| tail as usize);
    let since = optional_string(arguments, "since")?;
    Ok((project, environment, resource, tail, since))
}

fn wait_arguments(arguments: &Value) -> std::result::Result<(String, Option<u64>), String> {
    let operation_id = required_string(arguments, "operationId")?;
    let timeout_ms = optional_count(arguments, "timeoutMs")?;
    Ok((operation_id, timeout_ms))
}

/// `create_project` takes exactly one of `fixture` or `manifest`.
fn create_arguments(
    arguments: &Value,
) -> std::result::Result<(Option<String>, Option<String>), String> {
    let field = |name: &str| -> std::result::Result<Option<String>, String> {
        match arguments.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
            Some(_) => Err(format!("`{name}` must be a non-empty string")),
        }
    };
    let fixture = field("fixture")?;
    let manifest = field("manifest")?;
    match (&fixture, &manifest) {
        (Some(_), None) | (None, Some(_)) => Ok((fixture, manifest)),
        _ => Err("pass exactly one of `fixture` or `manifest`".to_string()),
    }
}

fn invalid_arguments(id: Value, message: &str) -> Value {
    error_response(
        id,
        -32602,
        message,
        Some(json!({ "code": "ValidationFailed" })),
    )
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

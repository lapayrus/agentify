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
`get_project` to inspect its Environment, Resources, and URLs. Nothing is started yet; `whoami` reports \
scopes, versions, and capabilities.";

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
            // Forward the daemon's stable code (and a validation path, when
            // there is one) so an Agent branches on the same values over MCP as
            // over HTTP.
            let mut data = json!({ "code": err.code() });
            if let Some(problem) = err.problem() {
                for field in ["path", "detail", "status"] {
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

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

const INSTRUCTIONS: &str = "Agentify exposes local Project environments to agents. This build is a \
walking skeleton: `whoami` reports Project context, scopes, versions, and capabilities. Project and \
Environment tools arrive in later tickets.";

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
        }
    ])
}

fn call_tool(id: Value, params: Value, client: &DaemonClient) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match name {
        "whoami" => match client.whoami() {
            Ok(payload) => ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": payload.to_string() }],
                    "structuredContent": payload,
                    "isError": false,
                }),
            ),
            Err(err) => error_response(
                id,
                -32603,
                &err.to_string(),
                Some(json!({ "code": err.code() })),
            ),
        },
        other => error_response(
            id,
            -32602,
            &format!("unknown tool: {other}"),
            Some(json!({ "code": "NotFound" })),
        ),
    }
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

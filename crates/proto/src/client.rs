//! A client for the daemon's loopback API.
//!
//! The shim does not read the state database and does not know the daemon's
//! internals: it reads the discovery file and speaks HTTP, exactly like any
//! other client.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Discovery;
use serde_json::Value;

/// Request timeout for a single API call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum ClientError {
    /// No discovery file: the daemon is not running (or has never run).
    NoDaemon { path: PathBuf, detail: String },
    /// The discovery file is there but unusable.
    BadDiscovery { path: PathBuf, detail: String },
    /// The port from the discovery file refused the connection.
    Unreachable { url: String, detail: String },
    /// The daemon answered with an error status.
    Status { status: u16, body: String },
    /// The daemon answered with something that is not JSON.
    BadBody { detail: String },
}

impl ClientError {
    /// Stable machine-readable code, mirroring the API's problem codes. When the
    /// daemon sent a problem body, its `code` wins: an Agent branches on the
    /// same code over HTTP and over MCP.
    pub fn code(&self) -> String {
        match self {
            ClientError::NoDaemon { .. } | ClientError::Unreachable { .. } => {
                "BackendUnavailable".to_string()
            }
            ClientError::BadDiscovery { .. } | ClientError::BadBody { .. } => "Internal".to_string(),
            ClientError::Status { status, body } => problem(body)
                .and_then(|problem| {
                    problem
                        .get("code")
                        .and_then(|code| code.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    match status {
                        401 | 403 => "Unauthorized",
                        404 => "NotFound",
                        409 => "AlreadyExists",
                        422 => "ValidationFailed",
                        _ => "Internal",
                    }
                    .to_string()
                }),
        }
    }

    /// The problem body the daemon returned, when there was one. Carries the
    /// `path` of a validation failure through to the caller.
    pub fn problem(&self) -> Option<Value> {
        match self {
            ClientError::Status { body, .. } => problem(body),
            _ => None,
        }
    }
}

fn problem(body: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.is_object().then_some(value)
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NoDaemon { path, detail } => write!(
                f,
                "daemon unreachable: no discovery file at {} ({detail}). Start the Agentify daemon, or point {} at its data root.",
                path.display(),
                crate::HOME_ENV
            ),
            ClientError::BadDiscovery { path, detail } => write!(
                f,
                "daemon unreachable: {} is not a usable discovery file ({detail})",
                path.display()
            ),
            ClientError::Unreachable { url, detail } => write!(
                f,
                "daemon unreachable: nothing is serving {url} ({detail})"
            ),
            ClientError::Status { status, body } => {
                write!(f, "daemon returned {status}: {body}")
            }
            ClientError::BadBody { detail } => write!(f, "daemon returned invalid JSON: {detail}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Talks to the daemon described by a data root's discovery file.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    data_root: PathBuf,
}

impl DaemonClient {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    /// A client for the data root selected by the environment.
    pub fn from_env() -> Self {
        Self::new(crate::resolve_data_root())
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Read the discovery file of the daemon this client would talk to.
    pub fn discovery(&self) -> Result<Discovery, ClientError> {
        let path = crate::discovery_path(&self.data_root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|err| ClientError::BadDiscovery {
                path,
                detail: err.to_string(),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ClientError::NoDaemon {
                path,
                detail: err.to_string(),
            }),
            Err(err) => Err(ClientError::BadDiscovery {
                path,
                detail: err.to_string(),
            }),
        }
    }

    /// `GET <path>` and parse the JSON body.
    pub fn get_json(&self, path: &str) -> Result<Value, ClientError> {
        let discovery = self.discovery()?;
        let url = format!("{}{}", discovery.url, path);
        let response = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", discovery.token))
            .timeout(REQUEST_TIMEOUT)
            .call();
        parse(response, &url)
    }

    /// `POST <path>` with a JSON body and parse the JSON response.
    pub fn post_json(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let discovery = self.discovery()?;
        let url = format!("{}{}", discovery.url, path);
        let response = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", discovery.token))
            .set("Content-Type", "application/json")
            .timeout(REQUEST_TIMEOUT)
            .send_string(&body.to_string());
        parse(response, &url)
    }

    /// `GET /v1/whoami`.
    pub fn whoami(&self) -> Result<Value, ClientError> {
        self.get_json("/v1/whoami")
    }

    /// `GET /v1/projects`.
    pub fn list_projects(&self) -> Result<Value, ClientError> {
        self.get_json("/v1/projects")
    }

    /// `GET /v1/projects/{project}`, by name or id.
    pub fn get_project(&self, project: &str) -> Result<Value, ClientError> {
        self.get_json(&format!("/v1/projects/{}", encode_segment(project)))
    }

    /// `POST /v1/projects` with exactly one source.
    pub fn create_project(
        &self,
        fixture: Option<&str>,
        manifest: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut body = serde_json::Map::new();
        if let Some(fixture) = fixture {
            body.insert("fixture".to_string(), Value::String(fixture.to_string()));
        }
        if let Some(manifest) = manifest {
            body.insert("manifest".to_string(), Value::String(manifest.to_string()));
        }
        self.post_json("/v1/projects", &Value::Object(body))
    }

    /// `DELETE /v1/projects/{project}`. Answers `202` with an `approvalId` and
    /// an `operationId`: nothing is removed until a human approves.
    pub fn delete_project(&self, project: &str) -> Result<Value, ClientError> {
        let discovery = self.discovery()?;
        let url = format!(
            "{}/v1/projects/{}",
            discovery.url,
            encode_segment(project)
        );
        let response = ureq::request("DELETE", &url)
            .set("Authorization", &format!("Bearer {}", discovery.token))
            .timeout(REQUEST_TIMEOUT)
            .call();
        parse(response, &url)
    }

    fn environment_path(project: &str, environment: &str) -> String {
        format!(
            "/v1/projects/{}/environments/{}",
            encode_segment(project),
            encode_segment(environment)
        )
    }

    /// `POST .../environments/{env}/start`. Blocks until the Environment is up,
    /// or until the daemon answers `504` with an operation id.
    pub fn start_environment(
        &self,
        project: &str,
        environment: &str,
    ) -> Result<Value, ClientError> {
        let path = format!("{}/start", Self::environment_path(project, environment));
        self.post_json(&path, &Value::Object(serde_json::Map::new()))
    }

    /// `POST .../environments/{env}/stop`.
    pub fn stop_environment(
        &self,
        project: &str,
        environment: &str,
    ) -> Result<Value, ClientError> {
        let path = format!("{}/stop", Self::environment_path(project, environment));
        self.post_json(&path, &Value::Object(serde_json::Map::new()))
    }

    /// `POST .../environments/{env}/reset`. Answers `202` with an `approvalId`
    /// and an `operationId`: the data is cleared only after a human approves.
    pub fn reset_environment(
        &self,
        project: &str,
        environment: &str,
    ) -> Result<Value, ClientError> {
        let path = format!("{}/reset", Self::environment_path(project, environment));
        self.post_json(&path, &Value::Object(serde_json::Map::new()))
    }

    /// `GET /v1/approvals?state=...`.
    pub fn approvals(&self, state: Option<&str>) -> Result<Value, ClientError> {
        match state {
            Some(state) if !state.trim().is_empty() => {
                self.get_json(&format!("/v1/approvals?state={}", encode_segment(state)))
            }
            _ => self.get_json("/v1/approvals"),
        }
    }

    /// `POST /v1/approvals/{id}/approve`. The daemon prompts through the OS; a
    /// caller cannot pass the gesture on its own.
    pub fn approve(&self, approval_id: &str, body: &Value) -> Result<Value, ClientError> {
        self.post_json(
            &format!("/v1/approvals/{}/approve", encode_segment(approval_id)),
            body,
        )
    }

    /// `POST /v1/approvals/{id}/deny`. The target is left unchanged.
    pub fn deny(&self, approval_id: &str, body: &Value) -> Result<Value, ClientError> {
        self.post_json(
            &format!("/v1/approvals/{}/deny", encode_segment(approval_id)),
            body,
        )
    }

    /// `GET /v1/activity`: the audit chain, newest first, with its verdict.
    pub fn activity(&self, limit: Option<usize>) -> Result<Value, ClientError> {
        match limit {
            Some(limit) => self.get_json(&format!("/v1/activity?limit={limit}")),
            None => self.get_json("/v1/activity"),
        }
    }

    /// `GET .../environments/{env}/status`.
    pub fn environment_status(
        &self,
        project: &str,
        environment: &str,
    ) -> Result<Value, ClientError> {
        self.get_json(&format!("{}/status", Self::environment_path(project, environment)))
    }

    /// `POST .../environments/{env}/resources/{resource}/exec`.
    pub fn exec(
        &self,
        project: &str,
        environment: &str,
        resource: &str,
        command: &[String],
        timeout_ms: Option<u64>,
    ) -> Result<Value, ClientError> {
        let mut body = serde_json::Map::new();
        body.insert(
            "command".to_string(),
            Value::Array(command.iter().cloned().map(Value::String).collect()),
        );
        if let Some(timeout_ms) = timeout_ms {
            body.insert("timeoutMs".to_string(), Value::from(timeout_ms));
        }
        let path = format!(
            "{}/resources/{}/exec",
            Self::environment_path(project, environment),
            encode_segment(resource)
        );
        self.post_json(&path, &Value::Object(body))
    }

    /// `GET .../environments/{env}/logs`: a bounded tail, optionally filtered.
    pub fn logs(
        &self,
        project: &str,
        environment: &str,
        resource: Option<&str>,
        tail: Option<usize>,
        since: Option<&str>,
    ) -> Result<Value, ClientError> {
        let mut query: Vec<String> = Vec::new();
        if let Some(tail) = tail {
            query.push(format!("tail={tail}"));
        }
        if let Some(since) = since {
            query.push(format!("since={}", encode_segment(since)));
        }
        if let Some(resource) = resource {
            query.push(format!("resource={}", encode_segment(resource)));
        }
        let path = format!(
            "{}/logs{}",
            Self::environment_path(project, environment),
            if query.is_empty() {
                String::new()
            } else {
                format!("?{}", query.join("&"))
            }
        );
        self.get_json(&path)
    }

    /// `GET .../environments/{env}/stats`.
    pub fn stats(&self, project: &str, environment: &str) -> Result<Value, ClientError> {
        self.get_json(&format!("{}/stats", Self::environment_path(project, environment)))
    }

    /// `GET /v1/operations/{id}`.
    pub fn operation(&self, id: &str) -> Result<Value, ClientError> {
        self.get_json(&format!("/v1/operations/{}", encode_segment(id)))
    }

    /// `GET /v1/operations/{id}/wait`.
    pub fn wait_for_operation(
        &self,
        id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value, ClientError> {
        let mut path = format!("/v1/operations/{}/wait", encode_segment(id));
        if let Some(timeout_ms) = timeout_ms {
            path.push_str(&format!("?timeoutMs={timeout_ms}"));
        }
        self.get_json(&path)
    }
}

fn parse(response: Result<ureq::Response, ureq::Error>, url: &str) -> Result<Value, ClientError> {
    match response {
        Ok(response) => {
            let body = response.into_string().unwrap_or_default();
            serde_json::from_str(&body).map_err(|err| ClientError::BadBody {
                detail: format!("{err}: {body}"),
            })
        }
        Err(ureq::Error::Status(status, response)) => Err(ClientError::Status {
            status,
            body: response.into_string().unwrap_or_default(),
        }),
        Err(err) => Err(ClientError::Unreachable {
            url: url.to_string(),
            detail: err.to_string(),
        }),
    }
}

/// Percent-encode the characters that cannot appear in a path segment. A
/// Project name is lower-case kebab, so this is belt and braces.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_daemon_is_reported_as_unreachable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let client = DaemonClient::new(temp.path());
        let err = client.whoami().expect_err("no daemon");
        assert_eq!(err.code(), "BackendUnavailable");
        assert!(
            err.to_string().contains("daemon unreachable"),
            "got: {err}"
        );
    }
}

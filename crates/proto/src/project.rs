//! Project payloads served under `/v1/projects`.
//!
//! A Project is a Manifest plus an Environment; these types are what the API,
//! the MCP shim, and the desktop shell agree on. They carry no secret values:
//! a Manifest names secrets, and the daemon owns the values.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The only Environment v0 recognises.
pub const DEV_ENVIRONMENT: &str = "dev";

/// Environment state before anything is started. Nothing runs in this ticket;
/// the daemon owns the real state machine later.
pub const STATE_STOPPED: &str = "stopped";

/// Where a Project's Manifest came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectSource {
    /// A bundled fixture, which brings its source files with it.
    Fixture,
    /// An inline Manifest sent with the request.
    Manifest,
}

impl ProjectSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectSource::Fixture => "fixture",
            ProjectSource::Manifest => "manifest",
        }
    }

    /// Parse the value stored in the `source_kind` column.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fixture" => Some(ProjectSource::Fixture),
            "manifest" => Some(ProjectSource::Manifest),
            _ => None,
        }
    }
}

/// Resource cost placeholders. `None` means "not measured yet", which is the
/// truth until stats land.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub disk: Option<String>,
}

/// A Project's one `dev` Environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentView {
    pub id: String,
    pub name: String,
    pub state: String,
    pub cost: Cost,
}

/// A derived Route: `<hostname>.<project>.localhost`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteView {
    pub resource: String,
    pub hostname: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortView {
    pub port: u16,
    pub hostname: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildView {
    pub context: String,
    pub dockerfile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthcheckView {
    pub port: u16,
    pub path: Option<String>,
    pub interval: String,
}

/// A Resource as served to clients. One flat shape with nullable fields, so the
/// UI can render a table without branching on the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceView {
    pub id: String,
    pub name: String,
    /// `app` or `database`.
    pub kind: String,
    pub image: Option<String>,
    pub build: Option<BuildView>,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<PortView>,
    #[serde(rename = "dependsOn")]
    pub depends_on: Vec<String>,
    pub healthcheck: Option<HealthcheckView>,
    /// The first port's URL, for a one-line summary.
    pub url: Option<String>,
    pub engine: Option<String>,
    pub version: Option<String>,
    pub volume: Option<String>,
    pub username: Option<String>,
    /// Placeholder until a Backend reports health.
    pub state: String,
    pub cost: Cost,
}

/// A Project with its Environment, Resources, and derived URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub source: ProjectSource,
    /// The fixture name when the source is a bundled fixture.
    #[serde(rename = "sourceName")]
    pub source_name: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// The daemon-owned directory the Project's source lives in.
    pub workspace: String,
    pub environment: EnvironmentView,
    pub resources: Vec<ResourceView>,
    pub routes: Vec<RouteView>,
}

/// `GET /v1/projects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<Project>,
}

/// `POST /v1/projects`: exactly one of a bundled fixture or an inline Manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProjectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
}

impl CreateProjectRequest {
    pub fn from_fixture(name: impl Into<String>) -> Self {
        Self {
            fixture: Some(name.into()),
            manifest: None,
        }
    }

    pub fn from_manifest(manifest: impl Into<String>) -> Self {
        Self {
            fixture: None,
            manifest: Some(manifest.into()),
        }
    }
}

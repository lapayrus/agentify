//! The normalized Manifest.
//!
//! Everything the daemon needs to persist and act on, with the defaults the
//! parser fills in (a volume name, a database version, a TCP healthcheck). No
//! daemon-owned names beyond what the manifest lets a person pin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The only Manifest version v0 understands.
pub const MANIFEST_VERSION: u32 = 1;

/// Postgres version used when a database does not pin one.
pub const DEFAULT_DATABASE_VERSION: &str = "16";

/// Dockerfile name used when a build does not name one.
pub const DEFAULT_DOCKERFILE: &str = "Dockerfile";

/// Healthcheck period used when an app does not set one.
pub const DEFAULT_HEALTHCHECK_INTERVAL: &str = "5s";

/// The only database engine v0 supports.
pub const POSTGRES: &str = "postgres";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub project: String,
    pub secrets: BTreeMap<String, Secret>,
    /// Keyed by name, sorted, so the normalized spec is deterministic.
    pub resources: BTreeMap<String, Resource>,
}

impl Manifest {
    /// The Routes this Project serves: one per declared port.
    pub fn routes(&self) -> Vec<Route> {
        let mut routes = Vec::new();
        for resource in self.resources.values() {
            if let ResourceKind::App(app) = &resource.kind {
                for port in &app.ports {
                    routes.push(Route {
                        resource: resource.name.clone(),
                        hostname: port.hostname.clone(),
                        url: port.url(&self.project),
                    });
                }
            }
        }
        routes
    }

    /// The normalized spec stored with the Project.
    ///
    /// This is what the daemon acts on later: derived volume names, default
    /// healthchecks, port URLs, and the secret names a Project references. It
    /// never contains a secret value.
    pub fn to_spec(&self) -> serde_json::Value {
        let routes: Vec<serde_json::Value> = self
            .routes()
            .into_iter()
            .map(|route| {
                serde_json::json!({
                    "resource": route.resource,
                    "hostname": route.hostname,
                    "url": route.url,
                })
            })
            .collect();

        let resources: serde_json::Map<String, serde_json::Value> = self
            .resources
            .values()
            .map(|resource| {
                let body = match &resource.kind {
                    ResourceKind::App(app) => serde_json::json!({
                        "kind": "app",
                        "image": app.image,
                        "build": app.build,
                        "command": app.command,
                        "env": app.env,
                        "ports": app
                            .ports
                            .iter()
                            .map(|port| serde_json::json!({
                                "port": port.port,
                                "hostname": port.hostname,
                                "url": port.url(&self.project),
                            }))
                            .collect::<Vec<_>>(),
                        "dependsOn": app.depends_on,
                        "healthcheck": app.healthcheck,
                    }),
                    ResourceKind::Database(database) => serde_json::json!({
                        "kind": "database",
                        "engine": database.engine,
                        "version": database.version,
                        "credentials": {
                            "username": database.credentials.username,
                            "passwordSecret": database.credentials.password_secret,
                        },
                        "volume": database.volume,
                    }),
                };
                (resource.name.clone(), body)
            })
            .collect();

        serde_json::json!({
            "version": self.version,
            "project": self.project,
            "secrets": self.secrets,
            "routes": routes,
            "resources": resources,
        })
    }
}

/// A Route: `<resource>.<project>.localhost`, HTTPS by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub resource: String,
    pub hostname: String,
    pub url: String,
}

/// A named secret the Manifest references but never contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate: Option<SecretKind>,
    #[serde(rename = "fromEnv", skip_serializing_if = "Option::is_none")]
    pub from_env: Option<String>,
}

impl Secret {
    pub fn generated(kind: SecretKind) -> Self {
        Self {
            generate: Some(kind),
            from_env: None,
        }
    }

    pub fn from_env(name: impl Into<String>) -> Self {
        Self {
            generate: None,
            from_env: Some(name.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    Password,
    Token,
}

impl std::fmt::Display for SecretKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SecretKind::Password => "password",
            SecretKind::Token => "token",
        })
    }
}

/// A Resource: an app or a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub name: String,
    pub kind: ResourceKind,
}

impl Resource {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            ResourceKind::App(_) => "app",
            ResourceKind::Database(_) => "database",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceKind {
    App(App),
    Database(Database),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct App {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,
    #[serde(rename = "dependsOn", skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<Healthcheck>,
}

/// A build context. Both paths are relative to the Project's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Build {
    pub context: String,
    pub dockerfile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Port {
    pub port: u16,
    pub hostname: String,
}

impl Port {
    /// `https://<hostname>.<project>.localhost`.
    pub fn url(&self, project: &str) -> String {
        format!("https://{}.{}.localhost", self.hostname, project)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Healthcheck {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub interval: String,
}

/// A Postgres database Resource. The password is a secret reference, never a
/// value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Database {
    pub engine: String,
    pub version: String,
    pub credentials: Credentials,
    pub volume: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Credentials {
    pub username: String,
    #[serde(rename = "passwordSecret")]
    pub password_secret: String,
}

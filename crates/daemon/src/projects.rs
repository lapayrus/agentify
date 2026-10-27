//! Creating and reading Projects.
//!
//! A Project is a Manifest plus its one `dev` Environment. Creating one is the
//! whole of this module's write path: the daemon parses the Manifest strictly,
//! mints every daemon-owned name (Project id, Environment id, Resource ids,
//! workspace path), copies the source into a workspace it owns, and records the
//! normalized spec in one transaction. Nothing starts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentify_proto::{
    Cost, CreateProjectRequest, EnvironmentView, Project, ProjectSource, ResourceView, RouteView,
    DEV_ENVIRONMENT, STATE_STOPPED,
};
use anyhow::{Context, Result};
use serde::Deserialize;

use crate::clock;
use crate::fixtures;
use crate::lifecycle::LifecycleError;
use crate::secrets::Secrets;
use crate::state::{NewProject, NewSecret, ProjectRecord, State};

/// Directory, inside the data root, holding one directory per Project.
pub const PROJECTS_DIR: &str = "projects";

/// Directory, inside a Project's directory, that holds its source. The daemon
/// owns it; a Manifest never names a host path.
pub const WORKSPACE_DIR: &str = "workspace";

/// Why a Project could not be created.
#[derive(Debug)]
pub enum CreateError {
    /// The request or the Manifest is wrong. `path` names the field, so an
    /// Agent can fix the right line.
    Validation { path: String, detail: String },
    /// The source the Manifest needs is not available on this machine.
    SourceMissing(String),
    /// A `fromEnv` secret has no value on this machine. Nothing was created.
    SecretSourceMissing(String),
    /// A Project with this name already exists.
    AlreadyExists(String),
    /// The state database failed.
    Store(anyhow::Error),
}

impl CreateError {
    fn validation(path: impl Into<String>, detail: impl Into<String>) -> Self {
        CreateError::Validation {
            path: path.into(),
            detail: detail.into(),
        }
    }
}

/// Create a Project from exactly one of a bundled fixture or an inline
/// Manifest, copy its source into a daemon-owned workspace, and persist it with
/// its `dev` Environment, its Resources, and its secrets' metadata.
///
/// Every secret is resolved — generated or imported — before anything is
/// written, so a `SecretSourceMissing` refusal leaves no row, no directory, and
/// no credential behind. The values go to the secret store; the metadata goes to
/// SQLite in the same transaction as the Project.
pub fn create(
    state: &State,
    data_root: &Path,
    secrets: &Secrets,
    request: CreateProjectRequest,
) -> std::result::Result<Project, CreateError> {
    let (source, source_name, manifest_text, fixture) = match (
        request.fixture.as_deref(),
        request.manifest.as_deref(),
    ) {
        (Some(_), Some(_)) => {
            return Err(CreateError::validation(
                "manifest",
                "create a Project from exactly one of `fixture` or `manifest`, not both",
            ))
        }
        (None, None) => {
            return Err(CreateError::validation(
                "manifest",
                "create a Project from exactly one of `fixture` or `manifest`",
            ))
        }
        (Some(name), None) => {
            let fixture = fixtures::get(name).ok_or_else(|| {
                CreateError::SourceMissing(format!(
                    "no bundled fixture named {name:?}; available fixtures: {}",
                    fixtures::names().join(", ")
                ))
            })?;
            (
                ProjectSource::Fixture,
                Some(fixture.name.to_string()),
                fixture.manifest,
                Some(fixture),
            )
        }
        (None, Some(text)) => (ProjectSource::Manifest, None, text, None),
    };

    let manifest = agentify_manifest::parse(manifest_text)
        .map_err(|err| CreateError::validation(err.path, err.message))?;

    // v0 has no host mounts and no source sync: a `build` Resource needs the
    // fixture's embedded source. Ticket 09 makes the fixture real.
    if fixture.is_none() {
        let builds: Vec<&str> = manifest
            .resources
            .values()
            .filter(|resource| match &resource.kind {
                agentify_manifest::ResourceKind::App(app) => app.build.is_some(),
                agentify_manifest::ResourceKind::Database(_) => false,
            })
            .map(|resource| resource.name.as_str())
            .collect();
        if !builds.is_empty() {
            return Err(CreateError::SourceMissing(format!(
                "Resource(s) {} use `build`, but an inline Manifest brings no source; \
                 create build Resources from a bundled fixture instead",
                builds.join(", ")
            )));
        }
    }

    if let Some(existing) = state
        .find_project(&manifest.project)
        .map_err(CreateError::Store)?
    {
        let _ = existing;
        return Err(CreateError::AlreadyExists(format!(
            "a Project named {:?} already exists",
            manifest.project
        )));
    }

    // Every declared secret is resolved here — generated, or imported from the
    // host — before anything is written. A missing source refuses the whole
    // creation, so a failed create leaves no row, no directory, and no
    // credential behind.
    let planned = secrets
        .plan(&manifest.secrets)
        .map_err(|missing| CreateError::SecretSourceMissing(missing.detail()))?;

    let id = uuid::Uuid::new_v4().to_string();
    let project_dir = data_root.join(PROJECTS_DIR).join(&id);
    let workspace = project_dir.join(WORKSPACE_DIR);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("creating the workspace {}", workspace.display()))
        .map_err(CreateError::Store)?;

    let written = match fixture {
        Some(fixture) => write_fixture(fixture, &workspace).map_err(CreateError::Store),
        None => Ok(()),
    };
    if let Err(err) = written {
        discard(&project_dir);
        return Err(err);
    }

    let created_at = clock::now_rfc3339();
    let resources: Vec<(String, String, String)> = manifest
        .resources
        .values()
        .map(|resource| {
            (
                uuid::Uuid::new_v4().to_string(),
                resource.name.clone(),
                resource.kind_label().to_string(),
            )
        })
        .collect();

    let record = NewProject {
        id: id.clone(),
        name: manifest.project.clone(),
        source_kind: source.as_str().to_string(),
        source_name,
        spec_json: manifest.to_spec().to_string(),
        workspace_path: workspace.display().to_string(),
        created_at,
        environment_id: uuid::Uuid::new_v4().to_string(),
        environment_name: DEV_ENVIRONMENT.to_string(),
        resources,
        secrets: planned
            .iter()
            .map(|secret| NewSecret {
                name: secret.name.clone(),
                source_kind: secret.source_kind.to_string(),
                source_ref: secret.source_ref.clone(),
            })
            .collect(),
    };

    match state.create_project(record) {
        Ok(true) => {}
        Ok(false) => {
            discard(&project_dir);
            return Err(CreateError::AlreadyExists(format!(
                "a Project named {:?} already exists",
                manifest.project
            )));
        }
        Err(err) => {
            discard(&project_dir);
            return Err(CreateError::Store(err));
        }
    }

    // Only now do the values reach the store: a row without a value is
    // recoverable, a value with no row is not. A failure here puts everything
    // back the way it was.
    let mut written: Vec<&str> = Vec::new();
    for secret in &planned {
        if let Err(err) = secrets.put(&id, &secret.name, &secret.value) {
            for name in &written {
                let _ = secrets.delete(&id, name);
            }
            let _ = state.delete_project(&id);
            discard(&project_dir);
            return Err(CreateError::Store(anyhow::anyhow!(
                "storing the secret {:?} failed: {err}",
                secret.name
            )));
        }
        written.push(&secret.name);
    }

    let stored = state
        .find_project(&id)
        .map_err(CreateError::Store)?
        .ok_or_else(|| {
            CreateError::Store(anyhow::anyhow!(
                "the Project was written but could not be read back"
            ))
        })?;
    view(&stored).map_err(CreateError::Store)
}

/// Every Project, oldest first.
pub fn list(state: &State) -> Result<Vec<Project>> {
    state
        .list_projects()?
        .iter()
        .map(view)
        .collect::<Result<Vec<_>>>()
}

/// A Project by name or id.
pub fn get(state: &State, reference: &str) -> Result<Option<Project>> {
    match state.find_project(reference)? {
        Some(record) => Ok(Some(view(&record)?)),
        None => Ok(None),
    }
}

/// The directory the daemon owns for one Project's source.
pub fn workspace_path(data_root: &Path, id: &str) -> PathBuf {
    data_root.join(PROJECTS_DIR).join(id).join(WORKSPACE_DIR)
}

fn write_fixture(fixture: &fixtures::Fixture, workspace: &Path) -> Result<()> {
    for (relative, contents) in fixture.files {
        let target = workspace.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, contents)
            .with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(())
}

fn discard(project_dir: &Path) {
    let _ = std::fs::remove_dir_all(project_dir);
}

/// Build the wire view from the stored rows and the normalized spec.
fn view(record: &ProjectRecord) -> Result<Project> {
    let spec = stored_spec(record)?;

    let source = ProjectSource::parse(&record.source_kind).unwrap_or(ProjectSource::Manifest);
    let environment_state = if record.environment.observed_state.is_empty() {
        STATE_STOPPED.to_string()
    } else {
        record.environment.observed_state.clone()
    };
    let resources = record
        .resources
        .iter()
        .map(|resource| {
            let stored = spec.resources.get(&resource.name).cloned().unwrap_or_default();
            let mut view =
                stored.into_view(resource.id.clone(), resource.name.clone(), resource.kind.clone());
            // Resources share the Environment's observed state until per-Resource
            // state is observed separately.
            view.state = environment_state.clone();
            view
        })
        .collect();

    Ok(Project {
        id: record.id.clone(),
        name: record.name.clone(),
        source,
        source_name: record.source_name.clone(),
        created_at: record.created_at.clone(),
        workspace: record.workspace_path.clone(),
        environment: EnvironmentView {
            id: record.environment.id.clone(),
            name: record.environment.name.clone(),
            state: environment_state,
            // Per-Environment cost belongs to the status bar, which reads
            // `GET .../stats`; the rail keeps its placeholder.
            cost: Cost::default(),
        },
        resources,
        routes: spec.routes,
    })
}

/// One `<hostname>.<project>.localhost` route a Project declares.
///
/// A route is declared by a Resource port that names a hostname. A database
/// port is leased and probed but never published: nothing useful is served by
/// `db.<project>.localhost` over HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredRoute {
    pub resource: String,
    /// The guest port this route fronts, which is how the lease is found.
    pub guest_port: u16,
    /// The full hostname, e.g. `web.hello-pg.localhost`.
    pub hostname: String,
    /// The URL the UI shows, e.g. `https://web.hello-pg.localhost`.
    pub url: String,
}

/// The routes a Project declares, in Resource order.
pub fn declared_routes(record: &ProjectRecord) -> Result<Vec<DeclaredRoute>> {
    let spec = stored_spec(record)?;
    let mut routes = Vec::new();
    for resource in record.resources.iter() {
        let Some(stored) = spec.resources.get(&resource.name) else {
            continue;
        };
        for port in &stored.ports {
            routes.push(DeclaredRoute {
                resource: resource.name.clone(),
                guest_port: port.port,
                hostname: format!("{}.{}.localhost", port.hostname, record.name),
                url: port.url.clone(),
            });
        }
    }
    Ok(routes)
}

/// The guest ports this Project declares, as `(resource, guest port)` pairs the
/// daemon leases host ports for.
///
/// Only declared ports are leased. A database has no host port: it is reached
/// by `exec` inside its own Environment, never through the Windows loopback.
pub fn port_requests(record: &ProjectRecord) -> Result<Vec<(String, u16)>> {
    let spec = stored_spec(record)?;
    let mut requests = Vec::new();
    for resource in record.resources.iter() {
        let Some(stored) = spec.resources.get(&resource.name) else {
            continue;
        };
        for port in &stored.ports {
            requests.push((resource.name.clone(), port.port));
        }
    }
    Ok(requests)
}

/// The spec a Backend is handed, before the daemon adds its port leases.
///
/// Every `${secret:name}` the Manifest left in an env value or in a database's
/// credentials is resolved here, so the spec the Backend receives carries values
/// and the daemon's own stored spec never does. A Resource's environment is the
/// only place a value is injected: nothing else about the Project changes.
pub fn environment_spec(
    record: &ProjectRecord,
    secrets: &Secrets,
) -> std::result::Result<agentify_backends::EnvironmentSpec, LifecycleError> {
    let spec = stored_spec(record)?;
    let mut environment = agentify_backends::EnvironmentSpec::new(
        record.environment.id.clone(),
        record.name.clone(),
    );
    environment.workspace = PathBuf::from(&record.workspace_path);

    let values = secret_values(record, &spec, secrets)?;

    for resource in record.resources.iter() {
        let stored = spec.resources.get(&resource.name).cloned().unwrap_or_default();
        let kind = if stored.kind.is_empty() {
            resource.kind.clone()
        } else {
            stored.kind.clone()
        };

        let mut env = BTreeMap::new();
        for (key, raw) in &stored.env {
            env.insert(
                key.clone(),
                resolve(&resource.name, key, raw, &values)?,
            );
        }
        // A database's password is a secret reference too. The engine's own
        // variables are how it reaches the Resource, so the daemon resolves the
        // reference and passes it as `POSTGRES_PASSWORD` rather than inventing a
        // second mechanism.
        if kind == "database" {
            if let Some(credentials) = &stored.credentials {
                if let Some(username) = &credentials.username {
                    env.insert("POSTGRES_USER".to_string(), username.clone());
                }
                if let Some(secret_name) = &credentials.password_secret {
                    let value = values.get(secret_name).ok_or_else(|| {
                        LifecycleError::Secret(format!(
                            "database Resource {:?} takes its password from the secret {:?}, \
                             which this Project does not have",
                            resource.name, secret_name
                        ))
                    })?;
                    env.insert("POSTGRES_PASSWORD".to_string(), value.clone());
                }
            }
        }

        environment.with_resource(agentify_backends::ResourceSpec {
            name: resource.name.clone(),
            kind,
            image: stored.image.clone(),
            build: stored.build.as_ref().map(|build| agentify_backends::spec::BuildSpec {
                context: build.context.clone(),
                dockerfile: build.dockerfile.clone(),
            }),
            command: stored.command.clone(),
            env,
            ports: stored.ports.iter().map(|port| port.port).collect(),
            depends_on: stored.depends_on.clone(),
            healthcheck: stored.healthcheck.as_ref().map(|check| {
                agentify_backends::HealthcheckSpec {
                    port: check.port,
                    path: check.path.clone(),
                    interval: check.interval.clone(),
                }
            }),
            engine: stored.engine.clone(),
            version: stored.version.clone(),
            volume: stored.volume.clone(),
        });
    }
    Ok(environment)
}

/// The values of the secrets this Project's Manifest declares, by name.
///
/// A declared secret with no value in the store is a typed failure, not an empty
/// string: starting a Resource with a blank password would be worse than
/// refusing.
fn secret_values(
    record: &ProjectRecord,
    spec: &StoredSpec,
    secrets: &Secrets,
) -> std::result::Result<BTreeMap<String, String>, LifecycleError> {
    let mut values = BTreeMap::new();
    for name in spec.secrets.keys() {
        let value = secrets
            .get(&record.id, name)
            .map_err(|err| LifecycleError::Store(anyhow::anyhow!("{err}")))?
            .ok_or_else(|| {
                LifecycleError::Secret(format!(
                    "the secret {name:?} has no value in the credential manager; \
                     recreate it before starting the Environment"
                ))
            })?;
        values.insert(name.clone(), value);
    }
    Ok(values)
}

/// Replace every `${secret:name}` in one env value.
fn resolve(
    resource: &str,
    key: &str,
    raw: &str,
    values: &BTreeMap<String, String>,
) -> std::result::Result<String, LifecycleError> {
    const OPEN: &str = "${secret:";
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            return Err(LifecycleError::Validation {
                path: format!("resources.{resource}.env.{key}"),
                detail: format!("unterminated ${{secret:...}} in {raw:?}"),
            });
        };
        let name = &after[..end];
        let value = values.get(name).ok_or_else(|| {
            LifecycleError::Secret(format!(
                "resources.{resource}.env.{key} references the secret {name:?}, \
                 which this Project does not have"
            ))
        })?;
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn stored_spec(record: &ProjectRecord) -> Result<StoredSpec> {
    serde_json::from_str(&record.spec_json).with_context(|| {
        format!(
            "reading the stored spec of Project {:?} ({})",
            record.name, record.id
        )
    })
}

/// The normalized spec as it comes back out of the database.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StoredSpec {
    /// The secrets the Manifest declared, by name. Declarations only: no value
    /// is ever stored here.
    secrets: BTreeMap<String, agentify_manifest::Secret>,
    resources: BTreeMap<String, StoredResource>,
    routes: Vec<RouteView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct StoredResource {
    kind: String,
    image: Option<String>,
    build: Option<agentify_proto::BuildView>,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    ports: Vec<agentify_proto::PortView>,
    #[serde(rename = "dependsOn")]
    depends_on: Vec<String>,
    healthcheck: Option<agentify_proto::HealthcheckView>,
    engine: Option<String>,
    version: Option<String>,
    volume: Option<String>,
    credentials: Option<StoredCredentials>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct StoredCredentials {
    username: Option<String>,
    /// The secret the database's password comes from. A name, never a value.
    #[serde(rename = "passwordSecret")]
    password_secret: Option<String>,
}

impl StoredResource {
    fn into_view(&self, id: String, name: String, kind: String) -> ResourceView {
        let kind = if self.kind.is_empty() { kind } else { self.kind.clone() };
        ResourceView {
            id,
            name,
            url: self.ports.first().map(|port| port.url.clone()),
            image: self.image.clone(),
            build: self.build.clone(),
            command: self.command.clone(),
            env: self.env.clone(),
            ports: self.ports.clone(),
            depends_on: self.depends_on.clone(),
            healthcheck: self.healthcheck.clone(),
            engine: self.engine.clone(),
            version: self.version.clone(),
            volume: self.volume.clone(),
            username: self
                .credentials
                .as_ref()
                .and_then(|credentials| credentials.username.clone()),
            state: STATE_STOPPED.to_string(),
            cost: Cost::default(),
            kind,
        }
    }
}

/// Whether a Manifest still references a secret by name. A declared secret is
/// part of the Project's desired state, so deleting its value would break the
/// next start.
pub fn is_referenced(record: &ProjectRecord, name: &str) -> bool {
    stored_spec(record)
        .map(|spec| spec.secrets.contains_key(name))
        .unwrap_or(false)
}

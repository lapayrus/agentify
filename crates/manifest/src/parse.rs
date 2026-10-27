//! The parser: YAML text in, a normalized [`Manifest`] or a [`ManifestError`]
//! with a path out.
//!
//! Every object level has a closed key set (`deny_unknown`), so a typo is a
//! rejection rather than a silently ignored field. Semantic rules — exactly one
//! of `image`/`build`, resolvable secret references, unique hostnames, acyclic
//! dependencies, workspace-relative build contexts — are checked after the
//! shape is known, because they need the whole document.

use std::collections::{BTreeMap, BTreeSet};

use serde_yaml_ng::{Mapping, Value};

use crate::error::ManifestError;
use crate::model::{
    App, Build, Credentials, Database, Healthcheck, Manifest, Port, Resource, ResourceKind, Secret,
    SecretKind, DEFAULT_DATABASE_VERSION, DEFAULT_DOCKERFILE, DEFAULT_HEALTHCHECK_INTERVAL,
    MANIFEST_VERSION, POSTGRES,
};

/// Longest accepted Project, Resource, hostname, or volume name.
pub const MAX_NAME_LEN: usize = 48;

type Result<T> = std::result::Result<T, ManifestError>;

/// Parse and normalize a Manifest.
pub fn parse(source: &str) -> Result<Manifest> {
    let value: Value = serde_yaml_ng::from_str(source).map_err(|err| {
        ManifestError::new(
            "",
            format!("the manifest is not valid YAML: {}", one_line(&err.to_string())),
        )
    })?;
    let root = mapping("", &value)?;

    deny_unknown(&root, "", &["version", "project", "secrets", "resources"])?;

    let version = integer("version", require(&root, "", "version")?)?;
    if version != i64::from(MANIFEST_VERSION) {
        return Err(ManifestError::new(
            "version",
            format!(
                "unsupported manifest version {version}; only {MANIFEST_VERSION} is supported"
            ),
        ));
    }

    let project = string("project", require(&root, "", "project")?)?;
    validate_name("project", &project)?;

    let secrets = match get(&root, "secrets") {
        Some(value) => parse_secrets(value)?,
        None => BTreeMap::new(),
    };

    let resources_value = require(&root, "", "resources")?;
    let resources_map = mapping("resources", resources_value)?;
    if resources_map.is_empty() {
        return Err(ManifestError::new(
            "resources",
            "a Project needs at least one Resource",
        ));
    }

    let mut resources: BTreeMap<String, Resource> = BTreeMap::new();
    for (name, value) in resources_map {
        let name = match name.as_str() {
            Some(name) => name.to_string(),
            None => {
                return Err(ManifestError::new(
                    "resources",
                    "Resource names must be strings",
                ))
            }
        };
        let path = join("resources", &name);
        validate_name(&path, &name)?;
        let resource = parse_resource(&path, &name, &project, value, &secrets)?;
        resources.insert(name, resource);
    }

    check_dependencies(&resources)?;
    check_hostnames(&resources)?;

    Ok(Manifest {
        version: MANIFEST_VERSION,
        project,
        secrets,
        resources,
    })
}

fn parse_secrets(value: &Value) -> Result<BTreeMap<String, Secret>> {
    let map = mapping("secrets", value)?;
    let mut secrets = BTreeMap::new();
    for (name, value) in map {
        let name = name
            .as_str()
            .ok_or_else(|| ManifestError::new("secrets", "secret names must be strings"))?
            .to_string();
        let path = join("secrets", &name);
        validate_name(&path, &name)?;
        let body = mapping(&path, value)?;
        deny_unknown(&body, &path, &["generate", "fromEnv"])?;

        let generate = get(&body, "generate");
        let from_env = get(&body, "fromEnv");
        let secret = match (generate, from_env) {
            (Some(generate), None) => {
                let kind = string(&join(&path, "generate"), generate)?;
                let kind = match kind.as_str() {
                    "password" => SecretKind::Password,
                    "token" => SecretKind::Token,
                    other => {
                        return Err(ManifestError::new(
                            join(&path, "generate"),
                            format!("unsupported secret kind {other:?}; use `password` or `token`"),
                        ))
                    }
                };
                Secret::generated(kind)
            }
            (None, Some(from_env)) => {
                let env_path = join(&path, "fromEnv");
                let var = string(&env_path, from_env)?;
                validate_env_var_name(&env_path, &var)?;
                Secret::from_env(var)
            }
            (Some(_), Some(_)) => {
                return Err(ManifestError::new(
                    path,
                    "a secret has exactly one of `generate` or `fromEnv`",
                ))
            }
            (None, None) => {
                return Err(ManifestError::new(
                    path,
                    "a secret needs one of `generate` or `fromEnv`",
                ))
            }
        };
        secrets.insert(name, secret);
    }
    Ok(secrets)
}

fn parse_resource(
    path: &str,
    name: &str,
    project: &str,
    value: &Value,
    secrets: &BTreeMap<String, Secret>,
) -> Result<Resource> {
    let body = mapping(path, value)?;
    let kind_path = join(path, "kind");
    let kind = string(&kind_path, require(&body, path, "kind")?)?;

    let resource = match kind.as_str() {
        "app" => {
            deny_privileged(&body, path)?;
            deny_unknown(
                &body,
                path,
                &[
                    "kind",
                    "image",
                    "build",
                    "command",
                    "env",
                    "ports",
                    "dependsOn",
                    "healthcheck",
                ],
            )?;
            ResourceKind::App(parse_app(path, name, &body, secrets)?)
        }
        "database" => {
            deny_unknown(
                &body,
                path,
                &["kind", "engine", "version", "credentials", "volume"],
            )?;
            ResourceKind::Database(parse_database(path, name, project, &body, secrets)?)
        }
        other => {
            return Err(ManifestError::new(
                kind_path,
                format!("unsupported Resource kind {other:?}; v0 has `app` and `database`"),
            ))
        }
    };

    Ok(Resource {
        name: name.to_string(),
        kind: resource,
    })
}

fn parse_app(
    path: &str,
    name: &str,
    body: &Mapping,
    secrets: &BTreeMap<String, Secret>,
) -> Result<App> {
    let image = match get(body, "image") {
        Some(value) => {
            let image_path = join(path, "image");
            let image = string(&image_path, value)?;
            if image.trim().is_empty() {
                return Err(ManifestError::new(image_path, "must not be empty"));
            }
            Some(image)
        }
        None => None,
    };

    let build = match get(body, "build") {
        Some(value) => Some(parse_build(&join(path, "build"), value)?),
        None => None,
    };

    match (&image, &build) {
        (Some(_), Some(_)) => {
            return Err(ManifestError::new(
                path,
                "an app has exactly one of `image` or `build`, not both",
            ))
        }
        (None, None) => {
            return Err(ManifestError::new(
                path,
                "an app needs one of `image` or `build`",
            ))
        }
        _ => {}
    }

    let command = match get(body, "command") {
        Some(value) => {
            let command_path = join(path, "command");
            let items = sequence(&command_path, value)?;
            let mut command = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let item_path = format!("{command_path}[{index}]");
                let argument = string(&item_path, item)?;
                if argument.is_empty() {
                    return Err(ManifestError::new(
                        item_path,
                        "a command argument must not be empty",
                    ));
                }
                command.push(argument);
            }
            command
        }
        None => Vec::new(),
    };

    let env = match get(body, "env") {
        Some(value) => {
            let env_path = join(path, "env");
            let map = mapping(&env_path, value)?;
            let mut env = BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| ManifestError::new(&env_path, "env keys must be strings"))?
                    .to_string();
                validate_env_var_name(&join(&env_path, &key), &key)?;
                let value_path = join(&env_path, &key);
                let raw = string(&value_path, value)?;
                check_interpolations(&value_path, &raw, secrets)?;
                env.insert(key, raw);
            }
            env
        }
        None => BTreeMap::new(),
    };

    let ports = match get(body, "ports") {
        Some(value) => parse_ports(&join(path, "ports"), name, value)?,
        None => Vec::new(),
    };

    let depends_on = match get(body, "dependsOn") {
        Some(value) => {
            let depends_path = join(path, "dependsOn");
            let items = sequence(&depends_path, value)?;
            let mut depends_on = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let item_path = format!("{depends_path}[{index}]");
                let dependency = string(&item_path, item)?;
                validate_name(&item_path, &dependency)?;
                depends_on.push(dependency);
            }
            depends_on
        }
        None => Vec::new(),
    };

    let healthcheck = match get(body, "healthcheck") {
        Some(value) => Some(parse_healthcheck(&join(path, "healthcheck"), value)?),
        None => ports.first().map(|port| Healthcheck {
            port: port.port,
            path: None,
            interval: DEFAULT_HEALTHCHECK_INTERVAL.to_string(),
        }),
    };

    Ok(App {
        image,
        build,
        command,
        env,
        ports,
        depends_on,
        healthcheck,
    })
}

fn parse_build(path: &str, value: &Value) -> Result<Build> {
    let body = mapping(path, value)?;
    deny_unknown(&body, path, &["context", "dockerfile"])?;

    let context_path = join(path, "context");
    let context = string(&context_path, require(&body, path, "context")?)?;
    validate_build_context(&context_path, &context)?;

    let dockerfile = match get(&body, "dockerfile") {
        Some(value) => {
            let dockerfile_path = join(path, "dockerfile");
            let dockerfile = string(&dockerfile_path, value)?;
            validate_relative_file(&dockerfile_path, &dockerfile)?;
            dockerfile
        }
        None => DEFAULT_DOCKERFILE.to_string(),
    };

    Ok(Build {
        context,
        dockerfile,
    })
}

fn parse_ports(path: &str, resource: &str, value: &Value) -> Result<Vec<Port>> {
    let items = sequence(path, value)?;
    if items.is_empty() {
        return Err(ManifestError::new(path, "must declare at least one port"));
    }

    let mut ports = Vec::with_capacity(items.len());
    let mut seen: BTreeSet<u16> = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let body = mapping(&item_path, item)?;
        deny_unknown(&body, &item_path, &["port", "hostname"])?;

        let port_path = join(&item_path, "port");
        let port = integer(&port_path, require(&body, &item_path, "port")?)?;
        if !(1..=65535).contains(&port) {
            return Err(ManifestError::new(
                port_path,
                format!("{port} is not a port number (1-65535)"),
            ));
        }
        if !seen.insert(port as u16) {
            return Err(ManifestError::new(
                port_path,
                format!("port {port} is declared twice on {resource}"),
            ));
        }

        let hostname = match get(&body, "hostname") {
            Some(value) => {
                let hostname_path = join(&item_path, "hostname");
                let hostname = string(&hostname_path, value)?;
                validate_name(&hostname_path, &hostname)?;
                Some(hostname)
            }
            None => None,
        };

        ports.push((port as u16, hostname));
    }

    // One port may default its hostname to the Resource name; more than one
    // requires every hostname, because there is no sensible default.
    ports
        .into_iter()
        .enumerate()
        .map(|(index, (port, hostname))| {
            let hostname = match hostname {
                Some(hostname) => hostname,
                None if items.len() == 1 => resource.to_string(),
                None => {
                    return Err(ManifestError::new(
                        format!("{path}[{index}].hostname"),
                        "every port needs a hostname when a Resource declares more than one",
                    ))
                }
            };
            Ok(Port { port, hostname })
        })
        .collect()
}

fn parse_healthcheck(path: &str, value: &Value) -> Result<Healthcheck> {
    let body = mapping(path, value)?;
    deny_unknown(&body, path, &["port", "path", "interval"])?;

    let port_path = join(path, "port");
    let port = integer(&port_path, require(&body, path, "port")?)?;
    if !(1..=65535).contains(&port) {
        return Err(ManifestError::new(
            port_path,
            format!("{port} is not a port number (1-65535)"),
        ));
    }

    let path_value = match get(&body, "path") {
        Some(value) => {
            let path_field = join(path, "path");
            let raw = string(&path_field, value)?;
            if !raw.starts_with('/') {
                return Err(ManifestError::new(
                    path_field,
                    "an HTTP healthcheck path must start with `/`",
                ));
            }
            Some(raw)
        }
        None => None,
    };

    let interval = match get(&body, "interval") {
        Some(value) => {
            let interval_path = join(path, "interval");
            let raw = string(&interval_path, value)?;
            validate_interval(&interval_path, &raw)?;
            raw
        }
        None => DEFAULT_HEALTHCHECK_INTERVAL.to_string(),
    };

    Ok(Healthcheck {
        port: port as u16,
        path: path_value,
        interval,
    })
}

fn parse_database(
    path: &str,
    name: &str,
    project: &str,
    body: &Mapping,
    secrets: &BTreeMap<String, Secret>,
) -> Result<Database> {
    let engine_path = join(path, "engine");
    let engine = string(&engine_path, require(body, path, "engine")?)?;
    if engine != POSTGRES {
        return Err(ManifestError::new(
            engine_path,
            format!("unsupported database engine {engine:?}; v0 has `{POSTGRES}`"),
        ));
    }

    let version = match get(body, "version") {
        Some(value) => {
            let version_path = join(path, "version");
            let raw = scalar_string(&version_path, value)?;
            if raw.is_empty() || !raw.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                return Err(ManifestError::new(
                    version_path,
                    format!("{raw:?} is not a version number"),
                ));
            }
            raw
        }
        None => DEFAULT_DATABASE_VERSION.to_string(),
    };

    let credentials_path = join(path, "credentials");
    let credentials = mapping(&credentials_path, require(body, path, "credentials")?)?;
    deny_unknown(&credentials, &credentials_path, &["username", "password"])?;

    let username_path = join(&credentials_path, "username");
    let username = string(&username_path, require(&credentials, &credentials_path, "username")?)?;
    if username.trim().is_empty() {
        return Err(ManifestError::new(username_path, "must not be empty"));
    }

    let password_path = join(&credentials_path, "password");
    let password = string(&password_path, require(&credentials, &credentials_path, "password")?)?;
    let password_secret = secret_reference(&password_path, &password, secrets)?;

    let volume = match get(body, "volume") {
        Some(value) => {
            let volume_path = join(path, "volume");
            let raw = string(&volume_path, value)?;
            validate_name(&volume_path, &raw)?;
            raw
        }
        None => format!("{project}-{name}-data"),
    };

    Ok(Database {
        engine,
        version,
        credentials: Credentials {
            username,
            password_secret,
        },
        volume,
    })
}

/// Every `${secret:name}` in `raw` must resolve, and nothing else may be
/// interpolated.
fn check_interpolations(path: &str, raw: &str, secrets: &BTreeMap<String, Secret>) -> Result<()> {
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(ManifestError::new(
                path,
                format!("unterminated ${{...}} in {raw:?}"),
            ));
        };
        let expression = &after[..end];
        let name = expression.strip_prefix("secret:").ok_or_else(|| {
            ManifestError::new(
                path,
                format!("unsupported interpolation ${{{expression}}}; only ${{secret:name}} is supported"),
            )
        })?;
        if !secrets.contains_key(name) {
            return Err(ManifestError::new(
                path,
                format!("undefined secret {name:?}; declare it under `secrets:`"),
            ));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// A value that must be exactly one secret reference, such as a database
/// password.
fn secret_reference(
    path: &str,
    raw: &str,
    secrets: &BTreeMap<String, Secret>,
) -> Result<String> {
    check_interpolations(path, raw, secrets)?;
    let inner = raw
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'));
    match inner.and_then(|expression| expression.strip_prefix("secret:")) {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => Err(ManifestError::new(
            path,
            format!("must be a single ${{secret:name}} reference, not {raw:?}"),
        )),
    }
}

fn check_dependencies(resources: &BTreeMap<String, Resource>) -> Result<()> {
    for resource in resources.values() {
        let ResourceKind::App(app) = &resource.kind else {
            continue;
        };
        for (index, dependency) in app.depends_on.iter().enumerate() {
            let path = format!("resources.{}.dependsOn[{index}]", resource.name);
            if dependency == &resource.name {
                return Err(ManifestError::new(
                    path,
                    "a Resource cannot depend on itself",
                ));
            }
            if !resources.contains_key(dependency) {
                return Err(ManifestError::new(
                    path,
                    format!("unknown dependency {dependency:?}; no such Resource"),
                ));
            }
        }
    }

    // Depth-first search over `dependsOn` edges; a back edge is a cycle.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Unvisited,
        InProgress,
        Done,
    }
    fn dependencies(resource: &Resource) -> &[String] {
        match &resource.kind {
            ResourceKind::App(app) => &app.depends_on,
            ResourceKind::Database(_) => &[],
        }
    }
    fn visit(
        name: &str,
        resources: &BTreeMap<String, Resource>,
        marks: &mut BTreeMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        match marks.get(name).copied().unwrap_or(Mark::Unvisited) {
            Mark::Done => return Ok(()),
            Mark::InProgress => {
                let start = stack.iter().position(|item| item == name).unwrap_or(0);
                let mut cycle: Vec<&str> = stack[start..].iter().map(String::as_str).collect();
                cycle.push(name);
                return Err(ManifestError::new(
                    format!("resources.{name}.dependsOn"),
                    format!("dependency cycle: {}", cycle.join(" -> ")),
                ));
            }
            Mark::Unvisited => {}
        }

        marks.insert(name.to_string(), Mark::InProgress);
        stack.push(name.to_string());
        if let Some(resource) = resources.get(name) {
            for dependency in dependencies(resource) {
                if resources.contains_key(dependency) {
                    visit(dependency, resources, marks, stack)?;
                }
            }
        }
        stack.pop();
        marks.insert(name.to_string(), Mark::Done);
        Ok(())
    }

    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let mut stack = Vec::new();
    for name in resources.keys() {
        visit(name, resources, &mut marks, &mut stack)?;
    }
    Ok(())
}

fn check_hostnames(resources: &BTreeMap<String, Resource>) -> Result<()> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for resource in resources.values() {
        let ResourceKind::App(app) = &resource.kind else {
            continue;
        };
        for (index, port) in app.ports.iter().enumerate() {
            let path = format!("resources.{}.ports[{index}].hostname", resource.name);
            if let Some(owner) = seen.get(&port.hostname) {
                return Err(ManifestError::new(
                    path,
                    format!(
                        "duplicate hostname {:?}; it is already used by {owner} and hostnames are unique per Project",
                        port.hostname
                    ),
                ));
            }
            seen.insert(port.hostname.clone(), resource.name.clone());
        }
    }
    Ok(())
}

fn validate_name(path: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ManifestError::new(path, "must not be empty"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(ManifestError::new(
            path,
            format!("must be at most {MAX_NAME_LEN} characters"),
        ));
    }
    if !name.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        return Err(ManifestError::new(
            path,
            format!("{name:?} must start with a lower-case letter"),
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-'))
    {
        return Err(ManifestError::new(
            path,
            format!("{name:?} must be lower-case kebab-case; {bad:?} is not allowed"),
        ));
    }
    if name.ends_with('-') || name.contains("--") {
        return Err(ManifestError::new(
            path,
            format!("{name:?} must be lower-case kebab-case without empty segments"),
        ));
    }
    Ok(())
}

fn validate_env_var_name(path: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) => {
            (first.is_ascii_alphabetic() || first == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ManifestError::new(
            path,
            format!("{name:?} is not an environment variable name"),
        ))
    }
}

/// A build context must stay inside the Project's workspace: no absolute path,
/// no parent traversal, no Windows drive or UNC prefix.
fn validate_build_context(path: &str, context: &str) -> Result<()> {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::new(path, "must not be empty"));
    }
    if trimmed == "." {
        return Ok(());
    }
    let normalized = trimmed.replace('\\', "/");
    let absolute = normalized.starts_with('/')
        || normalized.starts_with("//")
        || normalized.get(1..2) == Some(":")
        || trimmed.starts_with('\\');
    if absolute {
        return Err(ManifestError::new(
            path,
            format!("{context:?} is absolute; a build context is relative to the Project's workspace"),
        ));
    }
    if normalized.split('/').any(|segment| segment == "..") {
        return Err(ManifestError::new(
            path,
            format!("{context:?} escapes the Project's workspace with `..`"),
        ));
    }
    Ok(())
}

fn validate_relative_file(path: &str, file: &str) -> Result<()> {
    let normalized = file.replace('\\', "/");
    let absolute = normalized.starts_with('/') || normalized.get(1..2) == Some(":");
    if absolute || normalized.split('/').any(|segment| segment == "..") {
        return Err(ManifestError::new(
            path,
            format!("{file:?} must be a path inside the build context"),
        ));
    }
    Ok(())
}

fn validate_interval(path: &str, raw: &str) -> Result<()> {
    let (digits, unit) = raw.split_at(
        raw.find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(raw.len()),
    );
    let valid = !digits.is_empty()
        && matches!(unit, "ms" | "s" | "m")
        && digits.parse::<u64>().map(|value| value > 0).unwrap_or(false);
    if valid {
        Ok(())
    } else {
        Err(ManifestError::new(
            path,
            format!("{raw:?} is not a duration such as 500ms, 5s, or 1m"),
        ))
    }
}

// ---------- small typed accessors over serde_yaml_ng::Value ----------

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.to_string()))
}

fn require<'a>(map: &'a Mapping, path: &str, key: &str) -> Result<&'a Value> {
    get(map, key).ok_or_else(|| {
        ManifestError::new(
            if path.is_empty() {
                key.to_string()
            } else {
                join(path, key)
            },
            format!("missing required field `{key}`"),
        )
    })
}

/// Reject the container privileges the threat model forbids, by name rather
/// than as an "unknown field" typo. The Backend cannot carry any of them, and
/// the parser is the first gate.
fn deny_privileged(map: &Mapping, path: &str) -> Result<()> {
    for key in map.keys() {
        let Some(key) = key.as_str() else {
            continue;
        };
        let message = match key {
            "privileged" => Some("privileged mode is not supported"),
            "hostNetwork" | "networkMode" => Some("host networking is not supported"),
            "pid" => Some("a host PID namespace is not supported"),
            "dockerSocket" => Some("Docker socket mounts are not supported"),
            "volumes" | "mounts" => Some("volume and socket mounts are not supported"),
            _ => None,
        };
        if let Some(message) = message {
            return Err(ManifestError::new(join(path, key), message.to_string()));
        }
    }
    Ok(())
}

fn deny_unknown(map: &Mapping, path: &str, allowed: &[&str]) -> Result<()> {
    for key in map.keys() {
        let Some(key) = key.as_str() else {
            return Err(ManifestError::new(
                path,
                "field names must be strings",
            ));
        };
        if !allowed.contains(&key) {
            let expected = allowed.join(", ");
            return Err(ManifestError::new(
                join(path, key),
                format!("unknown field `{key}`; expected one of {expected}"),
            ));
        }
    }
    Ok(())
}

fn mapping<'a>(path: &str, value: &'a Value) -> Result<&'a Mapping> {
    value
        .as_mapping()
        .ok_or_else(|| ManifestError::new(path, format!("expected a mapping, got {}", kind_of(value))))
}

fn sequence<'a>(path: &str, value: &'a Value) -> Result<&'a Vec<Value>> {
    value
        .as_sequence()
        .ok_or_else(|| ManifestError::new(path, format!("expected a list, got {}", kind_of(value))))
}

fn string(path: &str, value: &Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| ManifestError::new(path, format!("expected a string, got {}", kind_of(value))))
}

/// A scalar the author may write either quoted or bare, such as a Postgres
/// version (`"16"` or `16`).
fn scalar_string(path: &str, value: &Value) -> Result<String> {
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Ok(number.to_string());
    }
    if let Some(number) = value.as_f64() {
        return Ok(number.to_string());
    }
    Err(ManifestError::new(
        path,
        format!("expected a string or a number, got {}", kind_of(value)),
    ))
}

fn integer(path: &str, value: &Value) -> Result<i64> {
    if let Some(number) = value.as_i64() {
        return Ok(number);
    }
    if let Some(text) = value.as_str() {
        if let Ok(number) = text.trim().parse::<i64>() {
            return Ok(number);
        }
    }
    Err(ManifestError::new(
        path,
        format!("expected a whole number, got {}", kind_of(value)),
    ))
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

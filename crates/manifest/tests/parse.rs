//! Manifest parsing and normalization, table-driven.
//!
//! These are unit tests of the pure `manifest` module: no I/O, no daemon. The
//! same rules are exercised through the API seam in `crates/daemon/tests`.

use agentify_manifest::{parse, ManifestError, ResourceKind};

fn error(yaml: &str) -> ManifestError {
    parse(yaml).expect_err("the manifest must be rejected")
}

/// The path a rejection points at, so an Agent can fix the right line.
fn path(yaml: &str) -> String {
    error(yaml).path
}

#[test]
fn a_minimal_manifest_normalizes_one_app_with_an_image() {
    let manifest = parse(
        r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22-alpine
"#,
    )
    .expect("valid manifest");

    assert_eq!(manifest.version, 1);
    assert_eq!(manifest.project, "demo");
    let web = manifest.resources.get("web").expect("the web resource");
    match &web.kind {
        ResourceKind::App(app) => {
            assert_eq!(app.image.as_deref(), Some("node:22-alpine"));
            assert!(app.build.is_none());
            assert!(app.command.is_empty());
            assert!(app.ports.is_empty());
        }
        other => panic!("expected an app, got {other:?}"),
    }
}

#[test]
fn a_single_port_defaults_its_hostname_but_two_require_both_hostnames() {
    let defaulted = parse(
        r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    ports:
      - port: 3000
"#,
    )
    .expect("a single port may omit its hostname");
    let ResourceKind::App(app) = &defaulted.resources["web"].kind else {
        panic!("web is an app");
    };
    assert_eq!(app.ports[0].hostname, "web");
    assert_eq!(app.ports[0].url("demo"), "https://web.demo.localhost");

    let missing = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    ports:
      - { port: 3000, hostname: web }
      - port: 9229
"#;
    assert_eq!(path(missing), "resources.web.ports[1].hostname");
    assert!(error(missing).message.contains("hostname"), "{:?}", error(missing));

    let both = parse(
        r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    ports:
      - port: 3000
        hostname: api
      - port: 9229
        hostname: debug
"#,
    )
    .expect("two hostnames is fine");
    let ResourceKind::App(app) = &both.resources["web"].kind else {
        panic!("web is an app");
    };
    assert_eq!(app.ports[0].url("demo"), "https://api.demo.localhost");
    assert_eq!(app.ports[1].url("demo"), "https://debug.demo.localhost");
}

#[test]
fn a_healthcheck_defaults_to_a_tcp_probe_on_the_first_port() {
    let manifest = parse(
        r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    ports:
      - { port: 8080, hostname: web }
"#,
    )
    .expect("valid manifest");
    let ResourceKind::App(app) = &manifest.resources["web"].kind else {
        panic!("web is an app");
    };
    let healthcheck = app.healthcheck.as_ref().expect("a default healthcheck");
    assert_eq!(healthcheck.port, 8080);
    assert!(healthcheck.path.is_none(), "a TCP probe has no HTTP path");
    assert_eq!(healthcheck.interval, "5s");
}

#[test]
fn dependency_order_is_preserved_as_written() {
    let manifest = parse(
        r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    dependsOn: [cache, db]
  db:
    kind: database
    engine: postgres
    credentials: { username: demo, password: "${secret:pg}" }
  cache:
    kind: database
    engine: postgres
    version: 17
    credentials: { username: demo, password: "${secret:pg}" }
secrets:
  pg: { generate: password }
"#,
    )
    .expect("valid manifest");
    let ResourceKind::App(app) = &manifest.resources["web"].kind else {
        panic!("web is an app");
    };
    assert_eq!(
        app.depends_on,
        vec!["cache".to_string(), "db".to_string()]
    );
    let ResourceKind::Database(cache) = &manifest.resources["cache"].kind else {
        panic!("cache is a database");
    };
    assert_eq!(cache.version, "17", "a bare version number becomes a string");
    assert_eq!(cache.volume, "demo-cache-data");
}

#[test]
fn the_normalized_spec_carries_no_secret_values() {
    let yaml = include_str!("../../../fixtures/hello-pg/agentify.yaml");
    let manifest = parse(yaml).expect("the bundled fixture parses");
    let spec = manifest.to_spec();

    assert_eq!(spec["project"], "hello-pg");
    assert_eq!(spec["version"], 1);
    assert_eq!(spec["resources"]["web"]["kind"], "app");
    assert_eq!(
        spec["resources"]["web"]["ports"][0]["url"],
        "https://web.hello-pg.localhost"
    );
    assert_eq!(
        spec["resources"]["db"]["credentials"]["passwordSecret"],
        "pg-password"
    );
    assert_eq!(spec["resources"]["db"]["volume"], "hello-pg-db-data");
    assert_eq!(spec["secrets"]["pg-password"]["generate"], "password");
    assert_eq!(spec["secrets"]["api-token"]["fromEnv"], "HELLO_API_TOKEN");
    assert_eq!(
        spec["routes"],
        serde_json::json!([{
            "resource": "web",
            "hostname": "web",
            "url": "https://web.hello-pg.localhost",
        }])
    );
    assert!(
        !spec.to_string().contains("fromEnvValue"),
        "the spec describes secret sources, never values"
    );
}

#[test]
fn the_vertical_slice_manifest_normalizes_ports_secrets_and_the_database() {
    let yaml = include_str!("../../../fixtures/hello-pg/agentify.yaml");
    let manifest = parse(yaml).expect("the bundled fixture parses");

    assert_eq!(manifest.project, "hello-pg");
    assert_eq!(
        manifest.secrets.len(),
        2,
        "generated and imported secrets are both declared"
    );

    let web = manifest.resources.get("web").expect("web");
    let ResourceKind::App(app) = &web.kind else {
        panic!("web is an app");
    };
    assert_eq!(app.command, vec!["node".to_string(), "server.js".to_string()]);
    assert_eq!(app.depends_on, vec!["db".to_string()]);
    assert_eq!(app.ports.len(), 1);
    assert_eq!(app.ports[0].port, 3000);
    assert_eq!(app.ports[0].hostname, "web");
    assert_eq!(
        app.ports[0].url("hello-pg"),
        "https://web.hello-pg.localhost"
    );
    let healthcheck = app.healthcheck.as_ref().expect("declared healthcheck");
    assert_eq!(healthcheck.port, 3000);
    assert_eq!(healthcheck.path.as_deref(), Some("/health"));
    assert_eq!(healthcheck.interval, "5s");

    let db = manifest.resources.get("db").expect("db");
    let ResourceKind::Database(database) = &db.kind else {
        panic!("db is a database");
    };
    assert_eq!(database.engine, "postgres");
    assert_eq!(database.version, "16");
    assert_eq!(database.credentials.username, "hello");
    assert_eq!(database.credentials.password_secret, "pg-password");
    assert_eq!(
        database.volume, "hello-pg-db-data",
        "an omitted volume name is derived from the project and the resource"
    );

    let routes = manifest.routes();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].resource, "web");
    assert_eq!(routes[0].url, "https://web.hello-pg.localhost");
}

#[test]
fn unknown_fields_are_rejected_at_every_level() {
    let cases = [
        (
            "version: 1\nproject: demo\nextra: true\nresources:\n  web: { kind: app, image: node:22 }\n",
            "extra",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web: { kind: app, image: node:22, replicas: 2 }\n",
            "resources.web.replicas",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    build: { context: ., dockerfile: Dockerfile, target: prod }\n",
            "resources.web.build.target",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    image: node:22\n    ports:\n      - { port: 3000, hostname: web, protocol: tcp }\n",
            "resources.web.ports[0].protocol",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    image: node:22\n    healthcheck: { port: 3000, timeout: 2s }\n",
            "resources.web.healthcheck.timeout",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  db:\n    kind: database\n    engine: postgres\n    credentials: { username: demo, password: \"${secret:pg}\", database: demo }\nsecrets:\n  pg: { generate: password }\n",
            "resources.db.credentials.database",
        ),
        (
            "version: 1\nproject: demo\nsecrets:\n  pg: { generate: password, rotation: 30d }\nresources:\n  web: { kind: app, image: node:22 }\n",
            "secrets.pg.rotation",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web: { kind: app, image: node:22, privileged: true }\n",
            "resources.web.privileged",
        ),
    ];

    for (yaml, expected) in cases {
        let err = error(yaml);
        assert_eq!(err.path, expected, "wrong path for {yaml}");
        assert!(err.message.contains("unknown field"), "{:?}", err.message);
    }
}

#[test]
fn missing_required_keys_name_the_field() {
    let cases = [
        (
            "project: demo\nresources:\n  web: { kind: app, image: node:22 }\n",
            "version",
        ),
        (
            "version: 1\nresources:\n  web: { kind: app, image: node:22 }\n",
            "project",
        ),
        ("version: 1\nproject: demo\n", "resources"),
        (
            "version: 1\nproject: demo\nresources:\n  web: { image: node:22 }\n",
            "resources.web.kind",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  db:\n    kind: database\n    engine: postgres\n",
            "resources.db.credentials",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  db:\n    kind: database\n    engine: postgres\n    credentials: { username: demo }\n",
            "resources.db.credentials.password",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    build: { dockerfile: Dockerfile }\n",
            "resources.web.build.context",
        ),
    ];

    for (yaml, expected) in cases {
        let err = error(yaml);
        assert_eq!(err.path, expected, "wrong path for {yaml}");
        assert!(err.message.contains("missing required field"), "{:?}", err.message);
    }
}

#[test]
fn an_app_needs_exactly_one_of_image_or_build() {
    let neither = "version: 1\nproject: demo\nresources:\n  web: { kind: app }\n";
    assert_eq!(path(neither), "resources.web");
    assert!(error(neither).message.contains("image"));

    let both = "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    image: node:22\n    build: { context: . }\n";
    assert_eq!(path(both), "resources.web");
    assert!(error(both).message.contains("exactly one"));
}

#[test]
fn an_undefined_secret_reference_is_rejected() {
    let yaml = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    env:
      DATABASE_URL: postgres://demo:${secret:missing}@db:5432/demo
"#;
    let err = error(yaml);
    assert_eq!(err.path, "resources.web.env.DATABASE_URL");
    assert!(err.message.contains("undefined secret"), "{:?}", err.message);
    assert!(err.message.contains("missing"), "{:?}", err.message);
}

#[test]
fn only_the_secret_interpolation_is_supported() {
    let unsupported = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    env:
      GREETING: ${env:GREETING}
"#;
    let err = error(unsupported);
    assert_eq!(err.path, "resources.web.env.GREETING");
    assert!(
        err.message.contains("only") && err.message.contains("secret:name"),
        "{:?}",
        err.message
    );

    let unterminated = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    env:
      GREETING: ${secret:pg
secrets:
  pg: { generate: password }
"#;
    let err = error(unterminated);
    assert_eq!(err.path, "resources.web.env.GREETING");
    assert!(err.message.contains("unterminated"), "{:?}", err.message);
}

#[test]
fn a_database_password_must_be_a_secret_reference() {
    let literal = r#"
version: 1
project: demo
resources:
  db:
    kind: database
    engine: postgres
    credentials: { username: demo, password: hunter2 }
"#;
    let err = error(literal);
    assert_eq!(err.path, "resources.db.credentials.password");
    assert!(err.message.contains("${secret:name}"), "{:?}", err.message);
}

#[test]
fn only_postgres_and_version_one_are_supported() {
    let engine = r#"
version: 1
project: demo
resources:
  db:
    kind: database
    engine: mysql
    credentials: { username: demo, password: "${secret:pg}" }
secrets:
  pg: { generate: password }
"#;
    assert_eq!(path(engine), "resources.db.engine");

    let version = "version: 2\nproject: demo\nresources:\n  web: { kind: app, image: node:22 }\n";
    assert_eq!(path(version), "version");

    let kind = "version: 1\nproject: demo\nresources:\n  cache: { kind: service }\n";
    assert_eq!(path(kind), "resources.cache.kind");
}

#[test]
fn duplicate_hostnames_are_rejected() {
    let yaml = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    ports:
      - { port: 3000, hostname: app }
  api:
    kind: app
    image: node:22
    ports:
      - { port: 3000, hostname: app }
"#;
    let err = error(yaml);
    assert_eq!(err.path, "resources.web.ports[0].hostname");
    assert!(err.message.contains("duplicate hostname"), "{:?}", err.message);
    assert!(
        err.message.contains("api"),
        "the message names the Resource that claimed it first (Resources are visited by name): {:?}",
        err.message
    );
}

#[test]
fn dependency_cycles_self_dependencies_and_unknown_dependencies_are_rejected() {
    let cycle = r#"
version: 1
project: demo
resources:
  web:
    kind: app
    image: node:22
    dependsOn: [api]
  api:
    kind: app
    image: node:22
    dependsOn: [web]
"#;
    let err = error(cycle);
    assert!(err.path.starts_with("resources."), "path: {}", err.path);
    assert!(err.message.contains("dependency cycle"), "{:?}", err.message);

    let self_dependency = "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    image: node:22\n    dependsOn: [web]\n";
    let err = error(self_dependency);
    assert_eq!(err.path, "resources.web.dependsOn[0]");
    assert!(err.message.contains("itself"), "{:?}", err.message);

    let unknown = "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    image: node:22\n    dependsOn: [db]\n";
    let err = error(unknown);
    assert_eq!(err.path, "resources.web.dependsOn[0]");
    assert!(err.message.contains("unknown dependency"), "{:?}", err.message);
}

#[test]
fn names_are_lower_case_kebab() {
    for (yaml, expected) in [
        (
            "version: 1\nproject: Hello\nresources:\n  web: { kind: app, image: node:22 }\n",
            "project",
        ),
        (
            "version: 1\nproject: hello_pg\nresources:\n  web: { kind: app, image: node:22 }\n",
            "project",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  Web: { kind: app, image: node:22 }\n",
            "resources.Web",
        ),
        (
            "version: 1\nproject: demo\nresources:\n  web-: { kind: app, image: node:22 }\n",
            "resources.web-",
        ),
    ] {
        let err = error(yaml);
        assert_eq!(err.path, expected, "wrong path for {yaml}");
    }
}

#[test]
fn build_contexts_must_stay_inside_the_workspace() {
    for context in [r#"/etc"#, r#""C:\\secrets""#, r#""\\\\\\\\server\\share""#] {
        let yaml = format!(
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    build: {{ context: {context} }}\n"
        );
        let err = error(&yaml);
        assert_eq!(err.path, "resources.web.build.context", "context {context}");
        assert!(err.message.contains("absolute"), "{:?}", err.message);
    }

    for context in ["..", "../outside", "src/../../outside"] {
        let yaml = format!(
            "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    build: {{ context: {context} }}\n"
        );
        let err = error(&yaml);
        assert_eq!(err.path, "resources.web.build.context", "context {context}");
        assert!(err.message.contains("escapes"), "{:?}", err.message);
    }

    let nested = parse(
        "version: 1\nproject: demo\nresources:\n  web:\n    kind: app\n    build: { context: ./services/web }\n",
    )
    .expect("a workspace-relative context is fine");
    let ResourceKind::App(app) = &nested.resources["web"].kind else {
        panic!("web is an app");
    };
    assert_eq!(app.build.as_ref().expect("build").context, "./services/web");
    assert_eq!(app.build.as_ref().expect("build").dockerfile, "Dockerfile");
}

#[test]
fn malformed_yaml_is_a_validation_failure_without_a_path() {
    let err = error("version: 1\nproject: [unclosed\n");
    assert_eq!(err.path, "");
    assert!(!err.message.is_empty());
}

#[test]
fn an_empty_resources_map_is_rejected() {
    let err = error("version: 1\nproject: demo\nresources: {}\n");
    assert_eq!(err.path, "resources");
}

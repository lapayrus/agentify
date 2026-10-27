//! Bundled fixtures.
//!
//! The fixture's Manifest and source files are compiled into the daemon, so an
//! installed binary needs no repository paths: creating a Project from `hello-pg`
//! copies the embedded files into the Project's daemon-owned workspace.
//!
//! `hello-pg` is the vertical slice: a Node 22 app that reads a row from Postgres
//! 16, a migration an Agent runs through `exec`, and a health endpoint the daemon
//! probes. `hello` is the same Project under the shorter name, so the URL the
//! spec's acceptance check names — `https://web.hello.localhost` — exists without
//! a second copy of the source.

/// A fixture: a Manifest plus the files that make up its build context.
pub struct Fixture {
    /// Name an Agent passes as `{"fixture": "..."}`.
    pub name: &'static str,
    /// The fixture's `agentify.yaml`, parsed on create like any other Manifest.
    pub manifest: &'static str,
    /// `(relative path, contents)`, written into the Project workspace.
    pub files: &'static [(&'static str, &'static str)],
}

const HELLO_PG: Fixture = Fixture {
    name: "hello-pg",
    manifest: include_str!("../../../fixtures/hello-pg/agentify.yaml"),
    files: &[
        (
            "agentify.yaml",
            include_str!("../../../fixtures/hello-pg/agentify.yaml"),
        ),
        (
            "Dockerfile",
            include_str!("../../../fixtures/hello-pg/Dockerfile"),
        ),
        (
            "package.json",
            include_str!("../../../fixtures/hello-pg/package.json"),
        ),
        (
            "server.js",
            include_str!("../../../fixtures/hello-pg/server.js"),
        ),
        (
            "migrate.js",
            include_str!("../../../fixtures/hello-pg/migrate.js"),
        ),
    ],
};

/// The same slice under the Project name `hello`.
///
/// It is one `project:` line away from [`HELLO_PG`], and it exists because the
/// acceptance check names `https://web.hello.localhost`: a fixture whose name and
/// whose Project name are allowed to differ is the honest way to have both
/// without a second copy of the app that could drift from this one.
fn hello() -> &'static Fixture {
    static HELLO: std::sync::OnceLock<Fixture> = std::sync::OnceLock::new();
    HELLO.get_or_init(|| {
        let renamed = HELLO_PG.manifest.replace("project: hello-pg", "project: hello");
        assert_ne!(
            renamed, HELLO_PG.manifest,
            "the fixture's Manifest names its Project, or the rename above is a no-op"
        );
        let manifest: &'static str = Box::leak(renamed.into_boxed_str());
        let mut files = HELLO_PG.files.to_vec();
        files[0].1 = manifest;
        Fixture {
            name: "hello",
            manifest,
            files: Box::leak(files.into_boxed_slice()),
        }
    })
}

/// Every bundled fixture.
fn all() -> Vec<&'static Fixture> {
    vec![&HELLO_PG, hello()]
}

/// Look up a bundled fixture by name.
pub fn get(name: &str) -> Option<&'static Fixture> {
    all().into_iter().find(|fixture| fixture.name == name)
}

/// Names of the bundled fixtures, for error messages.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|fixture| fixture.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_fixture_parses_and_carries_its_build_context() {
        let fixture = get("hello-pg").expect("hello-pg is bundled");
        let manifest = agentify_manifest::parse(fixture.manifest).expect("the fixture parses");
        assert_eq!(manifest.project, "hello-pg");
        for required in ["Dockerfile", "server.js", "migrate.js", "package.json", "agentify.yaml"] {
            assert!(
                fixture.files.iter().any(|(path, _)| *path == required),
                "{required} is part of the fixture's build context"
            );
        }
        assert!(get("nope").is_none());
    }

    #[test]
    fn the_slice_runs_under_the_name_the_acceptance_check_uses() {
        let fixture = get("hello").expect("hello is bundled");
        let manifest = agentify_manifest::parse(fixture.manifest).expect("the fixture parses");
        assert_eq!(manifest.project, "hello", "https://web.hello.localhost");
        let web = manifest.resources.get("web").expect("the web Resource");
        match &web.kind {
            agentify_manifest::ResourceKind::App(app) => assert_eq!(
                app.ports.first().map(|port| port.url(&manifest.project)),
                Some("https://web.hello.localhost".to_string())
            ),
            other => panic!("web is an app, not {other:?}"),
        }
        let db = manifest.resources.get("db").expect("the db Resource");
        assert_eq!(db.kind_label(), "database");
        assert_eq!(
            manifest.routes().first().map(|route| route.url.as_str()),
            Some("https://web.hello.localhost")
        );
        // The same source as `hello-pg`: one app, two names.
        let sibling = get("hello-pg").expect("hello-pg");
        for (path, contents) in fixture.files {
            if *path == "agentify.yaml" {
                continue;
            }
            let original = sibling
                .files
                .iter()
                .find(|(other, _)| other == path)
                .map(|(_, contents)| *contents)
                .expect("the same file list");
            assert_eq!(*contents, original, "{path} is the same file in both fixtures");
        }
        assert_eq!(names(), vec!["hello-pg", "hello"]);
    }
}

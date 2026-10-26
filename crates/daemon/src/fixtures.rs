//! Bundled fixtures.
//!
//! The fixture's Manifest and source files are compiled into the daemon, so an
//! installed binary needs no repository paths: creating a Project from `hello-pg`
//! copies the embedded files into the Project's daemon-owned workspace.
//!
//! Ticket 09 turns this into a real vertical slice (migrations, a Postgres-backed
//! page, a health endpoint); ticket 02 only needs the build context to exist.

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
            "server.js",
            include_str!("../../../fixtures/hello-pg/server.js"),
        ),
    ],
};

/// Every bundled fixture.
const FIXTURES: &[Fixture] = &[HELLO_PG];

/// Look up a bundled fixture by name.
pub fn get(name: &str) -> Option<&'static Fixture> {
    FIXTURES.iter().find(|fixture| fixture.name == name)
}

/// Names of the bundled fixtures, for error messages.
pub fn names() -> Vec<&'static str> {
    FIXTURES.iter().map(|fixture| fixture.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_fixture_parses_and_carries_its_build_context() {
        let fixture = get("hello-pg").expect("hello-pg is bundled");
        let manifest = agentify_manifest::parse(fixture.manifest).expect("the fixture parses");
        assert_eq!(manifest.project, "hello-pg");
        assert!(
            fixture
                .files
                .iter()
                .any(|(path, _)| *path == "Dockerfile"),
            "a `build` Resource needs a Dockerfile in the workspace"
        );
        assert!(get("nope").is_none());
    }
}

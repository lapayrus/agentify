//! Route derivation: leases in, Caddy routes out.
//!
//! Routes are never edited by hand and never read back from Caddy: they are
//! recomputed from the Manifest plus the daemon's port leases on every start and
//! stop. That is what keeps "the URL" honest — if a lease moves, the route moves
//! with it, and a stopped Environment keeps its hostname answering 503.

use agentify_ingress::Route;

use crate::projects;
use crate::state::{LeaseRecord, ProjectRecord, State};

/// The page a stopped Environment serves at its own hostname.
///
/// It is written by the daemon, not proxied from an app that is not running, so
/// it can say what happened and what to do about it.
pub fn stopped_page(project: &str, hostname: &str) -> String {
    format!(
        "Project {project} is stopped.\n\n\
         {hostname} belongs to {project}, but nothing is running behind it.\n\
         Start the Environment in Agentify, then reload this page.\n"
    )
}

/// The routes one Environment should have right now.
///
/// `running` decides between a proxy to the lease and the stopped page; a
/// missing lease is the same as not running, because a route must never point at
/// a port nothing holds.
pub fn environment_routes(
    record: &ProjectRecord,
    leases: &[LeaseRecord],
    running: bool,
) -> anyhow::Result<Vec<Route>> {
    let mut routes = Vec::new();
    for declared in projects::declared_routes(record)? {
        let lease = leases
            .iter()
            .find(|lease| lease.resource == declared.resource && lease.guest_port == declared.guest_port);
        match (running, lease) {
            (true, Some(lease)) => routes.push(Route::upstream(declared.hostname, lease.host_port)),
            _ => {
                let body = stopped_page(&record.name, &declared.hostname);
                routes.push(Route::unavailable(declared.hostname, body));
            }
        }
    }
    Ok(routes)
}

/// Every route the daemon knows about, for a full load at startup.
///
/// A daemon that restarts has to republish what the machine was already
/// serving: an Environment it last saw running keeps proxying to its leases (the
/// Backend reconciles them separately), and one it last saw stopped keeps its
/// 503. Routes are never silently dropped by a restart.
pub fn recovery_routes(state: &State) -> anyhow::Result<Vec<(String, Route)>> {
    let mut routes = Vec::new();
    for record in state.list_projects()? {
        let leases = state.leases(&record.environment.id)?;
        let running = matches!(
            record.environment.observed_state.as_str(),
            agentify_proto::STATE_RUNNING | agentify_proto::STATE_DEGRADED
        );
        for route in environment_routes(&record, &leases, running)? {
            routes.push((record.environment.id.clone(), route));
        }
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentify_proto::{RouteView, PortView};
    use serde_json::json;

    fn record(spec: serde_json::Value) -> ProjectRecord {
        ProjectRecord {
            id: "project-1".to_string(),
            name: "hello-pg".to_string(),
            source_kind: "fixture".to_string(),
            source_name: Some("hello-pg".to_string()),
            spec_json: spec.to_string(),
            workspace_path: String::new(),
            created_at: String::new(),
            environment: crate::state::EnvironmentRecord {
                id: "env-1".to_string(),
                name: "dev".to_string(),
                desired_state: "stopped".to_string(),
                observed_state: "stopped".to_string(),
                health: "unknown".to_string(),
                updated_at: String::new(),
            },
            resources: vec![
                crate::state::ResourceRecord {
                    id: "r1".to_string(),
                    name: "web".to_string(),
                    kind: "app".to_string(),
                },
                crate::state::ResourceRecord {
                    id: "r2".to_string(),
                    name: "db".to_string(),
                    kind: "database".to_string(),
                },
            ],
        }
    }

    fn spec() -> serde_json::Value {
        json!({
            "routes": [RouteView {
                resource: "web".to_string(),
                hostname: "web".to_string(),
                url: "https://web.hello-pg.localhost".to_string(),
            }],
            "resources": {
                "web": {
                    "kind": "app",
                    "ports": [PortView {
                        port: 3000,
                        hostname: "web".to_string(),
                        url: "https://web.hello-pg.localhost".to_string(),
                    }],
                },
                "db": { "kind": "database", "engine": "postgres" },
            },
        })
    }

    fn lease(resource: &str, guest: u16, host: u16) -> LeaseRecord {
        LeaseRecord {
            environment_id: "env-1".to_string(),
            resource: resource.to_string(),
            guest_port: guest,
            host_port: host,
            created_at: String::new(),
        }
    }

    #[test]
    fn a_running_route_points_at_the_lease() {
        let routes =
            environment_routes(&record(spec()), &[lease("web", 3000, 40017)], true).expect("routes");
        assert_eq!(routes.len(), 1, "a database port is not published");
        assert_eq!(routes[0].hostname, "web.hello-pg.localhost");
        assert_eq!(
            routes[0].target,
            agentify_ingress::RouteTarget::Upstream { port: 40017 }
        );
    }

    #[test]
    fn a_stopped_route_keeps_its_hostname_and_explains_itself() {
        let routes = environment_routes(&record(spec()), &[], false).expect("routes");
        assert_eq!(routes[0].hostname, "web.hello-pg.localhost");
        match &routes[0].target {
            agentify_ingress::RouteTarget::Unavailable { body } => {
                assert!(body.contains("hello-pg"), "{body}");
                assert!(body.contains("web.hello-pg.localhost"), "{body}");
            }
            other => panic!("expected the stopped page, got {other:?}"),
        }
    }

    #[test]
    fn a_running_environment_with_no_lease_never_points_at_nothing() {
        let routes = environment_routes(&record(spec()), &[], true).expect("routes");
        assert!(!routes[0].is_upstream(), "no lease, no proxy");
    }
}

//! The Caddy admin client.
//!
//! Caddy is configured two ways: one full `POST /load` when it starts (and when
//! the daemon recovers), and granular writes afterwards — one route is appended,
//! replaced, or deleted inside one server, by index, with a stable `@id` on the
//! route itself. Nothing reloads the whole config to change one upstream.

use std::net::SocketAddr;

use serde_json::Value;

use crate::http::{Http, HttpError};

/// Where a server's routes live in the config tree.
fn routes_path(server: &str) -> String {
    format!("/config/apps/http/servers/{server}/routes")
}

fn route_path(server: &str, index: usize) -> String {
    format!("{}/{index}", routes_path(server))
}

/// Why an admin call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminError {
    /// The admin endpoint is not answering: Caddy is not running, or it died.
    Unreachable { addr: String, detail: String },
    /// Caddy answered with an error status.
    Refused {
        method: String,
        path: String,
        status: u16,
        body: String,
    },
    /// Caddy answered with something that is not the JSON we asked for.
    Malformed(String),
}

impl AdminError {
    pub fn detail(&self) -> String {
        match self {
            AdminError::Unreachable { addr, detail } => {
                format!("the Caddy admin endpoint at {addr} is unreachable: {detail}")
            }
            AdminError::Refused {
                method,
                path,
                status,
                body,
            } => {
                let body = body.trim();
                let body: String = body.chars().take(400).collect();
                format!("Caddy refused {method} {path} with {status}: {body}")
            }
            AdminError::Malformed(detail) => detail.clone(),
        }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for AdminError {}

impl From<HttpError> for AdminError {
    fn from(err: HttpError) -> Self {
        match err {
            HttpError::Transport(detail) => AdminError::Unreachable {
                addr: String::new(),
                detail,
            },
            HttpError::Malformed(detail) => AdminError::Malformed(detail),
        }
    }
}

/// The four admin operations ingress needs.
pub trait AdminClient: Send + Sync {
    /// The loopback endpoint this client talks to.
    fn addr(&self) -> SocketAddr;

    /// Replace the entire config. Used at startup and on recovery.
    fn load(&self, config: &Value) -> Result<(), AdminError>;

    /// Read one server's routes, in order.
    fn routes(&self, server: &str) -> Result<Vec<Value>, AdminError>;

    /// Append one route to a server.
    fn append_route(&self, server: &str, route: &Value) -> Result<(), AdminError>;

    /// Replace the route at `index`.
    fn put_route(&self, server: &str, index: usize, route: &Value) -> Result<(), AdminError>;

    /// Delete the route at `index`.
    fn delete_route(&self, server: &str, index: usize) -> Result<(), AdminError>;
}

/// A client over the loopback admin HTTP API.
#[derive(Debug, Clone)]
pub struct HttpAdminClient {
    http: Http,
}

impl HttpAdminClient {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            http: Http::new(addr),
        }
    }

    fn expect(&self, method: &str, path: &str, response: crate::http::Response) -> Result<(), AdminError> {
        if response.is_success() {
            return Ok(());
        }
        Err(AdminError::Refused {
            method: method.to_string(),
            path: path.to_string(),
            status: response.status,
            body: response.body_string(),
        })
    }
}

impl AdminClient for HttpAdminClient {
    fn addr(&self) -> SocketAddr {
        self.http.addr()
    }

    fn load(&self, config: &Value) -> Result<(), AdminError> {
        let body = serde_json::to_vec(config)
            .map_err(|err| AdminError::Malformed(format!("serialising the config: {err}")))?;
        let response = self.http.post_json("/load", &body)?;
        self.expect("POST", "/load", response)
    }

    fn routes(&self, server: &str) -> Result<Vec<Value>, AdminError> {
        let path = routes_path(server);
        let response = self.http.get(&path)?;
        if !response.is_success() {
            return Err(AdminError::Refused {
                method: "GET".to_string(),
                path,
                status: response.status,
                body: response.body_string(),
            });
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|err| AdminError::Malformed(format!("routes are not JSON: {err}")))?;
        match value {
            Value::Array(routes) => Ok(routes),
            other => Err(AdminError::Malformed(format!(
                "routes at {} are {} , not an array",
                self.addr(),
                match other {
                    Value::Null => "null".to_string(),
                    other => other.to_string().chars().take(80).collect(),
                }
            ))),
        }
    }

    fn append_route(&self, server: &str, route: &Value) -> Result<(), AdminError> {
        let path = routes_path(server);
        let body = serde_json::to_vec(route)
            .map_err(|err| AdminError::Malformed(format!("serialising a route: {err}")))?;
        let response = self.http.post_json(&path, &body)?;
        self.expect("POST", &path, response)
    }

    fn put_route(&self, server: &str, index: usize, route: &Value) -> Result<(), AdminError> {
        let path = route_path(server, index);
        let body = serde_json::to_vec(route)
            .map_err(|err| AdminError::Malformed(format!("serialising a route: {err}")))?;
        let response = self.http.put_json(&path, &body)?;
        self.expect("PUT", &path, response)
    }

    fn delete_route(&self, server: &str, index: usize) -> Result<(), AdminError> {
        let path = route_path(server, index);
        let response = self.http.delete(&path)?;
        self.expect("DELETE", &path, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{HttpMode, Route};
    use crate::testing::FakeAdmin;
    use serde_json::json;

    #[test]
    fn a_full_load_replaces_the_whole_config() {
        let admin = FakeAdmin::start();
        let config = json!({"apps": {"http": {"servers": {}}}});
        admin.client().load(&config).expect("load");
        assert_eq!(admin.loads(), 1);
        assert_eq!(admin.config(), config);
    }

    #[test]
    fn a_route_is_appended_then_replaced_then_deleted_by_index() {
        let admin = FakeAdmin::start();
        admin
            .client()
            .load(&json!({"apps": {"http": {"servers": {"s": {"routes": []}}}}}))
            .expect("load");

        let first = Route::upstream("web.demo.localhost", 40001).tls_json();
        admin.client().append_route("s", &first).expect("append");
        let second = Route::upstream("api.demo.localhost", 40002).tls_json();
        admin.client().append_route("s", &second).expect("append");

        let routes = admin.client().routes("s").expect("routes");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["@id"], first["@id"]);
        assert_eq!(
            routes[0]["handle"][0]["upstreams"][0]["dial"], "127.0.0.1:40001",
            "the first route is still first: append does not reorder"
        );

        let stopped = Route::unavailable("web.demo.localhost", "stopped").tls_json();
        admin.client().put_route("s", 0, &stopped).expect("replace");
        let routes = admin.client().routes("s").expect("routes");
        assert_eq!(routes.len(), 2, "replacing one route leaves the other alone");
        assert_eq!(routes[0]["handle"][0]["status_code"], 503);
        assert_eq!(routes[1]["@id"], second["@id"]);

        admin.client().delete_route("s", 0).expect("delete");
        let routes = admin.client().routes("s").expect("routes");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["@id"], second["@id"]);
    }

    #[test]
    fn the_two_servers_hold_separate_route_arrays() {
        let admin = FakeAdmin::start();
        admin
            .client()
            .load(&json!({
                "apps": {"http": {"servers": {
                    "tls": {"routes": []},
                    "http": {"routes": []}
                }}}
            }))
            .expect("load");
        let route = Route::upstream("web.demo.localhost", 40001);
        admin.client().append_route("tls", &route.tls_json()).expect("tls");
        admin
            .client()
            .append_route("http", &route.http_json(HttpMode::Redirect))
            .expect("http");

        assert_eq!(admin.client().routes("tls").expect("tls").len(), 1);
        let http = admin.client().routes("http").expect("http");
        assert_eq!(http.len(), 1);
        assert_eq!(http[0]["handle"][0]["status_code"], 308);
    }

    #[test]
    fn an_unreachable_admin_endpoint_is_a_typed_error() {
        // Port 1 on loopback: nothing listens there.
        let client = HttpAdminClient::new(SocketAddr::from(([127, 0, 0, 1], 1)));
        let err = client.load(&json!({})).expect_err("nothing listens");
        assert!(
            matches!(err, AdminError::Unreachable { .. }),
            "expected Unreachable, got {err:?}"
        );
    }

    #[test]
    fn a_refused_write_names_the_method_path_and_status() {
        let admin = FakeAdmin::start();
        admin
            .client()
            .load(&json!({"apps": {"http": {"servers": {"s": {"routes": []}}}}}))
            .expect("load");
        let err = admin
            .client()
            .put_route("s", 4, &json!({"@id": "x"}))
            .expect_err("there is no route 4");
        match err {
            AdminError::Refused { method, path, status, .. } => {
                assert_eq!(method, "PUT");
                assert_eq!(path, "/config/apps/http/servers/s/routes/4");
                assert_eq!(status, 404);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }
}

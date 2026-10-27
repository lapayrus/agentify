//! The route model, and the JSON Caddy is asked to serve.
//!
//! One route is one hostname. Two Caddy servers are configured: one terminating
//! TLS on the HTTPS port and one plain on the HTTP port, so the daemon decides
//! whether HTTP redirects (trust installed) or serves the app directly (trust
//! refused) without Caddy's own automatic redirect getting in the way.
//!
//! Every route carries an `@id` so it can be identified in the running config,
//! and route updates are granular: one route is put, replaced, or deleted.

use serde_json::{json, Value};

/// The Caddy server that terminates TLS.
pub const SERVER_TLS: &str = "agentify_tls";

/// The Caddy server that answers plain HTTP.
pub const SERVER_HTTP: &str = "agentify_http";

/// The `@id` prefix every route this daemon owns shares.
pub const ROUTE_ID_PREFIX: &str = "agentify";

/// What HTTP does on the plain port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMode {
    /// Serve the same routes as HTTPS: the CA root is not trusted, and an
    /// unverifiable redirect would only break the app.
    Serve,
    /// Redirect to the `https://` origin, because the root is trusted.
    Redirect,
}

/// Where a route points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteTarget {
    /// Reverse proxy to a lease the daemon holds on loopback.
    Upstream { port: u16 },
    /// A daemon-authored 503. A stopped Environment keeps its route; only
    /// removing the Environment deletes it.
    Unavailable { body: String },
}

/// One `<resource>.<project>.localhost` route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The full hostname, e.g. `web.hello-pg.localhost`.
    pub hostname: String,
    pub target: RouteTarget,
}

impl Route {
    pub fn upstream(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            target: RouteTarget::Upstream { port },
        }
    }

    pub fn unavailable(hostname: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            target: RouteTarget::Unavailable { body: body.into() },
        }
    }

    /// The `@id` Caddy knows this route by. Unique per server, which is why the
    /// scheme is part of it.
    pub fn id(&self, scheme: Scheme) -> String {
        format!("{ROUTE_ID_PREFIX}:{}:{}", scheme.as_str(), self.hostname)
    }

    /// True when traffic for this hostname is proxied rather than refused.
    pub fn is_upstream(&self) -> bool {
        matches!(self.target, RouteTarget::Upstream { .. })
    }

    /// The route as the TLS server serves it.
    pub fn tls_json(&self) -> Value {
        self.route_json(Scheme::Https, self.handler_json())
    }

    /// The route as the plain-HTTP server serves it.
    pub fn http_json(&self, mode: HttpMode) -> Value {
        let handler = match mode {
            HttpMode::Serve => self.handler_json(),
            HttpMode::Redirect => redirect_handler(&self.hostname),
        };
        self.route_json(Scheme::Http, handler)
    }

    fn route_json(&self, scheme: Scheme, handler: Value) -> Value {
        json!({
            "@id": self.id(scheme),
            "match": [{ "host": [self.hostname] }],
            "handle": [handler],
            "terminal": true,
        })
    }

    fn handler_json(&self) -> Value {
        match &self.target {
            RouteTarget::Upstream { port } => proxy_handler(*port),
            RouteTarget::Unavailable { body } => stopped_handler(body),
        }
    }
}

/// Which server a route id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Https,
    Http,
}

impl Scheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scheme::Https => "https",
            Scheme::Http => "http",
        }
    }

    pub fn server(&self) -> &'static str {
        match self {
            Scheme::Https => SERVER_TLS,
            Scheme::Http => SERVER_HTTP,
        }
    }
}

/// A reverse proxy to one loopback lease.
///
/// `flush_interval: -1` disables response buffering, which is what makes SSE
/// work; WebSockets need nothing extra because Caddy handles the upgrade
/// itself; and no timeouts are set, so a slow app is left alone.
fn proxy_handler(port: u16) -> Value {
    json!({
        "handler": "reverse_proxy",
        "upstreams": [{ "dial": format!("127.0.0.1:{port}") }],
        "flush_interval": -1,
        "headers": {
            "request": {
                "set": {
                    "X-Forwarded-For": ["{http.request.remote.host}"],
                    "X-Forwarded-Proto": ["{http.request.scheme}"],
                    "X-Forwarded-Host": ["{http.request.host}"],
                }
            }
        }
    })
}

/// The daemon's own 503: the Environment is stopped, the route is kept.
fn stopped_handler(body: &str) -> Value {
    json!({
        "handler": "static_response",
        "status_code": 503,
        "body": body,
        "headers": {
            "Content-Type": ["text/plain; charset=utf-8"],
            "Cache-Control": ["no-store"],
        }
    })
}

fn redirect_handler(hostname: &str) -> Value {
    json!({
        "handler": "static_response",
        "status_code": 308,
        "headers": {
            "Location": [format!("https://{hostname}{{http.request.uri}}")],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "web.hello-pg.localhost";

    fn proxied() -> Route {
        Route::upstream(HOST, 40017)
    }

    #[test]
    fn a_proxied_route_matches_its_hostname_and_carries_an_id_per_server() {
        let route = proxied();
        assert_eq!(route.id(Scheme::Https), "agentify:https:web.hello-pg.localhost");
        assert_eq!(route.id(Scheme::Http), "agentify:http:web.hello-pg.localhost");
        assert_ne!(
            route.id(Scheme::Https),
            route.id(Scheme::Http),
            "@id must be unique across the whole config, so the scheme is part of it"
        );

        let json = route.tls_json();
        assert_eq!(json["@id"], "agentify:https:web.hello-pg.localhost");
        assert_eq!(json["match"][0]["host"][0], HOST);
        assert_eq!(json["terminal"], true, "one route owns a hostname");
    }

    #[test]
    fn a_proxied_route_dials_the_lease_on_loopback_and_never_the_lan() {
        let json = proxied().tls_json();
        let handler = &json["handle"][0];
        assert_eq!(handler["handler"], "reverse_proxy");
        assert_eq!(handler["upstreams"][0]["dial"], "127.0.0.1:40017");
    }

    #[test]
    fn streaming_is_unbuffered_and_no_timeout_is_imposed() {
        let handler = proxied().tls_json()["handle"][0].clone();
        assert_eq!(
            handler["flush_interval"], -1,
            "SSE needs unbuffered responses: flush after every write"
        );
        for forbidden in ["transport", "timeouts", "dial_timeout", "response_header_timeout"] {
            assert!(
                handler.get(forbidden).is_none(),
                "no artificial {forbidden}: the app decides how slow it may be"
            );
        }
    }

    #[test]
    fn websockets_are_left_to_caddy() {
        let handler = proxied().tls_json()["handle"][0].clone();
        let websocket_keys = ["websocket", "upgrade", "websocket_upgrade"];
        for key in websocket_keys {
            assert!(
                handler.get(key).is_none(),
                "reverse_proxy upgrades WebSockets by default; {key} would be a lie"
            );
        }
    }

    #[test]
    fn forwarded_headers_are_set_from_the_request() {
        let set = &proxied().tls_json()["handle"][0]["headers"]["request"]["set"];
        assert_eq!(set["X-Forwarded-For"][0], "{http.request.remote.host}");
        assert_eq!(set["X-Forwarded-Proto"][0], "{http.request.scheme}");
        assert_eq!(set["X-Forwarded-Host"][0], "{http.request.host}");
    }

    #[test]
    fn a_stopped_environment_answers_a_daemon_authored_503() {
        let route = Route::unavailable(HOST, "Project hello-pg is stopped.");
        assert!(!route.is_upstream());

        let json = route.tls_json();
        let handler = &json["handle"][0];
        assert_eq!(handler["handler"], "static_response");
        assert_eq!(handler["status_code"], 503);
        assert_eq!(handler["body"], "Project hello-pg is stopped.");
        assert_eq!(handler["headers"]["Content-Type"][0], "text/plain; charset=utf-8");
        assert_eq!(
            handler["headers"]["Cache-Control"][0], "no-store",
            "a browser must not cache the stopped page and miss the start"
        );
        assert_eq!(
            json["match"][0]["host"][0], HOST,
            "the stopped route keeps its hostname: only removing the Environment deletes it"
        );
    }

    #[test]
    fn http_serves_the_same_route_when_the_root_is_not_trusted() {
        let route = proxied();
        let tls = route.tls_json();
        let http = route.http_json(HttpMode::Serve);
        assert_eq!(
            http["handle"], tls["handle"],
            "trust refused means HTTP serves, not redirects"
        );
        assert_eq!(http["match"], tls["match"]);
        assert_eq!(http["@id"], "agentify:http:web.hello-pg.localhost");
    }

    #[test]
    fn http_redirects_to_https_when_the_root_is_trusted() {
        let json = proxied().http_json(HttpMode::Redirect);
        let handler = &json["handle"][0];
        assert_eq!(handler["handler"], "static_response");
        assert_eq!(handler["status_code"], 308, "308 keeps the method and body");
        assert_eq!(
            handler["headers"]["Location"][0],
            "https://web.hello-pg.localhost{http.request.uri}"
        );
    }

    #[test]
    fn a_stopped_route_still_redirects_when_https_is_trusted() {
        // The redirect is about the scheme, not about what is running: the
        // https route then answers its own 503.
        let json = Route::unavailable(HOST, "stopped").http_json(HttpMode::Redirect);
        assert_eq!(json["handle"][0]["status_code"], 308);
    }
}

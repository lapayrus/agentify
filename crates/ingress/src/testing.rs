//! Test doubles.
//!
//! Two substitutions, and only two:
//!
//! * [`FakeAdmin`] speaks Caddy's admin API over real loopback HTTP, so the
//!   daemon's own route code and admin client are exercised end to end without
//!   starting Caddy.
//! * [`FakeTrustInstaller`] stands in for the certificate-store writes, so no
//!   automated test can reach the user's trust store.
//!
//! Available under `cfg(test)` and behind the `test-support` feature, which the
//! daemon's dev-dependencies enable.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::admin::HttpAdminClient;
use crate::trust::{Elevation, TrustError, TrustInstaller, TrustPrepare, TrustReport, TrustState};

// ---- a fake Caddy admin endpoint -------------------------------------------

#[derive(Debug, Default)]
struct Recorded {
    config: Value,
    loads: usize,
    requests: Vec<(String, String)>,
}

/// A tiny HTTP server that behaves like the slice of Caddy's admin API ingress
/// uses: `/load`, and `GET`/`POST`/`PUT`/`DELETE` under `/config/...`.
pub struct FakeAdmin {
    addr: SocketAddr,
    recorded: Arc<Mutex<Recorded>>,
    stop: Arc<AtomicBool>,
}

impl FakeAdmin {
    pub fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake admin");
        let addr = listener.local_addr().expect("fake admin addr");
        listener
            .set_nonblocking(true)
            .expect("non-blocking fake admin");
        let recorded = Arc::new(Mutex::new(Recorded {
            config: Value::Null,
            loads: 0,
            requests: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = recorded.clone();
        let stopper = stop.clone();
        std::thread::Builder::new()
            .name("fake-caddy-admin".to_string())
            .spawn(move || {
                while !stopper.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => handle(stream, &worker),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => return,
                    }
                }
            })
            .expect("spawn fake admin");

        Self {
            addr,
            recorded,
            stop,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn client(&self) -> HttpAdminClient {
        HttpAdminClient::new(self.addr)
    }

    /// How many full configs were loaded.
    pub fn loads(&self) -> usize {
        self.recorded.lock().expect("fake admin").loads
    }

    /// The config Caddy would be running right now.
    pub fn config(&self) -> Value {
        self.recorded.lock().expect("fake admin").config.clone()
    }

    /// One server's routes, in order.
    pub fn routes(&self, server: &str) -> Vec<Value> {
        let config = self.config();
        at_pointer(&config, &format!("/apps/http/servers/{server}/routes"))
            .and_then(|value| value.as_array())
            .map(|routes| routes.clone())
            .unwrap_or_default()
    }

    /// Every request, in order, as `(METHOD, path)`.
    pub fn requests(&self) -> Vec<(String, String)> {
        self.recorded.lock().expect("fake admin").requests.clone()
    }
}

impl Drop for FakeAdmin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle(mut stream: TcpStream, recorded: &Arc<Mutex<Recorded>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // On Windows an accepted socket inherits the listening socket's non-blocking
    // mode, so a request head that has not landed yet reads as `WouldBlock`
    // rather than blocking. Treating that as end-of-request drops a connection
    // the client is still writing to — intermittently, and more often the busier
    // the machine is — so wait for it, with a bound.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let split = loop {
        match stream.read(&mut buffer) {
            Ok(0) => break None,
            Ok(read) => {
                raw.extend_from_slice(&buffer[..read]);
                if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                    break Some(position);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break None,
        }
    };
    let Some(split) = split else { return };

    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_uppercase();
    let path = parts.next().unwrap_or_default().to_string();

    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = raw[split + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&buffer[..read]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break,
        }
    }

    let (status, payload) = dispatch(&method, &path, &body, recorded);
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

fn dispatch(
    method: &str,
    path: &str,
    body: &[u8],
    recorded: &Arc<Mutex<Recorded>>,
) -> (u16, Value) {
    let mut state = recorded.lock().expect("fake admin");
    state
        .requests
        .push((method.to_string(), path.to_string()));

    if path == "/load" {
        if method != "POST" {
            return (405, Value::Null);
        }
        match serde_json::from_slice::<Value>(body) {
            Ok(config) => {
                state.config = config;
                state.loads += 1;
                (200, Value::Null)
            }
            Err(err) => (405, serde_json::json!({ "error": err.to_string() })),
        }
    } else if let Some(rest) = path.strip_prefix("/config") {
        let mut segments: Vec<String> = rest
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect();
        if segments.is_empty() {
            return match method {
                "GET" => (200, state.config.clone()),
                _ => (405, Value::Null),
            };
        }
        let pointer = format!("/{}", segments.join("/"));
        match method {
            "GET" => match at_pointer(&state.config, &pointer) {
                Some(value) => (200, value.clone()),
                None => (404, Value::Null),
            },
            "PUT" => {
                let Some(value) = take_json(body) else {
                    return (405, Value::Null);
                };
                let (Some(parent), Some(last)) = (parent_pointer(&pointer), segments.pop()) else {
                    state.config = value;
                    return (200, Value::Null);
                };
                let Some(target) = at_pointer_mut(&mut state.config, &parent) else {
                    return (404, Value::Null);
                };
                match target {
                    Value::Object(map) => {
                        map.insert(last, value);
                        (200, Value::Null)
                    }
                    Value::Array(items) => match last.parse::<usize>() {
                        Ok(index) if index < items.len() => {
                            items[index] = value;
                            (200, Value::Null)
                        }
                        _ => (404, Value::Null),
                    },
                    _ => (404, Value::Null),
                }
            }
            "POST" => {
                let Some(value) = take_json(body) else {
                    return (405, Value::Null);
                };
                match at_pointer_mut(&mut state.config, &pointer) {
                    Some(Value::Array(items)) => {
                        items.push(value);
                        (200, Value::Null)
                    }
                    Some(_) => (405, Value::Null),
                    // Appending to a list that does not exist yet is how Caddy
                    // behaves for an unset path too: refuse, loudly.
                    None => (404, Value::Null),
                }
            }
            "DELETE" => {
                let Some(last) = segments.pop() else {
                    return (405, Value::Null);
                };
                let Some(parent) = parent_pointer(&pointer) else {
                    return (405, Value::Null);
                };
                let Some(target) = at_pointer_mut(&mut state.config, &parent) else {
                    return (404, Value::Null);
                };
                match target {
                    Value::Array(items) => match last.parse::<usize>() {
                        Ok(index) if index < items.len() => {
                            items.remove(index);
                            (200, Value::Null)
                        }
                        _ => (404, Value::Null),
                    },
                    Value::Object(map) => {
                        map.remove(&last);
                        (200, Value::Null)
                    }
                    _ => (404, Value::Null),
                }
            }
            _ => (405, Value::Null),
        }
    } else {
        // Anything else: the endpoint the daemon asked for does not exist.
        (404, Value::Null)
    }
}

fn take_json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(body).ok()
}

fn at_pointer<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    root.pointer(path)
}

fn at_pointer_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    root.pointer_mut(path)
}

/// `/apps/http/servers/s/routes/0` → `/apps/http/servers/s/routes`.
fn parent_pointer(pointer: &str) -> Option<String> {
    let (parent, _) = pointer.rsplit_once('/')?;
    if parent.is_empty() {
        None
    } else {
        Some(parent.to_string())
    }
}

// ---- a fake trust installer ------------------------------------------------

/// Records what an install would have done, and never touches a certificate
/// store.
#[derive(Debug, Default)]
pub struct FakeTrustInstaller {
    calls: Mutex<Vec<String>>,
    refuses: bool,
}

impl FakeTrustInstaller {
    /// Trust installs successfully.
    pub fn trusting() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            refuses: false,
        }
    }

    /// The user declines: trust is refused, and HTTP keeps serving.
    pub fn refusing() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            refuses: true,
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("fake trust").clone()
    }
}

impl TrustInstaller for FakeTrustInstaller {
    fn commands(&self, root_cert: &Path) -> TrustPrepare {
        TrustPrepare {
            steps: vec![crate::trust::TrustStep {
                program: "certutil".to_string(),
                args: vec![
                    "-addstore".to_string(),
                    "-user".to_string(),
                    "Root".to_string(),
                    root_cert.display().to_string(),
                ],
                elevation: Elevation::NotRequired,
                purpose: "fake".to_string(),
            }],
            notes: Vec::new(),
        }
    }

    fn removal_commands(&self) -> TrustPrepare {
        TrustPrepare {
            steps: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn install(&self, root_cert: &Path) -> Result<TrustReport, TrustError> {
        self.calls
            .lock()
            .expect("fake trust")
            .push(format!("install {}", root_cert.display()));
        let state = if self.refuses {
            TrustState::Refused
        } else {
            TrustState::Installed
        };
        Ok(TrustReport {
            state,
            applied: Vec::new(),
            manual: Vec::new(),
            notes: vec![if self.refuses {
                "the user declined HTTPS trust; HTTP serves the same routes".to_string()
            } else {
                "trusted".to_string()
            }],
        })
    }

    fn remove(&self) -> Result<TrustReport, TrustError> {
        self.calls.lock().expect("fake trust").push("remove".to_string());
        Ok(TrustReport {
            state: TrustState::Absent,
            applied: Vec::new(),
            manual: Vec::new(),
            notes: Vec::new(),
        })
    }
}

/// A map of what a config says, for a readable failure message.
pub fn summarise_routes(routes: &[Value]) -> BTreeMap<String, String> {
    routes
        .iter()
        .filter_map(|route| {
            let id = route.get("@id")?.as_str()?.to_string();
            let handler = route.get("handle")?.get(0)?;
            let what = match handler.get("handler")?.as_str()? {
                "reverse_proxy" => format!(
                    "proxy {}",
                    handler
                        .get("upstreams")
                        .and_then(|upstreams| upstreams.get(0))
                        .and_then(|upstream| upstream.get("dial"))
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ),
                "static_response" => format!(
                    "static {}",
                    handler
                        .get("status_code")
                        .map(|code| code.to_string())
                        .unwrap_or_default()
                ),
                other => other.to_string(),
            };
            Some((id, what))
        })
        .collect()
}

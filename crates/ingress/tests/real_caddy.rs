//! The one test that runs the real Caddy.
//!
//! It is opt-in (`AGENTIFY_TEST_CADDY=1`) because it starts a process and binds
//! two real ports. It proves three things no fake can:
//!
//! 1. Caddy accepts the JSON this daemon generates, including the granular
//!    route writes (`POST` to append, `PUT` by index, `DELETE` by index).
//! 2. `https://web.<project>.localhost` terminates TLS with the local CA and
//!    reaches the listener the daemon leased.
//! 3. The route carries `X-Forwarded-For`, `X-Forwarded-Proto`, and
//!    `X-Forwarded-Host` all the way to the app.
//!
//! It never installs trust: the TLS client here pins Caddy's root directly, so
//! the user's certificate store is untouched.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentify_ingress::{
    AdminClient, AdminMode, CaddyBinary, HttpAdminClient, Ingress, IngressOptions, Route,
    SERVER_HTTP, SERVER_TLS,
};

const HTTP_PORT: u16 = 18_080;
const HTTPS_PORT: u16 = 18_443;
const HOST: &str = "web.hello-pg.localhost";
const LEASE_ANSWER: &str = "hello from the lease";

fn enabled() -> bool {
    matches!(
        std::env::var("AGENTIFY_TEST_CADDY").as_deref().map(str::trim),
        Ok("1")
    )
}

/// An upstream that answers every request and records what it was sent.
struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
        let port = listener.local_addr().expect("upstream addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        std::thread::Builder::new()
            .name("upstream".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let recorder = recorder.clone();
                    std::thread::spawn(move || {
                        let mut raw = Vec::new();
                        let mut buffer = [0u8; 2048];
                        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => raw.extend_from_slice(&buffer[..read]),
                            }
                        }
                        recorder
                            .lock()
                            .expect("record")
                            .push(String::from_utf8_lossy(&raw).to_string());
                        let body = LEASE_ANSWER;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    });
                }
            })
            .expect("spawn upstream");
        Self { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("recorded requests").clone()
    }
}

#[test]
fn real_caddy_serves_https_for_a_localhost_route() {
    if !enabled() {
        eprintln!("skipping: set AGENTIFY_TEST_CADDY=1 to run the real-Caddy test");
        return;
    }

    let temp = tempfile::tempdir().expect("temp dir");
    let upstream = Upstream::start();

    // The bundled development binary, via the same lookup the daemon uses.
    let binary = CaddyBinary::locate(temp.path(), None).expect("a caddy binary in target/tools");
    eprintln!("real-caddy: using {}", binary.path.display());

    let ingress = Ingress::new(
        IngressOptions::new(temp.path())
            .with_ports(HTTP_PORT, HTTPS_PORT)
            .with_caddy(Some(binary.path.clone()))
            .with_admin(AdminMode::Spawn),
    );

    // Start, and add the route the way a `start_environment` would.
    ingress
        .sync_environment("env-1", &[Route::upstream(HOST, upstream.port)])
        .expect("sync the route into real Caddy");

    // 1. The granular writes landed: read the running config back.
    let admin_addr = admin_addr_from_config(&ingress).expect("the generated config names its admin port");
    let admin = HttpAdminClient::new(admin_addr);
    let routes = admin.routes(SERVER_TLS).expect("read the TLS routes back");
    assert_eq!(routes.len(), 1, "one route: {routes:?}");
    assert_eq!(routes[0]["@id"], "agentify:https:web.hello-pg.localhost");
    assert_eq!(
        routes[0]["handle"][0]["upstreams"][0]["dial"],
        format!("127.0.0.1:{}", upstream.port)
    );

    // 2. The local CA exists where the trust installer will look for it.
    let root = ingress.options().root_cert();
    let ca_pem = wait_for_ca(&root).unwrap_or_else(|| {
        panic!(
            "Caddy never wrote a root certificate at {}",
            root.display()
        )
    });

    // 3. https://web.<project>.localhost, pinned to that root, reaches the lease.
    //
    // The first handshake is what makes Caddy issue the leaf, so it is allowed
    // to take a moment; a browser does the same thing by retrying.
    let response = retry_https(HTTPS_PORT, HOST, &ca_pem, Duration::from_secs(30)).unwrap_or_else(
        |err| {
            panic!(
                "TLS request to Caddy: {err}\n--- caddy log ---\n{}",
                caddy_log(&ingress)
            )
        },
    );
    assert!(
        response.contains("200") && response.contains(LEASE_ANSWER),
        "the app behind the route answered: {response}"
    );

    // 4. Forwarded headers made it to the app.
    let seen = upstream.requests();
    assert!(!seen.is_empty(), "the upstream was reached");
    let request = seen.last().expect("a recorded request").to_ascii_lowercase();
    assert!(request.contains("x-forwarded-for:"), "{request}");
    assert!(request.contains("x-forwarded-proto: https"), "{request}");
    assert!(
        request.contains("x-forwarded-host: web.hello-pg.localhost"),
        "{request}"
    );

    // 5. Plain HTTP serves the same app when trust is absent.
    let plain = plain_get(HTTP_PORT, HOST, "/");
    assert!(
        plain.contains("200") && plain.contains(LEASE_ANSWER),
        "HTTP serves the route, not a redirect, without trust: {plain}"
    );

    // 6. A stopped Environment keeps the route and answers 503.
    ingress
        .sync_environment(
            "env-1",
            &[Route::unavailable(HOST, "Project hello-pg is stopped.")],
        )
        .expect("sync the stopped route");
    let stopped = https_get(HTTPS_PORT, HOST, &ca_pem).expect("TLS request after stop");
    assert!(stopped.contains(" 503"), "a stopped Environment is a 503: {stopped}");
    assert!(stopped.contains("Project hello-pg is stopped"), "{stopped}");

    // 7. Removing the Environment deletes the route: nothing answers for it.
    ingress.remove_environment("env-1").expect("remove the route");
    assert!(
        admin.routes(SERVER_TLS).expect("routes").is_empty(),
        "the route is gone from the running config"
    );
    let after = https_get(HTTPS_PORT, HOST, &ca_pem);
    match after {
        // Caddy has no route for the host any more: an empty 200/404 from the
        // default server, or a TLS failure because no certificate matches.
        Ok(response) => assert!(
            !response.contains(LEASE_ANSWER) && !response.contains("Project hello-pg"),
            "nothing serves the removed route: {response}"
        ),
        Err(_) => {}
    }

    drop(ingress);
    assert!(
        TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, HTTPS_PORT)),
            Duration::from_millis(500)
        )
        .is_err(),
        "stopping ingress stops Caddy"
    );
    let _ = SERVER_HTTP;
}

/// The admin port the generated config names.
fn admin_addr_from_config(ingress: &Ingress) -> Option<SocketAddr> {
    let path = ingress.options().config_path();
    let raw = std::fs::read_to_string(path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&raw).ok()?;
    config
        .get("admin")?
        .get("listen")?
        .as_str()?
        .parse()
        .ok()
}

/// Caddy's own log, for a failure that only it can explain.
fn caddy_log(ingress: &Ingress) -> String {
    std::fs::read_to_string(ingress.options().log_path()).unwrap_or_default()
}

/// Wait until Caddy has written its local root certificate.
fn wait_for_ca(path: &Path) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(pem) = std::fs::read_to_string(path) {
            if pem.contains("BEGIN CERTIFICATE") {
                return Some(pem);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// `GET /` over TLS, retried while Caddy issues the certificate for the name.
fn retry_https(port: u16, host: &str, ca_pem: &str, budget: Duration) -> Result<String, String> {
    let deadline = Instant::now() + budget;
    let mut last;
    loop {
        last = match https_get(port, host, ca_pem) {
            Ok(response) if response.starts_with("HTTP/1.") => return Ok(response),
            Ok(response) => format!("unexpected response: {response:?}"),
            Err(err) => err,
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `GET /` over TLS with `host` as the SNI name, trusting only `ca_pem`.
fn https_get(port: u16, host: &str, ca_pem: &str) -> Result<String, String> {
    let mut roots = rustls::RootCertStore::empty();
    for der in certificates(ca_pem) {
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|err| format!("adding the CA: {err}"))?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|err| format!("{host}: {err}"))?;
    let mut connection = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|err| format!("TLS setup: {err}"))?;

    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .map_err(|err| format!("connecting to 127.0.0.1:{port}: {err}"))?;
    let _ = socket.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(30)));

    {
        let mut tls = rustls::Stream::new(&mut connection, &mut socket);
        tls.write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|err| format!("TLS write: {err}"))?;
        let mut raw = Vec::new();
        let _ = tls.read_to_end(&mut raw);
        Ok(String::from_utf8_lossy(&raw).to_string())
    }
}

fn plain_get(port: u16, host: &str, path: &str) -> String {
    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut raw = Vec::new();
    let _ = socket.read_to_end(&mut raw);
    String::from_utf8_lossy(&raw).to_string()
}

/// Pull the DER bytes out of every PEM certificate block.
fn certificates(pem: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut base64 = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.contains("BEGIN CERTIFICATE") {
            inside = true;
            base64.clear();
        } else if line.contains("END CERTIFICATE") {
            if let Some(der) = decode_base64(&base64) {
                out.push(der);
            }
            inside = false;
            base64.clear();
        } else if inside {
            base64.push_str(line);
        }
    }
    out
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits: u32 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;
        bits = (bits << 6) | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

#[test]
fn the_pem_reader_finds_the_der_bytes() {
    // One tiny certificate, base64 of the DER a PEM block holds.
    let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    let der = certificates(pem);
    assert_eq!(der.len(), 1);
    assert!(!der[0].is_empty());
}

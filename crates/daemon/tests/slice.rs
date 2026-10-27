//! The vertical slice, end to end, on the real substrate.
//!
//! One scripted Agent run against a real daemon, the real WSL Backend, and the
//! real Caddy: create the bundled fixture, start it, run the migration through
//! `exec`, and read the page over `https://web.<project>.localhost`. Then the
//! parts only a real machine can prove — a daemon restart that keeps the data, a
//! second Project that cannot see the first one's data, and a delete that leaves
//! no distro, route, or lease behind.
//!
//! It is gated (`AGENTIFY_TEST_SLICE=1`) because it builds images, starts two
//! WSL distros, binds 80 and 443, and takes minutes. Nothing here installs trust:
//! the TLS client pins Caddy's own root, read from its data directory.
//!
//! Every path — including a panicking assertion — unregisters the distros it
//! minted, so `wsl -l -v` afterwards is exactly what it was before.

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::sync::Once;
use std::time::{Duration, Instant};

use agentify_backends::wsl::cli::Wsl;
use agentify_backends::wsl::template;
use agentify_daemon::consent::ConsentMode;
use agentify_daemon::Config;
use agentify_ingress::{IngressOptions, TrustState};
use serde_json::{json, Value};

/// The Project under test: the fixture's second name, so the URL is the one the
/// spec's acceptance check names.
const PROJECT: &str = "hello";
const HOST: &str = "web.hello.localhost";
/// A second Project, for the isolation checks.
const OTHER: &str = "hello-pg";
const OTHER_HOST: &str = "web.hello-pg.localhost";
/// Distro prefix for this suite, so its runtimes are obvious in `wsl -l -v`.
const PREFIX: &str = "agentify-slice";
/// The row `migrate.js` inserts, and the page renders.
const ROW: &str = "hello from postgres";
/// The value the fixture's `api-token` imports from the host.
const API_TOKEN: &str = "slice-api-token-from-the-host";
/// How long one image build may take.
const BUILD_BUDGET: Duration = Duration::from_secs(1800);

fn enabled() -> bool {
    matches!(
        std::env::var("AGENTIFY_TEST_SLICE").as_deref().map(str::trim),
        Ok("1")
    )
}

fn checkout() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(".."))
}

/// The daemon's data root, chosen so the cached rootfs and golden template under
/// `target/wsl-test/wsl/` are reused instead of rebuilt.
fn home() -> PathBuf {
    checkout().join("target").join("wsl-test")
}

fn caddy() -> PathBuf {
    checkout().join("target").join("tools").join("caddy.exe")
}

/// The knobs the daemon's `AGENTIFY_BACKEND=wsl` path reads. Set once, before any
/// daemon boots, and identical for every run in this binary.
fn prepare() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("AGENTIFY_WSL_PREFIX", PREFIX);
        std::env::set_var(
            "AGENTIFY_WSL_START_TIMEOUT_MS",
            BUILD_BUDGET.as_millis().to_string(),
        );
        // A slice always runs the Resources: no listener-only override, the
        // engine on, and the pinned rootfs (the cache lives under `home()`).
        std::env::remove_var("AGENTIFY_WSL_ROOTFS");
        std::env::remove_var("AGENTIFY_WSL_ENGINE");
        std::env::remove_var("AGENTIFY_WSL_WORKLOADS");
        std::env::remove_var("AGENTIFY_WSL_SYSTEMD");
    });
}

/// A repeatable run: clear the daemon's mutable state but keep the cached
/// rootfs and golden template, and unregister anything a previous run leaked.
fn reset() {
    let home = home();
    for name in [
        "state.db",
        "state.db-wal",
        "state.db-shm",
        "discovery.json",
        "slice.lock",
    ] {
        let _ = std::fs::remove_file(home.join(name));
    }
    for dir in ["projects", "ingress", "wsl/env"] {
        let _ = std::fs::remove_dir_all(home.join(dir));
    }
    let _ = template::unregister_all(&Wsl::new(PREFIX));
}

fn config() -> Config {
    let home = home();
    let ingress = IngressOptions::new(&home)
        .with_ports(80, 443)
        .with_caddy(Some(caddy()))
        // No trust is installed: HTTPS is still primary, and the test pins the
        // root instead of the machine trusting it.
        .with_trust(TrustState::Absent);
    let mut config = Config::new(&home)
        // The delete Approval is answered by the scripted verifier; no OS prompt
        // is ever opened.
        .with_consent(ConsentMode::Granted)
        .with_ingress(ingress)
        .with_secret_env([("HELLO_API_TOKEN", API_TOKEN)])
        .with_operation_timeout(BUILD_BUDGET);
    config.backend = "wsl".to_string();
    config.exec_timeout = Duration::from_secs(600);
    config.probe_timeout = Duration::from_secs(2);
    config
}

// ---------------------------------------------------------------------------
// A daemon the test drives over its HTTP API, exactly as an Agent would.
// ---------------------------------------------------------------------------

struct Daemon {
    inner: Option<agentify_daemon::RunningDaemon>,
}

impl Daemon {
    fn boot() -> Daemon {
        Daemon {
            inner: Some(
                agentify_daemon::start_in_background(config())
                    .expect("boot the daemon with the WSL Backend"),
            ),
        }
    }

    /// Boot, retrying while a previous Caddy is still letting go of 80 and 443.
    fn boot_after_restart() -> Daemon {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match agentify_daemon::start_in_background(config()) {
                Ok(daemon) => {
                    return Daemon {
                        inner: Some(daemon),
                    }
                }
                Err(err) if Instant::now() < deadline => {
                    eprintln!("waiting for the previous ingress to let go of 80/443: {err:#}");
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(err) => panic!("the daemon did not come back after a restart: {err:#}"),
            }
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.as_ref().expect("running").base_url())
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.request("POST", path, Some(&body.to_string()))
    }

    fn delete(&self, path: &str) -> (u16, Value) {
        self.request("DELETE", path, None)
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let url = self.url(path);
        let token = self.inner.as_ref().expect("running").token().to_string();
        let mut call = match method {
            "GET" => ureq::get(&url),
            "DELETE" => ureq::request("DELETE", &url),
            _ => ureq::post(&url),
        };
        call = call
            .set("Authorization", &format!("Bearer {token}"))
            .timeout(Duration::from_secs(1_900));
        if body.is_some() {
            call = call.set("Content-Type", "application/json");
        }
        let result = match body {
            Some(body) => call.send_string(body),
            None => call.call(),
        };
        let (status, text) = match result {
            Ok(response) => (response.status(), response.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(status, response)) => {
                (status, response.into_string().unwrap_or_default())
            }
            Err(err) => panic!("transport error for {url}: {err}"),
        };
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn create(&self, fixture: &str) {
        let (status, body) = self.post("/v1/projects", &json!({ "fixture": fixture }));
        assert_eq!(status, 201, "create {fixture}: {body}");
    }

    fn start(&self, project: &str) -> Value {
        let (status, body) = self.post(
            &format!("/v1/projects/{project}/environments/dev/start"),
            &json!({}),
        );
        assert_eq!(status, 200, "start {project}: {body}");
        body
    }

    fn status(&self, project: &str) -> Value {
        let (status, body) = self.get(&format!(
            "/v1/projects/{project}/environments/dev/status"
        ));
        assert_eq!(status, 200, "status of {project}: {body}");
        body
    }

    fn exec(&self, project: &str, resource: &str, command: &[&str]) -> Value {
        let (status, body) = self.post(
            &format!(
                "/v1/projects/{project}/environments/dev/resources/{resource}/exec"
            ),
            &json!({ "command": command }),
        );
        assert_eq!(status, 200, "exec {command:?} in {project}/{resource}: {body}");
        body
    }

    fn wait(&self, operation_id: &str) -> Value {
        let (status, operation) = self.get(&format!(
            "/v1/operations/{operation_id}/wait?timeoutMs=900000"
        ));
        assert_eq!(status, 200, "waiting for {operation_id}: {operation}");
        operation
    }

    fn shutdown(&mut self) {
        if let Some(mut daemon) = self.inner.take() {
            daemon.shutdown().expect("stop the daemon");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// The machine's distro list: unchanged afterwards, whatever happens.
// ---------------------------------------------------------------------------

struct Machine {
    before: Vec<String>,
}

impl Machine {
    fn new() -> Machine {
        Machine { before: names() }
    }

    fn cleanup(&self) {
        let _ = template::unregister_all(&Wsl::new(PREFIX));
    }

    fn finish(&self) {
        self.cleanup();
        let after = names();
        let leaked: Vec<&String> = after
            .iter()
            .filter(|name| name.to_ascii_lowercase().starts_with(PREFIX))
            .collect();
        assert!(
            leaked.is_empty(),
            "the slice left these distros registered: {leaked:?}"
        );
        let mut before = self.before.clone();
        let mut after_sorted = after.clone();
        before.sort();
        after_sorted.sort();
        assert_eq!(
            after_sorted, before,
            "`wsl -l -v` changed: before {:?}, after {:?}",
            self.before, after
        );
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // The safety net: even a panicking test leaves no runtime behind.
        self.cleanup();
    }
}

fn names() -> Vec<String> {
    Wsl::new("agentify")
        .list()
        .expect("list the machine's distros")
        .into_iter()
        .map(|distro| distro.name)
        .collect()
}

// ---------------------------------------------------------------------------
// Reading the page over TLS, with Caddy's root pinned
// ---------------------------------------------------------------------------

fn ca_pem() -> String {
    let root = IngressOptions::new(&home()).root_cert();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(pem) = std::fs::read_to_string(&root) {
            if pem.contains("BEGIN CERTIFICATE") {
                return pem;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Caddy never wrote a root certificate at {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `GET /` over TLS, retried while Caddy issues the certificate for the name.
fn get_page(host: &str, pem: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let last = match https_get(host, pem) {
            Ok(response) if response.starts_with("HTTP/1.") => return response,
            Ok(response) => format!("unexpected response: {response:?}"),
            Err(err) => err,
        };
        if Instant::now() >= deadline {
            panic!("no TLS answer from {host}: {last}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `GET /` over TLS, retried until the page says what it should.
///
/// A restart is the one moment the route can answer before the app does: the
/// daemon adopts the Environment in the background, so the first probes are a
/// 502 from Caddy until the workload is back. Waiting for the *content* is what
/// makes "the URL still serves" a statement about the app, not about Caddy.
fn get_page_saying(host: &str, pem: &str, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut last = String::new();
    loop {
        if let Ok(response) = https_get(host, pem) {
            if response.contains(" 200 ") && body(&response).contains(expected) {
                return response;
            }
            last = response;
        }
        assert!(
            Instant::now() < deadline,
            "{host} never served {expected:?} again; the last answer was: {last}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn https_get(host: &str, ca_pem: &str) -> Result<String, String> {
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
    let mut connection = rustls::ClientConnection::new(std::sync::Arc::new(config), name)
        .map_err(|err| format!("TLS setup: {err}"))?;

    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 443))
        .map_err(|err| format!("connecting to 127.0.0.1:443: {err}"))?;
    let _ = socket.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(30)));

    let mut tls = rustls::Stream::new(&mut connection, &mut socket);
    tls.write_all(
        format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .map_err(|err| format!("TLS write: {err}"))?;
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    Ok(String::from_utf8_lossy(&raw).to_string())
}

fn body(response: &str) -> &str {
    response.split("\r\n\r\n").nth(1).unwrap_or("")
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

// ---------------------------------------------------------------------------
// Leases as the API reports them
// ---------------------------------------------------------------------------

/// Every `(guest, host)` port pair the daemon leased for one Resource.
fn leases(status: &Value, resource: &str) -> Vec<(u16, u16)> {
    status["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("no resources in {status}"))
        .iter()
        .find(|entry| entry["name"] == resource)
        .and_then(|entry| entry["ports"].as_array())
        .map(|ports| {
            ports
                .iter()
                .filter_map(|port| {
                    Some((
                        port["guestPort"].as_u64()? as u16,
                        port["hostPort"].as_u64()? as u16,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// Poll the Environment's status until the daemon says it is running.
///
/// A daemon that restarts adopts its Environments in the background, so the app
/// can answer before the daemon has finished saying so; a command sent in that
/// window is refused with `NotProvisioned`. Waiting for the verdict is what a
/// client does.
fn running(daemon: &Daemon, project: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let status = daemon.status(project);
        if status["environment"]["state"] == "running" {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{project} never came back up: {status}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A row read straight out of a Resource's Postgres, through `exec`.
fn greeting(daemon: &Daemon, project: &str) -> String {
    let answer = daemon.exec(
        project,
        "db",
        &[
            "psql",
            "-U",
            "hello",
            "-d",
            "hello",
            "-tAc",
            "select message from greetings order by id",
        ],
    );
    assert_eq!(
        answer["exitCode"], 0,
        "{project}: reading the row failed: {answer}"
    );
    answer["stdout"].as_str().unwrap_or_default().trim().to_string()
}

/// The connection string a Resource was handed, so two Projects' credentials can
/// be compared without the API ever returning a secret.
fn database_url(daemon: &Daemon, project: &str) -> String {
    let answer = daemon.exec(
        project,
        "web",
        &["node", "-e", "process.stdout.write(process.env.DATABASE_URL || '')"],
    );
    assert_eq!(answer["exitCode"], 0, "{project}: {answer}");
    answer["stdout"].as_str().unwrap_or_default().to_string()
}

/// A digest of the connection string. The value itself is redacted on the way
/// out, so two Projects' credentials can only be compared without exposing them.
fn database_url_digest(daemon: &Daemon, project: &str) -> String {
    let answer = daemon.exec(
        project,
        "web",
        &[
            "node",
            "-e",
            "const c=require('crypto');process.stdout.write(c.createHash('sha256').update(process.env.DATABASE_URL||'').digest('hex'))",
        ],
    );
    assert_eq!(answer["exitCode"], 0, "{project}: {answer}");
    answer["stdout"].as_str().unwrap_or_default().trim().to_string()
}

/// The verifier a database cluster stores for the role the fixture uses.
///
/// Two Projects generate their passwords independently, so their clusters hold
/// different verifiers. Comparing them proves distinct credentials without
/// either password ever leaving a container: the value printed is a hash of a
/// hash, and neither cluster's password can be recovered from it.
fn stored_verifier(daemon: &Daemon, project: &str) -> String {
    let answer = daemon.exec(
        project,
        "db",
        &[
            "psql",
            "-U",
            "hello",
            "-d",
            "hello",
            "-tAc",
            "select md5(coalesce(rolpassword, '')) from pg_authid where rolname = 'hello'",
        ],
    );
    assert_eq!(answer["exitCode"], 0, "{project}: {answer}");
    let verifier = answer["stdout"].as_str().unwrap_or_default().trim().to_string();
    assert_eq!(verifier.len(), 32, "{project}: unexpected verifier {verifier:?}");
    verifier
}

/// Run a command and return `(exit code, stdout, stderr)`.
fn run(daemon: &Daemon, project: &str, resource: &str, command: &[&str]) -> (i64, String, String) {
    let answer = daemon.exec(project, resource, command);
    (
        answer["exitCode"].as_i64().unwrap_or(-1),
        answer["stdout"].as_str().unwrap_or_default().to_string(),
        answer["stderr"].as_str().unwrap_or_default().to_string(),
    )
}

/// `C:\a\b` → `/mnt/c/a/b`: the path a distro with automount would use for a
/// Windows directory, which is what "reading B's workspace from A" would mean if
/// anything ever mounted the daemon's data root into a Project.
fn windows_path_to_wsl(path: &str) -> String {
    let drive = path
        .chars()
        .next()
        .map(|letter| letter.to_ascii_lowercase())
        .unwrap_or('c');
    let rest = path.get(2..).unwrap_or("").replace('\\', "/");
    format!("/mnt/{drive}{rest}")
}

/// The audit chain: `{ entries, integrity }`, newest entry first.
fn activity(daemon: &Daemon) -> Value {
    let (status, body) = daemon.get("/v1/activity?limit=500");
    assert_eq!(status, 200, "the activity feed: {body}");
    body
}

/// One entry of the feed, by what it recorded.
fn audited<'a>(page: &'a Value, operation: &str, target: &str, result: &str) -> &'a Value {
    page["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("no entries in {page}"))
        .iter()
        .find(|entry| {
            entry["operation"] == operation
                && entry["target"] == target
                && entry["result"] == result
        })
        .unwrap_or_else(|| panic!("no {operation} entry for {target} ({result}): {page:#}"))
}

// ---------------------------------------------------------------------------
// The slice
// ---------------------------------------------------------------------------

#[test]
fn the_vertical_slice_runs_end_to_end_on_the_real_substrate() {
    if !enabled() {
        eprintln!("skipping: set AGENTIFY_TEST_SLICE=1 to run the vertical slice");
        return;
    }
    prepare();
    reset();
    let machine = Machine::new();
    assert!(
        caddy().is_file(),
        "the slice needs Caddy at {} (set it up once; see the ticket)",
        caddy().display()
    );

    // ---- 1. Create, and the Project appears with two Resources -------------
    let mut daemon = Daemon::boot();
    daemon.create(PROJECT);
    let (status, project) = daemon.get(&format!("/v1/projects/{PROJECT}"));
    assert_eq!(status, 200, "{project}");
    assert_eq!(project["name"], PROJECT);
    assert_eq!(project["environment"]["name"], "dev");
    assert_eq!(project["resources"].as_array().expect("resources").len(), 2);
    assert_eq!(project["routes"][0]["url"], format!("https://{HOST}"));

    // ---- 2. Start: the workloads build, run, and are reported healthy ------
    let started = daemon.start(PROJECT);
    assert_eq!(
        started["environment"]["state"], "running",
        "start must leave the Environment running: {started}"
    );
    assert_eq!(started["health"], "healthy", "{started}");
    assert_eq!(started["backend"]["name"], "wsl", "{started}");
    let resources = started["resources"].as_array().expect("resources");
    let web = resources
        .iter()
        .find(|resource| resource["name"] == "web")
        .unwrap_or_else(|| panic!("no web Resource: {started}"));
    assert_eq!(web["state"], "running", "{web}");
    assert_eq!(web["health"], "healthy", "{web}");
    let db = resources
        .iter()
        .find(|resource| resource["name"] == "db")
        .unwrap_or_else(|| panic!("no db Resource: {started}"));
    assert_eq!(db["state"], "running", "{db}");
    assert_eq!(
        db["health"], "unknown",
        "the database is internal, with no host port to probe: {db}"
    );
    assert!(
        !leases(&started, "web").is_empty(),
        "the app holds the host lease: {started}"
    );
    assert!(
        leases(&started, "db").is_empty(),
        "the database is reached by exec, not a host port: {started}"
    );

    // The route reaches the app before the migration has run. A 500 naming the
    // missing table is still proof that Caddy, the lease, and the container are
    // wired; the migration below makes the page a 200.
    let pem = ca_pem();
    let page = get_page(HOST, &pem);
    assert!(
        page.starts_with("HTTP/1."),
        "the app did not answer over the published route: {page}"
    );

    // ---- 3. The migration, through exec, and the row appears ---------------
    let migrated = daemon.exec(PROJECT, "web", &["node", "migrate.js"]);
    assert_eq!(migrated["exitCode"], 0, "migrate: {migrated}");
    assert!(
        migrated["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("1 row(s)"),
        "the migration reports the row it wrote: {migrated}"
    );
    // Idempotent: a retried migration changes nothing.
    let again = daemon.exec(PROJECT, "web", &["node", "migrate.js"]);
    assert_eq!(again["exitCode"], 0, "migrate twice: {again}");
    assert_eq!(greeting(&daemon, PROJECT), ROW);

    // ---- 4. https://web.hello.localhost renders that row -------------------
    let page = get_page(HOST, &pem);
    assert!(page.contains(" 200 OK") || page.contains(" 200 "), "{page}");
    assert!(
        body(&page).contains(ROW),
        "the page must render the row from Postgres: {page}"
    );

    // ---- 5. Restart the daemon: the row and the URL survive ----------------
    let web_lease = leases(&daemon.status(PROJECT), "web")[0].1;
    let project_id = project["id"].as_str().expect("the Project id").to_string();
    daemon.shutdown();
    let mut daemon = Daemon::boot_after_restart();
    // Right after a restart the daemon reports what it observed, not what it
    // wished: the supervisor it was holding is gone.
    let observed = daemon.status(PROJECT);
    eprintln!("after the restart: {observed}");

    // No `start` and no second migration: the daemon republishes the route from
    // the database and adopts the Environment, so the app is back on the same
    // hostname with the data the migration wrote before the restart.
    let page = get_page_saying(HOST, &pem, ROW);
    assert!(
        page.starts_with("HTTP/1.") && page.contains(" 200 "),
        "the URL must serve again as soon as the daemon is back: {page}"
    );
    let adopted = running(&daemon, PROJECT);
    eprintln!("adopted after the restart: {adopted}");
    assert_eq!(
        greeting(&daemon, PROJECT),
        ROW,
        "the row survived the daemon restart"
    );

    // A start after a restart still converges on the same runtime, the same
    // lease, and the same data: a restart is not a reset.
    let restarted = daemon.start(PROJECT);
    assert_eq!(
        restarted["environment"]["state"], "running",
        "a start after a restart converges on the same runtime: {restarted}"
    );
    assert_eq!(restarted["health"], "healthy", "{restarted}");
    assert_eq!(
        leases(&restarted, "web")[0].1,
        web_lease,
        "the restart keeps the lease the URL points at: {restarted}"
    );
    let page = get_page(HOST, &pem);
    assert!(
        body(&page).contains(ROW),
        "the URL still serves the row after a restart, on lease {web_lease}: {page}"
    );

    // ---- 6. A second Project starts alongside the first -------------------
    daemon.create(OTHER);
    let other = daemon.start(OTHER);
    assert_eq!(other["environment"]["state"], "running", "{other}");
    assert_eq!(other["health"], "healthy", "{other}");
    let (status, other_project) = daemon.get(&format!("/v1/projects/{OTHER}"));
    assert_eq!(status, 200, "{other_project}");
    let other_id = other_project["id"]
        .as_str()
        .expect("the second Project's id")
        .to_string();
    assert_ne!(project_id, other_id, "two Projects are two Projects");

    // Both routes are right, and distinct: one hostname per Project, each
    // pointing at its own lease.
    assert_eq!(project["routes"][0]["url"], format!("https://{HOST}"));
    assert_eq!(
        other_project["routes"][0]["url"],
        format!("https://{OTHER_HOST}")
    );
    assert_ne!(HOST, OTHER_HOST, "two Projects cannot share a hostname");

    // Separate leases: two Projects never share a host port.
    let hello_ports: Vec<u16> = ["web", "db"]
        .iter()
        .flat_map(|name| leases(&daemon.status(PROJECT), name))
        .map(|(_, host)| host)
        .collect();
    let other_ports: Vec<u16> = ["web", "db"]
        .iter()
        .flat_map(|name| leases(&daemon.status(OTHER), name))
        .map(|(_, host)| host)
        .collect();
    for port in &hello_ports {
        assert!(
            !other_ports.contains(port),
            "lease {port} is shared between two Projects: {hello_ports:?} and {other_ports:?}"
        );
    }
    // And each route fronts its own Project's lease, not the other's.
    assert!(hello_ports.contains(&web_lease), "{hello_ports:?}");
    assert_ne!(
        leases(&daemon.status(OTHER), "web")[0].1,
        web_lease,
        "the second Project has its own lease"
    );

    // Separate credentials: a password is generated per Project, so A's cannot
    // authenticate to B's database.
    let first = database_url(&daemon, PROJECT);
    let second = database_url(&daemon, OTHER);
    assert!(first.contains("@db:5432/"), "{first}");
    assert!(second.contains("@db:5432/"), "{second}");
    assert_ne!(
        database_url_digest(&daemon, PROJECT),
        database_url_digest(&daemon, OTHER),
        "two Projects must not share one database credential"
    );

    // ---- 6b. A cannot read B's volumes, credentials, or hostname -----------
    //
    // Volumes first. The same absolute path inside the database container is
    // each Project's own named volume, so a marker written by one is invisible
    // to the other: the control (`A` reads its own marker) makes the crossed
    // read a statement about the volume, not about a missing file.
    let marker = "/var/lib/postgresql/data/.agentify-isolation";
    let (code, out, err) = run(
        &daemon,
        PROJECT,
        "db",
        &["sh", "-c", &format!("echo {PROJECT} > {marker} && cat {marker}")],
    );
    assert_eq!(code, 0, "A writes into its own volume: {out}{err}");
    assert_eq!(out.trim(), PROJECT, "{out}");
    let (code, out, err) = run(
        &daemon,
        OTHER,
        "db",
        &["sh", "-c", &format!("echo {OTHER} > {marker} && cat {marker}")],
    );
    assert_eq!(code, 0, "B writes into its own volume: {out}{err}");
    assert_eq!(out.trim(), OTHER, "{out}");
    let (code, out, err) = run(&daemon, PROJECT, "db", &["cat", marker]);
    assert_eq!(code, 0, "A reads its own volume: {out}{err}");
    assert_eq!(
        out.trim(),
        PROJECT,
        "A read {out:?} from the path B wrote {OTHER:?} into: the volumes are not separate"
    );
    // B's volume, by the name the daemon gave it, is in B's runtime and nowhere
    // in A's.
    let other_volume = format!("/var/lib/docker/volumes/{OTHER}-db-data/_data/PG_VERSION");
    let (code, out, err) = run(&daemon, PROJECT, "db", &["cat", &other_volume]);
    assert_ne!(code, 0, "A read B's volume at {other_volume}: {out}{err}");

    // A's container has no host mount at all, so the daemon-owned workspace —
    // where both Projects' source lives — is outside it. That is the spec's
    // "no host mounts" rule, and it is what makes a workspace path unreachable
    // rather than merely unlisted.
    let (code, mounts, err) = run(&daemon, PROJECT, "web", &["cat", "/proc/mounts"]);
    assert_eq!(code, 0, "reading A's mount table: {err}");
    for forbidden in ["/mnt/c", "/mnt/wsl", " 9p ", "drvfs"] {
        assert!(
            !mounts.contains(forbidden),
            "A's container has {forbidden} mounted, so the host filesystem is reachable from it: {mounts}"
        );
    }
    // B's workspace, by the path a distro with automount would use, is not
    // readable from A; nor is the in-runtime copy of B's source.
    let other_workspace = other_project["workspace"]
        .as_str()
        .expect("B's workspace path");
    let guest = windows_path_to_wsl(other_workspace);
    let (code, out, err) = run(&daemon, PROJECT, "web", &["cat", &format!("{guest}/agentify.yaml")]);
    assert_ne!(
        code, 0,
        "A read B's workspace at {guest}: {out}{err}"
    );
    let (code, out, err) = run(
        &daemon,
        PROJECT,
        "web",
        &["cat", "/srv/agentify/project/agentify.yaml"],
    );
    assert_ne!(code, 0, "A read B's in-runtime workspace: {out}{err}");

    // B's hostname, from inside A. Windows resolves every name ending in
    // `.localhost` to loopback in every distro — the platform's rule, because
    // all WSL distros share one network namespace — so what must hold, and does,
    // is that the name leads nowhere but loopback and that loopback inside A is
    // A: nothing listens there, and B's page is not reachable by name.
    let (code, out, _) = run(
        &daemon,
        PROJECT,
        "web",
        &["sh", "-c", &format!("getent hosts {OTHER_HOST} || true")],
    );
    eprintln!(
        "`getent hosts {OTHER_HOST}` inside {PROJECT}/web: exit {code}, {:?}",
        out.trim()
    );
    for line in out.lines() {
        let address = line.split_whitespace().next().unwrap_or_default();
        assert!(
            address == "::1" || address == "127.0.0.1" || address.is_empty(),
            "B's hostname resolved to {address} inside A, which is not loopback: {out}"
        );
    }
    let fetch = format!(
        "fetch('http://{OTHER_HOST}/').then(r=>r.text()).then(()=>{{console.log('reached');process.exit(0)}})\
         .catch(e=>{{console.log('unreachable: '+((e.cause&&e.cause.code)||e.code||e.message));process.exit(1)}})"
    );
    let (code, out, _) = run(&daemon, PROJECT, "web", &["node", "-e", &fetch]);
    assert_ne!(
        code, 0,
        "A reached B's route through B's hostname: {out}"
    );

    // Separate data: A's row is not in B's database, because B's volume is its
    // own and A's migration never ran there.
    assert_eq!(greeting(&daemon, PROJECT), ROW);
    let crossed = daemon.exec(
        OTHER,
        "db",
        &[
            "psql",
            "-U",
            "hello",
            "-d",
            "hello",
            "-tAc",
            "select message from greetings order by id",
        ],
    );
    assert_ne!(
        crossed["exitCode"], 0,
        "Project B must not have Project A's table: {crossed}"
    );
    // The refusal names the missing table, so it is B's own empty database
    // answering rather than a connection that never happened.
    let refusal = format!(
        "{}{}",
        crossed["stdout"].as_str().unwrap_or_default(),
        crossed["stderr"].as_str().unwrap_or_default()
    );
    assert!(
        refusal.contains("greetings"),
        "B's own database says the table is missing: {refusal}"
    );

    // Separate credentials, proven at the server rather than by comparing
    // strings: each cluster stores a verifier for the password the daemon
    // generated for it, and the two verifiers are different passwords.
    assert_ne!(
        stored_verifier(&daemon, PROJECT),
        stored_verifier(&daemon, OTHER),
        "two Projects must not share one database password"
    );
    // And A's credentials do not open B's cluster: A's own connection string,
    // aimed at B's database, is refused. Neither value leaves a container.
    let (code, address, err) = run(&daemon, OTHER, "db", &["hostname", "-i"]);
    assert_eq!(code, 0, "B's address: {err}");
    let address = address
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(!address.is_empty(), "B's database has no address");
    let attempt = r#"
const pg = require('pg');
const url = new URL(process.env.DATABASE_URL);
const client = new pg.Client({
  host: process.argv[1],
  port: 5432,
  user: url.username,
  password: decodeURIComponent(url.password),
  database: url.pathname.slice(1),
  connectionTimeoutMillis: 5000,
});
client
  .connect()
  .then(() => client.query('select 1'))
  .then(() => { console.log('AUTHENTICATED'); return client.end(); })
  .then(() => process.exit(0))
  .catch((error) => {
    console.log('refused: ' + (error.code || error.message));
    process.exit(1);
  });
"#;
    let (code, out, err) = run(&daemon, PROJECT, "web", &["node", "-e", attempt, &address]);
    eprintln!("A's credentials against B's cluster at {address}: exit {code}, {:?}{:?}", out.trim(), err.trim());
    assert_ne!(code, 0, "A authenticated to B's database at {address}: {out}{err}");
    assert!(
        !out.contains("AUTHENTICATED"),
        "A authenticated to B's database: {out}"
    );

    // And B's own page is its own hostname. It has no migration yet, so a 500
    // naming the missing table is the expected answer.
    let other_page = get_page(OTHER_HOST, &pem);
    assert!(
        other_page.starts_with("HTTP/1."),
        "the second Project has its own URL: {other_page}"
    );

    // ---- 6c. The audit: every mutation, both Projects, one chain ----------
    let feed = activity(&daemon);
    assert_eq!(
        feed["integrity"]["verified"], true,
        "the chain verifies: {:#}",
        feed["integrity"]
    );
    let entries = feed["entries"].as_array().expect("entries");
    assert!(entries.len() >= 8, "the feed is not empty: {}", entries.len());
    // Newest first, so each entry links to the one after it in the array.
    for pair in entries.windows(2) {
        assert_eq!(
            pair[0]["prevHash"], pair[1]["hash"],
            "the chain is broken at seq {}: {pair:?}",
            pair[0]["seq"]
        );
    }
    let (status, whoami) = daemon.get("/v1/whoami");
    assert_eq!(status, 200, "{whoami}");
    let agent = whoami["actor"]["id"]
        .as_str()
        .expect("the Agent token id")
        .to_string();
    let expected: Vec<(&str, String, String, &str)> = vec![
        ("create_project", PROJECT.to_string(), project_id.clone(), "succeeded"),
        ("create_project", OTHER.to_string(), other_id.clone(), "succeeded"),
        ("start_environment", format!("{PROJECT}/dev"), project_id.clone(), "succeeded"),
        ("start_environment", format!("{OTHER}/dev"), other_id.clone(), "succeeded"),
        ("exec", format!("{PROJECT}/dev/web"), project_id.clone(), "succeeded"),
        ("exec", format!("{PROJECT}/dev/db"), project_id.clone(), "succeeded"),
        ("exec", format!("{OTHER}/dev/web"), other_id.clone(), "succeeded"),
        ("exec", format!("{OTHER}/dev/db"), other_id.clone(), "succeeded"),
    ];
    for (operation, target, project, result) in &expected {
        let entry = audited(&feed, *operation, target.as_str(), result);
        assert_eq!(entry["actor"]["kind"], "agent", "{entry}");
        assert_eq!(
            entry["actor"]["id"].as_str(),
            Some(agent.as_str()),
            "the token that made the call is the actor: {entry}"
        );
        assert_eq!(
            entry["projectId"].as_str(),
            Some(project.as_str()),
            "{entry}"
        );
        assert!(
            entry["hash"].as_str().map(str::len) == Some(64),
            "every entry is hashed: {entry}"
        );
    }

    // ---- 7. Delete, on the scripted consent, and nothing remains -----------
    let (accepted, asked) = daemon.delete(&format!("/v1/projects/{PROJECT}"));
    assert_eq!(accepted, 202, "a delete asks a human first: {asked}");
    let approval = asked["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("no approvalId: {asked}"))
        .to_string();
    let operation = asked["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("no operationId: {asked}"))
        .to_string();
    assert_eq!(
        daemon.get(&format!("/v1/projects/{PROJECT}")).0,
        200,
        "asking to delete changes nothing yet"
    );

    let (decided, decision) = daemon.post(&format!("/v1/approvals/{approval}/approve"), &json!({}));
    assert_eq!(decided, 200, "{decision}");
    let finished = daemon.wait(&operation);
    assert_eq!(finished["state"], "succeeded", "{finished}");

    let (missing, gone) = daemon.get(&format!("/v1/projects/{PROJECT}"));
    assert_eq!(missing, 404, "{gone}");

    // The distro is gone. Its name is minted from the Environment id, which the
    // daemon showed us when the Project was created.
    let environment_id = project["environment"]["id"]
        .as_str()
        .expect("the Environment id")
        .to_ascii_lowercase();
    let distro = format!("{PREFIX}-{environment_id}");
    assert!(
        Wsl::new(PREFIX).find(&distro).expect("list").is_none(),
        "the deleted Project's distro {distro} is still registered: {:?}",
        names()
    );
    // The route is gone: the hostname no longer serves the row.
    match https_get(HOST, &pem) {
        Ok(response) => assert!(
            !body(&response).contains(ROW),
            "the deleted Project's URL still serves its page: {response}"
        ),
        Err(_) => {}
    }
    // The lease is gone.
    for port in &hello_ports {
        assert!(!port_open(*port), "lease {port} is still bound after a delete");
    }

    // And the other Project is untouched by all of it.
    assert_eq!(daemon.status(OTHER)["environment"]["state"], "running");

    // ---- 8. The delete leaves the machine as it was -----------------------
    let (status, removed) = daemon.delete(&format!("/v1/projects/{OTHER}"));
    assert_eq!(status, 202, "{removed}");
    let approval = removed["approvalId"].as_str().expect("approvalId").to_string();
    let operation = removed["operationId"].as_str().expect("operationId").to_string();
    daemon
        .post(&format!("/v1/approvals/{approval}/approve"), &json!({}))
        .0;
    let finished = daemon.wait(&operation);
    assert_eq!(finished["state"], "succeeded", "{finished}");

    // ---- 9. The audit of both Projects, deletions included ----------------
    let feed = activity(&daemon);
    assert_eq!(
        feed["integrity"]["verified"], true,
        "the chain still verifies after both Projects are gone: {:#}",
        feed["integrity"]
    );
    let entries = feed["entries"].as_array().expect("entries");
    for pair in entries.windows(2) {
        assert_eq!(
            pair[0]["prevHash"], pair[1]["hash"],
            "the chain is broken at seq {}: {pair:?}",
            pair[0]["seq"]
        );
    }
    // A delete is three mutations — asked, decided, done — and each one names
    // the Project it touched. A deleted Project keeps its history.
    for name in [PROJECT, OTHER] {
        for result in ["requested", "approved", "succeeded"] {
            let entry = audited(&feed, "delete_project", name, result);
            assert_eq!(entry["target"], name, "{entry}");
            assert!(
                entry["projectId"].as_str().map(str::len) == Some(36),
                "every delete entry names the Project it touched: {entry}"
            );
        }
    }

    daemon.shutdown();
    machine.finish();
}

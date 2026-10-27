//! The vertical slice through the MCP shim: create → start → migrate → read,
//! and the four acceptance checks that need a real machine — a daemon restart
//! that keeps the data, a second Project the first cannot see, every mutation in
//! the audit, and a delete that leaves nothing behind.
//!
//! The shim is a client of the daemon API like any other, so this drives the real
//! `agentify-mcp` process over line-delimited JSON-RPC against a real daemon with
//! the real WSL Backend: an Agent that only ever speaks MCP can own the whole
//! environment.
//!
//! Gated by `AGENTIFY_TEST_SLICE=1`, for the same reason the API-seam slice is: it
//! builds an image, starts two distros, and binds 80 and 443. It takes the same
//! machine-wide lock, so the two gated suites can share one data root and one
//! pair of ports without stepping on each other.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The Project under test: the fixture under the shorter name, so the URL is the
/// one the acceptance check names.
const PROJECT: &str = "hello";
/// The hostname that Project's app is published at.
const HOST: &str = "web.hello.localhost";
/// A second Project, for the isolation check.
const OTHER: &str = "hello-pg";
/// And its hostname.
const OTHER_HOST: &str = "web.hello-pg.localhost";
/// Distro prefix, shared with the API-seam slice.
const PREFIX: &str = "agentify-slice";
/// The row `migrate.js` inserts.
const ROW: &str = "hello from postgres";
/// The value the fixture's `api-token` imports from the host.
const API_TOKEN: &str = "slice-api-token-from-the-host";

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

fn home() -> PathBuf {
    checkout().join("target").join("wsl-test")
}

fn caddy() -> PathBuf {
    checkout().join("target").join("tools").join("caddy.exe")
}

// ---------------------------------------------------------------------------
// One machine, one slice at a time
// ---------------------------------------------------------------------------

/// The slice owns port 80, port 443, and one data root, so two test binaries
/// cannot run it at once. This is a plain exclusive-create lock file with an
/// expiry, not a library: a crashed run must not wedge the next one.
struct SliceLock {
    path: PathBuf,
}

impl SliceLock {
    fn acquire() -> SliceLock {
        let path = home().join("slice.lock");
        let deadline = Instant::now() + Duration::from_secs(3_600);
        loop {
            if let Some(owner) = stale(&path) {
                eprintln!("taking over the slice lock from {owner}");
                let _ = std::fs::remove_file(&path);
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut handle) => {
                    let _ = writeln!(
                        handle,
                        "pid {} at {}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|elapsed| elapsed.as_secs())
                            .unwrap_or(0)
                    );
                    return SliceLock { path };
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    assert!(
                        Instant::now() < deadline,
                        "the slice lock {} is held by another run",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(_) => return SliceLock { path },
            }
        }
    }
}

impl Drop for SliceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A lock older than an hour belongs to a run that died; its owner is reported.
fn stale(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    if modified.elapsed().ok()? > Duration::from_secs(3_600) {
        Some(text.trim().to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The daemon and the shim
// ---------------------------------------------------------------------------

fn config() -> agentify_daemon::Config {
    std::env::set_var("AGENTIFY_WSL_PREFIX", PREFIX);
    std::env::set_var("AGENTIFY_WSL_START_TIMEOUT_MS", "1800000");
    std::env::remove_var("AGENTIFY_WSL_ROOTFS");
    std::env::remove_var("AGENTIFY_WSL_ENGINE");
    std::env::remove_var("AGENTIFY_WSL_WORKLOADS");

    let home = home();
    let ingress = agentify_ingress::IngressOptions::new(&home)
        .with_ports(80, 443)
        .with_caddy(Some(caddy()))
        .with_trust(agentify_ingress::TrustState::Absent);
    let mut config = agentify_daemon::Config::new(&home)
        // The delete Approval is answered by the scripted verifier: no OS prompt.
        .with_consent(agentify_daemon::consent::ConsentMode::Granted)
        .with_ingress(ingress)
        .with_secret_env([("HELLO_API_TOKEN", API_TOKEN)])
        .with_operation_timeout(Duration::from_secs(1_800));
    config.backend = "wsl".to_string();
    config.exec_timeout = Duration::from_secs(600);
    config.probe_timeout = Duration::from_secs(2);
    config
}

struct Shim {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Shim {
    fn spawn(home: &Path) -> Shim {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentify-mcp"))
            .env("AGENTIFY_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentify-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Shim {
            child,
            stdin,
            stdout,
            next: 1,
        }
    }

    fn send(&mut self, request: Value) {
        writeln!(self.stdin, "{request}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "the shim closed stdout");
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("not JSON ({err}): {line}"))
    }

    fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "agentify-slice", "version": "0.0.0" }
            }
        }));
        let initialized = self.read();
        assert_eq!(initialized["result"]["serverInfo"]["name"], "agentify");
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Call one tool and return its `structuredContent`, or the JSON-RPC error.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, Value> {
        self.next += 1;
        let id = self.next;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        let answer = self.read();
        assert_eq!(answer["id"], id);
        if let Some(error) = answer.get("error") {
            if !error.is_null() {
                return Err(error.clone());
            }
        }
        assert_eq!(answer["result"]["isError"], false, "{name}: {answer}");
        Ok(answer["result"]["structuredContent"].clone())
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        self.call(name, arguments)
            .unwrap_or_else(|err| panic!("{name} failed over MCP: {err}"))
    }

    /// `run_command`, as `(exit code, stdout, stderr)`. A command that exits
    /// non-zero is an answer, not an error: the tool reports what happened.
    fn run(&mut self, project: &str, resource: &str, command: &[&str]) -> (i64, String, String) {
        let answer = self.ok(
            "run_command",
            json!({
                "project": project,
                "resource": resource,
                "command": command,
                "timeoutMs": 300000
            }),
        );
        (
            answer["exitCode"].as_i64().unwrap_or(-1),
            answer["stdout"].as_str().unwrap_or_default().to_string(),
            answer["stderr"].as_str().unwrap_or_default().to_string(),
        )
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn names() -> Vec<String> {
    use agentify_backends::wsl::cli::Wsl;
    Wsl::new("agentify")
        .list()
        .expect("list the machine's distros")
        .into_iter()
        .map(|distro| distro.name)
        .collect()
}

/// Boot a daemon, retrying while a previous one is still letting go of 80/443.
fn boot() -> agentify_daemon::RunningDaemon {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match agentify_daemon::start_in_background(config()) {
            Ok(daemon) => return daemon,
            Err(err) if Instant::now() < deadline => {
                eprintln!("waiting for the previous ingress to let go of 80/443: {err:#}");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(err) => panic!("the daemon did not come back after a restart: {err:#}"),
        }
    }
}

/// A page from the daemon's own ingress, over plain HTTP on loopback.
///
/// Trust is deliberately absent in the slice (no test touches the machine's
/// certificate store), so Caddy serves the app directly on port 80 instead of
/// redirecting. Asking for the route by its `Host` header is the same route
/// HTTPS fronts; the pinned-certificate read of it is in the API-seam slice.
fn page(host: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(mut socket) = TcpStream::connect(("127.0.0.1", 80u16)) {
            let _ = socket.set_read_timeout(Some(Duration::from_secs(30)));
            let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
            if socket.write_all(request.as_bytes()).is_ok() {
                let mut raw = Vec::new();
                let _ = socket.read_to_end(&mut raw);
                let response = String::from_utf8_lossy(&raw).to_string();
                if response.starts_with("HTTP/1.") {
                    return response;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no answer from {host} on 127.0.0.1:80"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn body(response: &str) -> &str {
    response.split("\r\n\r\n").nth(1).unwrap_or("")
}

/// A page that says what it should, waited for.
///
/// After a restart the route can answer before the app does — the daemon adopts
/// the Environment in the background — so the first answers are Caddy's 502.
/// Waiting for the content is what makes "the URL still serves" a statement
/// about the app.
fn page_saying(host: &str, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut last;
    loop {
        let response = page(host);
        if response.contains(" 200 ") && body(&response).contains(expected) {
            return response;
        }
        last = response;
        assert!(
            Instant::now() < deadline,
            "{host} never served {expected:?} again; the last answer was: {last}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Poll `get_status` until the daemon says the Environment is running.
fn running(shim: &mut Shim, project: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let status = shim.ok("get_status", json!({ "project": project }));
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

/// The audit chain, read over the same API the UI's activity feed uses.
///
/// The MCP surface has no audit tool on purpose: the feed is an observation, not
/// an Agent action. Reading it over `GET /v1/activity` is the same call the
/// desktop shell makes.
fn activity(daemon: &agentify_daemon::RunningDaemon) -> Value {
    let url = format!("{}/v1/activity?limit=500", daemon.base_url());
    let text = ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .call()
        .unwrap_or_else(|err| panic!("reading the activity feed: {err}"))
        .into_string()
        .unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("activity is not JSON ({err}): {text}"))
}

/// One entry of the feed, by what it recorded: `(operation, target, result)`.
fn audited_as<'a>(page: &'a Value, operation: &str, target: &str, result: &str) -> &'a Value {
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

/// One entry that succeeded.
fn audited<'a>(page: &'a Value, operation: &str, target: &str) -> &'a Value {
    audited_as(page, operation, target, "succeeded")
}

/// The host port leased for a Project's `web` Resource.
fn web_lease(status: &Value) -> u16 {
    status["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("no resources in {status}"))
        .iter()
        .find(|resource| resource["name"] == "web")
        .and_then(|resource| resource["ports"].as_array())
        .and_then(|ports| ports.iter().find(|port| port["guestPort"] == 3000))
        .and_then(|port| port["hostPort"].as_u64())
        .unwrap_or_else(|| panic!("no host port leased for web: {status}")) as u16
}

/// `C:\a\b` → `/mnt/c/a/b`: the path a distro with automount would use for a
/// Windows directory.
fn windows_path_to_wsl(path: &str) -> String {
    let drive = path
        .chars()
        .next()
        .map(|letter| letter.to_ascii_lowercase())
        .unwrap_or('c');
    let rest = path.get(2..).unwrap_or("").replace('\\', "/");
    format!("/mnt/{drive}{rest}")
}

/// Ask to delete a Project over MCP, approve it as the human does, and wait for
/// the operation. The approval call is the one thing the suite makes directly:
/// the gesture is a human's, not an Agent's.
fn delete_project(shim: &mut Shim, daemon: &agentify_daemon::RunningDaemon, project: &str) {
    let asked = shim.ok("delete_project", json!({ "project": project }));
    let approval = asked["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("a delete must answer 202 with an approvalId: {asked}"))
        .to_string();
    let operation = asked["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("a delete must answer 202 with an operationId: {asked}"))
        .to_string();
    let still_there = shim.ok("get_project", json!({ "project": project }));
    assert_eq!(
        still_there["name"], project,
        "asking to delete changes nothing yet: {still_there}"
    );

    let approve_url = format!("{}/v1/approvals/{approval}/approve", daemon.base_url());
    ureq::post(&approve_url)
        .set("Authorization", &format!("Bearer {}", daemon.token()))
        .set("Content-Type", "application/json")
        .send_string("{}")
        .expect("approve the delete");

    let waited = shim.ok(
        "wait_for_operation",
        json!({ "operationId": operation, "timeoutMs": 600000 }),
    );
    assert_eq!(waited["state"], "succeeded", "{waited}");
    let missing = shim
        .call("get_project", json!({ "project": project }))
        .expect_err("the Project is gone");
    assert_eq!(missing["data"]["code"], "NotFound", "{missing}");
}

#[test]
fn an_mcp_client_creates_starts_migrates_and_reads_the_row() {
    if !enabled() {
        eprintln!("skipping: set AGENTIFY_TEST_SLICE=1 to run the MCP slice");
        return;
    }
    let machine_before = names();
    let _lock = SliceLock::acquire();
    let home = home();
    assert!(
        caddy().is_file(),
        "the slice needs Caddy at {}",
        caddy().display()
    );

    // The guard that unregisters whatever this test minted, on every path.
    struct Distros;
    impl Drop for Distros {
        fn drop(&mut self) {
            let _ = agentify_backends::wsl::template::unregister_all(
                &agentify_backends::wsl::cli::Wsl::new(PREFIX),
            );
        }
    }
    let distros = Distros;

    let mut daemon = boot();
    let mut shim = Shim::spawn(&home);
    shim.initialize();

    // ---- 1. create_project, from the bundled fixture ----------------------
    let created = shim.ok("create_project", json!({ "fixture": PROJECT }));
    assert_eq!(created["name"], PROJECT);
    assert_eq!(created["environment"]["name"], "dev");
    assert_eq!(created["environment"]["state"], "stopped");
    assert_eq!(
        created["resources"].as_array().expect("resources").len(),
        2,
        "{created}"
    );
    assert_eq!(
        created["routes"][0]["url"],
        "https://web.hello.localhost",
        "the Project's URL is its own hostname: {created}"
    );

    // ---- 2. start_environment returns only once the workloads are up ------
    let started = shim.ok(
        "start_environment",
        json!({ "project": PROJECT, "environment": "dev" }),
    );
    assert_eq!(started["environment"]["state"], "running", "{started}");
    assert_eq!(started["health"], "healthy", "{started}");
    assert_eq!(started["backend"]["name"], "wsl", "{started}");
    for resource in started["resources"].as_array().expect("resources") {
        assert_eq!(resource["state"], "running", "{resource}");
        match resource["name"].as_str() {
            Some("web") => assert_eq!(resource["health"], "healthy", "{resource}"),
            Some("db") => assert_eq!(
                resource["health"], "unknown",
                "the database is internal, with no host port to probe: {resource}"
            ),
            other => panic!("unexpected Resource {other:?}: {resource}"),
        }
    }

    // ---- 3. run_command runs the migration inside the app's container -----
    let migrated = shim.ok(
        "run_command",
        json!({
            "project": PROJECT,
            "resource": "web",
            "command": ["node", "migrate.js"],
            "timeoutMs": 300000
        }),
    );
    assert_eq!(migrated["exitCode"], 0, "the migration failed: {migrated}");
    assert!(
        migrated["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("row(s)"),
        "{migrated}"
    );

    // ---- 4. The row is in Postgres, read back through the same tool -------
    let read = shim.ok(
        "run_command",
        json!({
            "project": PROJECT,
            "resource": "db",
            "command": [
                "psql", "-U", "hello", "-d", "hello",
                "-tAc", "select message from greetings order by id"
            ]
        }),
    );
    assert_eq!(read["exitCode"], 0, "reading the row failed: {read}");
    assert_eq!(
        read["stdout"].as_str().unwrap_or_default().trim(),
        ROW,
        "the migration's row is in the database: {read}"
    );

    // Status and logs agree with what the tools just did.
    let status = shim.ok("get_status", json!({ "project": PROJECT }));
    assert_eq!(status["environment"]["state"], "running", "{status}");
    let logs = shim.ok("read_logs", json!({ "project": PROJECT, "tail": 50 }));
    assert!(
        !logs["lines"].as_array().expect("lines").is_empty(),
        "a running Environment has a log: {logs}"
    );

    // ---- 5. A daemon restart keeps the row and the URL (check 5) ----------
    //
    // The shim is a client of the discovery file, so it survives the restart:
    // the new daemon rotates its token, and the next call picks the new one up.
    let before_whoami = shim.ok("whoami", json!({}));
    let agent_before = before_whoami["actor"]["id"]
        .as_str()
        .expect("the Agent token before the restart")
        .to_string();
    daemon.shutdown().expect("stop the daemon");
    let mut daemon = boot();
    // No `start_environment` and no second migration: the daemon republishes the
    // route from the database and adopts the Environment, so the page is still
    // the one the migration wrote.
    let served = page_saying(HOST, ROW);
    assert!(
        served.starts_with("HTTP/1.") && served.contains(" 200 "),
        "the URL must serve again as soon as the daemon is back: {served}"
    );
    assert!(
        body(&served).contains(ROW),
        "the row survived the restart with no migration: {served}"
    );
    let adopted = running(&mut shim, PROJECT);
    eprintln!("adopted after the restart: {adopted}");
    let (code, out, err) = shim.run(
        PROJECT,
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
    assert_eq!(code, 0, "the row after the restart: {out}{err}");
    assert_eq!(out.trim(), ROW, "the row survived the daemon restart");
    let restarted = shim.ok(
        "start_environment",
        json!({ "project": PROJECT, "environment": "dev" }),
    );
    assert_eq!(
        restarted["environment"]["state"], "running",
        "a start after a restart converges on the same runtime: {restarted}"
    );
    let served = page(HOST);
    assert!(
        body(&served).contains(ROW),
        "the URL still serves the row after a restart: {served}"
    );

    // ---- 6. A second Project alongside, and the isolation (check 8) -------
    let other_created = shim.ok("create_project", json!({ "fixture": OTHER }));
    assert_eq!(
        other_created["routes"][0]["url"],
        format!("https://{OTHER_HOST}"),
        "the second Project has its own hostname: {other_created}"
    );
    let other_started = shim.ok(
        "start_environment",
        json!({ "project": OTHER, "environment": "dev" }),
    );
    assert_eq!(
        other_started["environment"]["state"], "running",
        "{other_started}"
    );

    // Both routes and both leases are right, and distinct.
    let mine = shim.ok("get_project", json!({ "project": PROJECT }));
    let theirs = shim.ok("get_project", json!({ "project": OTHER }));
    assert_eq!(mine["routes"][0]["url"], format!("https://{HOST}"));
    assert_eq!(theirs["routes"][0]["url"], format!("https://{OTHER_HOST}"));
    let mine_lease = web_lease(&shim.ok("get_status", json!({ "project": PROJECT })));
    let their_lease = web_lease(&shim.ok("get_status", json!({ "project": OTHER })));
    assert_ne!(
        mine_lease, their_lease,
        "two Projects must not share one host port"
    );

    // A cannot read B's volume: the same absolute path inside the database
    // container is each Project's own named volume.
    let marker = "/var/lib/postgresql/data/.agentify-isolation";
    for (project, what) in [(PROJECT, PROJECT), (OTHER, OTHER)] {
        let (code, out, err) = shim.run(
            project,
            "db",
            &["sh", "-c", &format!("echo {what} > {marker} && cat {marker}")],
        );
        assert_eq!(code, 0, "{project} writes into its own volume: {out}{err}");
        assert_eq!(out.trim(), what, "{project}: {out}");
    }
    let (code, out, err) = shim.run(PROJECT, "db", &["cat", marker]);
    assert_eq!(code, 0, "A reads its own volume: {out}{err}");
    assert_eq!(
        out.trim(),
        PROJECT,
        "A read {out:?} from the path B wrote {OTHER:?} into: the volumes are not separate"
    );
    let other_volume = format!("/var/lib/docker/volumes/{OTHER}-db-data/_data/PG_VERSION");
    let (code, out, err) = shim.run(PROJECT, "db", &["cat", &other_volume]);
    assert_ne!(code, 0, "A read B's volume at {other_volume}: {out}{err}");

    // A's container has no host mount, so neither Project's workspace — which
    // lives on the daemon's own filesystem — is reachable from it.
    let (code, mounts, err) = shim.run(PROJECT, "web", &["cat", "/proc/mounts"]);
    assert_eq!(code, 0, "reading A's mount table: {err}");
    for forbidden in ["/mnt/c", "/mnt/wsl", " 9p ", "drvfs"] {
        assert!(
            !mounts.contains(forbidden),
            "A's container has {forbidden} mounted: {mounts}"
        );
    }
    let guest = windows_path_to_wsl(
        theirs["workspace"]
            .as_str()
            .unwrap_or_else(|| panic!("no workspace in {theirs}")),
    );
    let (code, out, err) = shim.run(
        PROJECT,
        "web",
        &["cat", &format!("{guest}/agentify.yaml")],
    );
    assert_ne!(code, 0, "A read B's workspace at {guest}: {out}{err}");

    // A's credentials are not B's: each cluster stores a verifier for the
    // password generated for it, and the two verifiers are different.
    let verifier = r#"select md5(coalesce(rolpassword, '')) from pg_authid where rolname = 'hello'"#;
    let (code, mine_verifier, err) = shim.run(PROJECT, "db", &["psql", "-U", "hello", "-d", "hello", "-tAc", verifier]);
    assert_eq!(code, 0, "A's verifier: {err}");
    let (code, their_verifier, err) = shim.run(OTHER, "db", &["psql", "-U", "hello", "-d", "hello", "-tAc", verifier]);
    assert_eq!(code, 0, "B's verifier: {err}");
    assert_ne!(
        mine_verifier.trim(),
        their_verifier.trim(),
        "two Projects must not share one database password"
    );
    // And A's credentials do not open B's cluster. Neither value leaves a
    // container: the password is read inside A, the address inside B.
    let (code, address, err) = shim.run(OTHER, "db", &["hostname", "-i"]);
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
    let (code, out, err) = shim.run(PROJECT, "web", &["node", "-e", attempt, &address]);
    eprintln!(
        "A's credentials against B's cluster at {address}: exit {code}, {:?}{:?}",
        out.trim(),
        err.trim()
    );
    assert_ne!(
        code, 0,
        "A authenticated to B's database at {address}: {out}{err}"
    );

    // B's hostname, from inside A. Windows resolves every `.localhost` name to
    // loopback in every distro, because all WSL distros share one network
    // namespace, so what must hold is that the name leads nowhere but loopback
    // and that loopback inside A is A.
    let (code, out, _) = shim.run(
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
    let (code, out, _) = shim.run(PROJECT, "web", &["node", "-e", &fetch]);
    assert_ne!(code, 0, "A reached B's route through B's hostname: {out}");

    // B's database does not contain A's table.
    let (code, out, err) = shim.run(
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
    assert_ne!(code, 0, "B has A's table: {out}{err}");
    assert!(
        format!("{out}{err}").contains("greetings"),
        "B's own database says the table is missing: {out}{err}"
    );

    // ---- 7. Every mutation is in the audit (check 10) ---------------------
    let feed = activity(&daemon);
    assert_eq!(
        feed["integrity"]["verified"], true,
        "the chain verifies: {:#}",
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
    let whoami = shim.ok("whoami", json!({}));
    let agent = whoami["actor"]["id"]
        .as_str()
        .expect("the Agent token id")
        .to_string();
    // The restart rotates the token, so entries from the first run carry the
    // first token and entries after it carry the second.
    let mut seen_before = false;
    let mut seen_after = false;
    for (operation, target) in [
        ("create_project", PROJECT.to_string()),
        ("create_project", OTHER.to_string()),
        ("start_environment", format!("{PROJECT}/dev")),
        ("start_environment", format!("{OTHER}/dev")),
        ("exec", format!("{PROJECT}/dev/web")),
        ("exec", format!("{PROJECT}/dev/db")),
        ("exec", format!("{OTHER}/dev/db")),
    ] {
        let entry = audited(&feed, operation, &target);
        assert_eq!(entry["actor"]["kind"], "agent", "{entry}");
        let actor_id = entry["actor"]["id"].as_str().unwrap_or_default();
        assert!(
            actor_id == agent_before || actor_id == agent,
            "the token that made the call is the actor: {entry}"
        );
        if actor_id == agent_before {
            seen_before = true;
        } else {
            seen_after = true;
        }
        assert!(entry["projectId"].is_string(), "{entry}");
        assert!(
            entry["hash"].as_str().map(str::len) == Some(64),
            "every entry is hashed: {entry}"
        );
    }
    assert!(
        seen_before && seen_after,
        "the feed covers both daemon runs (before={seen_before}, after={seen_after})"
    );

    // ---- 8. delete_project still asks a human, and then removes everything -
    delete_project(&mut shim, &daemon, PROJECT);
    delete_project(&mut shim, &daemon, OTHER);

    // A delete is three mutations — asked, decided, done — and a deleted
    // Project keeps its history.
    let feed = activity(&daemon);
    assert_eq!(
        feed["integrity"]["verified"], true,
        "the chain still verifies after both Projects are gone: {:#}",
        feed["integrity"]
    );
    for name in [PROJECT, OTHER] {
        for result in ["requested", "approved", "succeeded"] {
            let entry = audited_as(&feed, "delete_project", name, result);
            assert_eq!(entry["target"], name, "{entry}");
        }
    }

    drop(shim);
    let _ = daemon.shutdown();

    // The distros this run minted are gone; the machine looks as it did.
    drop(distros);
    let after = names();
    let leaked: Vec<&String> = after
        .iter()
        .filter(|name| name.to_ascii_lowercase().starts_with(PREFIX))
        .collect();
    assert!(leaked.is_empty(), "leftover distros: {leaked:?}");
    let mut before_sorted = machine_before.clone();
    let mut after_sorted = after.clone();
    before_sorted.sort();
    after_sorted.sort();
    assert_eq!(
        after_sorted, before_sorted,
        "`wsl -l -v` changed: before {machine_before:?}, after {after:?}"
    );
}

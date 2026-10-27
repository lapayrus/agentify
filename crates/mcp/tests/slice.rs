//! The vertical slice through the MCP shim: create → start → migrate → read.
//!
//! The shim is a client of the daemon API like any other, so this drives the real
//! `agentify-mcp` process over line-delimited JSON-RPC against a real daemon with
//! the real WSL Backend: an Agent that only ever speaks MCP can own the whole
//! environment.
//!
//! Gated by `AGENTIFY_TEST_SLICE=1`, for the same reason the API-seam slice is: it
//! builds an image, starts a distro, and binds 80 and 443. It takes the same
//! machine-wide lock, so the two gated suites can share one data root and one
//! pair of ports without stepping on each other.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The Project under test: the fixture under the shorter name, so the URL is the
/// one the acceptance check names.
const PROJECT: &str = "hello";
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

    let mut daemon = agentify_daemon::start_in_background(config()).expect("boot the daemon");
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

    // ---- 5. delete_project still asks a human, and then removes everything -
    let asked = shim.ok("delete_project", json!({ "project": PROJECT }));
    let approval = asked["approvalId"]
        .as_str()
        .unwrap_or_else(|| panic!("a delete must answer 202 with an approvalId: {asked}"))
        .to_string();
    let operation = asked["operationId"]
        .as_str()
        .unwrap_or_else(|| panic!("a delete must answer 202 with an operationId: {asked}"))
        .to_string();
    let still_there = shim.ok("get_project", json!({ "project": PROJECT }));
    assert_eq!(
        still_there["name"], PROJECT,
        "asking to delete changes nothing yet: {still_there}"
    );

    let approve_url = format!(
        "{}/v1/approvals/{approval}/approve",
        daemon.base_url()
    );
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
        .call("get_project", json!({ "project": PROJECT }))
        .expect_err("the Project is gone");
    assert_eq!(missing["data"]["code"], "NotFound", "{missing}");

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

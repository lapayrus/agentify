//! Acceptance tests at the public API seam.
//!
//! Every test here boots the real `agentifyd` process against a temporary data
//! root and talks to it the way a client does: read the discovery file, then
//! speak HTTP. No test reaches into daemon internals.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentify_proto::{Discovery, Whoami};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// A daemon process booted by a test, killed on drop.
struct Daemon {
    child: Child,
    home: PathBuf,
    discovery: Discovery,
}

impl Daemon {
    fn boot(home: &Path) -> Daemon {
        std::fs::create_dir_all(home).expect("create data root");
        let child = Command::new(env!("CARGO_BIN_EXE_agentifyd"))
            .env("AGENTIFY_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentifyd");

        let mut daemon = Daemon {
            child,
            home: home.to_path_buf(),
            discovery: Discovery {
                pid: 0,
                port: 0,
                url: String::new(),
                token: String::new(),
                daemon_version: String::new(),
                api_version: String::new(),
                created_at: String::new(),
            },
        };
        daemon.discovery = await_discovery(home, &mut daemon.child);
        daemon
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.discovery.url)
    }

    fn whoami(&self) -> Whoami {
        let (status, body) = http_get(&self.url("/v1/whoami"), Some(&self.discovery.token));
        assert_eq!(status, 200, "whoami failed: {body}");
        serde_json::from_str(&body).expect("whoami payload is a Whoami")
    }

    fn get_raw(&self, path: &str, token: Option<&str>) -> (u16, String) {
        http_get(&self.url(path), token)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = &self.home;
    }
}

fn await_discovery(home: &Path, child: &mut Child) -> Discovery {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll daemon") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("daemon exited before publishing discovery ({status}): {stderr}");
        }
        if let Ok(discovery) = agentify_proto::read_discovery(home) {
            if !discovery.token.is_empty() && discovery.port != 0 {
                // A killed predecessor can leave a stale file behind; only a
                // reachable daemon counts as ready.
                if let Ok((200, _)) = try_http_get(&format!("{}/v1/healthz", discovery.url), None) {
                    return discovery;
                }
            }
        }
        if Instant::now() > deadline {
            panic!("daemon did not publish a working discovery file within {BOOT_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn try_http_get(url: &str, token: Option<&str>) -> Result<(u16, String), ureq::Error> {
    let request = ureq::get(url);
    let request = match token {
        Some(token) => request.set("Authorization", &format!("Bearer {token}")),
        None => request,
    };
    match request.call() {
        Ok(response) => Ok((
            response.status(),
            response.into_string().unwrap_or_default(),
        )),
        Err(ureq::Error::Status(status, response)) => {
            Ok((status, response.into_string().unwrap_or_default()))
        }
        Err(transport) => Err(transport),
    }
}

fn http_get(url: &str, token: Option<&str>) -> (u16, String) {
    try_http_get(url, token).unwrap_or_else(|err| panic!("transport error for {url}: {err}"))
}

#[test]
fn whoami_reports_global_context_scopes_versions_and_capabilities() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let payload = daemon.whoami();

    assert_eq!(payload.project, None, "no Environment exists yet: global context");
    assert_eq!(payload.scopes, vec!["read".to_string(), "operate".to_string()]);
    assert_eq!(payload.daemon_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(payload.api_version, "v1");
    assert!(payload.capabilities.events, "the daemon serves an events stream");
    assert!(
        !payload.scopes.iter().any(|scope| scope == "admin"),
        "the discovery token is Agent-scoped and never admin"
    );
    assert_eq!(payload.actor.kind, "agent");
    assert!(!payload.actor.id.is_empty(), "the token has a stable id");
    assert!(
        !payload.daemon.instance_id.is_empty(),
        "every data root mints an instance id"
    );
    assert!(
        payload.daemon.started_at.contains('T'),
        "started_at is RFC 3339, got {}",
        payload.daemon.started_at
    );
}

#[test]
fn whoami_requires_the_agent_token_from_the_discovery_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let (no_token_status, no_token_body) = daemon.get_raw("/v1/whoami", None);
    assert_eq!(no_token_status, 401, "body: {no_token_body}");
    let problem: serde_json::Value = serde_json::from_str(&no_token_body).expect("problem body");
    assert_eq!(problem["code"], "Unauthorized");
    assert_eq!(problem["status"], 401);

    let (bad_token_status, _) = daemon.get_raw("/v1/whoami", Some("not-the-token"));
    assert_eq!(bad_token_status, 401);
}

#[test]
fn the_discovery_file_grants_only_the_current_os_user() {
    let temp = tempfile::tempdir().expect("temp dir");
    let _daemon = Daemon::boot(temp.path());
    let path = agentify_proto::discovery_path(temp.path());

    let grantees = agentify_daemon::acl::grantees(&path).expect("read discovery ACL");
    assert_eq!(
        grantees,
        vec![agentify_daemon::acl::current_user().expect("current user")],
        "only the current OS user may read the discovery token"
    );
}

#[test]
fn state_survives_a_restart_and_the_token_rotates() {
    let temp = tempfile::tempdir().expect("temp dir");

    let first = {
        let daemon = Daemon::boot(temp.path());
        (daemon.whoami(), daemon.discovery.clone())
    };
    assert!(agentify_proto::state_db_path(temp.path()).exists());

    let second = {
        let daemon = Daemon::boot(temp.path());
        (daemon.whoami(), daemon.discovery.clone())
    };

    assert_eq!(
        first.0.daemon.instance_id, second.0.daemon.instance_id,
        "the instance id is minted once and survives a restart"
    );
    assert_eq!(first.0.project, second.0.project);
    assert_ne!(
        first.1.token, second.1.token,
        "each daemon run issues a fresh Agent token"
    );

    std::fs::remove_dir_all(temp.path()).expect("clean up data root");
}

#[test]
fn the_events_stream_opens_with_a_heartbeat() {
    let temp = tempfile::tempdir().expect("temp dir");
    let daemon = Daemon::boot(temp.path());

    let response = ureq::get(&daemon.url("/v1/events"))
        .set("Authorization", &format!("Bearer {}", daemon.discovery.token))
        .call()
        .expect("events stream opens");
    assert_eq!(response.status(), 200);
    assert!(
        response
            .header("content-type")
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "SSE content type, got {:?}",
        response.header("content-type")
    );

    let mut reader = std::io::BufReader::new(response.into_reader());
    let mut first_chunk = String::new();
    {
        use std::io::BufRead;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !first_chunk.contains("data:") {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read event stream");
            assert!(read > 0, "stream closed before a heartbeat: {first_chunk:?}");
            first_chunk.push_str(&line);
            assert!(Instant::now() < deadline, "no heartbeat within 10s");
        }
    }
    assert!(
        first_chunk.contains("event: heartbeat"),
        "heartbeats are named events: {first_chunk:?}"
    );
}

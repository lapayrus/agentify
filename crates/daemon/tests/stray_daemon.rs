//! Stray-daemon reconciliation, at the daemon API seam.
//!
//! A discovery file is not proof of a usable daemon: it may name a dead
//! process, a daemon of another build, or one that is mid-Operation. These tests
//! drive `agentify_proto::lifecycle` exactly as the shell and the CLI do, against
//! a real daemon and against a scripted one that answers with another build's
//! version.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use agentify_daemon::{Config, RunningDaemon};
use agentify_proto::lifecycle::{self, Ensure, EnsureError, Stop, SystemProcesses};
use serde_json::json;

/// A daemon booted in-process, shut down on drop.
struct Daemon {
    inner: Option<RunningDaemon>,
}

impl Daemon {
    fn boot(home: &Path) -> Daemon {
        Daemon {
            inner: Some(
                agentify_daemon::start_in_background(
                    Config::new(home).with_secret_env([("HELLO_API_TOKEN", "fixture-api-token")]),
                )
                .expect("boot daemon"),
            ),
        }
    }

    fn stop(mut self) {
        if let Some(mut daemon) = self.inner.take() {
            daemon.shutdown().expect("the daemon stops cleanly");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.inner.take() {
            let _ = daemon.shutdown();
        }
    }
}

/// A stale discovery file: a pid no process can have, and a port nothing serves.
fn write_stale_discovery(home: &Path) -> agentify_proto::Discovery {
    let discovery = agentify_proto::Discovery {
        pid: 4_000_000_000,
        port: 1,
        url: "http://127.0.0.1:1".to_string(),
        token: "stale".to_string(),
        daemon_version: "0.1.1".to_string(),
        api_version: "v1".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
    };
    std::fs::create_dir_all(home).expect("data root");
    std::fs::write(
        agentify_proto::discovery_path(home),
        serde_json::to_string(&discovery).expect("serialize"),
    )
    .expect("write discovery");
    discovery
}

/// A daemon of another build: it answers the three routes the reconciliation
/// uses, with a version this build does not ship.
///
/// A real daemon cannot be made to report another build's version, and the
/// version is exactly what the takeover decision turns on, so this is the
/// scripted substitute. It lives at the test process's pid, so a stale-file
/// check still sees a live process.
struct ForeignDaemon {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ForeignDaemon {
    fn serve(home: &Path, version: &str, operations_in_flight: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the fake daemon");
        let port = listener.local_addr().expect("local address").port();
        let discovery_path: PathBuf = agentify_proto::discovery_path(home);
        let discovery = agentify_proto::Discovery {
            pid: std::process::id(),
            port,
            url: format!("http://127.0.0.1:{port}"),
            token: "foreign-token".to_string(),
            daemon_version: version.to_string(),
            api_version: "v1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        std::fs::create_dir_all(home).expect("data root");
        std::fs::write(
            &discovery_path,
            serde_json::to_string(&discovery).expect("serialize"),
        )
        .expect("write discovery");

        let version = version.to_string();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let request = read_request(&mut stream);
                if request.starts_with("QUIT") {
                    break;
                }
                let (status, body, stop) = if request.starts_with("GET /v1/healthz") {
                    (
                        200,
                        json!({
                            "status": "ok",
                            "daemon_version": version,
                            "api_version": "v1"
                        })
                        .to_string(),
                        false,
                    )
                } else if request.starts_with("GET /v1/system/update_readiness") {
                    (
                        200,
                        json!({
                            "operations_in_flight": operations_in_flight,
                            "adopted": true,
                            "degraded_projects": []
                        })
                        .to_string(),
                        false,
                    )
                } else if request.starts_with("POST /v1/system/shutdown") {
                    (202, json!({ "status": "shutting_down" }).to_string(), true)
                } else {
                    (404, json!({ "code": "NotFound" }).to_string(), false)
                };
                respond(&mut stream, status, &body);
                if stop {
                    let _ = std::fs::remove_file(&discovery_path);
                    break;
                }
            }
        });

        ForeignDaemon {
            port,
            handle: Some(handle),
        }
    }
}

impl Drop for ForeignDaemon {
    fn drop(&mut self) {
        // Wake the blocking `incoming()` loop so the thread can end.
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) {
            let _ = stream.write_all(b"QUIT / HTTP/1.1\r\n\r\n");
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = [0u8; 4096];
    let read = stream.read(&mut buffer).unwrap_or(0);
    String::from_utf8_lossy(&buffer[..read]).to_string()
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[test]
fn a_matching_daemon_is_reused_and_nothing_is_started() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let daemon = Daemon::boot(home);
    let started = RefCell::new(false);

    let outcome = lifecycle::ensure(
        home,
        agentify_daemon::DAEMON_VERSION,
        &SystemProcesses,
        || {
            *started.borrow_mut() = true;
            Ok(())
        },
    )
    .expect("ensure");

    assert!(matches!(outcome, Ensure::Running(_)), "{outcome:?}");
    assert!(!*started.borrow(), "a healthy daemon is not replaced");
    daemon.stop();
}

#[test]
fn a_stale_discovery_file_is_discarded_and_the_daemon_starts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let stale = write_stale_discovery(home);

    let spawned: RefCell<Option<Daemon>> = RefCell::new(None);
    let outcome = lifecycle::ensure(
        home,
        agentify_daemon::DAEMON_VERSION,
        &SystemProcesses,
        || {
            *spawned.borrow_mut() = Some(Daemon::boot(home));
            Ok(())
        },
    )
    .expect("ensure");

    match outcome {
        Ensure::Started(discovery) => {
            assert_eq!(discovery.daemon_version, agentify_daemon::DAEMON_VERSION);
            assert_ne!(discovery.pid, stale.pid, "the stale pid is not trusted");
        }
        other => panic!("expected a started daemon, got {other:?}"),
    }
    assert!(spawned.borrow().is_some(), "the shipped daemon was started");
    spawned.borrow_mut().take().expect("running").stop();
}

#[test]
fn a_daemon_of_another_build_is_stopped_before_this_build_starts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let foreign = ForeignDaemon::serve(home, "9.9.9", false);
    let foreign_port = foreign.port;

    let spawned: RefCell<Option<Daemon>> = RefCell::new(None);
    let outcome = lifecycle::ensure(
        home,
        agentify_daemon::DAEMON_VERSION,
        &SystemProcesses,
        || {
            *spawned.borrow_mut() = Some(Daemon::boot(home));
            Ok(())
        },
    )
    .expect("ensure");

    match outcome {
        Ensure::Started(discovery) => {
            assert_eq!(discovery.daemon_version, agentify_daemon::DAEMON_VERSION);
        }
        other => panic!("expected a started daemon, got {other:?}"),
    }
    drop(foreign);
    assert!(
        TcpStream::connect(("127.0.0.1", foreign_port)).is_err(),
        "the old build must not be left serving"
    );
    assert!(
        agentify_proto::discovery_path(home).exists(),
        "the new daemon published its own discovery file"
    );
    spawned.borrow_mut().take().expect("running").stop();
}

#[test]
fn an_operation_in_flight_blocks_the_takeover() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let _foreign = ForeignDaemon::serve(home, "9.9.9", true);
    let started = RefCell::new(false);

    let err = lifecycle::ensure(
        home,
        agentify_daemon::DAEMON_VERSION,
        &SystemProcesses,
        || {
            *started.borrow_mut() = true;
            Ok(())
        },
    )
    .expect_err("an in-flight Operation must refuse the takeover");

    match err {
        EnsureError::Busy { found, .. } => assert_eq!(found, "9.9.9"),
        other => panic!("expected Busy, got {other:?}"),
    }
    assert!(!*started.borrow(), "nothing is started over live work");
    assert!(
        agentify_proto::discovery_path(home).exists(),
        "the daemon is left exactly as it was"
    );
}

#[test]
fn stop_asks_a_matching_daemon_to_shut_down_and_removes_the_discovery_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let daemon = Daemon::boot(home);

    let outcome = lifecycle::stop(home, &SystemProcesses).expect("stop");
    assert!(matches!(outcome, Stop::Stopped { .. }), "{outcome:?}");
    assert!(
        !agentify_proto::discovery_path(home).exists(),
        "a stopped daemon must not leave a trusted discovery file"
    );
    daemon.stop();
}

#[test]
fn a_stale_discovery_file_is_not_an_error_to_stop() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    write_stale_discovery(home);

    let outcome = lifecycle::stop(home, &SystemProcesses).expect("stop");
    assert_eq!(outcome, Stop::NotRunning);
    assert!(!agentify_proto::discovery_path(home).exists());
}

#[test]
fn an_operation_in_flight_is_the_one_refusal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let _foreign = ForeignDaemon::serve(home, "9.9.9", true);

    let outcome = lifecycle::stop(home, &SystemProcesses).expect("stop");
    match outcome {
        Stop::Busy { found, .. } => assert_eq!(found, "9.9.9"),
        other => panic!("expected Busy, got {other:?}"),
    }
    assert!(
        agentify_proto::discovery_path(home).exists(),
        "a refused stop leaves the file alone"
    );
}

#[test]
fn a_foreign_daemon_with_no_operation_is_stopped() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let _foreign = ForeignDaemon::serve(home, "9.9.9", false);

    let outcome = lifecycle::stop(home, &SystemProcesses).expect("stop");
    assert!(matches!(outcome, Stop::Stopped { .. }), "{outcome:?}");
    assert!(!agentify_proto::discovery_path(home).exists());
}

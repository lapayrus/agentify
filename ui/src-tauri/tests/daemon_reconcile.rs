//! The shell's startup reconciliation, at the shell seam.
//!
//! The shell must start the daemon this build ships, never the one a leftover
//! discovery file names. These tests call the same function `run` calls, with an
//! injected spawn step so a test can boot a real daemon.

use std::cell::RefCell;
use std::path::Path;

use agentify_daemon::Config;
use agentify_proto::lifecycle::Ensure;

fn stale_discovery(home: &Path) {
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
}

fn boot(home: &Path) -> agentify_daemon::RunningDaemon {
    agentify_daemon::start_in_background(
        Config::new(home).with_secret_env([("HELLO_API_TOKEN", "fixture-api-token")]),
    )
    .expect("boot daemon")
}

#[test]
fn a_stale_discovery_file_does_not_stop_the_shell_starting_its_daemon() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    stale_discovery(home);

    let spawned: RefCell<Option<agentify_daemon::RunningDaemon>> = RefCell::new(None);
    let outcome = agentify_shell::ensure_daemon_at(home, agentify_daemon::DAEMON_VERSION, || {
        *spawned.borrow_mut() = Some(boot(home));
        Ok(())
    })
    .expect("ensure");

    match outcome {
        Ensure::Started(discovery) => {
            assert_eq!(discovery.daemon_version, agentify_daemon::DAEMON_VERSION);
        }
        other => panic!("expected a started daemon, got {other:?}"),
    }
    assert!(spawned.borrow().is_some(), "the shipped daemon was started");
    spawned
        .borrow_mut()
        .take()
        .expect("running")
        .shutdown()
        .expect("stop");
}

#[test]
fn a_live_daemon_of_this_build_is_adopted_without_a_second_process() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();
    let mut daemon = boot(home);
    let started = RefCell::new(false);

    let outcome = agentify_shell::ensure_daemon_at(home, agentify_daemon::DAEMON_VERSION, || {
        *started.borrow_mut() = true;
        Ok(())
    })
    .expect("ensure");

    assert!(matches!(outcome, Ensure::Running(_)), "{outcome:?}");
    assert!(!*started.borrow(), "no second daemon is started");
    daemon.shutdown().expect("stop");
}

//! The real thing, read-only: what detection actually answers on this machine.
//!
//! Nothing here installs, elevates, reboots, or writes a registry value. It runs
//! the same three probes a first run runs — `wsl --version`, `wsl --status`, and
//! `wsl -l -v` — parses them, and asserts the verdict. On this machine the answer
//! is `Ready` and the whole run does nothing at all, which is the property that
//! matters: a machine that is already set up must not be asked for anything.

use std::path::Path;

use agentify_bootstrap::{Bootstrap, BootstrapState, Host, SystemHost};

/// A data root of its own, so the version probe never reads this machine's real
/// discovery file.
fn data_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("a scratch data root")
}

#[cfg(windows)]
#[test]
fn this_machine_reaches_ready_without_elevating_or_changing_anything() {
    let data_root = data_root();
    let bootstrap = Bootstrap::system(
        data_root.path(),
        Path::new("C:\\Program Files\\Agentify\\Agentify.exe"),
        &["--resume"],
    );

    let report = bootstrap.status();
    if !report.detection.available {
        // A Windows machine without WSL is the *other* test case, and this one
        // has nothing to assert about a machine it cannot see.
        eprintln!(
            "skipping: no WSL on this machine ({})",
            report.detection.summary()
        );
        return;
    }

    assert_eq!(
        report.state,
        BootstrapState::Ready,
        "a machine with WSL 2 must report Ready and do nothing: {report:?}"
    );
    assert!(report.ready());
    assert!(!report.wants_consent(), "there is nothing to agree to");
    assert!(!report.reboot_required, "no restart is needed");
    assert!(!report.resume_scheduled, "no resume was scheduled");
    assert_eq!(report.elevated_step, None, "nothing was left to run");
    assert_eq!(
        report.detection.default_version,
        Some(2),
        "WSL 2 is the default here: {:?}",
        report.detection
    );
    assert!(
        report.detection.wsl2_distro().is_some(),
        "Ubuntu is registered on WSL 2: {:?}",
        report.detection
    );
    assert!(
        report.detection.wsl_version.is_some(),
        "the WSL version line was parsed: {:?}",
        report.detection
    );
    let versions = report.versions.expect("the handshake ran");
    assert!(versions.ok, "this build matches its own pins: {:?}", versions);
    assert!(report.message.contains("matches"), "{}", report.message);
}

#[cfg(windows)]
#[test]
fn the_real_probe_sees_a_distro_and_a_default_version() {
    let host = SystemHost::new(data_root().path());
    let detection = host.detect();
    if !detection.available {
        eprintln!("skipping: no WSL on this machine");
        return;
    }
    assert!(
        detection.can_import(),
        "WSL 2 is the default, so the pinned rootfs can be imported: {detection:?}"
    );
    assert!(
        detection.summary().contains("WSL"),
        "{}",
        detection.summary()
    );
    // Virtualization is only worth asking about when WSL is missing; on a ready
    // machine the probe is skipped and nothing is claimed.
    assert_eq!(detection.virtualization, None);
}

#[test]
fn the_version_probe_reads_the_running_daemons_versions() {
    // A discovery file naming another build is what a mixed install looks like,
    // and the handshake has to see it. No daemon is started and nothing is
    // elevated: this is a file read.
    let data_root = data_root();
    let other = serde_json::json!({
        "pid": 4242,
        "port": 7391,
        "url": "http://127.0.0.1:7391",
        "token": "0123456789abcdef0123456789abcdef",
        "daemon_version": "9.9.9",
        "api_version": agentify_proto::API_VERSION,
        "created_at": "2026-01-01T00:00:00Z"
    });
    std::fs::write(
        agentify_proto::discovery_path(data_root.path()),
        other.to_string(),
    )
    .expect("write a discovery file");

    let observed = SystemHost::new(data_root.path()).versions();
    assert_eq!(observed.daemon.as_deref(), Some("9.9.9"));
    let report = agentify_proto::versions::evaluate(&observed);
    assert!(!report.ok, "another build's daemon is a mismatch");
    assert_eq!(
        report.first_mismatch().map(|row| row.component.as_str()),
        Some("daemon")
    );
}

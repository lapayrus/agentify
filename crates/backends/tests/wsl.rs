//! The WSL Backend, measured against a real distro.
//!
//! These tests create, use, and destroy real WSL distros, so they run only when
//! `AGENTIFY_TEST_WSL=1` is set. The default CI path stays on the fake and pays
//! nothing for this file.
//!
//! Every test:
//!
//! * uses a prefix that is unique to the run, so its distros are obvious in
//!   `wsl -l -v` and no two tests can collide;
//! * records `wsl -l -v` before it starts and asserts that the machine is
//!   unchanged afterwards, except for the distros it created and removed;
//! * unregisters everything it created on the way out, including on failure —
//!   a leaked test distro is a 500 MB mistake.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentify_backends::wsl::cli::Wsl;
use agentify_backends::wsl::{rootfs, cli};
use agentify_backends::{
    run_lifecycle_conformance, Backend, EnvironmentSpec, ResourceSpec, RootfsSource, WslBackend,
    WslConfig, STATE_ABSENT, STATE_RUNNING, STATE_STOPPED,
};

/// How long a single real start may take: import, first boot, and the
/// supervisor.
const START_BUDGET: Duration = Duration::from_secs(300);

fn enabled() -> bool {
    matches!(
        std::env::var("AGENTIFY_TEST_WSL").as_deref().map(str::trim),
        Ok("1")
    )
}

fn stamp() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos % 0xffff_ffff)
}

/// A backend plus a unique prefix and the machine's distro list, so a test can
/// prove it left nothing behind.
struct Harness {
    backend: WslBackend,
    prefix: String,
    before: Vec<String>,
}

impl Harness {
    fn new(label: &str) -> Harness {
        assert!(enabled(), "set AGENTIFY_TEST_WSL=1 to run the WSL suite");
        let prefix = format!("agentify-t7{}-{}", stamp(), label);
        assert!(prefix.len() < 40, "keep the prefix short");
        // One data root for the whole run, under `target/`: the golden template
        // is a few hundred megabytes and rebuilding it per test would be silly.
        let data_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("wsl-test");
        let mut config = WslConfig::new(&data_root)
            .with_prefix(prefix.clone())
            .with_start_timeout(START_BUDGET);
        config.rootfs = RootfsSource::from_override(
            std::env::var_os("AGENTIFY_WSL_ROOTFS").map(PathBuf::from),
        );
        let backend = WslBackend::new(config);
        let before = names();
        Harness {
            backend,
            prefix,
            before,
        }
    }

    fn distro(&self, environment: &str) -> String {
        self.backend.distro_name(environment)
    }

    /// Assert the machine's distro list is exactly what it was plus/ minus this
    /// test's own distros, then unregister anything left over.
    fn finish(self) {
        let removed = self.backend.unregister_all().expect("unregister leftovers");
        let after = names();
        let leaked: Vec<&String> = after
            .iter()
            .filter(|name| name.to_ascii_lowercase().starts_with(&self.prefix))
            .collect();
        assert!(
            leaked.is_empty(),
            "these test distros were left registered: {leaked:?} (removed: {removed:?})"
        );
        assert_eq!(
            after, self.before,
            "`wsl -l -v` changed: before {:?}, after {:?}",
            self.before, after
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The safety net: even a panicking test leaves no runtime behind.
        let _ = self.backend.unregister_all();
    }
}

fn names() -> Vec<String> {
    Wsl::new("agentify").list().expect("list distros").into_iter().map(|distro| distro.name).collect()
}

fn probe(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("ephemeral port")
        .local_addr()
        .expect("addr")
        .port()
}

fn spec(environment: &str) -> EnvironmentSpec {
    let mut spec = EnvironmentSpec::new(environment, "wsl-test");
    let web = free_port();
    let db = web; // one lease can serve two guest ports; the supervisor dedupes.
    spec.lease("web", 3000, web);
    spec.lease("db", 5432, db);
    spec.with_resource(ResourceSpec {
        name: "web".to_string(),
        kind: "app".to_string(),
        image: Some("node:22".to_string()),
        build: None,
        command: Vec::new(),
        env: Default::default(),
        ports: vec![3000],
        depends_on: vec!["db".to_string()],
        healthcheck: None,
        engine: None,
        version: None,
        volume: None,
    });
    spec.with_resource(ResourceSpec {
        name: "db".to_string(),
        kind: "database".to_string(),
        image: None,
        build: None,
        command: Vec::new(),
        env: Default::default(),
        ports: vec![5432],
        depends_on: Vec::new(),
        healthcheck: None,
        engine: Some("postgres".to_string()),
        version: Some("16".to_string()),
        volume: None,
    });
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lifecycle_half_is_green_against_a_real_distro() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("conf");
    let report = run_lifecycle_conformance(&harness.backend, "c").await;
    assert!(
        report.is_ok(),
        "the WSL adapter must satisfy the lifecycle contract:\n{}",
        report.summary()
    );
    assert_eq!(report.passed(), 7, "{}", report.summary());
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_keeps_the_data_and_remove_unregisters_everything() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("remove");
    let environment = "remove".to_string();
    let distro = harness.distro(&environment);
    let spec = spec(&environment);
    let ports: Vec<u16> = spec.ports.iter().map(|lease| lease.host_port).collect();

    harness.backend.start(&spec).await.expect("start");

    // A real distro exists, it is registered, and it is running.
    let registered = Wsl::new(&harness.prefix)
        .find(&distro)
        .expect("list")
        .expect("the distro is registered");
    assert_eq!(registered.version, "2", "every project distro is WSL 2");
    assert!(
        Wsl::new(&harness.prefix).is_running(&distro).expect("running"),
        "start leaves the distro running"
    );
    for port in &ports {
        assert!(probe(*port), "leased port {port} is bound on 127.0.0.1");
    }
    assert_eq!(
        harness.backend.status(&environment).await.expect("status").state,
        STATE_RUNNING
    );

    // Stop: the leases go, the disk stays.
    harness.backend.stop(&environment).await.expect("stop");
    assert!(
        !Wsl::new(&harness.prefix).is_running(&distro).expect("running"),
        "stop terminates the distro"
    );
    assert!(
        Wsl::new(&harness.prefix).find(&distro).expect("list").is_some(),
        "stop keeps the distro registered"
    );
    for port in &ports {
        assert!(!probe(*port), "port {port} is released by stop");
    }
    assert!(
        harness.backend.config().env_dir(&environment).is_dir(),
        "stop keeps the runtime's data"
    );
    assert_eq!(
        harness.backend.status(&environment).await.expect("status").state,
        STATE_STOPPED
    );

    // Start again on the same leases, then remove everything.
    harness.backend.start(&spec).await.expect("restart");
    for port in &ports {
        assert!(probe(*port), "the restarted runtime rebinds {port}");
    }
    harness.backend.remove(&environment, false).await.expect("remove");

    assert!(
        Wsl::new(&harness.prefix).find(&distro).expect("list").is_none(),
        "remove unregisters the distro"
    );
    assert!(
        !harness.backend.config().env_dir(&environment).exists(),
        "remove deletes the runtime's data"
    );
    for port in &ports {
        assert!(!probe(*port), "port {port} is free after remove");
    }
    assert_eq!(
        harness.backend.status(&environment).await.expect("status").state,
        STATE_ABSENT
    );
    // And a second remove is a no-op, not an error.
    harness.backend.remove(&environment, false).await.expect("remove twice");
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_what_the_machine_actually_says() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("status");
    let environment = "status".to_string();
    let distro = harness.distro(&environment);

    // Never started: absent, not an error.
    let fresh = harness.backend.status(&environment).await.expect("status");
    assert_eq!(fresh.state, STATE_ABSENT);

    let spec = spec(&environment);
    harness.backend.start(&spec).await.expect("start");
    assert_eq!(
        harness.backend.status(&environment).await.expect("status").state,
        STATE_RUNNING
    );
    let status = harness.backend.status(&environment).await.expect("status");
    assert_eq!(status.resources.len(), 2, "both Resources are reported");
    assert!(status.resources.iter().all(|resource| resource.state == STATE_RUNNING));

    // Kill the distro behind the adapter's back: the supervisor is gone, so
    // claiming `running` would be a lie the next probe would expose.
    Wsl::new(&harness.prefix).terminate(&distro).expect("terminate");
    let mut state = String::new();
    for _ in 0..100 {
        state = harness.backend.status(&environment).await.expect("status").state;
        if state != STATE_RUNNING {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        state != STATE_RUNNING,
        "a terminated distro must not report running, got {state:?}"
    );

    harness.backend.stop(&environment).await.expect("stop");
    harness.backend.remove(&environment, false).await.expect("remove");
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_container_engine_is_real_when_the_template_carries_one() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("engine");
    if !harness.backend.config().engine {
        eprintln!("AGENTIFY_WSL_ENGINE=0: the engine is deliberately off, nothing to check");
        harness.finish();
        return;
    }
    let environment = "engine".to_string();
    let distro = harness.distro(&environment);
    let spec = spec(&environment);
    harness.backend.start(&spec).await.expect("start");

    let wsl = Wsl::new(&harness.prefix);
    let version = wsl
        .run(&distro, &["docker", "version", "--format", "{{.Server.Version}}"])
        .expect("run docker");
    assert_eq!(
        version.code, 0,
        "the container engine must answer inside the distro: {}",
        version.diagnostic()
    );
    let reported = version.stdout_text();
    assert!(
        !reported.trim().is_empty(),
        "the engine reports a server version"
    );
    eprintln!("container engine in {}: {}", distro, reported.trim());

    harness.backend.stop(&environment).await.expect("stop");
    harness.backend.remove(&environment, false).await.expect("remove");
    harness.finish();
}

#[test]
fn the_rootfs_cache_holds_the_pinned_digest() {
    if !enabled() {
        return;
    }
    let data_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("wsl-test");
    let config = WslConfig::new(data_root);
    let verified = rootfs::ensure(&config.rootfs, &config.rootfs_dir()).expect("rootfs");
    match config.rootfs {
        RootfsSource::Pinned => assert_eq!(
            verified.sha256,
            rootfs::PINNED_ROOTFS_SHA256,
            "the cached rootfs matches the pinned digest"
        ),
        RootfsSource::Local { .. } => assert!(!verified.sha256.is_empty()),
    }
    assert!(verified.path.is_file());
}

#[test]
fn the_adapter_never_owns_a_distro_it_did_not_mint() {
    // True whether or not the suite is enabled: the guard that keeps `Ubuntu`
    // and `docker-desktop` out of reach is pure logic.
    let wsl = Wsl::new("agentify-t7-deadbeef");
    assert!(!wsl.owns("Ubuntu"));
    assert!(!wsl.owns("docker-desktop"));
    assert!(wsl.owns("agentify-t7-deadbeef-x"));
    if enabled() {
        // And on a real machine, `--terminate` against a distro we do not own
        // is refused before WSL is ever asked.
        let error = wsl.terminate("Ubuntu").expect_err("must refuse");
        assert_eq!(error.code(), "BackendUnavailable");
        let error = wsl.unregister("docker-desktop").expect_err("must refuse");
        assert_eq!(error.code(), "BackendUnavailable");
        let still_there = cli::Wsl::new("agentify")
            .list()
            .expect("list")
            .into_iter()
            .any(|distro| distro.name == "Ubuntu");
        assert!(still_there, "Ubuntu is untouched");
    }
}

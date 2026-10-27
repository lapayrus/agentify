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

use futures_util::StreamExt;

use agentify_backends::wsl::cli::Wsl;
use agentify_backends::wsl::{log, rootfs};
use agentify_backends::{
    run_conformance, run_lifecycle_conformance, Backend, ExecChunk, ExecSpec, EnvironmentSpec,
    LogOptions, ResourceSpec, RootfsSource, WslBackend, WslConfig, STATE_ABSENT, STATE_RUNNING,
    STATE_STOPPED,
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
        let prefix = format!("agentify-t8{}-{}", stamp(), label);
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
        // A conformance case hands the adapter an OS temporary directory, which
        // is not inside this run's data root. Production keeps the root
        // (`WslConfig::new` sets it); a test that wants to prove the rule sets it
        // back, which `a_transfer_outside_the_daemon_root_is_refused` does.
        config = config.with_host_transfer_root(None);
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

/// The whole contract, on real distros — the case this ticket exists for.
///
/// Every case provisions, drives, and destroys its own Environment, so this is
/// the expensive test in the file: thirteen real imports. If it is green, the
/// API does not have to learn anything about WSL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_whole_conformance_suite_is_green_against_a_real_distro() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("full");
    let report = run_conformance(&harness.backend, "w").await;
    assert!(
        report.is_ok(),
        "the WSL adapter must satisfy the whole Backend contract:\n{}",
        report.summary()
    );
    assert_eq!(
        report.passed(),
        13,
        "every case must run, not just the ones this substrate likes:\n{}",
        report.summary()
    );
    assert_eq!(report.skipped(), 0, "{}", report.summary());
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
        let still_there = Wsl::new("agentify")
            .list()
            .expect("list")
            .into_iter()
            .any(|distro| distro.name == "Ubuntu");
        assert!(still_there, "Ubuntu is untouched");
    }
}

// -- the operational surface, at the Backend seam ------------------------------
//
// Everything below drives the real adapter against a real distro: exec with its
// two streams and its exit code, cancellation, the Environment log with `tail`,
// `since`, and `follow`, statistics, and copy in and out. They are the same
// behaviours the conformance table checks, asserted here in the detail the table
// has no room for.

/// Run one command to completion and return `(stdout, stderr, exit_code)`.
async fn run(
    backend: &WslBackend,
    environment: &str,
    target: &str,
    command: &[&str],
) -> (String, String, i32) {
    let request = ExecSpec {
        command: command.iter().map(|part| part.to_string()).collect(),
        ..ExecSpec::default()
    };
    let session = backend
        .exec(environment, Some(target), &request)
        .await
        .expect("exec");
    let (stdout, stderr, code) = session.collect().await.expect("collect");
    (
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
        code,
    )
}

/// How many lines a file in the distro holds. Zero when it is not there.
async fn lines_in(backend: &WslBackend, environment: &str, path: &str) -> u64 {
    let (stdout, _, _) = run(
        backend,
        environment,
        "web",
        &["sh", "-c", &format!("wc -l < {path} 2>/dev/null || echo 0")],
    )
    .await;
    stdout.trim().parse::<u64>().unwrap_or(0)
}

async fn read_logs(
    backend: &WslBackend,
    environment: &str,
    options: LogOptions,
) -> Vec<agentify_backends::LogLine> {
    let stream = backend
        .logs(environment, None, &options)
        .await
        .expect("logs");
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|line| line.expect("a good log line"))
        .collect()
}

fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agentify-t8-{label}-{}", stamp()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_keeps_the_streams_apart_returns_the_exit_code_and_cancels() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("exec");
    let environment = "exec".to_string();
    let spec = spec(&environment);
    harness.backend.start(&spec).await.expect("start");

    // stdout, stderr, and a non-zero exit, all at once and all separate.
    let (stdout, stderr, code) = run(
        &harness.backend,
        &environment,
        "web",
        &["sh", "-c", "echo out; echo err 1>&2; exit 7"],
    )
    .await;
    assert_eq!(stdout, "out\n", "stdout is stdout");
    assert_eq!(stderr, "err\n", "stderr is stderr");
    assert_eq!(code, 7, "the exit code survives the launcher");

    // Output really streams: the first line arrives long before the command
    // finishes, or this is a buffered call wearing a stream's clothes.
    let started = std::time::Instant::now();
    let request = ExecSpec {
        command: vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo first; sleep 2; echo second".to_string(),
        ],
        ..ExecSpec::default()
    };
    let mut session = harness
        .backend
        .exec(&environment, Some("web"), &request)
        .await
        .expect("exec");
    let mut seen = String::new();
    let mut first_at = None;
    while let Some(chunk) = session.next_chunk().await {
        match chunk.expect("a good chunk") {
            ExecChunk::Stdout(bytes) => {
                first_at.get_or_insert(started.elapsed());
                seen.push_str(&String::from_utf8_lossy(&bytes));
            }
            ExecChunk::Stderr(_) => {}
            ExecChunk::Exit { code } => {
                assert_eq!(code, 0, "the streaming command succeeded");
                break;
            }
        }
    }
    assert_eq!(seen, "first\nsecond\n");
    let first_at = first_at.expect("at least one chunk arrived");
    assert!(
        first_at < Duration::from_millis(1500),
        "the first line arrived after {first_at:?}: that is not a stream"
    );

    // Cancellation terminates the guest process group — the shell *and* the
    // `sleep` it started — not just the process the daemon launched.
    let heartbeat = "/tmp/agentify-cancel-heartbeat";
    let _ = run(&harness.backend, &environment, "web", &["rm", "-f", heartbeat]).await;
    let request = ExecSpec {
        command: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("while :; do echo tick >> {heartbeat}; sleep 0.05; done"),
        ],
        ..ExecSpec::default()
    };
    let mut session = harness
        .backend
        .exec(&environment, Some("web"), &request)
        .await
        .expect("exec");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while lines_in(&harness.backend, &environment, heartbeat).await < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the looping command never wrote a tick"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(session.cancel(), "this Backend supports cancellation");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match tokio::time::timeout_at(deadline, session.next_chunk()).await {
            Ok(Some(Ok(ExecChunk::Exit { .. }))) | Ok(None) => break,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(err))) => panic!("a cancelled exec failed instead of ending: {err}"),
            Err(_) => panic!("a cancelled exec kept streaming for 15s"),
        }
    }
    drop(session);

    tokio::time::sleep(Duration::from_millis(500)).await;
    let settled = lines_in(&harness.backend, &environment, heartbeat).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let later = lines_in(&harness.backend, &environment, heartbeat).await;
    assert_eq!(
        settled, later,
        "the cancelled command's process group is still writing to the heartbeat file"
    );

    harness.backend.stop(&environment).await.expect("stop");
    harness.backend.remove(&environment, false).await.expect("remove");
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_serve_tail_since_and_follow_without_blocking_the_workload() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("logs");
    let environment = "logs".to_string();
    let spec = spec(&environment);
    harness.backend.start(&spec).await.expect("start");

    // The Environment log is the supervisor's own output, and it starts with
    // every leased port it bound.
    let all = read_logs(&harness.backend, &environment, LogOptions::default()).await;
    assert!(
        all.len() >= 2,
        "a started Environment has a log: {all:?}"
    );
    assert!(
        all.iter().all(|line| line.resource == "supervisor"),
        "supervisor output is filed under its own name, not a Resource's"
    );

    let tail = read_logs(
        &harness.backend,
        &environment,
        LogOptions {
            tail: Some(2),
            ..LogOptions::default()
        },
    )
    .await;
    assert_eq!(tail.len(), 2, "tail is a bound on the backlog");
    assert_eq!(
        tail.last().map(|line| line.seq),
        all.last().map(|line| line.seq),
        "tail keeps the newest lines"
    );

    let newest = all.last().expect("a line").seq;
    let since = read_logs(
        &harness.backend,
        &environment,
        LogOptions::default().since(newest.to_string()),
    )
    .await;
    assert!(since.is_empty(), "since is a cursor, not a filter: {since:?}");

    let as_timestamp = read_logs(
        &harness.backend,
        &environment,
        LogOptions::default().since("2999-01-01T00:00:00.000Z"),
    )
    .await;
    assert!(
        as_timestamp.is_empty(),
        "an RFC 3339 cursor is honoured too"
    );

    // Follow: the backlog, then a line written after the stream opened, while
    // nothing is polling the follower and the workload keeps working.
    let path = harness.backend.config().log_path(&environment);
    let mut follower = harness
        .backend
        .logs(&environment, None, &LogOptions::following())
        .await
        .expect("follow");
    let first = tokio::time::timeout(Duration::from_secs(10), follower.next())
        .await
        .expect("the backlog arrives promptly")
        .expect("a line")
        .expect("a good line");
    assert_eq!(first.seq, all[0].seq, "a follower starts at the beginning");

    let mut sink = log::LogSink::open(&path).expect("open the Environment log");
    sink.append("supervisor", "stdout", "agentify-supervise: live");

    // A slow consumer must not block the workload: the follower is not read for
    // the duration of this exec, and the exec still finishes.
    let (stdout, _, code) = run(
        &harness.backend,
        &environment,
        "web",
        &["echo", "workload-unblocked"],
    )
    .await;
    assert_eq!(stdout, "workload-unblocked\n");
    assert_eq!(code, 0);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let next = tokio::time::timeout_at(deadline, follower.next())
            .await
            .expect("the follower catches up");
        let line = next.expect("still open").expect("a good line");
        if line.line.contains("live") {
            break;
        }
    }
    drop(follower);

    // Statistics: measured, never invented.
    let running = harness.backend.stats(&environment).await.expect("stats");
    assert_eq!(running.state, STATE_RUNNING);
    assert!(
        running.memory_bytes > 0,
        "a running distro uses memory: {running:?}"
    );
    assert!(
        running.disk_bytes > 0,
        "a running distro has a rootfs with bytes in it: {running:?}"
    );
    assert!(running.cpu_percent >= 0.0, "a CPU rate cannot be negative");
    eprintln!(
        "wsl stats: cpu {:.2}%, memory {} B, disk {} B, {} per-Resource figure(s)",
        running.cpu_percent,
        running.memory_bytes,
        running.disk_bytes,
        running.resources.len()
    );

    harness.backend.stop(&environment).await.expect("stop");
    let stopped = harness.backend.stats(&environment).await.expect("stats");
    assert_eq!(stopped.state, STATE_STOPPED);
    assert_eq!(stopped.memory_bytes, 0, "a stopped runtime uses nothing");
    // Stopping keeps the log: reading it back is the point of keeping it.
    let after_stop = read_logs(&harness.backend, &environment, LogOptions::default()).await;
    assert!(
        after_stop.len() >= all.len(),
        "the log survives the runtime"
    );

    harness.backend.remove(&environment, false).await.expect("remove");
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_moves_bytes_both_ways_through_the_distro() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("copy");
    let environment = "copy".to_string();
    let spec = spec(&environment);
    harness.backend.start(&spec).await.expect("start");

    let dir = scratch_dir("copy");
    let source = dir.join("in.txt");
    let payload: &[u8] = b"agentify copy: text, a NUL \x00 and a high byte \xff\n";
    std::fs::write(&source, payload).expect("write the source");

    harness
        .backend
        .copy_in(&environment, Some("web"), &source, "/tmp/agentify-copy/in.txt")
        .await
        .expect("copy in");
    let (stdout, _, code) = run(&harness.backend, &environment, "web", &["cat", "/tmp/agentify-copy/in.txt"]).await;
    assert_eq!(code, 0, "the copied file is readable in the distro");
    assert_eq!(
        stdout.as_bytes(),
        payload,
        "the bytes arrived unchanged, including the NUL"
    );

    let target = dir.join("out.txt");
    harness
        .backend
        .copy_out(&environment, Some("web"), "/tmp/agentify-copy/in.txt", &target)
        .await
        .expect("copy out");
    assert_eq!(
        std::fs::read(&target).expect("read the copy"),
        payload,
        "the round trip is byte for byte"
    );

    // A guest path that climbs is refused at the boundary, before any runtime
    // work happens.
    let err = harness
        .backend
        .copy_in(&environment, Some("web"), &source, "/tmp/../etc/passwd")
        .await
        .expect_err("a traversal is refused");
    assert!(
        err.to_string().contains("climbs out"),
        "the refusal names the rule: {err}"
    );

    // And reading something that is not there is a typed answer, not an empty
    // file.
    let missing = dir.join("missing.txt");
    let err = harness
        .backend
        .copy_out(&environment, Some("web"), "/tmp/agentify-copy/none.txt", &missing)
        .await
        .expect_err("no such file in the distro");
    assert_eq!(err.code(), "NotProvisioned");

    let _ = std::fs::remove_dir_all(&dir);
    harness.backend.stop(&environment).await.expect("stop");
    harness.backend.remove(&environment, false).await.expect("remove");
    harness.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_outside_the_daemon_root_is_refused() {
    if !enabled() {
        return;
    }
    let harness = Harness::new("refuse");
    // The harness lifts the host-transfer root so the conformance copy case can
    // use an OS temporary directory. Production keeps it, so put it back.
    let data_root = harness.backend.config().data_root.clone();
    let strict = WslBackend::new(
        harness
            .backend
            .config()
            .clone()
            .with_host_transfer_root(Some(data_root.clone())),
    );

    let outside = std::env::temp_dir().join(format!("agentify-t8-outside-{}.txt", stamp()));
    std::fs::write(&outside, b"not the daemon's").expect("write");
    let err = strict
        .copy_in("env-never-started", Some("web"), &outside, "/tmp/x")
        .await
        .expect_err("a host path outside the data root is refused");
    assert!(
        err.to_string().contains("daemon-owned root"),
        "the refusal names the rule: {err}"
    );
    let err = strict
        .copy_out("env-never-started", Some("web"), "/tmp/x", &outside)
        .await
        .expect_err("a host path outside the data root is refused");
    assert!(
        err.to_string().contains("daemon-owned root"),
        "the refusal names the rule: {err}"
    );

    // Inside the root the path is accepted as far as the runtime, and only the
    // absence of a runtime stops it.
    let inside = data_root.join("wsl").join("payload.txt");
    std::fs::create_dir_all(inside.parent().unwrap()).expect("data root");
    std::fs::write(&inside, b"the daemon's").expect("write");
    let err = strict
        .copy_in("env-never-started", Some("web"), &inside, "/tmp/x")
        .await
        .expect_err("there is no runtime to copy into");
    assert_eq!(err.code(), "NotProvisioned", "{err}");

    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_file(&inside);
    harness.finish();
}

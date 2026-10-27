//! The fake Backend must pass the conformance suite the WSL adapter will run.
//!
//! These tests use only the crate's public surface: the trait, the fake, and
//! the suite. Nothing here reaches into the fake's internals.

use std::sync::Arc;
use std::time::Duration;

use agentify_backends::{
    run_conformance, run_lifecycle_conformance, Backend, BackendError, EnvironmentSpec, FakeBackend, FakeFault, FakeScript,
    Outcome,
};

#[tokio::test]
async fn the_fake_backend_passes_the_whole_conformance_suite() {
    let backend = FakeBackend::default();
    let report = run_conformance(&backend, "conf").await;
    assert!(
        report.is_ok(),
        "the fake must satisfy the contract it defines:\n{}",
        report.summary()
    );
    assert_eq!(report.passed(), 13, "every case runs:\n{}", report.summary());
    assert_eq!(report.skipped(), 0, "nothing is skipped:\n{}", report.summary());
}

#[tokio::test]
async fn the_lifecycle_half_runs_on_its_own() {
    // A Backend that only serves the lifecycle (the WSL adapter before its exec,
    // logs, and stats ticket) still proves the lifecycle through the same table.
    let report = run_lifecycle_conformance(&FakeBackend::default(), "life").await;
    assert!(report.is_ok(), "{}", report.summary());
    let names: Vec<&str> = report.cases.iter().map(|case| case.name).collect();
    assert_eq!(
        names,
        vec![
            "describe_reports_name_version_platform",
            "start_is_idempotent_and_reports_running",
            "leased_ports_are_bound_while_running",
            "stop_is_idempotent",
            "status_is_readable_before_any_start",
            "start_converges_after_remove",
            "unsupported_capabilities_are_typed",
        ],
        "the lifecycle half is exactly the cases a lifecycle-only adapter can serve"
    );
    assert_eq!(report.passed(), 7, "{}", report.summary());
    assert!(
        !names.contains(&"exec_returns_stdout_stderr_and_exit_code"),
        "exec is not part of the lifecycle half"
    );
}

#[tokio::test]
async fn the_suite_is_usable_through_a_trait_object() {
    // The daemon holds `Arc<dyn Backend>`; the suite must run through it.
    let backend: Arc<dyn Backend> = Arc::new(FakeBackend::default());
    let report = run_conformance(backend.as_ref(), "object").await;
    assert!(report.is_ok(), "{}", report.summary());
    assert!(
        report.summary().contains("fake 0.1.0"),
        "the report names the adapter: {}",
        report.summary()
    );
}

#[tokio::test]
async fn describe_reports_the_fake_capabilities() {
    let info = FakeBackend::default().describe();
    assert_eq!(info.name, "fake");
    assert_eq!(info.platform, "scripted");
    assert!(info.capabilities.events, "the fake reports state changes");
    assert!(info.capabilities.log_since, "the fake honours `since`");
    assert!(
        !info.capabilities.checkpoint,
        "checkpoints are a later ticket; the fake says so instead of pretending"
    );
    assert!(!info.capabilities.pty, "v0 has no PTY");
}

#[tokio::test]
async fn a_scripted_start_failure_is_a_typed_error() {
    let backend = FakeBackend::new(FakeScript::failing_start(FakeFault::BackendUnavailable));
    let spec = EnvironmentSpec::new("env-fail", "demo");
    let err = backend.start(&spec).await.expect_err("the script fails start");
    assert_eq!(err.code(), "BackendUnavailable");
    assert_eq!(err.status(), 503);
}

#[tokio::test]
async fn a_scripted_degradation_leaves_the_leased_port_closed() {
    // Degradation is real: the port is simply not bound, so a daemon TCP probe
    // fails and the daemon — not the fake — decides the Environment is degraded.
    let backend = FakeBackend::new(FakeScript::degraded(&["web"]));
    let mut spec = EnvironmentSpec::new("env-degraded", "demo");
    let web = free_port();
    let db = free_port();
    spec.lease("web", 3000, web);
    spec.lease("db", 5432, db);
    spec.with_resource(resource("web", "app", vec![3000]));
    spec.with_resource(resource("db", "database", vec![5432]));

    backend.start(&spec).await.expect("start still succeeds");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", db)).is_ok(),
        "the healthy Resource is bound"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", web)).is_err(),
        "the scripted Resource is not bound"
    );
    backend.stop(&spec.environment_id).await.expect("stop");
}

#[tokio::test]
async fn exec_inherits_nowhere_and_echoes_deterministically() {
    let backend = FakeBackend::default();
    let mut spec = EnvironmentSpec::new("env-exec", "demo");
    let port = free_port();
    spec.lease("web", 3000, port);
    spec.with_resource(resource("web", "app", vec![3000]));
    backend.start(&spec).await.expect("start");

    let session = backend
        .exec(
            "env-exec",
            Some("web"),
            &agentify_backends::ExecSpec {
                command: vec!["echo".into(), "hi".into()],
                ..Default::default()
            },
        )
        .await
        .expect("exec");
    let (stdout, stderr, code) = session.collect().await.expect("collect");
    assert_eq!(String::from_utf8_lossy(&stdout), "hi\n");
    assert!(stderr.is_empty());
    assert_eq!(code, 0);

    let unprovisioned = backend
        .exec(
            "env-exec",
            Some("nope"),
            &agentify_backends::ExecSpec {
                command: vec!["echo".into()],
                ..Default::default()
            },
        )
        .await
        .expect_err("no such Resource");
    assert_eq!(unprovisioned.code(), "NotProvisioned");

    backend.stop("env-exec").await.expect("stop");
}

#[tokio::test]
async fn logs_follow_new_lines() {
    use futures_util::StreamExt;

    let backend = FakeBackend::default();
    let mut spec = EnvironmentSpec::new("env-follow", "demo");
    spec.lease("web", 3000, free_port());
    spec.with_resource(resource("web", "app", vec![3000]));
    backend.start(&spec).await.expect("start");

    let mut stream = backend
        .logs("env-follow", Some("web"), &agentify_backends::LogOptions::following())
        .await
        .expect("follow");
    // Drain what start produced.
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("a stream opens with the lines start produced")
        .expect("a line")
        .expect("a good line");
    assert_eq!(first.resource, "web");

    // A new exec must appear on the open stream.
    backend
        .exec(
            "env-follow",
            Some("web"),
            &agentify_backends::ExecSpec {
                command: vec!["echo".into(), "live".into()],
                ..Default::default()
            },
        )
        .await
        .expect("exec")
        .collect()
        .await
        .expect("collect");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("a followed stream must yield the new line");
        let line = next.expect("still open").expect("good line");
        if line.line.contains("live") {
            break;
        }
    }
    drop(stream);
    backend.stop("env-follow").await.expect("stop");
}

#[tokio::test]
async fn stopping_releases_the_leased_ports_and_start_reacquires_them() {
    let backend = FakeBackend::default();
    let mut spec = EnvironmentSpec::new("env-reacquire", "demo");
    let port = free_port();
    spec.lease("web", 3000, port);
    spec.with_resource(resource("web", "app", vec![3000]));

    backend.start(&spec).await.expect("start");
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    backend.stop("env-reacquire").await.expect("stop");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "stop must release the leased port"
    );
    backend.start(&spec).await.expect("restart on the same lease");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "start must re-bind the lease the daemon still holds"
    );
    backend.stop("env-reacquire").await.expect("stop");
    backend.remove("env-reacquire", false).await.expect("remove");
}

#[test]
fn a_script_is_parsed_strictly_so_a_typo_is_not_silent() {
    let script = FakeScript::from_json(r#"{"start_delay_ms": 25, "unbound_resources": ["web"]}"#)
        .expect("a valid script");
    assert_eq!(script.start_delay_ms, 25);
    assert_eq!(script.unbound_resources, vec!["web".to_string()]);

    let typo = FakeScript::from_json(r#"{"start_dealy_ms": 25}"#).expect_err("a typo is rejected");
    assert!(typo.contains("unknown fake script key"), "{typo}");
    let bad_fault =
        FakeScript::from_json(r#"{"start_fault": "nope"}"#).expect_err("an unknown fault");
    assert!(bad_fault.contains("start_fault"), "{bad_fault}");
}

#[tokio::test]
async fn the_conformance_report_lists_every_case_by_name() {
    let report = run_conformance(&FakeBackend::default(), "names").await;
    let names: Vec<&str> = report.cases.iter().map(|case| case.name).collect();
    for expected in [
        "start_is_idempotent_and_reports_running",
        "leased_ports_are_bound_while_running",
        "start_converges_after_remove",
        "exec_returns_stdout_stderr_and_exit_code",
        "exec_cancels_where_supported",
        "logs_honour_tail_and_since",
        "stats_report_environment_level_figures",
        "unsupported_capabilities_are_typed",
    ] {
        assert!(names.contains(&expected), "missing case {expected}: {names:?}");
    }
    assert!(
        report.cases.iter().all(|case| !matches!(case.outcome, Outcome::Failed(_))),
        "{}",
        report.summary()
    );
}

#[tokio::test]
async fn an_unsupported_capability_names_itself() {
    let backend = FakeBackend::default();
    match backend.checkpoint("env-none").await {
        Err(BackendError::Unsupported { capability, .. }) => assert_eq!(capability, "checkpoint"),
        other => panic!("expected a typed Unsupported, got {other:?}"),
    }
}

fn resource(name: &str, kind: &str, ports: Vec<u16>) -> agentify_backends::ResourceSpec {
    agentify_backends::ResourceSpec {
        name: name.to_string(),
        kind: kind.to_string(),
        image: None,
        build: None,
        command: Vec::new(),
        env: Default::default(),
        ports,
        depends_on: Vec::new(),
        healthcheck: None,
        engine: None,
        version: None,
        volume: None,
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    listener.local_addr().expect("addr").port()
}

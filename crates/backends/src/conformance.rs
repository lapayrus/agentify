//! The reused Backend conformance suite.
//!
//! One table of cases every Backend must pass: lifecycle idempotence,
//! convergence after removal, exec streaming and exit codes, cancellation, log
//! tailing, port binding, capability reporting, and typed errors. The fake runs
//! it in CI on every OS; the WSL adapter will run it on Windows, and the macOS
//! adapters on their machines. If an adapter passes this, the API does not have
//! to learn about it.
//!
//! Every case gets its own Environment id, so the cases are independent and the
//! order they run in does not matter. Each case leaves its Environment removed.

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use crate::contract::{
    Backend, ExecChunk, ExecSpec, LogOptions, ResourceStatus, STATE_RUNNING, STATE_STOPPED,
};
use crate::error::BackendError;
use crate::spec::{EnvironmentSpec, ResourceSpec};

/// How a case ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    /// The Backend does not advertise the capability, so the case cannot run.
    /// A skip is not a pass: it is reported, and the caller decides.
    Skipped(String),
    Failed(String),
}

/// One named case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub name: &'static str,
    pub outcome: Outcome,
}

/// What running the suite produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub backend: String,
    pub cases: Vec<Case>,
}

impl Report {
    pub fn failures(&self) -> Vec<&Case> {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, Outcome::Failed(_)))
            .collect()
    }

    pub fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, Outcome::Passed))
            .count()
    }

    pub fn skipped(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, Outcome::Skipped(_)))
            .count()
    }

    /// True when nothing failed. A skip does not fail the suite, but it is
    /// visible in [`Report::summary`].
    pub fn is_ok(&self) -> bool {
        self.failures().is_empty()
    }

    pub fn summary(&self) -> String {
        let mut out = format!(
            "{}: {} passed, {} skipped, {} failed",
            self.backend,
            self.passed(),
            self.skipped(),
            self.failures().len()
        );
        for case in &self.cases {
            match &case.outcome {
                Outcome::Passed => out.push_str(&format!("\n  ok   {}", case.name)),
                Outcome::Skipped(why) => out.push_str(&format!("\n  skip {} ({why})", case.name)),
                Outcome::Failed(why) => out.push_str(&format!("\n  FAIL {} ({why})", case.name)),
            }
        }
        out
    }
}

/// Run every case against one Backend. `base` names the Environments, so two
/// runs against the same backend do not collide.
pub async fn run_conformance<B: Backend + ?Sized>(backend: &B, base: &str) -> Report {
    let mut cases = lifecycle_cases(backend, base).await;
    cases.extend(workload_cases(backend, base).await);
    let info = backend.describe();
    Report {
        backend: format!("{} {}", info.name, info.version),
        cases,
    }
}

/// Run only the lifecycle half: provision, report, stop, and destroy. An
/// adapter that has not implemented exec, logs, stats, or copy yet still runs
/// the same table for the part it does serve — [crate::wsl] does exactly that
/// until its exec and logs ticket lands.
pub async fn run_lifecycle_conformance<B: Backend + ?Sized>(backend: &B, base: &str) -> Report {
    let cases = lifecycle_cases(backend, base).await;
    let info = backend.describe();
    Report {
        backend: format!("{} {}", info.name, info.version),
        cases,
    }
}

/// The cases every Backend must pass before it can host an Environment.
async fn lifecycle_cases<B: Backend + ?Sized>(backend: &B, base: &str) -> Vec<Case> {
    let info = backend.describe();
    let mut cases = Vec::new();

    cases.push(run_case("describe_reports_name_version_platform", {
        let info = info.clone();
        async move {
            if info.name.trim().is_empty() {
                return Err("the Backend must report a name".to_string());
            }
            if info.version.trim().is_empty() {
                return Err("the Backend must report a version".to_string());
            }
            if info.platform.trim().is_empty() {
                return Err("the Backend must report a platform".to_string());
            }
            Ok(())
        }
    })
    .await);

    cases.push(
        run_case("start_is_idempotent_and_reports_running", {
            let env = format!("{base}-lifecycle");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                let first = backend.status(&env).await.map_err(err)?;
                if first.state != STATE_RUNNING {
                    return Err(format!("after start the state is {:?}, not running", first.state));
                }
                // Starting again must converge, not duplicate or fail.
                backend.start(&spec).await.map_err(err)?;
                let second = backend.status(&env).await.map_err(err)?;
                if second.state != STATE_RUNNING {
                    return Err(format!("after a second start the state is {:?}", second.state));
                }
                if second.resources.len() != spec.resources.len() {
                    return Err(format!(
                        "a second start changed the Resource set: {} then {}",
                        spec.resources.len(),
                        second.resources.len()
                    ));
                }
                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("leased_ports_are_bound_while_running", {
            let env = format!("{base}-ports");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                for lease in &spec.ports {
                    if !port_open(lease.host_port, Duration::from_secs(2)) {
                        return Err(format!(
                            "the Backend did not bind the leased port 127.0.0.1:{} for {}/{}",
                            lease.host_port, lease.resource, lease.guest_port
                        ));
                    }
                }
                backend.stop(&env).await.map_err(err)?;
                for lease in &spec.ports {
                    if port_open(lease.host_port, Duration::from_millis(500)) {
                        return Err(format!(
                            "port 127.0.0.1:{} stayed bound after stop",
                            lease.host_port
                        ));
                    }
                }
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("stop_is_idempotent", {
            let env = format!("{base}-stop");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                backend.stop(&env).await.map_err(err)?;
                let status = backend.status(&env).await.map_err(err)?;
                if status.state != STATE_STOPPED && status.state != crate::contract::STATE_ABSENT {
                    return Err(format!("after stop the state is {:?}", status.state));
                }
                // A second stop is a no-op, not an error.
                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("status_is_readable_before_any_start", {
            let env = format!("{base}-fresh");
            async move {
                match backend.status(&env).await {
                    Ok(status) => {
                        if status.state == STATE_RUNNING {
                            return Err("an Environment that was never started reports running".to_string());
                        }
                        Ok(())
                    }
                    Err(BackendError::NotProvisioned { .. }) => Ok(()),
                    Err(other) => Err(format!("unexpected error: {other}"))
                }
            }
        })
        .await,
    );

    cases.push(
        run_case("start_converges_after_remove", {
            let env = format!("{base}-converge");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                backend.start(&spec).await.map_err(err)?;
                let status = backend.status(&env).await.map_err(err)?;
                if status.state != STATE_RUNNING {
                    return Err(format!("after remove and start the state is {:?}", status.state));
                }
                for lease in &spec.ports {
                    if !port_open(lease.host_port, Duration::from_secs(2)) {
                        return Err(format!(
                            "the re-created runtime did not bind 127.0.0.1:{}",
                            lease.host_port
                        ));
                    }
                }
                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("unsupported_capabilities_are_typed", {
            let env = format!("{base}-caps");
            let info = info.clone();
            async move {
                // A capability the Backend does not advertise must fail with
                // `Unsupported`, never with a generic error or a panic.
                if !info.capabilities.checkpoint {
                    match backend.checkpoint(&env).await {
                        Err(BackendError::Unsupported { capability, .. }) => {
                            if capability.trim().is_empty() {
                                return Err("Unsupported must name the capability".to_string());
                            }
                        }
                        Err(other) => {
                            return Err(format!("an unsupported checkpoint failed with {other}"))
                        }
                        Ok(_) => {
                            return Err(
                                "the Backend claimed no checkpoint support but created one"
                                    .to_string(),
                            )
                        }
                    }
                }
                if !info.capabilities.events {
                    match backend.events(&env).await {
                        Err(BackendError::Unsupported { .. }) => {}
                        Err(other) => return Err(format!("unsupported events failed with {other}")),
                        Ok(_) => {
                            return Err(
                                "the Backend claimed no events support but opened a stream"
                                    .to_string(),
                            )
                        }
                    }
                }
                Ok(())
            }
        })
        .await,
    );

    cases
}

/// The workload half: exec, cancellation, logs, stats, copy, and the typed
/// `NotProvisioned` an unknown Environment earns. An adapter that has not
/// reached this ticket returns `Unsupported` for these commands instead.
async fn workload_cases<B: Backend + ?Sized>(backend: &B, base: &str) -> Vec<Case> {
    let mut cases = Vec::new();

    cases.push(
        run_case("exec_returns_stdout_stderr_and_exit_code", {
            let env = format!("{base}-exec");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;

                let ok = exec(&env, backend, Some("web"), &["echo", "hello", "world"]).await?;
                if ok.0 != "hello world\n" {
                    return Err(format!("stdout is {:?}, expected \"hello world\\n\"", ok.0));
                }
                if ok.2 != 0 {
                    return Err(format!("echo exited {}, expected 0", ok.2));
                }

                let failed = exec(&env, backend, Some("web"), &["fail", "3"]).await?;
                if failed.2 != 3 {
                    return Err(format!("`fail 3` exited {}, expected 3", failed.2));
                }
                if failed.1.is_empty() {
                    return Err("`fail 3` wrote nothing to stderr".to_string());
                }

                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("exec_cancels_where_supported", {
            let env = format!("{base}-cancel");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                let request = ExecSpec {
                    command: vec!["sleep".to_string(), "5000".to_string()],
                    ..ExecSpec::default()
                };
                let mut session = backend
                    .exec(&env, Some("web"), &request)
                    .await
                    .map_err(err)?;
                if !session.cancel() {
                    backend.stop(&env).await.map_err(err)?;
                    backend.remove(&env, false).await.map_err(err)?;
                    return Err("skip".to_string());
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err("a cancelled exec kept streaming for 3s".to_string());
                    }
                    match tokio::time::timeout(remaining, session.next_chunk()).await {
                        Ok(Some(Ok(ExecChunk::Exit { .. }))) | Ok(None) => break,
                        Ok(Some(Ok(_))) => continue,
                        Ok(Some(Err(err))) => return Err(format!("cancelled exec failed: {err}")),
                        Err(_) => return Err("a cancelled exec kept streaming for 3s".to_string()),
                    }
                }
                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("logs_honour_tail_and_since", {
            let env = format!("{base}-logs");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                exec(&env, backend, Some("web"), &["echo", "one"]).await?;

                let all = read_logs(&env, backend, LogOptions::default()).await?;
                if all.is_empty() {
                    return Err("a running Environment produced no log lines".to_string());
                }

                let tail = read_logs(
                    &env,
                    backend,
                    LogOptions {
                        tail: Some(2),
                        ..LogOptions::default()
                    },
                )
                .await?;
                if tail.len() != 2 {
                    return Err(format!("tail=2 returned {} lines", tail.len()));
                }
                let last_seq = all.last().map(|line| line.seq).unwrap_or(0);
                if tail.last().map(|line| line.seq) != Some(last_seq) {
                    return Err("tail returned lines that are not the newest".to_string());
                }

                let since = read_logs(
                    &env,
                    backend,
                    LogOptions::default().since(last_seq.to_string()),
                )
                .await?;
                if !since.is_empty() {
                    return Err(format!(
                        "since={last_seq} returned {} old lines",
                        since.len()
                    ));
                }

                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("stats_report_environment_level_figures", {
            let env = format!("{base}-stats");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                let running = backend.stats(&env).await.map_err(err)?;
                if running.memory_bytes == 0 {
                    return Err("a running Environment reported zero memory".to_string());
                }
                if running.cpu_percent < 0.0 {
                    return Err("cpu percent is negative".to_string());
                }
                backend.stop(&env).await.map_err(err)?;
                let stopped = backend.stats(&env).await.map_err(err)?;
                if stopped.state != STATE_STOPPED && stopped.state != crate::contract::STATE_ABSENT {
                    return Err(format!("a stopped Environment reports {:?}", stopped.state));
                }
                backend.remove(&env, false).await.map_err(err)?;
                Ok(())
            }
        })
        .await,
    );

    cases.push(
        run_case("an_unknown_environment_is_not_provisioned", {
            let env = format!("{base}-unknown");
            async move {
                let request = ExecSpec {
                    command: vec!["echo".to_string(), "hi".to_string()],
                    ..ExecSpec::default()
                };
                match backend.exec(&env, None, &request).await {
                    Err(BackendError::NotProvisioned { .. }) => Ok(()),
                    Err(other) => Err(format!("exec on an unknown Environment failed with {other}")),
                    Ok(_) => Err("exec on an unknown Environment returned a session".to_string()),
                }
            }
        })
        .await,
    );

    cases.push(
        run_case("copy_round_trips_a_daemon_owned_path", {
            let env = format!("{base}-copy");
            async move {
                let spec = spec(&env);
                backend.start(&spec).await.map_err(err)?;
                let dir = scratch_dir("copy");
                std::fs::create_dir_all(&dir).map_err(err)?;
                let source = dir.join("in.txt");
                std::fs::write(&source, b"payload").map_err(err)?;
                let copy_in = backend
                    .copy_in(&env, Some("web"), &source, "/tmp/agentify-in.txt")
                    .await;
                match copy_in {
                    Err(BackendError::Unsupported { .. }) => {
                        cleanup(&dir);
                        backend.stop(&env).await.map_err(err)?;
                        backend.remove(&env, false).await.map_err(err)?;
                        return Err("skip".to_string());
                    }
                    Err(other) => return Err(format!("copy_in failed with {other}")),
                    Ok(()) => {}
                }
                let target = dir.join("out.txt");
                backend
                    .copy_out(&env, Some("web"), "/tmp/agentify-in.txt", &target)
                    .await
                    .map_err(err)?;
                let round_tripped = std::fs::read(&target).map_err(err)?;
                cleanup(&dir);
                backend.stop(&env).await.map_err(err)?;
                backend.remove(&env, false).await.map_err(err)?;
                if round_tripped != b"payload" {
                    return Err(format!("copy round trip produced {:?}", round_tripped));
                }
                Ok(())
            }
        })
        .await,
    );

    cases
}

/// Run one case, turning a `skip` marker into [`Outcome::Skipped`].
async fn run_case<F>(name: &'static str, case: F) -> Case
where
    F: std::future::Future<Output = std::result::Result<(), String>>,
{
    let outcome = match case.await {
        Ok(()) => Outcome::Passed,
        Err(reason) if reason == "skip" => Outcome::Skipped("the Backend does not support it".into()),
        Err(reason) => Outcome::Failed(reason),
    };
    Case { name, outcome }
}

fn spec(environment: &str) -> EnvironmentSpec {
    let mut spec = EnvironmentSpec::new(environment, "conformance");
    let web_port = free_port();
    let db_port = free_port();
    spec.lease("web", 3000, web_port);
    spec.lease("db", 5432, db_port);
    spec.workspace = std::env::temp_dir().join("agentify-conformance-workspace");
    spec.with_resource(ResourceSpec {
        name: "web".to_string(),
        kind: "app".to_string(),
        image: Some("node:22".to_string()),
        build: None,
        command: vec!["node".to_string(), "server.js".to_string()],
        env: BTreeMap::new(),
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
        env: BTreeMap::new(),
        ports: vec![5432],
        depends_on: Vec::new(),
        healthcheck: None,
        engine: Some("postgres".to_string()),
        version: Some("16".to_string()),
        volume: Some("conformance-db-data".to_string()),
    });
    spec
}

/// A free loopback port. The suite leases nothing itself: it borrows a port the
/// OS says is free, exactly as the daemon would.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn port_open(port: u16, timeout: Duration) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, timeout).is_ok()
}

async fn exec<B: Backend + ?Sized>(
    environment: &str,
    backend: &B,
    target: Option<&str>,
    command: &[&str],
) -> std::result::Result<(String, String, i32), String> {
    let request = ExecSpec {
        command: command.iter().map(|part| part.to_string()).collect(),
        ..ExecSpec::default()
    };
    let session = backend
        .exec(environment, target, &request)
        .await
        .map_err(err)?;
    let (stdout, stderr, code) = session.collect().await.map_err(err)?;
    Ok((
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
        code,
    ))
}

async fn read_logs<B: Backend + ?Sized>(
    environment: &str,
    backend: &B,
    options: LogOptions,
) -> std::result::Result<Vec<crate::contract::LogLine>, String> {
    let stream = backend.logs(environment, None, &options).await.map_err(err)?;
    let collected: Vec<_> = stream.collect::<Vec<_>>().await;
    let mut lines = Vec::new();
    for item in collected {
        lines.push(item.map_err(err)?);
    }
    Ok(lines)
}

fn scratch_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("agentify-conformance-{label}-{unique}"))
}

fn cleanup(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Render a Resource list for a failure message.
#[allow(dead_code)]
fn describe_resources(resources: &[ResourceStatus]) -> String {
    resources
        .iter()
        .map(|resource| format!("{}={}", resource.name, resource.state))
        .collect::<Vec<_>>()
        .join(", ")
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

//! The fake Backend's per-Environment storage is isolated.
//!
//! The WSL slice proves volume and database isolation against a real distro;
//! this test proves the same boundary on the scripted fake, so CI always runs
//! a cross-Environment probe.

use std::path::PathBuf;

use agentify_backends::{Backend, BackendError, EnvironmentSpec, FakeBackend};

#[tokio::test]
async fn copying_into_one_environment_is_not_readable_from_another() {
    let backend = FakeBackend::default();

    let mut a = EnvironmentSpec::new("env-a", "alpha");
    a.workspace = PathBuf::from(std::env::temp_dir());
    let mut b = EnvironmentSpec::new("env-b", "beta");
    b.workspace = PathBuf::from(std::env::temp_dir());
    backend.start(&a).await.expect("start A");
    backend.start(&b).await.expect("start B");

    let source = std::env::temp_dir().join("agentify-fake-isolation-source");
    let target = std::env::temp_dir().join("agentify-fake-isolation-target");
    std::fs::write(&source, b"only A").expect("write source");

    backend
        .copy_in("env-a", None, &source, "marker.txt")
        .await
        .expect("copy into A");

    let error = backend
        .copy_out("env-b", None, "marker.txt", &target)
        .await
        .expect_err("B must not read a file only A copied");
    assert!(
        matches!(error, BackendError::NotProvisioned { .. }),
        "the cross-Environment read is a typed refusal: {error}"
    );

    backend
        .copy_out("env-a", None, "marker.txt", &target)
        .await
        .expect("A reads its own file");
    assert_eq!(std::fs::read(&target).expect("read target"), b"only A");
}

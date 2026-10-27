//! The cached golden template: the slow work, done once per machine.
//!
//! A Project's distro has to be ready in seconds, so nothing slow runs when a
//! Project starts. Instead the first start builds a template — import the base
//! rootfs, run the apt recipe, export the disk — and every later import (this
//! Project, the next one, this machine next week) is a plain `wsl --import` of
//! that tar.
//!
//! The template is keyed by everything that can change what it contains: the
//! recipe version, the rootfs digest, and whether the engine and systemd were
//! asked for. A change to any of them produces a new file, so a stale template
//! can never be handed to a Project. The building distro is unregistered in
//! every path, including failure: it must never show up in `wsl -l -v`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::cli::{Distro, Wsl};
use super::rootfs::Rootfs;
use super::{unix_script, WslConfig, SUPERVISE_PATH, SUPERVISE_SCRIPT};
use crate::error::{BackendError, Result};

/// Bump when [`PREPARE_SCRIPT`] changes in a way that must invalidate caches.
///
/// v2 adds Docker Compose to the template: the supervisor runs the Project's
/// Resources with it, and a template built by v1 has the engine but no Compose.
pub const RECIPE_VERSION: u32 = 2;

/// The in-distro recipe, compiled in so an installed daemon needs no files.
pub const PREPARE_SCRIPT: &str = include_str!("scripts/prepare.sh");

/// How long a template build may take before it is reported as a timeout.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(900);

/// A usable golden template on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub tar: PathBuf,
    /// True when the recipe managed to install the container engine.
    pub engine: bool,
    /// Recipe version that produced the tar.
    pub recipe: u32,
}

/// Marker written beside the tar, so a cache hit is explained rather than
/// guessed and a partial build is never reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    recipe: u32,
    rootfs_sha256: String,
    engine: bool,
    systemd: bool,
    bytes: u64,
}

/// Make sure a golden template exists for this configuration, and return it.
pub fn ensure(wsl: &Wsl, config: &WslConfig, rootfs: &Rootfs) -> Result<Template> {
    let dir = config.template_dir();
    std::fs::create_dir_all(&dir).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot create the template cache {}: {err}", dir.display()),
    })?;

    let tar = dir.join(format!(
        "golden-v{}-{}{}.tar",
        RECIPE_VERSION,
        if config.engine { "engine" } else { "plain" },
        if config.systemd { "-systemd" } else { "" }
    ));
    let marker_path = marker_path(&tar);

    if let Some(template) = cache_hit(&tar, &marker_path, rootfs) {
        return Ok(template);
    }
    // A partial or stale artefact is deleted before the rebuild, so a failed
    // build cannot leave something that looks usable.
    let _ = std::fs::remove_file(&tar);
    let _ = std::fs::remove_file(&marker_path);

    let built = build(wsl, config, rootfs, &tar)?;
    let marker = Marker {
        recipe: RECIPE_VERSION,
        rootfs_sha256: rootfs.sha256.clone(),
        engine: built.engine,
        systemd: config.systemd,
        bytes: std::fs::metadata(&tar).map(|meta| meta.len()).unwrap_or(0),
    };
    let encoded = serde_json::to_vec_pretty(&marker).unwrap_or_default();
    std::fs::write(&marker_path, encoded).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot write {}: {err}", marker_path.display()),
    })?;
    Ok(built)
}

fn marker_path(tar: &Path) -> PathBuf {
    tar.with_extension("json")
}

fn cache_hit(tar: &Path, marker: &Path, rootfs: &Rootfs) -> Option<Template> {
    if !tar.is_file() {
        return None;
    }
    let raw = std::fs::read(marker).ok()?;
    let marker: Marker = serde_json::from_slice(&raw).ok()?;
    if marker.recipe != RECIPE_VERSION || marker.rootfs_sha256 != rootfs.sha256 {
        return None;
    }
    let bytes = std::fs::metadata(tar).ok()?.len();
    if bytes == 0 || bytes != marker.bytes {
        return None;
    }
    Some(Template {
        tar: tar.to_path_buf(),
        engine: marker.engine,
        recipe: marker.recipe,
    })
}

/// Import, prepare, export, and unregister — cleaning up after itself whatever
/// happens.
fn build(wsl: &Wsl, config: &WslConfig, rootfs: &Rootfs, tar: &Path) -> Result<Template> {
    let name = format!(
        "{prefix}template-v{version}",
        prefix = wsl.prefix,
        version = RECIPE_VERSION
    );
    let dir = config.template_dir().join("build");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot create {}: {err}", dir.display()),
    })?;

    // A build interrupted last time may have left the distro registered.
    if wsl.find(&name)?.is_some() {
        let _ = wsl.unregister(&name);
    }

    let partial = tar.with_extension("part");
    let _ = std::fs::remove_file(&partial);

    let outcome = build_inner(wsl, config, rootfs, &name, &dir, &partial);

    // Clean up in every path: the build distro must never outlive the build.
    let _ = wsl.terminate(&name);
    let _ = wsl.unregister(&name);
    let _ = std::fs::remove_dir_all(&dir);

    let engine = outcome?;
    std::fs::rename(&partial, tar).map_err(|err| BackendError::BackendUnavailable {
        detail: format!("cannot move the golden template into place: {err}"),
    })?;
    Ok(Template {
        tar: tar.to_path_buf(),
        engine,
        recipe: RECIPE_VERSION,
    })
}

fn build_inner(
    wsl: &Wsl,
    config: &WslConfig,
    rootfs: &Rootfs,
    name: &str,
    dir: &Path,
    partial: &Path,
) -> Result<bool> {
    wsl.import(name, dir, &rootfs.path)?;

    let script = format!(
        "AGENTIFY_ENGINE={} AGENTIFY_SYSTEMD={} sh -s",
        if config.engine { 1 } else { 0 },
        if config.systemd { 1 } else { 0 }
    );
    let started = Instant::now();
    let output = wsl.run_with_stdin(
        name,
        &["sh", "-c", &script],
        unix_script(PREPARE_SCRIPT).as_bytes(),
    )?;
    if output.code != 0 {
        return Err(BackendError::BackendUnavailable {
            detail: format!(
                "the golden template recipe failed (exit {}): {}",
                output.code,
                output.diagnostic()
            ),
        });
    }
    tracing(started, "recipe");

    // The supervisor lives in the template *and* is rewritten on every start, so
    // a rebuilt daemon never runs an old copy against a new protocol.
    wsl.write_file(
        name,
        SUPERVISE_PATH,
        unix_script(SUPERVISE_SCRIPT).as_bytes(),
        "0755",
    )?;

    let engine = wsl
        .read_file(name, "/etc/agentify/engine")
        .ok()
        .flatten()
        .map(|bytes| !String::from_utf8_lossy(&bytes).trim().is_empty())
        .unwrap_or(false);

    let started = Instant::now();
    wsl.export(name, partial)?;
    tracing(started, "export");
    Ok(engine)
}

fn tracing(started: Instant, what: &str) {
    // One line on stderr: a template build is a minute of silence otherwise, and
    // this is the daemon's own diagnostic channel, not a control path.
    eprintln!(
        "agentify: golden template {what} took {} ms",
        started.elapsed().as_millis()
    );
}

/// Unregister every distro this WSL knows that matches the prefix — the
/// teardown path a test uses, and the reconciliation a later ticket will call.
pub fn unregister_all(wsl: &Wsl) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for Distro { name, .. } in wsl.list()? {
        if wsl.owns(&name) {
            let _ = wsl.terminate(&name);
            wsl.unregister(&name)?;
            removed.push(name);
        }
    }
    Ok(removed)
}



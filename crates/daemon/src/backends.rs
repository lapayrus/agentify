//! Which Backend the daemon drives.
//!
//! Selected with `AGENTIFY_BACKEND`: `fake` (the default) or `wsl`. A name
//! nothing implements is a startup failure, not a silent fallback: a daemon that
//! pretends to start Environments would be worse than one that refuses to boot.

use std::path::Path;
use std::sync::Arc;

use agentify_backends::{Backend, FakeBackend, FakeScript, WslBackend, WslConfig};
use anyhow::{bail, Result};

use crate::config::{BACKEND_FAKE, BACKEND_WSL};

/// Build the Backend named by configuration, with an optional script.
///
/// The fake reads its script from configuration rather than the environment
/// whenever one is supplied, so two daemons in one test process can run
/// different scripts without racing on a process-wide variable.
///
/// The WSL adapter reads its own knobs from the environment (`AGENTIFY_WSL_*`)
/// but takes the data root from the daemon, so a test's temporary root really
/// is where its rootfs cache, golden template, and distro disks live.
pub fn select(name: &str, script: Option<&str>, data_root: &Path) -> Result<Arc<dyn Backend>> {
    match name.trim() {
        BACKEND_FAKE => {
            let script = match script {
                Some(raw) => FakeScript::from_json(raw).map_err(|err| {
                    anyhow::anyhow!("the fake Backend script is not usable: {err}")
                })?,
                None => FakeScript::default(),
            };
            Ok(Arc::new(FakeBackend::new(script)))
        }
        BACKEND_WSL => {
            let mut config = WslConfig::from_env();
            config.set_data_root(data_root);
            if let Some(raw) = script {
                // A script has no meaning for WSL; refusing is clearer than
                // quietly ignoring a knob the caller thought mattered.
                bail!("AGENTIFY_FAKE_SCRIPT does not apply to the {BACKEND_WSL} Backend: {raw}");
            }
            if !cfg!(windows) {
                bail!("AGENTIFY_BACKEND={BACKEND_WSL} needs Windows; this build cannot drive WSL");
            }
            Ok(Arc::new(WslBackend::new(config)))
        }
        other => bail!(
            "AGENTIFY_BACKEND={other} is not a Backend this build knows; \
             v0 ships `fake` (the default) and `wsl`"
        ),
    }
}


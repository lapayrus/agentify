//! Which Backend the daemon drives.
//!
//! Selected with `AGENTIFY_BACKEND`; the fake is the default until ticket 07
//! wires the WSL adapter. A name nothing implements is a startup failure, not a
//! silent fallback: a daemon that pretends to start Environments would be worse
//! than one that refuses to boot.

use std::sync::Arc;

use agentify_backends::{Backend, FakeBackend, FakeScript};
use anyhow::{bail, Result};

use crate::config::BACKEND_FAKE;

/// Build the Backend named by configuration, with an optional script.
///
/// The fake reads its script from configuration rather than the environment
/// whenever one is supplied, so two daemons in one test process can run
/// different scripts without racing on a process-wide variable.
pub fn select(name: &str, script: Option<&str>) -> Result<Arc<dyn Backend>> {
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
        other => bail!(
            "AGENTIFY_BACKEND={other} is not a Backend this build knows; \
             v0 ships `fake` (the WSL adapter arrives in a later ticket)"
        ),
    }
}

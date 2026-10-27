//! Fakes for the three ports the bootstrap touches.
//!
//! Only compiled for tests. Each records what was asked of it, which is how the
//! suite proves that "already ready" does nothing: no elevation, no registry
//! write, no second install.

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::Result;

use crate::detect::{Detection, Host};
use crate::elevate::{ElevatedCommand, ElevationOutcome, Elevator};
use crate::resume::ResumeStore;

/// A machine whose WSL state, restart signal, and versions a test chooses.
#[derive(Debug, Clone)]
pub struct FakeHost {
    /// What `detect()` answers.
    pub detection: Detection,
    /// True when Windows says a restart is pending.
    pub reboot: bool,
    /// The versions the machine reports.
    pub observed: agentify_proto::versions::Observed,
}

impl FakeHost {
    pub fn ready() -> Self {
        Self {
            detection: crate::detect::builder(true, Some(2), crate::detect::ubuntu_like()),
            reboot: false,
            observed: crate::observed_for_this_build(),
        }
    }

    pub fn without_wsl() -> Self {
        Self {
            detection: Detection {
                available: false,
                error: Some("wsl.exe could not be run".to_string()),
                ..Detection::default()
            },
            reboot: false,
            observed: crate::observed_for_this_build(),
        }
    }
}

impl Host for FakeHost {
    fn detect(&self) -> Detection {
        self.detection.clone()
    }

    fn reboot_pending(&self) -> bool {
        self.reboot
    }

    fn versions(&self) -> agentify_proto::versions::Observed {
        self.observed.clone()
    }
}

/// An elevator that records the command instead of launching it.
pub struct FakeElevator {
    /// Every command that was asked for, in order.
    pub calls: Mutex<Vec<ElevatedCommand>>,
    /// What the fake run answers.
    pub outcome: Mutex<ElevationOutcome>,
}

impl FakeElevator {
    pub fn new(exit_code: i32) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcome: Mutex::new(ElevationOutcome {
                exit_code,
                stdout: String::new(),
                stderr: String::new(),
            }),
        }
    }

    /// A prompt the human answered No to.
    pub fn declined() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcome: Mutex::new(ElevationOutcome {
                exit_code: 1,
                stdout: String::new(),
                stderr: "Start-Process : This command cannot be run due to the error: \
                         The operation was canceled by the user."
                    .to_string(),
            }),
        }
    }

    pub fn calls(&self) -> Vec<ElevatedCommand> {
        self.calls.lock().expect("lock").clone()
    }
}

impl Elevator for FakeElevator {
    fn run(&self, command: &ElevatedCommand) -> Result<ElevationOutcome> {
        self.calls.lock().expect("lock").push(command.clone());
        Ok(self.outcome.lock().expect("lock").clone())
    }
}

/// An in-memory `RunOnce`, so the suite never leaves a registry entry behind.
#[derive(Default)]
pub struct FakeRunOnce {
    pub values: Mutex<BTreeMap<String, String>>,
    pub scheduled: Mutex<usize>,
    pub cleared: Mutex<usize>,
    pub fail_clear: Mutex<bool>,
}

impl FakeRunOnce {
    pub fn with(value_name: &str, command: &str) -> Self {
        let fake = Self::default();
        fake.values
            .lock()
            .expect("lock")
            .insert(value_name.to_string(), command.to_string());
        fake
    }

    pub fn count(&self) -> usize {
        self.values.lock().expect("lock").len()
    }

    pub fn get(&self, value_name: &str) -> Option<String> {
        self.values.lock().expect("lock").get(value_name).cloned()
    }

    pub fn scheduled(&self) -> usize {
        *self.scheduled.lock().expect("lock")
    }

    pub fn cleared(&self) -> usize {
        *self.cleared.lock().expect("lock")
    }
}

impl ResumeStore for FakeRunOnce {
    fn schedule(&self, value_name: &str, command: &str) -> Result<()> {
        self.values
            .lock()
            .expect("lock")
            .insert(value_name.to_string(), command.to_string());
        *self.scheduled.lock().expect("lock") += 1;
        Ok(())
    }

    fn pending(&self, value_name: &str) -> Result<Option<String>> {
        Ok(self.values.lock().expect("lock").get(value_name).cloned())
    }

    fn clear(&self, value_name: &str) -> Result<()> {
        *self.cleared.lock().expect("lock") += 1;
        if *self.fail_clear.lock().expect("lock") {
            anyhow::bail!("the registry refused the delete");
        }
        self.values.lock().expect("lock").remove(value_name);
        Ok(())
    }
}

/// A fake the test keeps a handle on after handing it to the bootstrap.
pub struct Shared<T>(pub std::sync::Arc<T>);

impl Elevator for Shared<FakeElevator> {
    fn run(&self, command: &ElevatedCommand) -> Result<ElevationOutcome> {
        self.0.run(command)
    }
}

impl ResumeStore for Shared<FakeRunOnce> {
    fn schedule(&self, value_name: &str, command: &str) -> Result<()> {
        self.0.schedule(value_name, command)
    }

    fn pending(&self, value_name: &str) -> Result<Option<String>> {
        self.0.pending(value_name)
    }

    fn clear(&self, value_name: &str) -> Result<()> {
        self.0.clear(value_name)
    }
}

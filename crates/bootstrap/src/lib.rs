//! First run on a clean Windows machine: detect WSL, ask a human, elevate once,
//! and survive the restart Windows insists on.
//!
//! The whole flow is one explicit state machine — [`BootstrapState`] — and the
//! only way to move between its states is [`transition`], which refuses every
//! edge that would let the machine install something a human did not agree to.
//! In particular `Installing` is only reachable from `ConsentRequired`, and
//! `ConsentRequired` is only left by a [`Consent`] that a human action produced.
//!
//! ```text
//!                probe: usable                 verify pins
//!   Missing ───────────────────► Ready ◄──────────────────────┐
//!      │                            ▲                         │
//!      │ probe: one step needed     │ installed, no restart   │
//!      ▼                            │                         │
//!   ConsentRequired ──► Installing ─┤                         │
//!      ▲                    │       └──► Resuming ────────────┘
//!      │                    │ windows needs a restart
//!      │                    ▼
//!      │            RebootRequired ──(RunOnce relaunch)──► Resuming
//!      │                    │
//!      └──── retry ── Failed ◄┘
//! ```
//!
//! Three things are injected, so tests never install WSL, prompt for UAC, leave a
//! registry entry behind, or reboot:
//!
//! - [`Host`] — read-only detection, the restart signal, and the version probe.
//! - [`Elevator`] — the one elevated command, which a fake records instead of
//!   launching.
//! - [`ResumeStore`] — the per-user `RunOnce` entry, which a fake keeps in memory.
//!
//! The production wiring is [`Bootstrap::system`]; nothing else in this crate
//! decides anything about the machine.
//!
//! An Agent can ask for [`Bootstrap::status`] — it is a read — and cannot
//! consent: [`Consent`] has no `Default`, no `Deserialize`, and no constructor
//! that takes a report or a token. The only two producers are the desktop app's
//! setup card and `agentify bootstrap --consent`, both of which are a human
//! clicking or typing.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use agentify_proto::versions::{self, Observed, VersionReport};

mod detect;
mod elevate;
mod resume;

#[cfg(test)]
mod fakes;

pub use detect::{
    decode_output, parse_distro_list, parse_status, parse_wsl_version, preferred_distro,
    Detection, Distro, Host, Status, SystemHost, Virtualization,
};
pub use elevate::{plan, ElevatedCommand, ElevationOutcome, Elevator, Plan, UacElevator};
pub use resume::{
    resume_command, ResumeStore, RunOnce, RESUME_FLAG, RESUME_VALUE_NAME, RUN_KEY,
};

/// What this build expects of itself and what it can know without a rootfs: the
/// starting point for the handshake that runs once the machine is ready.
///
/// The rootfs is absent here on purpose — a machine that has never downloaded one
/// has nothing to compare, and the adapter verifies the digest again before it
/// imports anything.
pub fn observed_for_this_build() -> Observed {
    Observed {
        api: Some(agentify_proto::API_VERSION.to_string()),
        daemon: Some(versions::DAEMON_VERSION.to_string()),
        mcp: Some(versions::MCP_VERSION.to_string()),
        caddy: None,
        adapter: Some("wsl".to_string()),
        adapter_version: Some(versions::WSL_ADAPTER_VERSION.to_string()),
        rootfs_sha256: None,
        images: Vec::new(),
    }
}

/// Where a first run is, exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapState {
    /// Nothing has been decided yet: the machine is being probed.
    Missing,
    /// A human has to agree before anything is installed or enabled.
    ConsentRequired,
    /// The one elevated step is running, or was just agreed to.
    Installing,
    /// Windows has to restart before WSL can work. The resume entry is in place.
    RebootRequired,
    /// Picking the work back up after the restart.
    Resuming,
    /// WSL 2 works and every pinned version matches. Nothing left to do.
    Ready,
    /// Something went wrong, with a sentence saying what.
    Failed,
}

impl BootstrapState {
    /// Every state, in the order a first run walks them.
    pub const ALL: [BootstrapState; 7] = [
        BootstrapState::Missing,
        BootstrapState::ConsentRequired,
        BootstrapState::Installing,
        BootstrapState::RebootRequired,
        BootstrapState::Resuming,
        BootstrapState::Ready,
        BootstrapState::Failed,
    ];

    /// The machine-readable name, as the API and the banner use it.
    pub fn as_str(&self) -> &'static str {
        match self {
            BootstrapState::Missing => "missing",
            BootstrapState::ConsentRequired => "consent_required",
            BootstrapState::Installing => "installing",
            BootstrapState::RebootRequired => "reboot_required",
            BootstrapState::Resuming => "resuming",
            BootstrapState::Ready => "ready",
            BootstrapState::Failed => "failed",
        }
    }

    /// What a person reads on the setup card.
    pub fn label(&self) -> &'static str {
        match self {
            BootstrapState::Missing => "checking this machine",
            BootstrapState::ConsentRequired => "needs your consent",
            BootstrapState::Installing => "installing WSL",
            BootstrapState::RebootRequired => "reboot required",
            BootstrapState::Resuming => "resuming setup",
            BootstrapState::Ready => "ready",
            BootstrapState::Failed => "setup failed",
        }
    }
}

impl fmt::Display for BootstrapState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A move the state machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    pub from: BootstrapState,
    pub to: BootstrapState,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the bootstrap cannot move from {} to {}",
            self.from, self.to
        )
    }
}

impl std::error::Error for TransitionError {}

/// The one function that decides whether a move is allowed.
///
/// The shape of this table is the consent rule: `Installing` has exactly one
/// entry, from `ConsentRequired`, so no sequence of states can install anything
/// before a human has agreed. Every state can re-report itself — asking a second
/// time is not an action — and `Ready` cannot leave, because a ready machine is
/// finished.
pub fn transition(
    from: BootstrapState,
    to: BootstrapState,
) -> Result<BootstrapState, TransitionError> {
    use BootstrapState::*;
    let allowed = matches!(
        (from, to),
        // A probe can conclude anything: a machine that turns out to be ready, a
        // machine that needs one agreed step, a resume left over from before an
        // update, or a machine that cannot run WSL at all.
        (Missing, Missing)
            | (Missing, ConsentRequired)
            | (Missing, Resuming)
            | (Missing, Ready)
            | (Missing, Failed)
            // Asking again is not consenting; the only way out (besides a probe
            // that finds the machine changed) is `Installing`.
            | (ConsentRequired, ConsentRequired)
            | (ConsentRequired, Missing)
            | (ConsentRequired, Installing)
            | (ConsentRequired, Ready)
            | (ConsentRequired, Failed)
            // The install ends in a restart, in an immediate resume, or in a
            // failure. Re-entering `Installing` while it runs is a re-read.
            | (Installing, Installing)
            | (Installing, RebootRequired)
            | (Installing, Resuming)
            | (Installing, Ready)
            | (Installing, Failed)
            // Saying "reboot required" again is how a second run avoids looping.
            | (RebootRequired, RebootRequired)
            | (RebootRequired, Resuming)
            | (RebootRequired, Ready)
            | (RebootRequired, Failed)
            | (Resuming, Resuming)
            | (Resuming, RebootRequired)
            | (Resuming, Ready)
            | (Resuming, Failed)
            | (Ready, Ready)
            // A failure offers one way forward: try again, with consent.
            | (Failed, Failed)
            | (Failed, ConsentRequired)
    );
    if allowed {
        Ok(to)
    } else {
        Err(TransitionError { from, to })
    }
}

/// Tracks a run's position and refuses an illegal move as it happens.
struct Progress {
    state: BootstrapState,
}

impl Progress {
    fn start() -> Self {
        Self {
            state: BootstrapState::Missing,
        }
    }

    fn to(&mut self, next: BootstrapState) -> Result<(), TransitionError> {
        self.state = transition(self.state, next)?;
        Ok(())
    }
}

/// A human's agreement to the one elevated step.
///
/// The field is private and there is no `Default`, no `Deserialize`, and no
/// conversion from a [`Report`]: an Agent can read the state and cannot mint one
/// of these. The two producers in the product are the desktop app's setup card
/// (a click) and `agentify bootstrap --consent` (a person typing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    who: String,
}

impl Consent {
    /// Record that a human agreed. `who` names the surface, so a failure can say
    /// who was asked.
    pub fn granted_by_human(who: impl Into<String>) -> Self {
        Self { who: who.into() }
    }

    /// The surface that produced the agreement: `ui`, `cli`, …
    pub fn who(&self) -> &str {
        &self.who
    }
}

/// What a first run found and what it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub state: BootstrapState,
    /// One sentence a person can act on.
    pub message: String,
    /// What detection found, so a banner can be specific.
    pub detection: Detection,
    /// The handshake, once the machine was ready enough to run it.
    pub versions: Option<VersionReport>,
    /// The failing version rows, when the handshake refused this build.
    pub version_mismatch: Option<String>,
    /// True when Windows has to restart before WSL works.
    pub reboot_required: bool,
    /// True when a per-user `RunOnce` resume is (or was just) in place.
    pub resume_scheduled: bool,
    /// The exact elevated command a consent would run, while one is still needed.
    pub elevated_step: Option<String>,
}

impl Report {
    /// True when nothing is left to do.
    pub fn ready(&self) -> bool {
        self.state == BootstrapState::Ready
    }

    /// True when a human is being asked to agree to something.
    pub fn wants_consent(&self) -> bool {
        self.state == BootstrapState::ConsentRequired
    }
}

/// A bootstrap whose ports are supplied by whoever builds it: the real
/// machine in production, fakes in the tests.
pub struct Bootstrap {
    host: Box<dyn Host>,
    elevator: Box<dyn Elevator>,
    resume: Box<dyn ResumeStore>,
    resume_value: String,
    resume_command: String,
}

impl Bootstrap {
    /// The real thing: `wsl.exe`, a UAC prompt, and `HKCU\…\RunOnce`.
    ///
    /// `program` and `resume_args` are the resume command line for this front
    /// end — `("agentify.exe", ["bootstrap", "--resume"])` from the CLI,
    /// `("Agentify.exe", ["--resume"])` from the desktop app.
    pub fn system(
        data_root: impl Into<PathBuf>,
        program: &Path,
        resume_args: &[&str],
    ) -> Self {
        Self {
            host: Box::new(SystemHost::new(data_root)),
            elevator: Box::new(UacElevator),
            resume: Box::new(RunOnce),
            resume_value: RESUME_VALUE_NAME.to_string(),
            resume_command: resume_command(program, resume_args),
        }
    }

    /// Every side effect injected. This is what the tests drive.
    pub fn injected(
        host: Box<dyn Host>,
        elevator: Box<dyn Elevator>,
        resume: Box<dyn ResumeStore>,
    ) -> Self {
        Self {
            host,
            elevator,
            resume,
            resume_value: RESUME_VALUE_NAME.to_string(),
            resume_command: r#""C:\Agentify\agentify.exe" bootstrap --resume"#.to_string(),
        }
    }

    /// Report where this machine is, without changing anything: no elevation, no
    /// registry write, no install. An Agent may call this.
    pub fn status(&self) -> Report {
        let detection = self.host.detect();
        let resume_scheduled = self.pending_resume();
        let mut progress = Progress::start();

        if detection.usable() {
            return self.settle(&mut progress, detection, resume_scheduled);
        }
        match elevate::plan(&detection) {
            Some(plan) => {
                if let Err(error) = progress.to(BootstrapState::ConsentRequired) {
                    return self.broken(detection, error);
                }
                Report {
                    state: BootstrapState::ConsentRequired,
                    message: consent_message(&plan),
                    detection,
                    versions: None,
                    version_mismatch: None,
                    reboot_required: plan.reboot_expected,
                    resume_scheduled,
                    elevated_step: Some(plan.step.command_line()),
                }
            }
            None => {
                if let Err(error) = progress.to(BootstrapState::Failed) {
                    return self.broken(detection, error);
                }
                let message = not_usable_message(&detection);
                failed_report(detection, message)
            }
        }
    }

    /// Do what a human just agreed to: at most one elevated step, and then either
    /// a resume or a restart.
    ///
    /// Nothing is installed before `consent` exists — that is the type system's
    /// job here, and the state machine refuses `Installing` from anywhere but
    /// `ConsentRequired`.
    pub fn consent(&self, consent: Consent) -> Report {
        let detection = self.host.detect();
        let mut progress = Progress::start();

        if detection.usable() {
            // Idempotent: a machine that is already set up installs nothing, and
            // a resume entry left over from before an update is cleaned up.
            match self.resume.clear(&self.resume_value) {
                Ok(()) => return self.settle(&mut progress, detection, false),
                Err(err) => {
                    return failed_report(
                        detection,
                        format!(
                            "WSL 2 is ready, but the leftover resume entry could not be \
                             removed: {err:#}"
                        ),
                    )
                }
            }
        }

        let Some(plan) = elevate::plan(&detection) else {
            if let Err(error) = progress.to(BootstrapState::Failed) {
                return self.broken(detection, error);
            }
            let message = not_usable_message(&detection);
            return failed_report(detection, message);
        };

        // `ConsentRequired` then `Installing`: the only path the machine allows
        // into an install.
        if let Err(error) = progress.to(BootstrapState::ConsentRequired) {
            return self.broken(detection, error);
        }
        if let Err(error) = progress.to(BootstrapState::Installing) {
            return self.broken(detection, error);
        }

        let outcome = match self.elevator.run(&plan.step) {
            Ok(outcome) => outcome,
            Err(err) => {
                return failed_report(
                    detection,
                    format!(
                        "the administrator step could not be started: {err:#}. Nothing was \
                         changed; try again when you are ready."
                    ),
                )
            }
        };

        if outcome.declined() {
            return failed_report(
                detection,
                format!(
                    "{} declined the administrator prompt, so nothing was changed. Run the \
                     setup again when you are ready.",
                    consent.who()
                ),
            );
        }
        if !outcome.succeeded() && !outcome.reboot_required() {
            let mut report = failed_report(detection, outcome.failure());
            report.elevated_step = Some(plan.step.command_line());
            return report;
        }

        let after = self.host.detect();
        if after.usable() {
            // Completed without a restart: resume immediately, in this process.
            if let Err(error) = progress.to(BootstrapState::Resuming) {
                return self.broken(after, error);
            }
            return self.finish_resume(progress, after);
        }

        let reboot = outcome.reboot_required() || self.host.reboot_pending();
        if !reboot {
            if let Err(error) = progress.to(BootstrapState::Failed) {
                return self.broken(after, error);
            }
            let message = format!(
                "the administrator step ran, but WSL 2 still is not usable: {}. Run \
                 `agentify bootstrap --consent` to try again.",
                after.summary()
            );
            let mut report = failed_report(after, message);
            report.elevated_step = Some(plan.step.command_line());
            return report;
        }

        // Windows needs to restart. Record exactly one resume and say so; do not
        // press on and do not loop.
        match self
            .resume
            .schedule(&self.resume_value, &self.resume_command)
        {
            Ok(()) => {
                if let Err(error) = progress.to(BootstrapState::RebootRequired) {
                    return self.broken(after, error);
                }
                Report {
                    state: BootstrapState::RebootRequired,
                    message: reboot_message(),
                    detection: after,
                    versions: None,
                    version_mismatch: None,
                    reboot_required: true,
                    resume_scheduled: true,
                    elevated_step: None,
                }
            }
            Err(err) => failed_report(
                after,
                format!(
                    "WSL was installed, but the per-user resume entry could not be written: \
                     {err:#}. Open Agentify again after the restart to finish setup."
                ),
            ),
        }
    }

    /// The `RunOnce` path: the app was relaunched with the resume flag.
    ///
    /// Never elevates. If the machine is ready it cleans up and finishes; if the
    /// restart has not happened yet it says so again and keeps exactly one resume
    /// entry; if the restart happened and WSL still does not work it stops with a
    /// sentence instead of scheduling itself forever.
    pub fn resume(&self) -> Report {
        let detection = self.host.detect();
        let mut progress = Progress::start();
        if let Err(error) = progress.to(BootstrapState::Resuming) {
            return self.broken(detection, error);
        }

        if detection.usable() {
            return self.finish_resume(progress, detection);
        }

        if self.host.reboot_pending() {
            let scheduled = self
                .resume
                .schedule(&self.resume_value, &self.resume_command)
                .is_ok();
            if let Err(error) = progress.to(BootstrapState::RebootRequired) {
                return self.broken(detection, error);
            }
            return Report {
                state: BootstrapState::RebootRequired,
                message: reboot_message(),
                detection,
                versions: None,
                version_mismatch: None,
                reboot_required: true,
                resume_scheduled: scheduled,
                elevated_step: None,
            };
        }

        if let Err(error) = progress.to(BootstrapState::Failed) {
            return self.broken(detection, error);
        }
        let message = format!(
            "WSL 2 is still not usable after the restart: {}. Run `agentify bootstrap \
             --consent` to try again; Agentify will not relaunch itself a second time.",
            detection.summary()
        );
        failed_report(detection, message)
    }

    /// Remove the resume entry (an entry Windows already consumed is fine) and
    /// finish by verifying the pins.
    fn finish_resume(&self, mut progress: Progress, detection: Detection) -> Report {
        if let Err(err) = self.resume.clear(&self.resume_value) {
            if let Err(error) = progress.to(BootstrapState::Failed) {
                return self.broken(detection, error);
            }
            return failed_report(
                detection,
                format!("the resume entry could not be removed: {err:#}"),
            );
        }
        self.settle(&mut progress, detection, false)
    }

    /// The last hop: verify the pinned versions first, and only then say Ready.
    ///
    /// A `VersionMismatch` is a `Failed` report, never a ready one, because a
    /// machine whose daemon, API, MCP shim, or adapter disagrees with this build
    /// cannot run a Project.
    fn settle(
        &self,
        progress: &mut Progress,
        detection: Detection,
        resume_scheduled: bool,
    ) -> Report {
        let versions = versions::evaluate(&self.host.versions());
        if !versions.ok {
            let summary = versions.summary();
            if let Err(error) = progress.to(BootstrapState::Failed) {
                return self.broken(detection, error);
            }
            return Report {
                state: BootstrapState::Failed,
                message: format!(
                    "{summary}. Reinstall Agentify so the app, the daemon, the MCP shim, \
                     Caddy, and the Backend adapters come from one build; nothing was started."
                ),
                detection,
                versions: Some(versions),
                version_mismatch: Some(summary),
                reboot_required: false,
                resume_scheduled,
                elevated_step: None,
            };
        }
        if let Err(error) = progress.to(BootstrapState::Ready) {
            return self.broken(detection, error);
        }
        Report {
            state: BootstrapState::Ready,
            message: ready_message(&detection),
            detection,
            versions: Some(versions),
            version_mismatch: None,
            reboot_required: false,
            resume_scheduled,
            elevated_step: None,
        }
    }

    fn pending_resume(&self) -> bool {
        matches!(self.resume.pending(&self.resume_value), Ok(Some(_)))
    }

    fn broken(&self, detection: Detection, error: TransitionError) -> Report {
        failed_report(
            detection,
            format!("the bootstrap refused its own next state: {error}"),
        )
    }
}

fn failed_report(detection: Detection, message: String) -> Report {
    Report {
        state: BootstrapState::Failed,
        message,
        detection,
        versions: None,
        version_mismatch: None,
        reboot_required: false,
        resume_scheduled: false,
        elevated_step: None,
    }
}

fn consent_message(plan: &Plan) -> String {
    let reboot = if plan.reboot_expected {
        " Windows will need to restart afterwards, and Agentify picks the work up again at \
         your next sign-in."
    } else {
        ""
    };
    format!(
        "Agentify needs one administrator step: {} — {}.{reboot} Nothing has been changed yet: \
         a human has to agree, in the app's setup card or with `agentify bootstrap --consent`.",
        plan.step.command_line(),
        plan.reason
    )
}

fn reboot_message() -> String {
    "Reboot required: WSL 2 is installed but Windows has to restart before it can run. \
     Agentify resumes by itself when you sign back in; nothing else is needed."
        .to_string()
}

fn ready_message(detection: &Detection) -> String {
    match detection.wsl2_distro() {
        Some(distro) => format!(
            "WSL 2 is ready ({}). Every pinned version matches; there is nothing to install.",
            distro.name
        ),
        None => "WSL 2 is ready and no distribution is installed yet: the pinned Ubuntu \
                 rootfs is imported when the first Project starts. Every pinned version \
                 matches."
            .to_string(),
    }
}

fn not_usable_message(detection: &Detection) -> String {
    let detail = detection
        .virtualization
        .as_ref()
        .map(|virtualization| virtualization.detail.clone())
        .unwrap_or_default();
    let tail = if detail.is_empty() {
        String::new()
    } else {
        format!(" {detail}.")
    };
    format!(
        "WSL 2 cannot be used on this machine: {}.{tail} Agentify cannot go further on this \
         machine.",
        detection.summary()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeElevator, FakeHost, FakeRunOnce};

    fn ready_machine() -> Bootstrap {
        Bootstrap::injected(
            Box::new(FakeHost::ready()),
            Box::new(FakeElevator::new(0)),
            Box::new(FakeRunOnce::default()),
        )
    }

    fn bare_machine(elevator: FakeElevator, resume: FakeRunOnce) -> Bootstrap {
        Bootstrap::injected(
            Box::new(FakeHost::without_wsl()),
            Box::new(elevator),
            Box::new(resume),
        )
    }

    // ---- the state machine ------------------------------------------------

    /// The legal moves, written out rather than derived from `transition`, so this
    /// test can fail when the table changes by accident.
    fn expected(from: BootstrapState, to: BootstrapState) -> bool {
        use BootstrapState::*;
        let row = match from {
            Missing => &[true, true, false, false, true, true, true][..],
            ConsentRequired => &[true, true, true, false, false, true, true][..],
            Installing => &[false, false, true, true, true, true, true][..],
            RebootRequired => &[false, false, false, true, true, true, true][..],
            Resuming => &[false, false, false, true, true, true, true][..],
            Ready => &[false, false, false, false, false, true, false][..],
            Failed => &[false, true, false, false, false, false, true][..],
        };
        let column = BootstrapState::ALL
            .iter()
            .position(|state| *state == to)
            .expect("a known state");
        row[column]
    }

    #[test]
    fn every_pair_of_states_is_explicitly_allowed_or_refused() {
        let mut legal = 0;
        for from in BootstrapState::ALL {
            for to in BootstrapState::ALL {
                let result = transition(from, to);
                let want = expected(from, to);
                assert_eq!(
                    result.is_ok(),
                    want,
                    "{} -> {} should be {}",
                    from,
                    to,
                    if want { "legal" } else { "refused" }
                );
                if want {
                    legal += 1;
                    assert_eq!(result.expect("legal"), to);
                } else {
                    let error = result.expect_err("refused");
                    assert_eq!(error.from, from);
                    assert_eq!(error.to, to);
                    assert!(
                        error.to_string().contains(from.as_str()),
                        "{error} names where it came from"
                    );
                }
            }
        }
        assert_eq!(legal, 26, "the table has {legal} legal edges");
    }

    #[test]
    fn every_state_can_re_report_itself() {
        for state in BootstrapState::ALL {
            assert_eq!(
                transition(state, state),
                Ok(state),
                "{state} asked twice is not an action"
            );
        }
    }

    #[test]
    fn installing_is_reachable_only_through_consent() {
        for state in BootstrapState::ALL {
            let allowed = transition(state, BootstrapState::Installing).is_ok();
            assert_eq!(
                allowed,
                state == BootstrapState::ConsentRequired || state == BootstrapState::Installing,
                "only a consent may lead to an install, not {state}"
            );
        }
    }

    #[test]
    fn a_ready_machine_cannot_go_backwards() {
        for target in BootstrapState::ALL {
            let allowed = transition(BootstrapState::Ready, target).is_ok();
            assert_eq!(
                allowed,
                target == BootstrapState::Ready,
                "ready is final, but it tried to reach {target}"
            );
        }
    }

    #[test]
    fn the_states_have_stable_names_and_labels() {
        assert_eq!(BootstrapState::Missing.as_str(), "missing");
        assert_eq!(BootstrapState::ConsentRequired.as_str(), "consent_required");
        assert_eq!(BootstrapState::Installing.as_str(), "installing");
        assert_eq!(BootstrapState::RebootRequired.as_str(), "reboot_required");
        assert_eq!(BootstrapState::Resuming.as_str(), "resuming");
        assert_eq!(BootstrapState::Ready.as_str(), "ready");
        assert_eq!(BootstrapState::Failed.as_str(), "failed");
        for state in BootstrapState::ALL {
            assert!(!state.label().is_empty());
            assert_eq!(state.to_string(), state.as_str());
        }
        assert_eq!(
            serde_json::to_string(&BootstrapState::RebootRequired).expect("serialises"),
            "\"reboot_required\""
        );
    }

    // ---- status: a read, never an action ----------------------------------

    #[test]
    fn a_machine_that_already_has_wsl2_is_ready_and_does_nothing() {
        let bootstrap = ready_machine();
        let report = bootstrap.status();
        assert_eq!(report.state, BootstrapState::Ready, "{report:?}");
        assert!(report.ready());
        assert!(report.message.contains("matches"), "{}", report.message);
        assert_eq!(report.elevated_step, None, "nothing is left to run");
        assert!(!report.reboot_required);
        assert!(!report.wants_consent());
        assert!(
            report
                .versions
                .as_ref()
                .is_some_and(|versions| versions.ok),
            "{:?}",
            report.versions
        );
        assert_eq!(report.detection.default_version, Some(2));
    }

    #[test]
    fn status_never_elevates_and_never_writes_a_resume_entry() {
        let elevator = FakeElevator::new(0);
        let resume = FakeRunOnce::default();
        let report = bare_machine(elevator, resume).status();
        assert_eq!(report.state, BootstrapState::ConsentRequired);
        // The report says what a consent would do, and nothing was done.
        assert_eq!(
            report.elevated_step.as_deref(),
            Some("wsl.exe --install --no-distribution")
        );
        assert!(report.message.contains("Nothing has been changed yet"));
        assert!(report.message.contains("agentify bootstrap --consent"));
    }

    #[test]
    fn a_machine_without_wsl_asks_for_consent_and_names_the_one_step() {
        let bootstrap = bare_machine(FakeElevator::new(0), FakeRunOnce::default());
        let report = bootstrap.status();
        assert_eq!(report.state, BootstrapState::ConsentRequired);
        assert!(report.wants_consent());
        assert!(report.reboot_required);
        assert!(report.message.contains("wsl.exe --install --no-distribution"));
        assert!(report.message.contains("administrator"));
        assert_eq!(report.versions, None, "the pins are checked once it is ready");
    }

    #[test]
    fn an_agent_may_read_the_status_and_has_nothing_to_consent_with() {
        // The read is a `status()` call; consent is a type only a human surface
        // can produce, and `who` records which one.
        let consent = Consent::granted_by_human("ui");
        assert_eq!(consent.who(), "ui");
        let cli = Consent::granted_by_human("cli --consent");
        assert_ne!(consent, cli);
    }

    // ---- consent and the one elevated step --------------------------------

    #[test]
    fn consent_runs_exactly_one_elevated_step() {
        let elevator = std::sync::Arc::new(FakeElevator::new(0));
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::without_wsl()),
            Box::new(crate::fakes::Shared(elevator.clone())),
            Box::new(FakeRunOnce::default()),
        );
        // The fake machine never changes, so this run installs and then finds
        // WSL still missing: what matters is that it ran the step once.
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::Failed, "{report:?}");
        assert_eq!(
            report.elevated_step.as_deref(),
            Some("wsl.exe --install --no-distribution")
        );
        assert!(report.message.contains("still is not usable"), "{}", report.message);
        let calls = elevator.calls();
        assert_eq!(calls.len(), 1, "one prompt, one command: {calls:?}");
        assert_eq!(calls[0], ElevatedCommand::install_wsl());
        assert_eq!(
            calls[0].command_line(),
            "wsl.exe --install --no-distribution",
            "and no distribution is installed alongside it"
        );
    }

    #[test]
    fn a_ready_machine_consents_to_nothing() {
        let bootstrap = ready_machine();
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::Ready);
        assert!(report.message.contains("matches"), "{}", report.message);
    }

    #[test]
    fn a_ready_machine_clears_a_stale_resume_entry() {
        let resume = FakeRunOnce::with(RESUME_VALUE_NAME, "stale");
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::ready()),
            Box::new(FakeElevator::new(0)),
            Box::new(resume),
        );
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::Ready);
        assert!(!report.resume_scheduled, "no resume survives a ready machine");
    }

    #[test]
    fn a_declined_prompt_changes_nothing_and_says_so() {
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::without_wsl()),
            Box::new(FakeElevator::declined()),
            Box::new(FakeRunOnce::default()),
        );
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::Failed);
        assert!(report.message.contains("declined the administrator prompt"), "{}", report.message);
        assert!(report.message.contains("nothing was changed"), "{}", report.message);
        assert!(report.message.contains("ui"), "the report names who was asked");
        assert!(!report.reboot_required);
    }

    // ---- reboot and resume ------------------------------------------------

    /// A host that has no WSL until the elevated step runs, then has WSL 2 with
    /// Ubuntu: the install-completes-without-a-restart path.
    struct InstallingHost {
        installed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reboot: bool,
    }

    impl Host for InstallingHost {
        fn detect(&self) -> Detection {
            if self.installed.load(std::sync::atomic::Ordering::SeqCst) {
                FakeHost::ready().detection
            } else {
                FakeHost::without_wsl().detection
            }
        }

        fn reboot_pending(&self) -> bool {
            self.reboot
        }

        fn versions(&self) -> Observed {
            observed_for_this_build()
        }
    }

    struct InstallingElevator(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Elevator for InstallingElevator {
        fn run(&self, _command: &ElevatedCommand) -> anyhow::Result<ElevationOutcome> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ElevationOutcome {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// The install enables the components and Windows asks for a restart.
    struct RestartHost {
        reboot: bool,
    }

    impl Host for RestartHost {
        fn detect(&self) -> Detection {
            Detection {
                available: true,
                wsl_version: Some("2.6.0.0".to_string()),
                default_version: Some(1),
                error: None,
                ..Detection::default()
            }
        }

        fn reboot_pending(&self) -> bool {
            self.reboot
        }

        fn versions(&self) -> Observed {
            observed_for_this_build()
        }
    }

    /// An elevated step that reports `3010`: done, and a restart will finish it.
    struct RestartingElevator;

    impl Elevator for RestartingElevator {
        fn run(&self, _command: &ElevatedCommand) -> anyhow::Result<ElevationOutcome> {
            Ok(ElevationOutcome {
                exit_code: 3010,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn an_install_that_needs_no_restart_resumes_immediately() {
        let installed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume = FakeRunOnce::default();
        let bootstrap = Bootstrap::injected(
            Box::new(InstallingHost {
                installed: installed.clone(),
                reboot: false,
            }),
            Box::new(InstallingElevator(installed.clone())),
            Box::new(resume),
        );
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::Ready, "{report:?}");
        assert!(!report.reboot_required, "no restart was needed");
        assert!(!report.resume_scheduled, "nothing was scheduled");
        assert!(installed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn an_install_that_needs_a_restart_writes_one_entry_and_stops_there() {
        let resume = std::sync::Arc::new(FakeRunOnce::default());
        let bootstrap = Bootstrap::injected(
            Box::new(RestartHost { reboot: true }),
            Box::new(RestartingElevator),
            Box::new(crate::fakes::Shared(resume.clone())),
        );
        let report = bootstrap.consent(Consent::granted_by_human("ui"));
        assert_eq!(report.state, BootstrapState::RebootRequired, "{report:?}");
        assert!(report.reboot_required);
        assert!(report.resume_scheduled);
        assert!(report.message.contains("Reboot required"), "{}", report.message);
        assert!(report.message.contains("resumes by itself"), "{}", report.message);
        assert_eq!(report.versions, None, "the pins wait for the resume");
        assert_eq!(resume.count(), 1, "exactly one per-user resume entry");
        assert_eq!(resume.scheduled(), 1);
        let entry = resume.get(RESUME_VALUE_NAME).expect("the entry");
        assert!(entry.contains("--resume"), "{entry}");
        assert!(entry.contains("agentify.exe"), "{entry}");
    }

    #[test]
    fn a_resume_after_the_restart_cleans_up_and_finishes() {
        let resume = FakeRunOnce::with(RESUME_VALUE_NAME, "the pending command");
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::ready()),
            Box::new(FakeElevator::new(0)),
            Box::new(resume),
        );
        let report = bootstrap.resume();
        assert_eq!(report.state, BootstrapState::Ready, "{report:?}");
        assert!(!report.resume_scheduled, "the entry is removed on success");
        assert!(report.message.contains("matches"), "{}", report.message);
    }

    #[test]
    fn a_resume_before_the_restart_says_reboot_required_rather_than_looping() {
        let resume = FakeRunOnce::default();
        let bootstrap = Bootstrap::injected(
            Box::new(RestartHost { reboot: true }),
            Box::new(FakeElevator::new(0)),
            Box::new(resume),
        );
        let first = bootstrap.resume();
        assert_eq!(first.state, BootstrapState::RebootRequired, "{first:?}");
        assert!(first.reboot_required);
        assert!(first.resume_scheduled);
        let second = bootstrap.resume();
        assert_eq!(second.state, BootstrapState::RebootRequired);
        assert!(second.message.contains("Reboot required"));
        // Never a second install: the resume path has no elevator.
        assert_eq!(second.elevated_step, None);
    }

    #[test]
    fn a_resume_that_still_does_not_work_stops_instead_of_rescheduling_itself() {
        let resume = FakeRunOnce::with(RESUME_VALUE_NAME, "the pending command");
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::without_wsl()),
            Box::new(FakeElevator::new(0)),
            Box::new(resume),
        );
        let report = bootstrap.resume();
        assert_eq!(report.state, BootstrapState::Failed, "{report:?}");
        assert!(!report.resume_scheduled);
        assert!(report.message.contains("still not usable after the restart"), "{}", report.message);
        assert!(report.message.contains("agentify bootstrap --consent"), "{}", report.message);
    }

    #[test]
    fn a_resume_that_cannot_clear_its_entry_is_a_failure_not_a_silent_leftover() {
        let resume = FakeRunOnce::with(RESUME_VALUE_NAME, "the pending command");
        *resume.fail_clear.lock().expect("lock") = true;
        let bootstrap = Bootstrap::injected(
            Box::new(FakeHost::ready()),
            Box::new(FakeElevator::new(0)),
            Box::new(resume),
        );
        let report = bootstrap.resume();
        assert_eq!(report.state, BootstrapState::Failed, "{report:?}");
        assert!(report.message.contains("could not be removed"), "{}", report.message);
    }

    // ---- the version handshake --------------------------------------------

    #[test]
    fn a_ready_machine_with_a_mismatched_version_is_versionmismatch_not_ready() {
        let mut host = FakeHost::ready();
        host.observed.daemon = Some("9.9.9".to_string());
        let bootstrap = Bootstrap::injected(
            Box::new(host),
            Box::new(FakeElevator::new(0)),
            Box::new(FakeRunOnce::default()),
        );
        let report = bootstrap.status();
        assert_eq!(report.state, BootstrapState::Failed, "{report:?}");
        assert!(
            report.message.starts_with("VersionMismatch: daemon: expected"),
            "{}",
            report.message
        );
        let mismatch = report.version_mismatch.expect("the failing rows");
        assert!(mismatch.contains("daemon"), "{mismatch}");
        let versions = report.versions.expect("the full report");
        assert_eq!(
            versions.first_mismatch().map(|row| row.component.as_str()),
            Some("daemon")
        );
    }

    #[test]
    fn a_mismatched_adapter_major_is_refused_before_a_project_can_start() {
        let mut host = FakeHost::ready();
        host.observed.adapter_version = Some("7.0.0".to_string());
        let bootstrap = Bootstrap::injected(
            Box::new(host),
            Box::new(FakeElevator::new(0)),
            Box::new(FakeRunOnce::default()),
        );
        let report = bootstrap.status();
        assert_eq!(report.state, BootstrapState::Failed);
        assert!(report.message.contains("adapter:wsl"), "{}", report.message);
    }

    #[test]
    fn a_run_that_reached_ready_verifies_the_pins_on_the_way() {
        // The resume path is the one that reaches Ready from a real probe; its
        // version check is the same one `status` uses.
        let mut host = FakeHost::ready();
        host.observed.api = Some("v2".to_string());
        let bootstrap = Bootstrap::injected(
            Box::new(host),
            Box::new(FakeElevator::new(0)),
            Box::new(FakeRunOnce::with(RESUME_VALUE_NAME, "the pending command")),
        );
        let report = bootstrap.resume();
        assert_eq!(report.state, BootstrapState::Failed);
        assert!(report.message.contains("api: expected"), "{}", report.message);
    }

    #[test]
    fn observed_for_this_build_matches_its_own_pins() {
        let report = versions::evaluate(&observed_for_this_build());
        assert!(report.ok, "{:?}", report.components);
        assert_eq!(report.summary(), "every pinned version matches");
    }
}

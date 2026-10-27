//! Per-user trust for the local CA.
//!
//! Caddy's internal CA signs `*.localhost`, so the browser only stops warning
//! once its root is trusted. Two facts shape this module:
//!
//! * on Windows the install is **per-user** (`certutil -addstore -user Root`),
//!   which needs no elevation and is what Chrome and Edge read;
//! * Firefox keeps its own store, so it needs the enterprise-roots bridge —
//!   a machine-wide policy plus, unhappily, elevation.
//!
//! Nothing here runs by default. [`TrustInstaller::install`] is the explicit
//! opt-in path (`AGENTIFY_TRUST=install`, ticket 11's installer); the builders
//! are pure so a test can assert the exact commands without touching the user's
//! certificate store. A refused or absent root is not an error: HTTP then
//! serves the same routes without redirecting.

use std::path::Path;

use crate::routes::HttpMode;

/// The CN the local root carries, so removal can name it.
pub const CA_COMMON_NAME: &str = "Agentify Local Root";

/// The policy key Firefox reads on Windows.
pub const FIREFOX_POLICY_KEY: &str = r"HKLM\Software\Policies\Mozilla\Firefox\Certificates";

/// Whether the local root is trusted on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustState {
    /// Not installed, or removed: HTTP serves the routes.
    #[default]
    Absent,
    /// Installed for the current user: HTTP redirects to HTTPS.
    Installed,
    /// The user declined: HTTP serves the routes, and the daemon stops asking.
    Refused,
}

impl TrustState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustState::Absent => "absent",
            TrustState::Installed => "installed",
            TrustState::Refused => "refused",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "absent" => Some(TrustState::Absent),
            "installed" => Some(TrustState::Installed),
            "refused" => Some(TrustState::Refused),
            _ => None,
        }
    }

    /// Installed trust is the only state in which an HTTP redirect helps; in
    /// every other state HTTP serves the app directly.
    pub fn http_mode(&self) -> HttpMode {
        match self {
            TrustState::Installed => HttpMode::Redirect,
            TrustState::Absent | TrustState::Refused => HttpMode::Serve,
        }
    }

    pub fn trusted(&self) -> bool {
        matches!(self, TrustState::Installed)
    }
}

/// Whether a step needs an elevated process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    NotRequired,
    Required,
}

/// One command: built, described, and — only on the opt-in path — run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStep {
    pub program: String,
    pub args: Vec<String>,
    pub elevation: Elevation,
    pub purpose: String,
}

impl TrustStep {
    fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        elevation: Elevation,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            elevation,
            purpose: purpose.into(),
        }
    }

    /// The command line, for a log, a report, or a "run this yourself" hint.
    pub fn describe(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            if arg.contains(' ') {
                line.push('"');
                line.push_str(arg);
                line.push('"');
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// The commands an install or removal needs, plus what a human must still do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustPrepare {
    pub steps: Vec<TrustStep>,
    /// Sentences a UI can show verbatim: what was skipped and why.
    pub notes: Vec<String>,
}

/// What an install or removal actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustReport {
    pub state: TrustState,
    pub applied: Vec<TrustStep>,
    pub manual: Vec<TrustStep>,
    pub notes: Vec<String>,
}

/// Why trust could not be installed or removed.
#[derive(Debug)]
pub enum TrustError {
    /// A command ran and failed.
    Command {
        step: TrustStep,
        code: Option<i32>,
        detail: String,
    },
    /// This build has no way to install trust.
    Unsupported(String),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::Command { step, code, detail } => write!(
                f,
                "`{}` failed ({}): {detail}",
                step.describe(),
                code.map(|code| code.to_string())
                    .unwrap_or_else(|| "no exit code".to_string())
            ),
            TrustError::Unsupported(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for TrustError {}

/// The seam: how this platform installs and removes the local root.
///
/// Tests substitute a fake, so no automated test can reach the user's
/// certificate store.
pub trait TrustInstaller: Send + Sync {
    /// The commands an install would run.
    fn commands(&self, root_cert: &Path) -> TrustPrepare;

    /// The commands an uninstall would run.
    fn removal_commands(&self) -> TrustPrepare;

    /// Install trust. Called only from an explicit opt-in path.
    fn install(&self, root_cert: &Path) -> Result<TrustReport, TrustError>;

    /// Remove trust, for uninstall.
    fn remove(&self) -> Result<TrustReport, TrustError>;
}

/// Windows: the per-user root store, then (optionally) the Firefox bridge.
///
/// `certutil -addstore -user Root` writes `HKCU\...\SystemCertificates\Root`
/// with no elevation, no UAC prompt, and no security dialogue. The Firefox
/// policy is machine-wide, so it is built but never run unasked: the report
/// hands it back for the installer (or the user) to approve.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsTrustInstaller;

impl WindowsTrustInstaller {
    pub fn new() -> Self {
        Self
    }
}

impl TrustInstaller for WindowsTrustInstaller {
    fn commands(&self, root_cert: &Path) -> TrustPrepare {
        TrustPrepare {
            steps: vec![
                TrustStep::new(
                    "certutil",
                    [
                        "-addstore".to_string(),
                        "-user".to_string(),
                        "Root".to_string(),
                        root_cert.display().to_string(),
                    ],
                    Elevation::NotRequired,
                    format!("trust {CA_COMMON_NAME} for the current user"),
                ),
                TrustStep::new(
                    "reg",
                    [
                        "add".to_string(),
                        FIREFOX_POLICY_KEY.to_string(),
                        "/v".to_string(),
                        "ImportEnterpriseRoots".to_string(),
                        "/t".to_string(),
                        "REG_DWORD".to_string(),
                        "/d".to_string(),
                        "1".to_string(),
                        "/f".to_string(),
                    ],
                    Elevation::Required,
                    "let Firefox trust roots installed in the Windows store".to_string(),
                ),
            ],
            notes: vec![
                "Trusting the root for the current user needs no elevation: it lands in \
                 HKCU\\Root, which Chrome and Edge read."
                    .to_string(),
                "The Firefox enterprise-roots policy is machine-wide and needs an elevated \
                 prompt. Without it, set security.enterprise_roots.enabled=true in about:config."
                    .to_string(),
            ],
        }
    }

    fn removal_commands(&self) -> TrustPrepare {
        TrustPrepare {
            steps: vec![
                TrustStep::new(
                    "certutil",
                    [
                        "-delstore".to_string(),
                        "-user".to_string(),
                        "Root".to_string(),
                        CA_COMMON_NAME.to_string(),
                    ],
                    Elevation::NotRequired,
                    "drop the local root from this user's store".to_string(),
                ),
                TrustStep::new(
                    "reg",
                    [
                        "delete".to_string(),
                        FIREFOX_POLICY_KEY.to_string(),
                        "/v".to_string(),
                        "ImportEnterpriseRoots".to_string(),
                        "/f".to_string(),
                    ],
                    Elevation::Required,
                    "stop pointing Firefox at the Windows store".to_string(),
                ),
            ],
            notes: vec![
                "Removal is best-effort: a root that is already gone is not an error."
                    .to_string(),
            ],
        }
    }

    fn install(&self, root_cert: &Path) -> Result<TrustReport, TrustError> {
        run(self.commands(root_cert), TrustState::Installed, TrustState::Absent)
    }

    fn remove(&self) -> Result<TrustReport, TrustError> {
        run(
            self.removal_commands(),
            TrustState::Absent,
            TrustState::Absent,
        )
    }
}

/// This build cannot install trust: hand the commands over, and serve over HTTP.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManualTrustInstaller;

impl TrustInstaller for ManualTrustInstaller {
    fn commands(&self, _root_cert: &Path) -> TrustPrepare {
        TrustPrepare {
            steps: Vec::new(),
            notes: vec![
                "This build has no automatic trust installation; HTTP keeps serving the same \
                 routes, which is a secure context for *.localhost."
                    .to_string(),
            ],
        }
    }

    fn removal_commands(&self) -> TrustPrepare {
        TrustPrepare::default()
    }

    fn install(&self, _root_cert: &Path) -> Result<TrustReport, TrustError> {
        Ok(TrustReport {
            state: TrustState::Absent,
            applied: Vec::new(),
            manual: Vec::new(),
            notes: self.commands(Path::new("")).notes,
        })
    }

    fn remove(&self) -> Result<TrustReport, TrustError> {
        Ok(TrustReport {
            state: TrustState::Absent,
            applied: Vec::new(),
            manual: Vec::new(),
            notes: Vec::new(),
        })
    }
}

/// Run the steps that need no elevation; hand back the rest.
fn run(
    prepare: TrustPrepare,
    on_success: TrustState,
    on_failure: TrustState,
) -> Result<TrustReport, TrustError> {
    let mut applied = Vec::new();
    let mut manual = Vec::new();
    for step in prepare.steps {
        if step.elevation == Elevation::Required {
            manual.push(step);
            continue;
        }
        let output = std::process::Command::new(&step.program)
            .args(&step.args)
            .output()
            .map_err(|err| TrustError::Command {
                step: step.clone(),
                code: None,
                detail: err.to_string(),
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if detail.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                detail
            };
            // A removal of something that is already gone is a success.
            if on_failure == TrustState::Absent && detail.is_empty() {
                continue;
            }
            return Err(TrustError::Command {
                step,
                code: output.status.code(),
                detail,
            });
        }
        applied.push(step);
    }
    Ok(TrustReport {
        state: on_success,
        applied,
        manual,
        notes: prepare.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(r"C:\Users\dev\AppData\Local\Agentify\ingress\caddy\pki\authorities\local\root.crt")
    }

    #[test]
    fn the_windows_install_is_per_user_and_needs_no_elevation() {
        let prepare = WindowsTrustInstaller::new().commands(&root());
        let certutil = &prepare.steps[0];
        assert_eq!(certutil.program, "certutil");
        assert_eq!(
            certutil.args,
            vec![
                "-addstore".to_string(),
                "-user".to_string(),
                "Root".to_string(),
                root().display().to_string(),
            ],
            "`-user` is what keeps this out of the machine store and out of UAC"
        );
        assert_eq!(certutil.elevation, Elevation::NotRequired);
    }

    #[test]
    fn the_windows_install_also_builds_the_firefox_enterprise_roots_command() {
        let prepare = WindowsTrustInstaller::new().commands(&root());
        let policy = prepare
            .steps
            .iter()
            .find(|step| step.program == "reg")
            .expect("a Firefox policy step");
        assert_eq!(
            policy.args,
            vec![
                "add".to_string(),
                FIREFOX_POLICY_KEY.to_string(),
                "/v".to_string(),
                "ImportEnterpriseRoots".to_string(),
                "/t".to_string(),
                "REG_DWORD".to_string(),
                "/d".to_string(),
                "1".to_string(),
                "/f".to_string(),
            ]
        );
        assert_eq!(
            policy.elevation,
            Elevation::Required,
            "the Firefox bridge is machine-wide; the daemon must not run it unasked"
        );
        assert!(
            prepare.notes.iter().any(|note| note.contains("about:config")),
            "there is a no-elevation alternative and the user should be told: {:?}",
            prepare.notes
        );
    }

    #[test]
    fn removal_names_the_root_and_the_policy_it_installed() {
        let prepare = WindowsTrustInstaller::new().removal_commands();
        assert_eq!(
            prepare.steps[0].args,
            vec![
                "-delstore".to_string(),
                "-user".to_string(),
                "Root".to_string(),
                CA_COMMON_NAME.to_string(),
            ],
            "removal by CN is what an uninstall can do without the certificate file"
        );
        assert_eq!(prepare.steps[1].program, "reg");
        assert_eq!(prepare.steps[1].args[0], "delete");
    }

    #[test]
    fn a_step_describes_itself_with_quoting_so_a_human_can_run_it() {
        let step = WindowsTrustInstaller::new()
            .commands(&PathBuf::from(r"C:\Program Files\Agentify\root.crt"))
            .steps
            .remove(0);
        assert_eq!(
            step.describe(),
            r#"certutil -addstore -user Root "C:\Program Files\Agentify\root.crt""#
        );
    }

    #[test]
    fn trust_states_decide_whether_http_serves_or_redirects() {
        assert_eq!(TrustState::Installed.http_mode(), HttpMode::Redirect);
        assert_eq!(TrustState::Absent.http_mode(), HttpMode::Serve);
        assert_eq!(
            TrustState::Refused.http_mode(),
            HttpMode::Serve,
            "a refusal must not cost the user the app"
        );
        assert!(!TrustState::Refused.trusted());
        assert_eq!(TrustState::parse("refused"), Some(TrustState::Refused));
        assert_eq!(TrustState::parse("nonsense"), None);
    }

    #[test]
    fn a_build_without_trust_installation_serves_over_http_and_says_so() {
        let report = ManualTrustInstaller.install(&root()).expect("no commands to run");
        assert_eq!(report.state, TrustState::Absent);
        assert!(report.applied.is_empty());
        assert!(report
            .notes
            .iter()
            .any(|note| note.contains("HTTP keeps serving")));
    }
}

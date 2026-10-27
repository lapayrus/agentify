//! The human gate.
//!
//! An Approval is decided by a person, not by a token. The daemon asks the OS
//! for an interactive gesture ([`windows`](https://learn.microsoft.com/windows/apps/develop/security/windows-hello)
//! Hello on Windows) and only a `Verified` answer lets a destructive operation
//! run. An Agent holds the bearer token but cannot produce that gesture, so
//! calling the endpoint is not enough.
//!
//! The verifier is an injected seam: tests script it, so no automated test ever
//! opens a real OS prompt.

use std::sync::Arc;

/// What a consent prompt can answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// A human passed the gesture.
    Verified,
    /// A human refused or abandoned the prompt. The Approval stays pending.
    Declined(String),
    /// There is no way to ask on this machine, or the ask itself failed.
    Unavailable(String),
}

/// Asks the OS for an interactive gesture. Implementations must never be
/// reachable from a test without an explicit choice.
pub trait ConsentVerifier: Send + Sync {
    /// Show the prompt (or answer as scripted) for one Approval.
    fn verify(&self, message: &str) -> ConsentOutcome;

    /// Short name for diagnostics: `windows-hello`, `scripted`, `none`.
    fn describe(&self) -> &'static str;
}

/// How the daemon answers "can a human approve here?".
///
/// The default for a test-configured daemon is [`ConsentMode::Declined`]: a
/// scripted verifier that never opens a prompt, because a suite that popped a
/// Windows Hello dialog would be a flaky, intrusive test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    /// Ask the OS: Windows Hello on Windows, unavailable elsewhere.
    Os,
    /// A human passes the gesture.
    Granted,
    /// A human refuses the gesture.
    Declined,
    /// No verifier is installed: approvals cannot be decided at all.
    Unavailable,
}

impl ConsentMode {
    /// Parse `AGENTIFY_CONSENT`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "os" | "windows-hello" | "windows" | "hello" => Some(ConsentMode::Os),
            "granted" | "allow" => Some(ConsentMode::Granted),
            "declined" | "deny" => Some(ConsentMode::Declined),
            "unavailable" | "none" => Some(ConsentMode::Unavailable),
            _ => None,
        }
    }

    /// Build the verifier this mode names.
    pub fn verifier(self) -> Arc<dyn ConsentVerifier> {
        match self {
            ConsentMode::Os => Arc::new(OsConsent),
            ConsentMode::Granted => Arc::new(ScriptedConsent {
                outcome: ConsentOutcome::Verified,
            }),
            ConsentMode::Declined => Arc::new(ScriptedConsent {
                outcome: ConsentOutcome::Declined(
                    "nobody at this machine passed the consent gesture".to_string(),
                ),
            }),
            ConsentMode::Unavailable => Arc::new(UnavailableConsent),
        }
    }
}

/// The mode the daemon should start with when nobody says otherwise: the real
/// OS prompt in production, a scripted one in tests (see [`ConsentMode::parse`]).
pub fn default_mode() -> ConsentMode {
    if cfg!(windows) {
        ConsentMode::Os
    } else {
        ConsentMode::Unavailable
    }
}

/// The account name of the person at this machine, for the audit.
pub fn os_account() -> String {
    for name in ["USERNAME", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    "local-user".to_string()
}

/// A verifier that answers from a script. Tests use it; nothing else should.
struct ScriptedConsent {
    outcome: ConsentOutcome,
}

impl ConsentVerifier for ScriptedConsent {
    fn verify(&self, _message: &str) -> ConsentOutcome {
        self.outcome.clone()
    }

    fn describe(&self) -> &'static str {
        "scripted"
    }
}

/// A verifier installed where no gesture is possible.
struct UnavailableConsent;

impl ConsentVerifier for UnavailableConsent {
    fn verify(&self, _message: &str) -> ConsentOutcome {
        ConsentOutcome::Unavailable(
            "no consent verifier is installed on this machine".to_string(),
        )
    }

    fn describe(&self) -> &'static str {
        "none"
    }
}

/// Windows Hello, through `Windows.Security.Credentials.UI.UserConsentVerifier`.
///
/// The WinRT call is blocking and needs an initialized apartment, so it runs on
/// its own thread; the daemon never blocks a runtime worker on an OS dialog for
/// longer than the caller's own request.
pub struct OsConsent;

impl ConsentVerifier for OsConsent {
    fn verify(&self, message: &str) -> ConsentOutcome {
        #[cfg(windows)]
        {
            windows_hello(message)
        }
        #[cfg(not(windows))]
        {
            let _ = message;
            ConsentOutcome::Unavailable(
                "interactive consent is only implemented for Windows in v0".to_string(),
            )
        }
    }

    fn describe(&self) -> &'static str {
        #[cfg(windows)]
        {
            "windows-hello"
        }
        #[cfg(not(windows))]
        {
            "none"
        }
    }
}

#[cfg(windows)]
fn windows_hello(message: &str) -> ConsentOutcome {
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    // WinRT needs an initialized apartment on this thread. A second
    // initialization on the same thread is a no-op error worth ignoring.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }

    match UserConsentVerifier::CheckAvailabilityAsync() {
        Ok(operation) => match operation.get() {
            Ok(UserConsentVerifierAvailability::Available) => {}
            Ok(other) => {
                return ConsentOutcome::Unavailable(format!(
                    "Windows Hello is not usable here: {}",
                    availability_label(other)
                ))
            }
            Err(err) => {
                return ConsentOutcome::Unavailable(format!(
                    "Windows Hello availability could not be read: {err}"
                ))
            }
        },
        Err(err) => {
            return ConsentOutcome::Unavailable(format!(
                "Windows Hello could not be asked: {err}"
            ))
        }
    }

    match UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(message)) {
        Ok(operation) => match operation.get() {
            Ok(UserConsentVerificationResult::Verified) => ConsentOutcome::Verified,
            Ok(UserConsentVerificationResult::Canceled) => {
                ConsentOutcome::Declined("the gesture was cancelled".to_string())
            }
            Ok(UserConsentVerificationResult::RetriesExhausted) => {
                ConsentOutcome::Declined("too many failed gesture attempts".to_string())
            }
            Ok(UserConsentVerificationResult::DeviceBusy) => {
                ConsentOutcome::Unavailable("the Windows Hello device is busy".to_string())
            }
            Ok(UserConsentVerificationResult::DeviceNotPresent) => {
                ConsentOutcome::Unavailable("this machine has no Windows Hello device".to_string())
            }
            Ok(UserConsentVerificationResult::NotConfiguredForUser) => ConsentOutcome::Unavailable(
                "Windows Hello is not configured for this user".to_string(),
            ),
            Ok(UserConsentVerificationResult::DisabledByPolicy) => ConsentOutcome::Unavailable(
                "Windows Hello is disabled by policy".to_string(),
            ),
            Ok(other) => ConsentOutcome::Declined(format!(
                "the gesture was not verified: {}",
                result_label(other)
            )),
            Err(err) => ConsentOutcome::Unavailable(format!(
                "the consent prompt failed: {err}"
            )),
        },
        Err(err) => ConsentOutcome::Unavailable(format!(
            "the consent prompt could not be shown: {err}"
        )),
    }
}

#[cfg(windows)]
fn availability_label(value: windows::Security::Credentials::UI::UserConsentVerifierAvailability) -> String {
    use windows::Security::Credentials::UI::UserConsentVerifierAvailability;
    match value {
        UserConsentVerifierAvailability::Available => "available".to_string(),
        UserConsentVerifierAvailability::DeviceNotPresent => "no device".to_string(),
        UserConsentVerifierAvailability::NotConfiguredForUser => "not configured".to_string(),
        UserConsentVerifierAvailability::DisabledByPolicy => "disabled by policy".to_string(),
        UserConsentVerifierAvailability::DeviceBusy => "device busy".to_string(),
        _ => format!("{value:?}"),
    }
}

#[cfg(windows)]
fn result_label(value: windows::Security::Credentials::UI::UserConsentVerificationResult) -> String {
    format!("{value:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_mode_is_not_a_scripted_yes() {
        assert_ne!(default_mode(), ConsentMode::Granted);
        assert_ne!(default_mode(), ConsentMode::Declined);
    }

    #[test]
    fn modes_parse_from_the_environment() {
        assert_eq!(ConsentMode::parse("os"), Some(ConsentMode::Os));
        assert_eq!(
            ConsentMode::parse("windows-hello"),
            Some(ConsentMode::Os)
        );
        assert_eq!(
            ConsentMode::parse("granted"),
            Some(ConsentMode::Granted)
        );
        assert_eq!(
            ConsentMode::parse("declined"),
            Some(ConsentMode::Declined)
        );
        assert_eq!(
            ConsentMode::parse("unavailable"),
            Some(ConsentMode::Unavailable)
        );
        assert_eq!(ConsentMode::parse("nonsense"), None);
        assert_eq!(ConsentMode::parse(""), None);
    }

    #[test]
    fn a_scripted_verifier_answers_without_a_prompt() {
        let granted = ConsentMode::Granted.verifier();
        assert_eq!(granted.verify("anything"), ConsentOutcome::Verified);
        let declined = ConsentMode::Declined.verifier();
        assert!(matches!(
            declined.verify("anything"),
            ConsentOutcome::Declined(_)
        ));
        let unavailable = ConsentMode::Unavailable.verifier();
        assert!(matches!(
            unavailable.verify("anything"),
            ConsentOutcome::Unavailable(_)
        ));
    }
}

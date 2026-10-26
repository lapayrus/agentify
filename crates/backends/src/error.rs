//! Typed Backend failures.
//!
//! The set is fixed by the spec, so an Agent can branch on a stable code
//! instead of parsing prose: the same strings surface as the `code` of an
//! `application/problem+json` response and as the `data.code` of an MCP tool
//! error.

use std::fmt;

/// One typed Backend failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// This Backend cannot do that: the capability is not advertised.
    Unsupported { capability: String, detail: String },
    /// The substrate's major version is not the one this daemon expects.
    VersionMismatch { expected: String, found: String },
    /// Nothing has been provisioned for this Environment (or Resource) yet.
    NotProvisioned { detail: String },
    /// The Environment already exists where it must not.
    AlreadyExists { detail: String },
    /// A host port could not be bound; `holder` names what has it.
    PortUnavailable { port: u16, holder: String },
    /// A command ran and failed, or could not be run.
    ExecutionFailed { code: i32, detail: String },
    /// The substrate is not reachable at all.
    BackendUnavailable { detail: String },
    /// The call did not finish in the time allowed.
    Timeout { detail: String },
    /// The caller (or the daemon) cancelled the call.
    Cancelled { detail: String },
}

impl BackendError {
    /// Stable machine-readable code, identical to the API's `code`.
    pub fn code(&self) -> &'static str {
        match self {
            BackendError::Unsupported { .. } => "Unsupported",
            BackendError::VersionMismatch { .. } => "VersionMismatch",
            BackendError::NotProvisioned { .. } => "NotProvisioned",
            BackendError::AlreadyExists { .. } => "AlreadyExists",
            BackendError::PortUnavailable { .. } => "PortUnavailable",
            BackendError::ExecutionFailed { .. } => "ExecutionFailed",
            BackendError::BackendUnavailable { .. } => "BackendUnavailable",
            BackendError::Timeout { .. } => "Timeout",
            BackendError::Cancelled { .. } => "Cancelled",
        }
    }

    /// HTTP status the daemon answers with when this failure escapes a route.
    pub fn status(&self) -> u16 {
        match self {
            BackendError::Unsupported { .. } => 501,
            BackendError::VersionMismatch { .. } => 409,
            BackendError::NotProvisioned { .. } => 409,
            BackendError::AlreadyExists { .. } => 409,
            BackendError::PortUnavailable { .. } => 409,
            BackendError::ExecutionFailed { .. } => 422,
            BackendError::BackendUnavailable { .. } => 503,
            BackendError::Timeout { .. } => 504,
            BackendError::Cancelled { .. } => 409,
        }
    }

    pub fn unsupported(capability: impl Into<String>) -> Self {
        let capability = capability.into();
        BackendError::Unsupported {
            detail: format!("this Backend does not support {capability}"),
            capability,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Unsupported { detail, .. } => write!(f, "{}", detail),
            BackendError::VersionMismatch { expected, found } => {
                write!(f, "expected Backend version {expected}, found {found}")
            }
            BackendError::NotProvisioned { detail } => write!(f, "{}", detail),
            BackendError::AlreadyExists { detail } => write!(f, "{}", detail),
            BackendError::PortUnavailable { port, holder } => {
                write!(f, "port {port} is taken by {holder}")
            }
            BackendError::ExecutionFailed { code, detail } => {
                write!(f, "command failed with exit code {code}: {detail}")
            }
            BackendError::BackendUnavailable { detail } => write!(f, "{}", detail),
            BackendError::Timeout { detail } => write!(f, "{}", detail),
            BackendError::Cancelled { detail } => write!(f, "{}", detail),
        }
    }
}

impl std::error::Error for BackendError {}

/// Every Backend call returns this.
pub type Result<T> = std::result::Result<T, BackendError>;

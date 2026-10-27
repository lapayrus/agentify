//! Pinned versions, and the handshake that refuses a mismatch.
//!
//! One table says what this build expects of everything it talks to: the loopback
//! API, the daemon and the MCP shim (shipped together, so their versions are the
//! same), the Caddy binary, each Backend adapter's major, and the rootfs and
//! image digests. [`evaluate`] compares what was actually found against that
//! table; a mismatch is a `VersionMismatch` and the Backend behind it is refused
//! rather than driven blindly.
//!
//! Nothing here performs I/O. The daemon collects the [`Observed`] set at
//! startup, the CLI and the shell read the report the daemon publishes, and the
//! MCP shim compares the daemon it found against the same table.

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Version of the daemon this build ships.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the MCP shim this build ships.
///
/// The daemon and the MCP shim are built from one workspace and released
/// together, so any difference means two builds have been mixed.
pub const MCP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Caddy release this build drives. `major.minor` is the contract: a patch
/// release cannot change the admin API shape, a minor one might.
pub const CADDY_VERSION: &str = "2.10.2";

/// The `wsl` adapter's version, as `describe()` reports it.
pub const WSL_ADAPTER_VERSION: &str = "0.1.0";

/// The `fake` adapter's version, as `describe()` reports it.
pub const FAKE_ADAPTER_VERSION: &str = "0.1.0";

/// Ubuntu base rootfs digest this build accepts. Duplicated from the adapter's
/// own pin on purpose: a daemon must be able to report what it expects without
/// loading a Backend, and the two are checked against each other in a test.
pub const WSL_ROOTFS_SHA256: &str =
    "e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86";

/// Image digests (repository+tag → `sha256:…`) this build accepts. Empty in v0:
/// Manifests may use tags, and digest pinning is enforced only for images this
/// table names.
pub const IMAGE_DIGESTS: &[(&str, &str)] = &[];

/// How a found version is compared with the pinned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Every component must match: `2.10.2` is not `2.10.3`.
    Exact,
    /// `major.minor` must match; a patch release is accepted.
    MajorMinor,
    /// `major` must match; the adapter may move within a major.
    Major,
    /// A content digest must match byte for byte.
    Digest,
}

impl Rule {
    /// Stable machine-readable name, for the report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Rule::Exact => "exact",
            Rule::MajorMinor => "major_minor",
            Rule::Major => "major",
            Rule::Digest => "digest",
        }
    }

    /// Does `found` satisfy `expected` under this rule?
    pub fn accepts(&self, expected: &str, found: &str) -> bool {
        match self {
            Rule::Exact => expected == found,
            Rule::MajorMinor => major_minor(expected) == major_minor(found),
            Rule::Major => major(expected) == major(found),
            Rule::Digest => digest(expected) == digest(found),
        }
    }
}

/// One row of the pin table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// `api`, `daemon`, `mcp`, `caddy`, `adapter:<name>`, `rootfs`, `image:<ref>`.
    pub name: String,
    /// What this build expects.
    pub expected: String,
    pub rule: Rule,
}

/// What this build expects of everything it talks to.
///
/// The adapter rows are per-adapter because the two substrates are not on the
/// same version line: each carries its own major.
pub fn pinned() -> Vec<Component> {
    let mut components = vec![
        Component {
            name: "api".to_string(),
            expected: API_VERSION.to_string(),
            rule: Rule::Exact,
        },
        Component {
            name: "daemon".to_string(),
            expected: DAEMON_VERSION.to_string(),
            rule: Rule::Exact,
        },
        Component {
            name: "mcp".to_string(),
            expected: MCP_VERSION.to_string(),
            rule: Rule::Exact,
        },
        Component {
            name: "caddy".to_string(),
            expected: CADDY_VERSION.to_string(),
            rule: Rule::MajorMinor,
        },
        Component {
            name: "adapter:fake".to_string(),
            expected: FAKE_ADAPTER_VERSION.to_string(),
            rule: Rule::Major,
        },
        Component {
            name: "adapter:wsl".to_string(),
            expected: WSL_ADAPTER_VERSION.to_string(),
            rule: Rule::Major,
        },
        Component {
            name: "rootfs".to_string(),
            expected: WSL_ROOTFS_SHA256.to_string(),
            rule: Rule::Digest,
        },
    ];
    for (reference, digest) in IMAGE_DIGESTS {
        components.push(Component {
            name: format!("image:{reference}"),
            expected: (*digest).to_string(),
            rule: Rule::Digest,
        });
    }
    components
}

/// What was actually found on this machine, at this moment.
///
/// Fields are `Option` where a component may legitimately not be present yet: a
/// Caddy binary that has not been installed, a rootfs that has not been
/// downloaded, an ingress that is switched off. An absent component is reported
/// as not checked, not as a mismatch — a missing component has its own typed
/// error at the point of use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// API version the daemon serves or the client speaks.
    pub api: Option<String>,
    pub daemon: Option<String>,
    pub mcp: Option<String>,
    /// `caddy version` output, when a binary was found.
    pub caddy: Option<String>,
    /// The active adapter's name, as `describe()` reports it.
    pub adapter: Option<String>,
    /// The active adapter's version, as `describe()` reports it.
    pub adapter_version: Option<String>,
    /// SHA-256 of the cached rootfs, when one has been downloaded.
    pub rootfs_sha256: Option<String>,
    /// `(repository+tag, digest)` for every image a caller wants checked.
    pub images: Vec<(String, String)>,
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentReport {
    /// `api`, `daemon`, `mcp`, `caddy`, `adapter:wsl`, `rootfs`, `image:<ref>`.
    pub component: String,
    pub expected: String,
    /// `null` when the component was not present to check.
    pub found: Option<String>,
    pub rule: String,
    pub ok: bool,
}

/// The verdict on one machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionReport {
    /// True when every component that was present matched its pin.
    pub ok: bool,
    /// One row per pin, in table order, plus one per observed image.
    pub components: Vec<ComponentReport>,
}

impl VersionReport {
    /// The first row that failed, if any.
    pub fn first_mismatch(&self) -> Option<&ComponentReport> {
        self.components.iter().find(|component| !component.ok)
    }

    /// One sentence a human can act on, naming every failing component.
    pub fn summary(&self) -> String {
        let failures: Vec<String> = self
            .components
            .iter()
            .filter(|component| !component.ok)
            .map(|component| {
                format!(
                    "{}: expected {}, found {}",
                    component.component,
                    component.expected,
                    component.found.as_deref().unwrap_or("nothing")
                )
            })
            .collect();
        if failures.is_empty() {
            return "every pinned version matches".to_string();
        }
        format!("VersionMismatch: {}", failures.join("; "))
    }
}

/// Compare what was found against the pin table.
pub fn evaluate(observed: &Observed) -> VersionReport {
    let mut components = Vec::new();
    for pin in pinned() {
        let found = match pin.name.as_str() {
            "api" => observed.api.clone(),
            "daemon" => observed.daemon.clone(),
            "mcp" => observed.mcp.clone(),
            "caddy" => observed.caddy.clone(),
            "rootfs" => observed.rootfs_sha256.clone(),
            other if other.starts_with("adapter:") => {
                let name = other.trim_start_matches("adapter:");
                match observed.adapter.as_deref() {
                    // The adapter that is running is the only one whose version
                    // can be found; the others are not checked at all.
                    Some(active) if active == name => observed.adapter_version.clone(),
                    _ => None,
                }
            }
            other if other.starts_with("image:") => {
                let reference = other.trim_start_matches("image:");
                observed
                    .images
                    .iter()
                    .find(|(candidate, _)| candidate == reference)
                    .map(|(_, digest)| digest.clone())
            }
            _ => None,
        };
        let ok = match &found {
            Some(found) => pin.rule.accepts(&pin.expected, found),
            None => true,
        };
        components.push(ComponentReport {
            component: pin.name,
            expected: pin.expected,
            found,
            rule: pin.rule.as_str().to_string(),
            ok,
        });
    }

    // An adapter this build has never heard of cannot be checked, so it is
    // refused rather than waved through.
    if let Some(name) = observed.adapter.as_deref() {
        let known = components
            .iter()
            .any(|component| component.component == format!("adapter:{name}"));
        if !known {
            components.push(ComponentReport {
                component: format!("adapter:{name}"),
                expected: "a Backend this build pins".to_string(),
                found: observed.adapter_version.clone(),
                rule: Rule::Major.as_str().to_string(),
                ok: false,
            });
        }
    }

    // An image digest that disagrees with a pinned one is a mismatch even when
    // the caller never asked for that image: the pin is the contract.
    for (reference, digest) in &observed.images {
        if let Some(pin) = components
            .iter_mut()
            .find(|component| component.component == format!("image:{reference}"))
        {
            pin.found = Some(digest.clone());
            pin.ok = Rule::Digest.accepts(&pin.expected, digest);
        }
    }

    let ok = components.iter().all(|component| component.ok);
    VersionReport { ok, components }
}

/// `2.10.2` → `2.10`, `v2.10` → `2.10`, `2` → `2`.
pub fn major_minor(version: &str) -> String {
    let parts: Vec<&str> = version.trim().trim_start_matches('v').split('.').collect();
    match parts.as_slice() {
        [major, minor, ..] => format!("{major}.{minor}"),
        [major] => (*major).to_string(),
        _ => String::new(),
    }
}

/// `2.10.2` → `2`, `v2` → `2`.
pub fn major(version: &str) -> String {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `sha256:ABC`, `SHA256:abc` and `abc` all compare equal; case does not
/// matter, and the prefix is optional.
fn digest(value: &str) -> String {
    // Lower-case first: `trim_start_matches` is case-sensitive, so a digest
    // written `SHA256:ABC` would otherwise keep its prefix and never match.
    value
        .trim()
        .to_ascii_lowercase()
        .trim_start_matches("sha256:")
        .to_string()
}

/// The version out of `caddy version` output: `v2.10.2 h1:…` → `2.10.2`.
pub fn parse_caddy_version(output: &str) -> Option<String> {
    let first = output.split_whitespace().next()?;
    let version = first.trim().trim_start_matches('v');
    if version.is_empty() {
        return None;
    }
    // A Caddy built from a branch reports `(devel)`, which cannot be pinned.
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed_ok() -> Observed {
        Observed {
            api: Some(API_VERSION.to_string()),
            daemon: Some(DAEMON_VERSION.to_string()),
            mcp: Some(MCP_VERSION.to_string()),
            caddy: Some(format!("v{CADDY_VERSION} h1:abcdef0123456789")),
            adapter: Some("wsl".to_string()),
            adapter_version: Some(WSL_ADAPTER_VERSION.to_string()),
            rootfs_sha256: Some(WSL_ROOTFS_SHA256.to_string()),
            images: Vec::new(),
        }
    }

    #[test]
    fn a_matching_machine_passes() {
        let report = evaluate(&observed_ok());
        assert!(report.ok, "{:?}", report);
        assert!(report.first_mismatch().is_none());
        assert_eq!(report.summary(), "every pinned version matches");
    }

    #[test]
    fn an_absent_component_is_not_a_mismatch() {
        // A rootfs that has not been downloaded and an ingress that is off are
        // reported as not checked, not as failures.
        let observed = Observed {
            caddy: None,
            rootfs_sha256: None,
            ..observed_ok()
        };
        let report = evaluate(&observed);
        assert!(report.ok, "{:?}", report);
        let caddy = report
            .components
            .iter()
            .find(|component| component.component == "caddy")
            .expect("a caddy row");
        assert_eq!(caddy.found, None);
        assert!(caddy.ok);
    }

    #[test]
    fn an_api_mismatch_fails_and_names_itself() {
        let observed = Observed {
            api: Some("v2".to_string()),
            ..observed_ok()
        };
        let report = evaluate(&observed);
        assert!(!report.ok);
        assert_eq!(report.first_mismatch().unwrap().component, "api");
        assert!(report.summary().starts_with("VersionMismatch: api: expected v1"));
    }

    #[test]
    fn a_daemon_minor_difference_is_a_mismatch() {
        let observed = Observed {
            daemon: Some("9.9.9".to_string()),
            ..observed_ok()
        };
        assert!(!evaluate(&observed).ok);
    }

    #[test]
    fn caddy_accepts_a_patch_release_but_not_a_minor_one() {
        let same_minor = Observed {
            caddy: Some(format!("v{}.99 h1:x", major_minor(CADDY_VERSION))),
            ..observed_ok()
        };
        assert!(evaluate(&same_minor).ok);
        let other_minor = Observed {
            caddy: Some("v1.0.0 h1:x".to_string()),
            ..observed_ok()
        };
        let report = evaluate(&other_minor);
        assert!(!report.ok);
        assert_eq!(report.first_mismatch().unwrap().component, "caddy");
    }

    #[test]
    fn an_adapter_major_bump_is_a_mismatch() {
        let observed = Observed {
            adapter_version: Some("1.0.0".to_string()),
            ..observed_ok()
        };
        let report = evaluate(&observed);
        assert!(!report.ok);
        let row = report.first_mismatch().unwrap();
        assert_eq!(row.component, "adapter:wsl");
        assert_eq!(row.found.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn an_unknown_adapter_is_refused() {
        let observed = Observed {
            adapter: Some("lima".to_string()),
            adapter_version: Some("2.0.0".to_string()),
            ..observed_ok()
        };
        let report = evaluate(&observed);
        assert!(!report.ok);
        assert_eq!(report.first_mismatch().unwrap().component, "adapter:lima");
    }

    #[test]
    fn a_rootfs_digest_mismatch_is_a_mismatch() {
        let observed = Observed {
            rootfs_sha256: Some("deadbeef".to_string()),
            ..observed_ok()
        };
        let report = evaluate(&observed);
        assert!(!report.ok);
        assert_eq!(report.first_mismatch().unwrap().component, "rootfs");
    }

    #[test]
    fn the_pinned_rootfs_digest_looks_like_a_digest() {
        assert_eq!(WSL_ROOTFS_SHA256.len(), 64);
        assert!(WSL_ROOTFS_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn version_parsing() {
        assert_eq!(major("2.10.2"), "2");
        assert_eq!(major("v3"), "3");
        assert_eq!(major_minor("v2.10.2"), "2.10");
        assert_eq!(major_minor("2"), "2");
        assert_eq!(parse_caddy_version("v2.10.2 h1:abc"), Some("2.10.2".to_string()));
        assert_eq!(parse_caddy_version("(devel) h1:abc"), None);
        assert_eq!(parse_caddy_version(""), None);
    }

    #[test]
    fn digests_compare_with_or_without_their_prefix() {
        assert!(Rule::Digest.accepts("SHA256:ABC", "sha256:abc"));
        assert!(!Rule::Digest.accepts("abc", "abd"));
    }
}

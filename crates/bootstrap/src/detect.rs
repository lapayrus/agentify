//! What this machine has, read without elevation.
//!
//! Three read-only commands answer everything a first run needs to know, and
//! none of them asks Windows for a privilege:
//!
//! - `wsl --version` — the Windows optional components are on and `wsl.exe`
//!   runs. The output carries its own version.
//! - `wsl --status` — `Default Version: 2` and the default distro.
//! - `wsl -l -v` — every distro, its state, and its WSL version.
//!
//! `wsl.exe` writes UTF-16LE when its output is redirected, so every read is
//! decoded before it is parsed; [`decode_output`] is the one place that knows.
//!
//! Virtualization is probed too, but only when WSL is *missing*, because that is
//! the only moment it explains anything — and it never gates the verdict: on a
//! machine where the hypervisor owns the CPU, `VMMonitorModeExtensions` and
//! `SecondLevelAddressTranslationExtensions` both read False while WSL 2 works
//! perfectly. Gating on SLAT would refuse a machine that is fine.

// Every probe in this module runs a Windows tool; on another platform there is
// nothing to run and the module answers "not available" instead.
#[cfg(windows)]
use std::process::Command;

use serde::{Deserialize, Serialize};

use agentify_proto::versions::{self, Observed};

/// One distro, as `wsl -l -v` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distro {
    /// The registered name, e.g. `Ubuntu`.
    pub name: String,
    /// `Running`, `Stopped`, `Installing`, … exactly as WSL printed it.
    pub state: String,
    /// The WSL version the distro runs on: 1 or 2.
    pub version: u8,
    /// True for the distro marked `*`.
    pub default: bool,
}

/// What the virtualization probe found, when it was worth asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Virtualization {
    /// `Win32_ComputerSystem.HypervisorPresent`, when it could be read.
    pub hypervisor_present: Option<bool>,
    /// One sentence for a human, never a gate.
    pub detail: String,
}

/// Everything detection found, and whether any of it can be used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    /// `wsl.exe` exists and answered: the optional components are enabled.
    pub available: bool,
    /// The `WSL version:` line, e.g. `2.6.3.0`.
    pub wsl_version: Option<String>,
    /// `Default Version` from `wsl --status`, or the default distro's version.
    pub default_version: Option<u8>,
    /// `Default Distribution` from `wsl --status`, when it named one.
    pub default_distro: Option<String>,
    /// Every registered distro, in the order WSL listed them.
    pub distros: Vec<Distro>,
    /// Filled in only when WSL is missing: the diagnostics that explain it.
    pub virtualization: Option<Virtualization>,
    /// Why detection could not answer, when it could not.
    pub error: Option<String>,
}

impl Detection {
    /// A distro that already runs on WSL 2.
    pub fn wsl2_distro(&self) -> Option<&Distro> {
        self.distros.iter().find(|distro| distro.version == 2)
    }

    /// True when a new distro could be imported from the pinned rootfs: WSL is
    /// present and WSL 2 is what a new distro would be created on.
    pub fn can_import(&self) -> bool {
        self.available && self.default_version == Some(2)
    }

    /// Is this machine able to run a Project right now?
    ///
    /// WSL 2 must be installed and the default for new distros, and there must be
    /// either a distro already on WSL 2 or the ability to import the pinned
    /// rootfs. Everything else is a first-run problem, not a working machine.
    pub fn usable(&self) -> bool {
        self.available
            && self.default_version == Some(2)
            && (self.wsl2_distro().is_some() || self.can_import())
    }

    /// One sentence saying what was found, for a report or a banner.
    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if !self.available {
            return "WSL is not installed".to_string();
        }
        let distros = self.distros.len();
        let default = self
            .default_distro
            .clone()
            .or_else(|| self.wsl2_distro().map(|distro| distro.name.clone()))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "WSL {} ({}), default version {}, default distro {}, {} registered",
            self.wsl_version.as_deref().unwrap_or("?"),
            if self.default_version == Some(2) {
                "WSL 2"
            } else {
                "not WSL 2"
            },
            self.default_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            default,
            distros
        )
    }
}

/// What the bootstrap needs to know about the machine, and the only I/O it has
/// besides the one elevated step and the resume entry.
///
/// A test supplies its own implementation, so no unit test touches WSL, the
/// registry, or a UAC prompt.
pub trait Host {
    /// Read the machine's WSL state. Read-only, never elevated.
    fn detect(&self) -> Detection;
    /// Does Windows say a restart is pending?
    fn reboot_pending(&self) -> bool;
    /// The versions this build expects and the ones that are actually present,
    /// for the handshake that runs once the machine is ready.
    fn versions(&self) -> Observed;
}

/// The real machine.
pub struct SystemHost {
    data_root: std::path::PathBuf,
}

impl SystemHost {
    pub fn new(data_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }
}

impl Host for SystemHost {
    fn detect(&self) -> Detection {
        detect()
    }

    fn reboot_pending(&self) -> bool {
        reboot_pending()
    }

    fn versions(&self) -> Observed {
        // A daemon that is already running reports its own versions; that is what
        // catches two builds mixed in one data root. With no daemon, the answer
        // is this build's own pins, which cannot disagree with themselves.
        let discovery = agentify_proto::read_discovery(&self.data_root).ok();
        Observed {
            api: Some(
                discovery
                    .as_ref()
                    .map(|discovery| discovery.api_version.clone())
                    .unwrap_or_else(|| agentify_proto::API_VERSION.to_string()),
            ),
            daemon: Some(
                discovery
                    .map(|discovery| discovery.daemon_version)
                    .unwrap_or_else(|| versions::DAEMON_VERSION.to_string()),
            ),
            mcp: Some(versions::MCP_VERSION.to_string()),
            caddy: None,
            adapter: Some("wsl".to_string()),
            adapter_version: Some(versions::WSL_ADAPTER_VERSION.to_string()),
            // The adapter verifies the rootfs digest again before it imports
            // anything; a rootfs that has not been downloaded is not a mismatch.
            rootfs_sha256: None,
            images: Vec::new(),
        }
    }
}

/// Probe this machine. Read-only, no elevation, no writes.
pub fn detect() -> Detection {
    #[cfg(not(windows))]
    {
        Detection {
            error: Some(
                "WSL is a Windows feature; Agentify v0 ships the Windows bootstrap only"
                    .to_string(),
            ),
            ..Detection::default()
        }
    }
    #[cfg(windows)]
    {
        let version = run_wsl(&["--version"]);
        let status = run_wsl(&["--status"]);
        let list = run_wsl(&["-l", "-v"]);

        let mut detection = Detection {
            available: version.as_ref().is_some_and(|output| output.success),
            wsl_version: version
                .as_ref()
                .and_then(|output| parse_wsl_version(&output.text)),
            ..Detection::default()
        };

        if let Some(status) = status.filter(|output| output.success) {
            let status = parse_status(&status.text);
            detection.default_version = status.default_version;
            detection.default_distro = status.default_distribution;
            if detection.wsl_version.is_none() {
                detection.wsl_version = status.wsl_version;
            }
        }

        if let Some(list) = list.filter(|output| output.success) {
            detection.distros = parse_distro_list(&list.text);
            if detection.default_version.is_none() {
                // Older builds print no `Default Version`: the marked distro's
                // own version is the closest honest answer.
                detection.default_version = detection
                    .distros
                    .iter()
                    .find(|distro| distro.default)
                    .map(|distro| distro.version);
                if detection.default_version.is_none() {
                    detection.default_version = detection
                        .distros
                        .first()
                        .map(|distro| distro.version);
                }
            }
            if detection.default_distro.is_none() {
                detection.default_distro = detection
                    .distros
                    .iter()
                    .find(|distro| distro.default)
                    .map(|distro| distro.name.clone());
            }
        }

        if !detection.available {
            detection.error = version
                .as_ref()
                .map(|output| {
                    let text = output.text.trim().to_string();
                    if text.is_empty() {
                        format!("wsl --version failed with exit code {}", output.exit_code)
                    } else {
                        text
                    }
                })
                .or_else(|| Some("wsl.exe could not be run".to_string()));
            detection.virtualization = Some(probe_virtualization());
        }

        detection
    }
}

/// Windows' own "a restart is pending" markers, read from the registry.
///
/// Reads only, and none of them needs elevation. A machine that has never
/// pending anything simply answers false.
pub fn reboot_pending() -> bool {
    #[cfg(not(windows))]
    {
        false
    }
    #[cfg(windows)]
    {
        const MARKERS: [(&str, Option<&str>); 3] = [
            (
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending",
                None,
            ),
            (
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired",
                None,
            ),
            (
                r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager",
                Some("PendingFileRenameOperations"),
            ),
        ];
        MARKERS.iter().any(|(key, value)| {
            let mut command = Command::new("reg.exe");
            command.args(["query", key]);
            if let Some(value) = value {
                command.args(["/v", value]);
            }
            command
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        })
    }
}

/// `wsl.exe` output, decoded and flattened.
#[cfg(windows)]
struct WslOutput {
    success: bool,
    exit_code: i32,
    text: String,
}

#[cfg(windows)]
fn run_wsl(args: &[&str]) -> Option<WslOutput> {
    let output = Command::new("wsl.exe").args(args).output().ok()?;
    let mut bytes = output.stdout;
    if bytes.is_empty() {
        bytes = output.stderr;
    }
    Some(WslOutput {
        success: output.status.success(),
        exit_code: output.status.code().unwrap_or(-1),
        text: decode_output(&bytes),
    })
}

/// Decode a command's output: UTF-16LE when it looks like it, UTF-8 otherwise.
///
/// `wsl.exe` writes UTF-16LE whenever its output is a pipe, BOM or no BOM, so a
/// plain `from_utf8` would turn every line into `W\0S\0L\0`.
pub fn decode_output(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8]| -> String {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return utf16(&bytes[2..]);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        // Big-endian UTF-16, which Windows does not produce but a tool might.
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    // No BOM: a NUL byte in the first few bytes is the tell.
    if bytes.iter().take(16).any(|byte| *byte == 0) {
        return utf16(bytes);
    }
    String::from_utf8_lossy(bytes).to_string()
}

/// `WSL version: 2.6.3.0` → `2.6.3.0`.
pub fn parse_wsl_version(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("WSL version") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// What `wsl --status` said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub default_version: Option<u8>,
    pub default_distribution: Option<String>,
    pub wsl_version: Option<String>,
}

/// `Default Version: 2` and `Default Distribution: Ubuntu`.
pub fn parse_status(text: &str) -> Status {
    let mut status = Status {
        wsl_version: parse_wsl_version(text),
        ..Status::default()
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("Default Version") {
            status.default_version = value.parse::<u8>().ok();
        } else if key.eq_ignore_ascii_case("Default Distribution") {
            status.default_distribution = Some(value.to_string()).filter(|name| name != "None");
        }
    }
    status
}

/// Parse `wsl -l -v`: a `NAME STATE VERSION` header, then one row per distro,
/// with `*` marking the default.
pub fn parse_distro_list(text: &str) -> Vec<Distro> {
    let mut distros = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (default, rest) = match line.strip_prefix('*') {
            Some(rest) => (true, rest.trim()),
            None => (false, line),
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // The version is the last field and the state the one before it; a name
        // may contain a space, so everything to the left is the name.
        if fields.len() < 3 {
            continue;
        }
        let Ok(version) = fields[fields.len() - 1].parse::<u8>() else {
            continue;
        };
        if version == 0 {
            continue;
        }
        let state = fields[fields.len() - 2].to_string();
        let name = fields[..fields.len() - 2].join(" ");
        if name.eq_ignore_ascii_case("NAME") {
            continue;
        }
        distros.push(Distro {
            name,
            state,
            version,
            default,
        });
    }
    distros
}

/// `HypervisorPresent`, read through CIM. Only asked when WSL is missing.
#[cfg(windows)]
fn probe_virtualization() -> Virtualization {
    let script = "(Get-CimInstance Win32_ComputerSystem).HypervisorPresent";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();
    match output {
        Ok(output) => {
            let text = decode_output(&output.stdout);
            let trimmed = text.trim();
            let present = match trimmed.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            let detail = match present {
                Some(true) => {
                    "the hypervisor is present, so WSL 2 can run once its features are enabled"
                        .to_string()
                }
                Some(false) => {
                    "no hypervisor is present: WSL 2 additionally needs virtualization enabled in firmware"
                        .to_string()
                }
                None => "the hypervisor state could not be read".to_string(),
            };
            Virtualization {
                hypervisor_present: present,
                detail,
            }
        }
        Err(err) => Virtualization {
            hypervisor_present: None,
            detail: format!("the hypervisor state could not be read: {err}"),
        },
    }
}

/// The WSL distro name the bootstrap would reuse when one is already installed.
pub fn preferred_distro(detection: &Detection) -> Option<String> {
    detection
        .wsl2_distro()
        .map(|distro| distro.name.clone())
        .or_else(|| detection.default_distro.clone())
}

/// A `Detection` built by hand, for a test that must not touch the machine.
#[cfg(test)]
pub fn builder(available: bool, default_version: Option<u8>, distros: Vec<Distro>) -> Detection {
    Detection {
        available,
        wsl_version: available.then(|| "2.6.3.0".to_string()),
        default_version,
        default_distro: distros.iter().find(|distro| distro.default).map(|distro| distro.name.clone()),
        distros,
        virtualization: None,
        error: None,
    }
}

/// A machine with WSL 2 and Ubuntu, as this one is.
#[cfg(test)]
pub fn ubuntu_like() -> Vec<Distro> {
    vec![Distro {
        name: "Ubuntu".to_string(),
        state: "Running".to_string(),
        version: 2,
        default: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wsl --version` as this machine prints it, after decoding.
    const VERSION: &str = "WSL version: 2.6.3.0\r\nKernel version: 6.6.87.2-1\r\nWSLg version: 1.0.71\r\n";

    /// `wsl --status` on a machine with Ubuntu.
    const STATUS: &str = "Default Distribution: Ubuntu\r\nDefault Version: 2\r\n";

    /// `wsl -l -v` with two WSL 2 distros.
    const LIST: &str = "  NAME              STATE           VERSION\r\n* Ubuntu            Running         2\r\n  docker-desktop    Running         2\r\n";

    #[test]
    fn utf16_output_is_decoded_before_it_is_parsed() {
        // The real bytes `wsl.exe` writes into a pipe: UTF-16LE, no BOM.
        let bytes: Vec<u8> = "WSL version: 2.6.3.0\r\n"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let text = decode_output(&bytes);
        assert_eq!(text, "WSL version: 2.6.3.0\r\n");
        assert_eq!(parse_wsl_version(&text), Some("2.6.3.0".to_string()));
    }

    #[test]
    fn a_byte_order_mark_and_plain_utf8_are_both_understood() {
        let mut with_bom = vec![0xFF, 0xFE];
        for unit in "Default Version: 2".encode_utf16() {
            with_bom.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_output(&with_bom), "Default Version: 2");
        assert_eq!(decode_output(b"Default Version: 2\n"), "Default Version: 2\n");
        assert_eq!(decode_output(&[]), "");
    }

    #[test]
    fn the_status_line_yields_the_default_version_and_distro() {
        let status = parse_status(STATUS);
        assert_eq!(status.default_version, Some(2));
        assert_eq!(status.default_distribution.as_deref(), Some("Ubuntu"));

        // An older WSL prints the version but no default distribution.
        let old = parse_status("Default Version: 2\r\n");
        assert_eq!(old.default_version, Some(2));
        assert_eq!(old.default_distribution, None);

        // A WSL 1 default is reported as such, not waved through.
        assert_eq!(
            parse_status("Default Version: 1\r\n").default_version,
            Some(1)
        );
        assert_eq!(parse_status("").default_version, None);
    }

    #[test]
    fn the_distro_table_parses_with_the_default_marked() {
        let distros = parse_distro_list(LIST);
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0].name, "Ubuntu");
        assert_eq!(distros[0].state, "Running");
        assert_eq!(distros[0].version, 2);
        assert!(distros[0].default);
        assert!(!distros[1].default);
        assert_eq!(distros[1].name, "docker-desktop");
    }

    #[test]
    fn a_distro_name_with_a_space_survives_parsing() {
        let text = "  NAME    STATE     VERSION\r\n* Ubuntu 22.04  Stopped  2\r\n";
        let distros = parse_distro_list(text);
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "Ubuntu 22.04");
        assert_eq!(distros[0].state, "Stopped");
        assert_eq!(distros[0].version, 2);
    }

    #[test]
    fn a_machine_with_no_distributions_lists_none() {
        // The real message from `wsl -l -v` on a machine with no distros.
        let text = "Windows Subsystem for Linux has no installed distributions.\r\n";
        assert!(parse_distro_list(text).is_empty());
        assert!(parse_distro_list("").is_empty());
        assert!(parse_distro_list("NAME STATE VERSION").is_empty());
    }

    #[test]
    fn a_machine_ready_for_wsl2_is_usable() {
        let detection = builder(true, Some(2), parse_distro_list(LIST));
        assert!(detection.available);
        assert!(detection.can_import());
        assert!(detection.usable());
        assert_eq!(detection.wsl2_distro().map(|d| d.name.as_str()), Some("Ubuntu"));
        assert_eq!(preferred_distro(&detection).as_deref(), Some("Ubuntu"));
        assert!(Detection::default().summary().contains("not installed"));
    }

    #[test]
    fn wsl1_alone_or_a_wsl1_default_is_not_usable() {
        let wsl1_default = builder(true, Some(1), parse_distro_list(LIST));
        assert!(!wsl1_default.usable(), "a WSL 1 default cannot run a Project");

        let wsl1_disto = builder(
            true,
            Some(2),
            vec![Distro {
                name: "legacy".to_string(),
                state: "Stopped".to_string(),
                version: 1,
                default: true,
            }],
        );
        // WSL 2 is the default for new distros, so the pinned rootfs can still
        // be imported: a WSL 1 distro lying around is not a blocker.
        assert!(wsl1_disto.usable());
        assert!(wsl1_disto.wsl2_distro().is_none());
    }

    #[test]
    fn no_wsl_at_all_is_not_usable() {
        let detection = Detection {
            available: false,
            error: Some("wsl.exe could not be run".to_string()),
            ..Detection::default()
        };
        assert!(!detection.usable());
        assert!(!detection.can_import());
        assert_eq!(detection.summary(), "wsl.exe could not be run");
    }

    #[test]
    fn the_summary_names_the_version_and_the_default_distro() {
        let mut detection = builder(true, Some(2), parse_distro_list(LIST));
        detection.wsl_version = Some("2.6.3.0".to_string());
        let summary = detection.summary();
        assert!(summary.contains("WSL 2.6.3.0"), "{summary}");
        assert!(summary.contains("Ubuntu"), "{summary}");
        assert!(summary.contains("2 registered"), "{summary}");
    }

    #[test]
    fn the_version_line_is_the_one_wsl_prints() {
        assert_eq!(parse_wsl_version(VERSION), Some("2.6.3.0".to_string()));
        assert_eq!(parse_wsl_version("Kernel version: 6.6.0\n"), None);
        assert_eq!(parse_wsl_version(""), None);
    }
}

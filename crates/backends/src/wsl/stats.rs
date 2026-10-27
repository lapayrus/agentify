//! Environment-level figures, measured inside the Environment.
//!
//! The daemon never guesses a number. [`STATS_SCRIPT`] runs in the Project's
//! distro and prints only what that distro can prove:
//!
//! * **CPU** from two `/proc/stat` samples 200 ms apart — a rate, not a stored
//!   total. A WSL2 distro has its own utility VM and its own `/proc`, so this is
//!   this Environment's CPU and not the machine's.
//! * **Memory** as `MemTotal - MemAvailable` from `/proc/meminfo`.
//! * **Disk** as the bytes used on the distro's own filesystem (`df -B1 /`).
//! * **Per-Resource** CPU and memory from `docker stats --no-stream`, but only
//!   when the container engine is installed and running *and* has containers.
//!   There is no honest per-Resource figure without a per-Resource substrate, so
//!   the list is empty rather than divided up by guesswork.
//!
//! An omitted figure stays zero. Zero here means "the substrate could not prove
//! it", never "an estimate".

use crate::contract::{ResourceStat, Stats};
use crate::error::{BackendError, Result};

use super::cli::Wsl;

/// The probe, compiled into the daemon.
pub const STATS_SCRIPT: &str = include_str!("scripts/stats.sh");

/// What one probe measured, before it is labelled with an Environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Figures {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub resources: Vec<ResourceStat>,
}

impl Figures {
    /// Attach an Environment and a state to a measurement.
    pub fn stats(&self, environment: &str, state: &str) -> Stats {
        Stats {
            environment: environment.to_string(),
            state: state.to_string(),
            cpu_percent: self.cpu_percent,
            memory_bytes: self.memory_bytes,
            disk_bytes: self.disk_bytes,
            resources: self.resources.clone(),
        }
    }
}

/// An Environment that is not running has no figures to report, and saying so is
/// the honest answer: no CPU rate, no memory, no disk of a runtime that is off.
pub fn stopped(environment: &str, state: &str) -> Stats {
    Figures::default().stats(environment, state)
}

/// Ask one running distro what it is using.
pub fn measure(wsl: &Wsl, distro: &str) -> Result<Figures> {
    let output = wsl.run_with_stdin(distro, &["sh", "-s"], STATS_SCRIPT.as_bytes())?;
    if output.code != 0 {
        return Err(BackendError::BackendUnavailable {
            detail: format!(
                "the stats probe failed in {distro} (exit {}): {}",
                output.code,
                output.diagnostic()
            ),
        });
    }
    Ok(parse(&output.stdout_text()))
}

/// Parse the probe's `key=value` lines. Unknown keys are ignored, so a newer
/// probe on an older daemon degrades instead of failing.
pub fn parse(text: &str) -> Figures {
    let mut figures = Figures::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(raw) = line.strip_prefix("cpu_percent=") {
            if let Ok(value) = raw.trim().parse::<f64>() {
                figures.cpu_percent = value.max(0.0);
            }
            continue;
        }
        if let Some(raw) = line.strip_prefix("memory_bytes=") {
            figures.memory_bytes = raw.trim().parse::<u64>().unwrap_or(0);
            continue;
        }
        if let Some(raw) = line.strip_prefix("disk_bytes=") {
            figures.disk_bytes = raw.trim().parse::<u64>().unwrap_or(0);
            continue;
        }
        if let Some(raw) = line.strip_prefix("container=") {
            if let Some(stat) = parse_container(raw) {
                figures.resources.push(stat);
            }
        }
    }
    figures
}

/// `name|cpu%|used / limit`, as `docker stats --format` prints it.
fn parse_container(raw: &str) -> Option<ResourceStat> {
    let mut parts = raw.split('|');
    let name = parts.next()?.trim();
    let cpu = parts.next()?.trim();
    let memory = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let cpu_percent = cpu.trim_end_matches('%').trim().parse::<f64>().ok()?;
    let used = memory.split('/').next()?.trim();
    Some(ResourceStat {
        name: name.to_string(),
        cpu_percent_milli: (cpu_percent * 1000.0).round().max(0.0) as u32,
        memory_bytes: bytes(used),
    })
}

/// `1.5GiB` → bytes. An unknown unit is zero: a figure nobody can read is not a
/// figure.
fn bytes(raw: &str) -> u64 {
    let trimmed = raw.trim();
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let Ok(value) = number.trim().parse::<f64>() else {
        return 0;
    };
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "" => 1.0,
        "kib" | "kb" => 1024.0,
        "mib" | "mb" => 1024.0 * 1024.0,
        "gib" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "tib" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return 0,
    };
    (value * multiplier).max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_is_parsed_and_nothing_is_invented() {
        let figures = parse(
            "cpu_percent=12.50\nmemory_bytes=268435456\ndisk_bytes=1073741824\n",
        );
        assert_eq!(figures.cpu_percent, 12.5);
        assert_eq!(figures.memory_bytes, 268_435_456);
        assert_eq!(figures.disk_bytes, 1_073_741_824);
        assert!(figures.resources.is_empty(), "no engine, no per-Resource figure");
    }

    #[test]
    fn a_figure_the_probe_could_not_measure_stays_zero() {
        let figures = parse("cpu_percent=0.00\nmemory_bytes=1024\n");
        assert_eq!(figures.disk_bytes, 0, "an omitted figure is not guessed");
        let figures = parse("garbage\ncpu_percent=not-a-number\nmemory_bytes=2048\n");
        assert_eq!(figures.cpu_percent, 0.0);
        assert_eq!(figures.memory_bytes, 2048);
        let empty = parse("");
        assert_eq!(empty, Figures::default());
    }

    #[test]
    fn a_stopped_environment_reports_no_figures_and_its_state() {
        let stats = stopped("env-1", "stopped");
        assert_eq!(stats.state, "stopped");
        assert_eq!(stats.memory_bytes, 0);
        assert_eq!(stats.cpu_percent, 0.0);
        assert!(stats.resources.is_empty());
    }

    #[test]
    fn a_container_line_becomes_a_resource_figure() {
        let figures = parse("container=web-1|3.50%|12.5MiB / 1GiB\n");
        assert_eq!(figures.resources.len(), 1);
        let stat = &figures.resources[0];
        assert_eq!(stat.name, "web-1");
        assert_eq!(stat.cpu_percent_milli, 3_500);
        assert_eq!(stat.memory_bytes, 13_107_200);

        assert_eq!(bytes("0B"), 0);
        assert_eq!(bytes("1KiB"), 1024);
        assert_eq!(bytes("2GiB"), 2 * 1024 * 1024 * 1024);
        assert_eq!(bytes("nonsense"), 0, "an unreadable figure is not a figure");
        assert!(parse("container=broken").resources.is_empty());
    }

    #[test]
    fn figures_are_labelled_with_the_environment_the_caller_asked_about() {
        let figures = Figures {
            cpu_percent: 1.0,
            memory_bytes: 2,
            disk_bytes: 3,
            resources: vec![ResourceStat {
                name: "web".into(),
                cpu_percent_milli: 1500,
                memory_bytes: 4096,
            }],
        };
        let stats = figures.stats("env-9", "running");
        assert_eq!(stats.environment, "env-9");
        assert_eq!(stats.state, "running");
        assert_eq!(stats.disk_bytes, 3);
        assert_eq!(stats.resources.len(), 1);
    }
}

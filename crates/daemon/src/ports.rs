//! The machine-wide port registry.
//!
//! A host port is a machine-wide resource: two Environments cannot share one,
//! and a stale lease cannot silently hand out a port something else already
//! bound. The daemon is the only allocator, it records every lease in SQLite,
//! releases an Environment's leases on `stop`, and reacquires them on `start`.
//!
//! Allocation is deterministic: an Environment gets the same host port back for
//! the same `(resource, guest port)` when it is still free, which keeps URLs in
//! a test stable across a stop/start cycle.

use std::net::TcpListener;

use crate::state::{LeaseRecord, State};

/// The inclusive range of host ports the daemon may lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub const fn new(start: u16, end: u16) -> Self {
        Self { start, end }
    }

    /// `"40000-40999"` or a single port `"40000"`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (start, end) = match raw.split_once('-') {
            Some((start, end)) => (start.trim(), end.trim()),
            None => (raw, raw),
        };
        let start: u16 = start
            .parse()
            .map_err(|_| format!("{raw:?} does not start with a port number"))?;
        let end: u16 = end
            .parse()
            .map_err(|_| format!("{raw:?} does not end with a port number"))?;
        if start == 0 || end == 0 {
            return Err("port 0 cannot be leased".to_string());
        }
        if end < start {
            return Err(format!("the lease range {raw} ends before it starts"));
        }
        Ok(Self { start, end })
    }

    /// How many ports the range holds.
    pub fn span(&self) -> u32 {
        u32::from(self.end) - u32::from(self.start) + 1
    }

    pub fn contains(&self, port: u16) -> bool {
        port >= self.start && port <= self.end
    }
}

/// Why a lease could not be granted.
#[derive(Debug)]
pub enum PortError {
    /// Every candidate was held by another Environment or bound by something
    /// else on the machine. `holder` names what has the port, so the user can
    /// fix the machine instead of guessing.
    Unavailable { port: u16, holder: String },
    /// The registry could not be read or written.
    Store(anyhow::Error),
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortError::Unavailable { port, holder } => {
                write!(f, "port {port} is available to no Environment: {holder}")
            }
            PortError::Store(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for PortError {}

/// The allocator. It owns the range and reads and writes leases through the
/// daemon's single SQLite connection; it holds no state of its own.
#[derive(Debug, Clone)]
pub struct PortRegistry {
    range: PortRange,
}

impl PortRegistry {
    pub fn new(range: PortRange) -> Self {
        Self { range }
    }

    pub fn range(&self) -> PortRange {
        self.range
    }

    /// Lease a host port for `(environment, resource, guest_port)`.
    ///
    /// `attempt` salts the preferred port: after a bind race the caller retries
    /// with the next attempt and gets a different part of the range, instead of
    /// hammering the port it just lost.
    pub fn allocate(
        &self,
        state: &State,
        environment_id: &str,
        resource: &str,
        guest_port: u16,
        attempt: u32,
    ) -> Result<u16, PortError> {
        let existing = state
            .leases(environment_id)
            .map_err(PortError::Store)?
            .into_iter()
            .find(|lease| lease.resource == resource && lease.guest_port == guest_port);

        let mut candidates: Vec<u16> = Vec::new();
        let mut preferred: Option<u16> = None;
        if let Some(previous) = existing.as_ref() {
            // Reacquisition: the same Environment keeps the port it had. It is
            // already ours, so it is not probed for bindability — the Backend is
            // holding it open, which is exactly what the lease is for.
            if self.range.contains(previous.host_port) {
                preferred = Some(previous.host_port);
                candidates.push(previous.host_port);
            }
        }
        let span = self.range.span();
        let offset = fnv1a(environment_id, resource, guest_port).wrapping_add(
            attempt.wrapping_mul(0x9e37_79b9),
        ) % span;
        for step in 0..span {
            let port = (u32::from(self.range.start) + ((offset + step) % span)) as u16;
            if is_privileged(port) || Some(port) == preferred {
                continue;
            }
            candidates.push(port);
        }

        let mut blocking: Option<(u16, String)> = None;
        for port in candidates {
            match state.lease_holder(port).map_err(PortError::Store)? {
                Some(holder) if holder.environment_id != environment_id => {
                    blocking.get_or_insert((
                        port,
                        format!("Environment {}", holder.environment_id),
                    ));
                    continue;
                }
                Some(holder) if Some(port) != preferred => {
                    // The same Environment already uses this port for another
                    // Resource: one host port belongs to exactly one lease.
                    blocking.get_or_insert((
                        port,
                        format!(
                            "Resource {} of Environment {}",
                            holder.resource, holder.environment_id
                        ),
                    ));
                    continue;
                }
                _ => {}
            }
            if Some(port) != preferred && !bindable(port) {
                blocking.get_or_insert((port, "another process on this machine".to_string()));
                continue;
            }
            let lease = LeaseRecord {
                environment_id: environment_id.to_string(),
                resource: resource.to_string(),
                guest_port,
                host_port: port,
                created_at: crate::clock::now_rfc3339(),
            };
            let stored = state.put_lease(&lease).map_err(PortError::Store)?;
            if stored {
                return Ok(port);
            }
            blocking.get_or_insert((port, "another Environment".to_string()));
        }

        let (port, holder) = blocking.unwrap_or((
            self.range.start,
            format!(
                "no free port in the lease range {}-{}",
                self.range.start, self.range.end
            ),
        ));
        Err(PortError::Unavailable { port, holder })
    }

    /// Release every lease an Environment holds.
    pub fn release_all(&self, state: &State, environment_id: &str) -> Result<(), PortError> {
        state
            .release_leases(environment_id)
            .map_err(PortError::Store)
    }

    /// The leases an Environment currently holds.
    pub fn leases(&self, state: &State, environment_id: &str) -> Result<Vec<LeaseRecord>, PortError> {
        state.leases(environment_id).map_err(PortError::Store)
    }
}

/// Ports the daemon never leases: everything below 1024 needs privilege on some
/// platforms, and the daemon's own API lives on a high ephemeral port anyway.
fn is_privileged(port: u16) -> bool {
    port < 1024
}

/// True when the port can be bound right now.
fn bindable(port: u16) -> bool {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(_) => false,
    }
}

/// FNV-1a over the lease key: stable across runs and platforms, so an
/// Environment's preferred port does not move between tests.
fn fnv1a(environment_id: &str, resource: &str, guest_port: u16) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in environment_id
        .bytes()
        .chain(b":".iter().copied())
        .chain(resource.bytes())
        .chain(b":".iter().copied())
        .chain(guest_port.to_string().bytes())
    {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_parses_from_one_port_or_two() {
        assert_eq!(PortRange::parse("40000").expect("single"), PortRange::new(40000, 40000));
        assert_eq!(
            PortRange::parse("40000-40009").expect("range"),
            PortRange::new(40000, 40009)
        );
        assert!(PortRange::parse("40009-40000").is_err());
        assert!(PortRange::parse("0").is_err());
        assert!(PortRange::parse("nope").is_err());
        assert_eq!(PortRange::new(40000, 40009).span(), 10);
    }

    #[test]
    fn the_preferred_port_is_stable_for_a_lease_key() {
        assert_eq!(fnv1a("env-a", "web", 3000), fnv1a("env-a", "web", 3000));
        assert_ne!(fnv1a("env-a", "web", 3000), fnv1a("env-b", "web", 3000));
        assert_ne!(fnv1a("env-a", "web", 3000), fnv1a("env-a", "db", 3000));
    }

    #[test]
    fn a_single_port_range_can_be_exhausted() {
        let state = State::open_in_memory().expect("state");
        let registry = PortRegistry::new(PortRange::new(1, 1));
        // Port 1 is below the leaseable floor, so nothing can be leased.
        let err = registry
            .allocate(&state, "env-1", "web", 3000, 0)
            .expect_err("no port in range");
        assert!(matches!(err, PortError::Unavailable { .. }));
    }
}

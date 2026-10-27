//! Probing the ingress ports before Caddy is started.
//!
//! Port 80 or 443 being taken is a fact about the machine, not a bug in the
//! daemon: the answer is a typed `PortUnavailable` that names the holder, never
//! a silent move to another port. A URL a user bookmarked and a port that
//! drifts under them is the failure mode this avoids.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// How long a holder probe waits for a connection.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Why a port cannot be used for ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortIssue {
    pub port: u16,
    /// What holds it, as precisely as this machine will say.
    pub holder: String,
}

impl PortIssue {
    /// The sentence the API and the UI show.
    pub fn detail(&self) -> String {
        format!(
            "port {} is already in use by {}; Agentify will not move ingress to another port, \
             so free the port or stop that process",
            self.port, self.holder
        )
    }
}

impl std::fmt::Display for PortIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for PortIssue {}

/// Checks the ports Caddy is about to bind.
#[derive(Debug, Clone, Copy, Default)]
pub struct PortProbe;

impl PortProbe {
    /// Verify every port is free. Two questions, because Windows answers them
    /// differently: is anything *answering* on the port, and can the wildcard
    /// address still be bound (a socket can be held without listening).
    pub fn check(ports: &[u16]) -> Result<(), PortIssue> {
        let mut seen: Vec<u16> = Vec::new();
        for port in ports {
            if seen.contains(port) {
                continue;
            }
            seen.push(*port);
            if listening(*port) || !bindable(*port) {
                return Err(PortIssue {
                    port: *port,
                    holder: holder(*port),
                });
            }
        }
        Ok(())
    }
}

/// True when something accepts connections on the loopback port. This is the
/// reliable test on Windows, where a wildcard bind can still succeed while a
/// specific address is in use.
fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), PROBE_TIMEOUT).is_ok()
}

/// True when the wildcard address on `port` can be bound right now.
fn bindable(port: u16) -> bool {
    match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))) {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(_) => false,
    }
}

/// Best-effort description of who holds a port. It never fails: a vague answer
/// is still better than none, and naming the port alone tells the user where to
/// look.
fn holder(port: u16) -> String {
    if let Some(owner) = owner(port) {
        return owner;
    }
    if listening(port) {
        return format!("a process listening on 127.0.0.1:{port}");
    }
    format!("another process with a socket bound to port {port}")
}

/// Ask the OS which process owns the listening socket on `port`.
#[cfg(windows)]
fn owner(port: u16) -> Option<String> {
    let netstat = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !netstat.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&netstat.stdout);
    let pid = listing.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 || !fields[3].eq_ignore_ascii_case("LISTENING") {
            return None;
        }
        let local = fields[1];
        let local_port: u16 = local.rsplit(':').next()?.parse().ok()?;
        if local_port != port {
            return None;
        }
        Some(fields[4].to_string())
    })?;

    match image_name(&pid) {
        Some(name) => Some(format!("PID {pid} ({name}) on port {port}")),
        None => Some(format!("PID {pid} on port {port}")),
    }
}

#[cfg(windows)]
fn image_name(pid: &str) -> Option<String> {
    let tasklist = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !tasklist.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&tasklist.stdout);
    let first = listing.lines().next()?.trim();
    let name = first.split(',').next()?.trim_matches('"').to_string();
    if name.is_empty() || name.starts_with("INFO:") {
        None
    } else {
        Some(name)
    }
}

#[cfg(not(windows))]
fn owner(_port: u16) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_port_passes() {
        // Port 0 is never a real ingress port; ask the OS for a free high port
        // instead so the test does not race another process.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        assert_eq!(PortProbe::check(&[port]), Ok(()));
    }

    #[test]
    fn a_taken_port_names_itself_and_the_holder() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let issue = PortProbe::check(&[port]).expect_err("the port is held");
        assert_eq!(issue.port, port);
        assert!(
            !issue.holder.is_empty(),
            "a conflict must name a holder, even if it can only say who is listening"
        );
        assert!(
            issue.detail().contains(&port.to_string()),
            "the message names the port: {}",
            issue.detail()
        );
        assert!(
            issue.detail().contains("will not move ingress"),
            "there is no silent port change: {}",
            issue.detail()
        );
    }

    #[test]
    fn the_same_port_twice_is_probed_once() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(PortProbe::check(&[port, port]).is_err());
    }
}

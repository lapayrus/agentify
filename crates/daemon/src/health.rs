//! Health probing through leased host ports.
//!
//! The daemon — never the Backend — decides whether an Environment is up. It
//! leased the port, so it can knock on it: a successful TCP connect means the
//! Resource behind that port is listening. Every Backend therefore looks the
//! same from the outside, and a Backend cannot claim health it does not have.

use std::net::SocketAddr;
use std::time::Duration;

/// Probe one leased port on loopback. Returns `true` when a connection was
/// accepted within `timeout`.
pub async fn tcp_probe(port: u16, timeout: Duration) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await,
        Ok(Ok(_))
    )
}

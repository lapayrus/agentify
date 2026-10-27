//! A very small blocking HTTP/1.1 client.
//!
//! The Caddy admin API lives on a random loopback port, speaks JSON, and has no
//! transport security to negotiate. Pulling a full HTTP stack into the daemon
//! for four verbs on `127.0.0.1` would be worse than these hundred lines.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long one admin request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A parsed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Why a request did not produce a response.
#[derive(Debug)]
pub enum HttpError {
    /// The connection failed, or the peer went away mid-request.
    Transport(String),
    /// The response could not be parsed.
    Malformed(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Transport(detail) => write!(f, "{detail}"),
            HttpError::Malformed(detail) => write!(f, "malformed HTTP response: {detail}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// A client bound to one admin endpoint.
#[derive(Debug, Clone)]
pub struct Http {
    addr: SocketAddr,
    timeout: Duration,
}

impl Http {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            timeout: REQUEST_TIMEOUT,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn get(&self, path: &str) -> Result<Response, HttpError> {
        self.send("GET", path, None)
    }

    pub fn post_json(&self, path: &str, body: &[u8]) -> Result<Response, HttpError> {
        self.send("POST", path, Some(body))
    }

    pub fn put_json(&self, path: &str, body: &[u8]) -> Result<Response, HttpError> {
        self.send("PUT", path, Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Response, HttpError> {
        self.send("DELETE", path, None)
    }

    /// Send one request with `Connection: close` and read the whole answer.
    ///
    /// Asking the server to close keeps the parse simple: there is no pipelining
    /// and no reuse to get wrong, and the admin API is called a handful of times
    /// per Environment transition.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response, HttpError> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.timeout)
            .map_err(|err| HttpError::Transport(format!("connecting to {}: {err}", self.addr)))?;
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));

        let mut request = String::new();
        request.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
        request.push_str(&format!("Host: {}\r\n", self.addr));
        request.push_str("Accept: application/json\r\n");
        request.push_str("Connection: close\r\n");
        if let Some(body) = body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");

        stream
            .write_all(request.as_bytes())
            .and_then(|()| match body {
                Some(body) => stream.write_all(body),
                None => Ok(()),
            })
            .and_then(|()| stream.flush())
            .map_err(|err| {
                HttpError::Transport(format!("sending {method} {path} to {}: {err}", self.addr))
            })?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|err| {
            HttpError::Transport(format!("reading the answer to {method} {path}: {err}"))
        })?;
        parse(&raw)
    }
}

/// Split a response into status and body, decoding chunked framing.
fn parse(raw: &[u8]) -> Result<Response, HttpError> {
    let split = find(raw, b"\r\n\r\n")
        .ok_or_else(|| HttpError::Malformed("no header terminator".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let rest = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Malformed("no status line".to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| HttpError::Malformed(format!("bad status line {status_line:?}")))?;

    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "transfer-encoding" => {
                chunked = value.to_ascii_lowercase().contains("chunked");
            }
            "content-length" => length = value.parse().ok(),
            _ => {}
        }
    }

    let body = if chunked {
        decode_chunked(rest)?
    } else if let Some(length) = length {
        rest.get(..length.min(rest.len())).unwrap_or(rest).to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Response { status, body })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(mut rest: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let split = find(rest, b"\r\n")
            .ok_or_else(|| HttpError::Malformed("truncated chunk header".to_string()))?;
        let size_line = String::from_utf8_lossy(&rest[..split]).to_string();
        let size_hex = size_line.split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_hex, 16)
            .map_err(|_| HttpError::Malformed(format!("bad chunk size {size_hex:?}")))?;
        rest = &rest[split + 2..];
        if size == 0 {
            return Ok(body);
        }
        let take = size.min(rest.len());
        body.extend_from_slice(&rest[..take]);
        rest = rest.get(take.saturating_add(2)..).unwrap_or(&[]);
        if rest.is_empty() {
            return Ok(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_with_a_content_length_is_parsed() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let response = parse(raw).expect("parse");
        assert_eq!(response.status, 200);
        assert_eq!(response.body_string(), "{\"a\":1}");
        assert!(response.is_success());
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let response = parse(raw).expect("parse");
        assert_eq!(response.body_string(), "hello world");
    }

    #[test]
    fn an_empty_body_is_an_empty_string() {
        let raw = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse(raw).expect("parse").body_string(), "");
    }

    #[test]
    fn a_truncated_response_is_malformed_not_a_panic() {
        assert!(parse(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse(b"nonsense").is_err());
    }
}

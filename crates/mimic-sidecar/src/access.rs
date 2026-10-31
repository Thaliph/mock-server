//! Access log lines (spec §4.1): one per HTTP exchange and one per opaque or TLS connection,
//! written when it ends. Only the fields of the spec table are logged: never a header value,
//! a query string or a body.

use std::net::SocketAddr;
use std::time::Duration;

use crate::listener::Direction;

/// Longest path logged, in bytes.
const MAX_PATH: usize = 1024;

/// How an exchange or connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    UpstreamReset,
    ClientAbort,
    /// A `400`/`431` answered by the sidecar itself, or an HTTP/2 connection broken by the client.
    Rejected,
    /// Closed at the end of the shutdown grace.
    Shutdown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::UpstreamReset => "upstream_reset",
            Outcome::ClientAbort => "client_abort",
            Outcome::Rejected => "rejected",
            Outcome::Shutdown => "shutdown",
        }
    }
}

/// One finished HTTP exchange.
pub struct Exchange {
    pub direction: Direction,
    pub protocol: &'static str,
    pub dst: SocketAddr,
    pub authority: String,
    pub method: Option<String>,
    /// Path without the query string.
    pub path: Option<String>,
    pub status: Option<u16>,
    pub grpc_status: Option<u64>,
    pub upgrade: bool,
    pub outcome: Outcome,
    /// `None` for an exchange rejected before any request is read.
    pub duration: Option<Duration>,
    pub head: Option<Duration>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// One finished opaque or TLS connection.
pub struct Connection {
    pub direction: Direction,
    pub protocol: &'static str,
    pub dst: SocketAddr,
    pub sni: Option<String>,
    pub outcome: Outcome,
    pub duration: Duration,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `path` cut to [`MAX_PATH`] bytes on a character boundary.
pub fn truncate_path(path: &str) -> &str {
    let mut end = path.len().min(MAX_PATH);
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    &path[..end]
}

pub fn exchange(e: &Exchange) {
    tracing::info!(
        target: "access",
        direction = e.direction.as_str(),
        protocol = e.protocol,
        dst = %e.dst,
        authority = e.authority.as_str(),
        method = e.method.as_deref(),
        path = e.path.as_deref().map(truncate_path),
        status = e.status,
        grpc_status = e.grpc_status,
        upgrade = e.upgrade,
        result = e.outcome.as_str(),
        duration_ms = e.duration.map(millis),
        head_ms = e.head.map(millis),
        bytes_in = e.bytes_in,
        bytes_out = e.bytes_out,
        "http exchange"
    );
}

pub fn connection(c: &Connection) {
    tracing::info!(
        target: "access",
        direction = c.direction.as_str(),
        protocol = c.protocol,
        dst = %c.dst,
        sni = c.sni.as_deref(),
        result = c.outcome.as_str(),
        duration_ms = millis(c.duration),
        bytes_in = c.bytes_in,
        bytes_out = c.bytes_out,
        "connection"
    );
}

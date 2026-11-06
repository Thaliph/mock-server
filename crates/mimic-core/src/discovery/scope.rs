//! The scope key of a report (spec §6.1): a listening port for inbound exchanges, the
//! destination authority `<host>:<port>` for outbound ones.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::redact::{MARKER, value_rule};

/// Longest host of an authority, in bytes (a DNS name).
const MAX_HOST: usize = 253;

/// Where the endpoints of a report are grouped: inbound by port, outbound by authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKey {
    Port(u16),
    /// `<host>:<port>` in the grammar of §6.1.
    Authority(String),
}

impl ScopeKey {
    /// The key written as `text`, only when `text` is its canonical form.
    pub fn parse(text: &str) -> Option<ScopeKey> {
        if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            return canonical_port(text).map(ScopeKey::Port);
        }
        valid_authority(text).then(|| ScopeKey::Authority(text.to_string()))
    }

    /// The port: the key itself, or the one an authority ends with.
    pub fn port(&self) -> u16 {
        match self {
            ScopeKey::Port(port) => *port,
            ScopeKey::Authority(authority) => authority
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok())
                .unwrap_or(0),
        }
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeKey::Port(port) => write!(f, "{port}"),
            ScopeKey::Authority(authority) => f.write_str(authority),
        }
    }
}

impl Serialize for ScopeKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ScopeKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        ScopeKey::parse(&text).ok_or_else(|| D::Error::custom("invalid scope key"))
    }
}

/// Decimal 1–65535 without sign or leading zero.
fn canonical_port(text: &str) -> Option<u16> {
    if text.is_empty()
        || text.len() > 5
        || text.starts_with('0')
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

/// A lower-case DNS name of at most 253 bytes: labels of 1 to 63 letters, digits, `-` or `_`,
/// not starting or ending with `-`. IPv4 literals match.
fn valid_dns(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn valid_authority(text: &str) -> bool {
    let Some((host, port)) = text.rsplit_once(':') else {
        return false;
    };
    if canonical_port(port).is_none() {
        return false;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner
            .parse::<Ipv6Addr>()
            .is_ok_and(|ip| ip.to_string() == inner);
    }
    host == MARKER || valid_dns(host)
}

/// The scope key of an outbound exchange (spec §6.1): the target authority (`default_port` when
/// it has none), else the SNI with the destination port, else the destination. Never panics;
/// the result always matches the grammar of [`ScopeKey::Authority`].
pub fn authority_key(
    target: Option<&str>,
    default_port: u16,
    sni: Option<&str>,
    dst: SocketAddr,
) -> String {
    if let Some((host, port)) = target.and_then(|t| split_target(t, default_port)) {
        return join(host, port);
    }
    // A destination has a port; 0 cannot be one, and the grammar has no room for it.
    let dst_port = dst.port().max(1);
    if let Some(host) = sni.and_then(dns_host) {
        return join(host, dst_port);
    }
    let ip = dst.ip().to_canonical();
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    join(host, dst_port)
}

/// Host and port of a `Host` / `:authority` value, normalised.
fn split_target(raw: &str, default_port: u16) -> Option<(String, u16)> {
    if raw.is_empty() || raw.len() > MAX_HOST + 8 {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let (host, port) = if let Some(rest) = lower.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        let ip: Ipv6Addr = addr.parse().ok()?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{ip}]"), port)
    } else {
        let (host, port) = match lower.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (lower.as_str(), None),
        };
        (dns_host(host)?, port)
    };
    let port = match port {
        None => default_port,
        Some(p) => canonical_port(p)?,
    };
    Some((host, port))
}

/// A DNS name in the grammar, from any case and with one trailing dot.
fn dns_host(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let host = lower.strip_suffix('.').unwrap_or(&lower);
    valid_dns(host).then(|| host.to_string())
}

/// `host:port`, with the marker instead of a host a value detector catches.
fn join(host: String, port: u16) -> String {
    if !host.starts_with('[') && value_rule(&host).is_some() {
        return format!("{MARKER}:{port}");
    }
    format!("{host}:{port}")
}

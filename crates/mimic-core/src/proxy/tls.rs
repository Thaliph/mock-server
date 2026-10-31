//! Pure TLS logic for outbound interception (spec §5): ClientHello facts, the interception
//! decision, exclusions, bounded memory and the intermediate CA lifecycle.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::time::Duration;

/// Capacity of the remembered destinations and of the leaf cache.
pub const MEMORY_CAPACITY: usize = 4096;

/// Most entries `MIMIC_TLS_EXCLUDE` accepts.
pub const MAX_EXCLUDES: usize = 256;

/// What the sidecar needs to know about a ClientHello.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientHello {
    /// The `host_name` of the `server_name` extension.
    pub sni: Option<String>,
    /// ALPN protocol names, in the client's order.
    pub alpn: Vec<String>,
    /// TLS 1.2 is offered.
    pub tls12: bool,
    /// TLS 1.3 is offered.
    pub tls13: bool,
    /// An SNI was sent but is not a valid DNS name; `sni` is then `None`.
    pub sni_invalid: bool,
}

/// Result of [`parse_client_hello`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Not enough bytes to decide.
    NeedMore,
    /// Not a TLS ClientHello.
    NotTls,
    /// A ClientHello. One that is malformed beyond the record layer yields no facts: no name,
    /// no ALPN and no version, hence no interception.
    Hello(ClientHello),
}

/// Parse a ClientHello out of the bytes read so far, possibly split over several handshake
/// records. Total on arbitrary input: never panics, never reads past `buf`.
pub fn parse_client_hello(buf: &[u8]) -> Parsed {
    let mut hello = Vec::new();
    let mut pos = 0;
    let mut first = true;
    loop {
        let Some(header) = buf.get(pos..pos + 5) else {
            return Parsed::NeedMore;
        };
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if first {
            if header[0] != 0x16 || header[1] != 3 || header[2] > 4 || len == 0 || len > 1 << 14 {
                return Parsed::NotTls;
            }
            first = false;
        } else if header[0] != 0x16 {
            return Parsed::Hello(ClientHello::default());
        }
        pos += 5;
        let end = (pos + len).min(buf.len());
        hello.extend_from_slice(&buf[pos..end]);
        if hello.len() >= 4 && hello[0] != 1 {
            return Parsed::NotTls;
        }
        if hello.len() >= 4 {
            let want = 4
                + (usize::from(hello[1]) << 16
                    | usize::from(hello[2]) << 8
                    | usize::from(hello[3]));
            if hello.len() >= want {
                return Parsed::Hello(facts(&hello[4..want]).unwrap_or_default());
            }
        }
        if end < pos + len {
            return Parsed::NeedMore;
        }
        pos = end;
    }
}

/// Bounds-checked big-endian reader.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<usize> {
        Some(usize::from(self.take(1)?[0]))
    }

    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }
}

/// The facts of a ClientHello body (after the 4-byte handshake header), if well formed.
fn facts(body: &[u8]) -> Option<ClientHello> {
    let mut r = Reader(body);
    let legacy = r.u16()?;
    r.take(32)?; // random
    let n = r.u8()?;
    r.take(n)?; // session id
    let n = r.u16()?;
    r.take(n)?; // cipher suites
    let n = r.u8()?;
    r.take(n)?; // compression methods
    let n = r.u16()?;
    let mut exts = Reader(r.take(n)?);
    let mut out = ClientHello::default();
    let mut versions = None;
    while !exts.0.is_empty() {
        let kind = exts.u16()?;
        let n = exts.u16()?;
        let data = exts.take(n)?;
        match kind {
            0 if out.sni.is_none() && !out.sni_invalid => {
                // An SNI is a key of the bounded memories and a leaf SAN: only DNS names.
                if let Some(name) = server_name(data)? {
                    match valid_host(&name) {
                        Some(_) => out.sni = Some(name),
                        None => out.sni_invalid = true,
                    }
                }
            }
            16 if out.alpn.is_empty() => out.alpn = alpn(data)?,
            43 if versions.is_none() => versions = Some(supported_versions(data)?),
            _ => {}
        }
    }
    (out.tls12, out.tls13) = versions.unwrap_or((legacy >= 0x0303, false));
    Some(out)
}

/// The first `host_name` of a `server_name` extension; `Some(None)` when it has none or an
/// unusable one.
fn server_name(data: &[u8]) -> Option<Option<String>> {
    let mut list = Reader(data);
    let n = list.u16()?;
    let mut list = Reader(list.take(n)?);
    while !list.0.is_empty() {
        let name_type = list.u8()?;
        let n = list.u16()?;
        let name = list.take(n)?;
        if name_type == 0 {
            return Some(Some(String::from_utf8_lossy(name).into_owned()));
        }
    }
    Some(None)
}

fn alpn(data: &[u8]) -> Option<Vec<String>> {
    let mut list = Reader(data);
    let n = list.u16()?;
    let mut list = Reader(list.take(n)?);
    let mut out = Vec::new();
    while !list.0.is_empty() {
        let n = list.u8()?;
        out.push(String::from_utf8_lossy(list.take(n)?).into_owned());
    }
    Some(out)
}

/// Whether TLS 1.2 and TLS 1.3 are listed (GREASE and older versions are ignored).
fn supported_versions(data: &[u8]) -> Option<(bool, bool)> {
    let mut r = Reader(data);
    let n = r.u8()?;
    let mut list = Reader(r.take(n)?);
    let (mut tls12, mut tls13) = (false, false);
    while !list.0.is_empty() {
        match list.u16()? {
            0x0303 => tls12 = true,
            0x0304 => tls13 = true,
            _ => {}
        }
    }
    Some((tls12, tls13))
}

/// Why a connection is not intercepted (the `interception` field of the access log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Excluded,
    NoCa,
    Unsupported,
    UpstreamUntrusted,
    ClientCertRequested,
    ClientRejected,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Excluded => "excluded",
            Reason::NoCa => "no_ca",
            Reason::Unsupported => "unsupported",
            Reason::UpstreamUntrusted => "upstream_untrusted",
            Reason::ClientCertRequested => "client_cert_requested",
            Reason::ClientRejected => "client_rejected",
        }
    }
}

/// Outcome of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    PassThrough(Reason),
    /// Intercept, offering the destination these ALPN protocols (client order).
    Intercept {
        alpn: Vec<String>,
    },
}

/// Key of a remembered destination: the SNI (lower case, no trailing dot) else the
/// destination IP, and the destination port.
pub type DestKey = (String, u16);

/// The remembered-destinations key of a connection.
pub fn dest_key(hello: &ClientHello, dst: IpAddr, port: u16) -> DestKey {
    let host = match &hello.sni {
        Some(sni) => normalise_host(sni),
        None => dst.to_string(),
    };
    (host, port)
}

fn normalise_host(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix('.').unwrap_or(&lower).to_string()
}

/// The §5 table, in order.
pub fn decide(
    hello: &ClientHello,
    dst: IpAddr,
    port: u16,
    exclusions: &Exclusions,
    remembered: &Bounded<DestKey, Reason>,
    has_ca: bool,
) -> Decision {
    if hello.sni_invalid {
        return Decision::PassThrough(Reason::Unsupported);
    }
    if exclusions.matches(hello.sni.as_deref(), dst) {
        return Decision::PassThrough(Reason::Excluded);
    }
    if let Some(&reason) = remembered.get(&dest_key(hello, dst, port)) {
        return Decision::PassThrough(reason);
    }
    if !has_ca {
        return Decision::PassThrough(Reason::NoCa);
    }
    let alpn: Vec<String> = hello
        .alpn
        .iter()
        .filter(|p| matches!(p.as_str(), "h2" | "http/1.1"))
        .cloned()
        .collect();
    let version_ok = hello.tls12 || hello.tls13;
    let alpn_ok = hello.alpn.is_empty() || !alpn.is_empty();
    if !version_ok || !alpn_ok {
        return Decision::PassThrough(Reason::Unsupported);
    }
    Decision::Intercept { alpn }
}

/// One `MIMIC_TLS_EXCLUDE` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Exclude {
    Exact(String),
    /// `*.suffix`, stored as `.suffix`.
    Suffix(String),
    Ip(IpAddr),
}

/// The parsed `MIMIC_TLS_EXCLUDE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions(Vec<Exclude>);

impl Exclusions {
    /// Parse the comma-separated value; an empty string means no exclusion.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Ok(Self::default());
        }
        let mut out = Vec::new();
        for raw in value.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                return Err("empty entry".to_string());
            }
            out.push(parse_entry(entry)?);
            if out.len() > MAX_EXCLUDES {
                return Err(format!("more than {MAX_EXCLUDES} entries"));
            }
        }
        Ok(Self(out))
    }

    /// With an SNI, host entries are compared with it; without, IP entries with the
    /// destination address.
    pub fn matches(&self, sni: Option<&str>, dst: IpAddr) -> bool {
        match sni {
            Some(sni) => {
                let host = normalise_host(sni);
                self.0.iter().any(|e| match e {
                    Exclude::Exact(h) => *h == host,
                    Exclude::Suffix(s) => host.len() > s.len() && host.ends_with(s.as_str()),
                    Exclude::Ip(_) => false,
                })
            }
            None => self
                .0
                .iter()
                .any(|e| matches!(e, Exclude::Ip(ip) if *ip == dst)),
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn parse_entry(entry: &str) -> Result<Exclude, String> {
    if let Ok(ip) = entry.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Ok(Exclude::Ip(ip));
    }
    if let Some(rest) = entry.strip_prefix("*.") {
        let host = valid_host(rest).ok_or_else(|| format!("invalid pattern {entry:?}"))?;
        return Ok(Exclude::Suffix(format!(".{host}")));
    }
    valid_host(entry)
        .map(Exclude::Exact)
        .ok_or_else(|| format!("invalid name {entry:?}"))
}

/// A DNS name: labels of 1 to 63 letters, digits, `-` or `_`, not starting or ending with
/// `-`; at most 253 bytes; one trailing dot accepted. Returned normalised.
fn valid_host(name: &str) -> Option<String> {
    let host = normalise_host(name);
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    host.split('.')
        .all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
        .then_some(host)
}

/// Insertion-ordered map with a capacity: the oldest insertion is forgotten first and a key
/// already present keeps its first value.
#[derive(Debug, Clone)]
pub struct Bounded<K, V> {
    capacity: usize,
    map: BTreeMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Ord + Clone, V> Bounded<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// `true` when the key was new.
    pub fn insert(&mut self, key: K, value: V) -> bool {
        if self.capacity == 0 || self.map.contains_key(&key) {
            return false;
        }
        if self.map.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
        true
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// Validity window of the intermediate (and of every leaf), in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub not_before: u64,
    pub not_after: u64,
}

impl Validity {
    /// Within the window at `now`.
    pub fn usable(self, now: u64) -> bool {
        self.not_before <= now && now < self.not_after
    }

    /// Less than half of the validity remains.
    pub fn renewal_due(self, now: u64) -> bool {
        let total = self.not_after.saturating_sub(self.not_before);
        let remaining = self.not_after.saturating_sub(now);
        remaining.saturating_mul(2) < total
    }

    /// A leaf is valid exactly as long as the intermediate that signs it.
    pub fn leaf(self) -> Validity {
        self
    }
}

/// An intermediate exists and is within its validity.
pub fn ca_usable(intermediate: Option<Validity>, now: u64) -> bool {
    intermediate.is_some_and(|v| v.usable(now))
}

/// A new intermediate must be requested: none, unusable, or renewal due.
pub fn fetch_due(intermediate: Option<Validity>, now: u64) -> bool {
    intermediate.is_none_or(|v| !v.usable(now) || v.renewal_due(now))
}

/// Retry delay after failures: 1, 2, 4 … 60 s, reset by a success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// Record a failure and return how long to wait before retrying.
    pub fn failure(&mut self) -> Duration {
        let secs = 1u64 << self.failures.min(6);
        self.failures = self.failures.saturating_add(1);
        Duration::from_secs(secs.min(60))
    }

    pub fn success(&mut self) {
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests;

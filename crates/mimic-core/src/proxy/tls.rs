//! Pure TLS logic for outbound interception (spec §5). Stub for the red commit.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

pub const MEMORY_CAPACITY: usize = 4096;
pub const MAX_EXCLUDES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientHello {
    pub sni: Option<String>,
    pub alpn: Vec<String>,
    pub tls12: bool,
    pub tls13: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    NeedMore,
    NotTls,
    Hello(ClientHello),
}

pub fn parse_client_hello(_buf: &[u8]) -> Parsed {
    todo!()
}

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
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    PassThrough(Reason),
    Intercept { alpn: Vec<String> },
}

pub type DestKey = (String, u16);

pub fn dest_key(_hello: &ClientHello, _dst: IpAddr, _port: u16) -> DestKey {
    todo!()
}

pub fn decide(
    _hello: &ClientHello,
    _dst: IpAddr,
    _port: u16,
    _exclusions: &Exclusions,
    _remembered: &Bounded<DestKey, Reason>,
    _has_ca: bool,
) -> Decision {
    todo!()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions(Vec<String>);

impl Exclusions {
    pub fn parse(_value: &str) -> Result<Self, String> {
        todo!()
    }
    pub fn matches(&self, _sni: Option<&str>, _dst: IpAddr) -> bool {
        todo!()
    }
    pub fn len(&self) -> usize {
        todo!()
    }
    pub fn is_empty(&self) -> bool {
        todo!()
    }
}

#[derive(Debug, Clone)]
pub struct Bounded<K, V> {
    map: BTreeMap<K, V>,
    capacity: usize,
}

impl<K: Ord + Clone, V> Bounded<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: BTreeMap::new(),
            capacity,
        }
    }
    pub fn insert(&mut self, _key: K, _value: V) -> bool {
        let _ = self.capacity;
        todo!()
    }
    pub fn get(&self, _key: &K) -> Option<&V> {
        todo!()
    }
    pub fn len(&self) -> usize {
        todo!()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub not_before: u64,
    pub not_after: u64,
}

impl Validity {
    pub fn usable(self, _now: u64) -> bool {
        todo!()
    }
    pub fn renewal_due(self, _now: u64) -> bool {
        todo!()
    }
    pub fn leaf(self) -> Validity {
        todo!()
    }
}

pub fn ca_usable(_intermediate: Option<Validity>, _now: u64) -> bool {
    todo!()
}

pub fn fetch_due(_intermediate: Option<Validity>, _now: u64) -> bool {
    todo!()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff;

impl Backoff {
    pub fn failure(&mut self) -> Duration {
        todo!()
    }
    pub fn success(&mut self) {
        todo!()
    }
}

#[cfg(test)]
mod tests;

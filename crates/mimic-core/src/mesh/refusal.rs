//! The head of a trusted refusal of an inbound sidecar (spec §4.1 "Admission contract" and
//! "Refused app connections"), read once for its two readers: the retries of a caller's sidecar
//! (§7.9, `retry::trusted`) and the load generator (§7.8, `load::answer`). Each adds its own
//! binding to the pod. Pure.

use std::fmt::Write as _;
use std::net::SocketAddr;

use super::reason::Reason;

const ERROR: &[u8] = b"x-mimic-error";
const ORIGIN: &[u8] = b"x-mimic-origin";
const ENDPOINT: &[u8] = b"x-mimic-endpoint";

/// One field seen none, once or more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen<'a> {
    None,
    One(&'a [u8]),
    Many,
}

impl<'a> Seen<'a> {
    fn add(&mut self, value: &'a [u8]) {
        *self = match self {
            Seen::None => Seen::One(value),
            _ => Seen::Many,
        };
    }

    /// The value of a field seen exactly once.
    pub fn one(self) -> Option<&'a [u8]> {
        match self {
            Seen::One(value) => Some(value),
            _ => None,
        }
    }
}

/// The refusal fields of a head, gathered one field at a time (names compared without case).
#[derive(Debug, Clone, Copy)]
pub struct RefusalFields<'a> {
    error: Seen<'a>,
    origin: Seen<'a>,
    endpoint: Seen<'a>,
}

impl Default for RefusalFields<'_> {
    fn default() -> Self {
        RefusalFields {
            error: Seen::None,
            origin: Seen::None,
            endpoint: Seen::None,
        }
    }
}

impl<'a> RefusalFields<'a> {
    /// Takes a field of the head; any other name is ignored.
    pub fn add(&mut self, name: &[u8], value: &'a [u8]) {
        if name.eq_ignore_ascii_case(ERROR) {
            self.error.add(value);
        } else if name.eq_ignore_ascii_case(ORIGIN) {
            self.origin.add(value);
        } else if name.eq_ignore_ascii_case(ENDPOINT) {
            self.endpoint.add(value);
        }
    }

    /// The refusal of an answer of `status` with these fields: `503`, exactly one
    /// `x-mimic-error` of [`Reason::trusted_inbound`] and exactly one `x-mimic-endpoint` that
    /// parses as `<ipv4>:<port>` or `[<ipv6>]:<port>`. The origin is left to the reader.
    pub fn head(self, status: u16) -> Option<RefusalHead<'a>> {
        if status != 503 {
            return None;
        }
        let (error, written) = (self.error.one()?, self.endpoint.one()?);
        let endpoint = std::str::from_utf8(written)
            .ok()?
            .parse::<SocketAddr>()
            .ok()?;
        let reason = Reason::parse(error)?;
        Reason::trusted_inbound()
            .contains(&reason)
            .then_some(RefusalHead {
                reason,
                endpoint,
                origin: self.origin,
                written,
            })
    }
}

/// The refusal head of an answer of `status` whose head holds `fields` (see
/// [`RefusalFields::head`]).
pub fn refusal_head<'a>(
    status: u16,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Option<RefusalHead<'a>> {
    let mut seen = RefusalFields::default();
    for (name, value) in fields {
        seen.add(name, value);
    }
    seen.head(status)
}

/// A refusal head, not yet bound to a pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalHead<'a> {
    /// One of [`Reason::trusted_inbound`].
    pub reason: Reason,
    pub endpoint: SocketAddr,
    pub origin: Seen<'a>,
    /// The `x-mimic-endpoint` value as received.
    written: &'a [u8],
}

impl RefusalHead<'_> {
    /// Whether the endpoint is `addr`, written in its display form: `[fd00::7]:8080`, never
    /// `[fd00:0::7]:8080`. Compared as it is written, without allocating.
    pub fn displays(&self, addr: SocketAddr) -> bool {
        struct Rest<'b>(&'b [u8]);
        impl std::fmt::Write for Rest<'_> {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                self.0 = self.0.strip_prefix(s.as_bytes()).ok_or(std::fmt::Error)?;
                Ok(())
            }
        }
        let mut rest = Rest(self.written);
        self.endpoint == addr && write!(rest, "{addr}").is_ok() && rest.0.is_empty()
    }
}

/// An `x-mimic-origin` of an inbound sidecar (spec §4.1 "Refused app connections").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// `inbound`: the sidecar knows no pod name.
    Inbound,
    /// `inbound; pod=<name>`.
    Pod(&'a [u8]),
}

/// The inbound origin `value` is: `inbound`, or `inbound; pod=<name>` with a name of 1 to 253
/// characters `a-z 0-9 . -`; none for any other value.
pub fn origin(value: &[u8]) -> Option<Origin<'_>> {
    match value.strip_prefix(b"inbound")? {
        b"" => Some(Origin::Inbound),
        rest => rest
            .strip_prefix(b"; pod=")
            .filter(|name| {
                (1..=253).contains(&name.len())
                    && name.iter().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
                    })
            })
            .map(Origin::Pod),
    }
}

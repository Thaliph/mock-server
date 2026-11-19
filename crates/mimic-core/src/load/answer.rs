//! What a target's answer to a load request is (spec §7.8 "Generator"): the app's, a mode
//! refusal of the load marker, a trusted refusal of the target's inbound admission, or
//! unverified. Pure: the head's status and fields and the target are passed in.

use std::net::SocketAddr;

use super::summary::MimicError;
use crate::mesh::reason::Reason;

const MARKER: &[u8] = b"x-mimic-load";
const ERROR: &[u8] = b"x-mimic-error";
const ORIGIN: &[u8] = b"x-mimic-origin";
const ENDPOINT: &[u8] = b"x-mimic-endpoint";
/// The `x-mimic-origin` of an inbound sidecar, before the pod's name (spec §4.1).
const INBOUND_POD: &[u8] = b"inbound; pod=";

/// The target a load request went to: the pod of the targets answer, and the `ip:port` dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected<'a> {
    pub pod: &'a str,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// `x-mimic-load: ok`: the app's answer, counted by its status class.
    App,
    /// `x-mimic-load: refused; reason=mode`: the pod left mock.
    ModeLeft,
    /// A refusal of the target's inbound admission, bound to its pod and address.
    Inbound(MimicError),
    /// Anything else.
    Unverified,
}

/// One field seen none, once or more than once.
#[derive(Clone, Copy)]
enum Seen<'a> {
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

    fn one(self) -> Option<&'a [u8]> {
        match self {
            Seen::One(value) => Some(value),
            _ => None,
        }
    }
}

/// Classifies an answer of `status` whose head holds `fields` (every field, in order, a repeated
/// name once per value). Names are compared without case, values exactly but the marker's.
pub fn classify<'a>(
    status: u16,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    expected: &Expected<'_>,
) -> Answer {
    let mut marker = None;
    let (mut error, mut origin, mut endpoint) = (Seen::None, Seen::None, Seen::None);
    for (name, value) in fields {
        if name.eq_ignore_ascii_case(MARKER) {
            marker = marker.or(Some(value));
        } else if name.eq_ignore_ascii_case(ERROR) {
            error.add(value);
        } else if name.eq_ignore_ascii_case(ORIGIN) {
            origin.add(value);
        } else if name.eq_ignore_ascii_case(ENDPOINT) {
            endpoint.add(value);
        }
    }
    if let Some(value) = marker {
        return marked(value);
    }
    inbound(status, (error, origin, endpoint), expected).map_or(Answer::Unverified, Answer::Inbound)
}

/// The answer of the first `x-mimic-load` value: as `HeaderValue::to_str`, a byte outside
/// visible ASCII and tab reads as no value.
fn marked(value: &[u8]) -> Answer {
    if !value
        .iter()
        .all(|&b| b == b'\t' || (0x20..0x7f).contains(&b))
    {
        return Answer::Unverified;
    }
    let value = value.trim_ascii();
    if value == b"ok" {
        return Answer::App;
    }
    let Some(rest) = value.strip_prefix(b"refused") else {
        return Answer::Unverified;
    };
    let reason = rest
        .split(|&b| b == b';')
        .find_map(|part| part.trim_ascii().strip_prefix(b"reason="))
        .unwrap_or_default();
    if reason.trim_ascii() == b"mode" {
        Answer::ModeLeft
    } else {
        Answer::Unverified
    }
}

/// The trusted refusal: `503`, exactly one of each field, a trusted reason, the target's pod
/// and the address dialled, compared exactly.
fn inbound(
    status: u16,
    (error, origin, endpoint): (Seen<'_>, Seen<'_>, Seen<'_>),
    expected: &Expected<'_>,
) -> Option<MimicError> {
    if status != 503 {
        return None;
    }
    let (error, origin, endpoint) = (error.one()?, origin.one()?, endpoint.one()?);
    let refusal = MimicError::new(Reason::parse(error)?)?;
    if origin.strip_prefix(INBOUND_POD) != Some(expected.pod.as_bytes()) {
        return None;
    }
    (endpoint == expected.addr.to_string().as_bytes()).then_some(refusal)
}

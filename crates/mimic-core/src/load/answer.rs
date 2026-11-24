//! What a target's answer to a load request is (spec §7.8 "Generator"): the app's, a mode
//! refusal of the load marker, a trusted refusal of the target's inbound admission, or
//! unverified. Pure: the head's status and fields and the target are passed in.

use std::net::SocketAddr;

use super::summary::MimicError;
use crate::mesh::refusal::{Origin, RefusalFields, RefusalHead, origin};

/// The load marker of an answer (spec §7.8 "Sidecar"): [`OK`], or [`REFUSED`] and a reason.
pub const MARKER: &str = "x-mimic-load";
/// The marker of the app's answer.
pub const OK: &str = "ok";
/// The marker of a refusal, before its reason (read back more loosely, see [`classify`]).
pub const REFUSED: &str = "refused; reason=";

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

/// Classifies an answer of `status` whose head holds `fields` (every field, in order, a repeated
/// name once per value). Names are compared without case, values exactly but the marker's.
pub fn classify<'a>(
    status: u16,
    fields: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    expected: &Expected<'_>,
) -> Answer {
    let mut marker = None;
    let mut refusal = RefusalFields::default();
    for (name, value) in fields {
        if name.eq_ignore_ascii_case(MARKER.as_bytes()) {
            marker = marker.or(Some(value));
        } else {
            refusal.add(name, value);
        }
    }
    if let Some(value) = marker {
        return marked(value);
    }
    refusal
        .head(status)
        .and_then(|head| inbound(&head, expected))
        .map_or(Answer::Unverified, Answer::Inbound)
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
    if value == OK.as_bytes() {
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

/// The trusted refusal (see [`RefusalFields::head`]) bound to the target: exactly one
/// `x-mimic-origin` naming its pod, and the address dialled in its display form.
fn inbound(head: &RefusalHead<'_>, expected: &Expected<'_>) -> Option<MimicError> {
    let pod = Origin::Pod(expected.pod.as_bytes());
    if head.origin.one().and_then(origin) != Some(pod) || !head.displays(expected.addr) {
        return None;
    }
    MimicError::new(head.reason)
}

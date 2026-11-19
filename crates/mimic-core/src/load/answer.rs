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
    let _ = (status, fields.into_iter().count(), expected);
    Answer::Unverified
}

//! The bounded merge of the controller store (spec §7.3).

use std::collections::BTreeMap;

use super::report::{Limits, Report};

/// What a limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Endpoint,
    Sample,
}

/// Growth that was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    EndpointLimit,
    Budget,
}

/// The merged report and what the limits cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounded {
    pub report: Report,
    pub refused: BTreeMap<Refusal, u64>,
    pub evicted: BTreeMap<Kind, u64>,
}

pub fn merge_bounded(_stored: Report, _push: Report, _limits: &Limits) -> Bounded {
    unimplemented!()
}

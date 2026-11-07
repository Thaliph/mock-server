//! The start gate and the targets of a run (spec §7.8 "Lifecycle", "Targets").

use std::collections::BTreeMap;
use std::net::IpAddr;

use k8s_openapi::api::discovery::v1::EndpointSlice;

use crate::Mode;

/// A pod the generator may load.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub pod: String,
    pub ip: IpAddr,
}

/// Why a run waits, or that it may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Open,
    WaitingForMock,
    WaitingForTargets,
}

/// Ready endpoints of pods of `deployment`, one per pod, sorted by pod.
pub fn targets(slices: &[EndpointSlice], deployment: &str) -> Vec<Target> {
    todo!()
}

/// `pulls`: the `:pulls` hash, pod -> `<mode> <ms>`.
pub fn gate(
    mode: Mode,
    targets: &[Target],
    pulls: &BTreeMap<String, String>,
    now_ms: u64,
    window_ms: u64,
) -> Gate {
    todo!()
}

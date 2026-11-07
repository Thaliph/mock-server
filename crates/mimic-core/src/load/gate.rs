//! The start gate and the targets of a run (spec §7.8 "Lifecycle", "Targets").

use std::collections::BTreeMap;
use std::net::IpAddr;

use k8s_openapi::api::discovery::v1::EndpointSlice;

use crate::Mode;
use crate::identity::pod_of_deployment;

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
///
/// A pod listed twice (several Services, several addresses) keeps the lowest first address.
pub fn targets(slices: &[EndpointSlice], deployment: &str) -> Vec<Target> {
    let mut by_pod: BTreeMap<&str, IpAddr> = BTreeMap::new();
    for endpoint in slices.iter().flat_map(|s| &s.endpoints) {
        if endpoint.conditions.as_ref().and_then(|c| c.ready) == Some(false) {
            continue;
        }
        let Some(pod) = endpoint
            .target_ref
            .as_ref()
            .filter(|r| r.kind.as_deref() == Some("Pod"))
            .and_then(|r| r.name.as_deref())
            .filter(|name| pod_of_deployment(name, deployment))
        else {
            continue;
        };
        let Some(ip) = endpoint.addresses.first().and_then(|a| a.parse().ok()) else {
            continue;
        };
        by_pod
            .entry(pod)
            .and_modify(|known| *known = (*known).min(ip))
            .or_insert(ip);
    }
    by_pod
        .into_iter()
        .map(|(pod, ip)| Target {
            pod: pod.to_string(),
            ip,
        })
        .collect()
}

/// How far in the future a `:pulls` timestamp may be (clock skew between replicas).
const SKEW_MS: u64 = 60_000;

/// Whether `field` (`<mode> <ms>`) says `mock`, written within the window and not further
/// than `SKEW_MS` ahead of `now_ms`.
fn pulled_mock(field: &str, now_ms: u64, window_ms: u64) -> bool {
    field.split_once(' ').is_some_and(|(mode, at)| {
        mode == Mode::Mock.as_str()
            && at.parse::<u64>().is_ok_and(|at| {
                at <= now_ms.saturating_add(SKEW_MS) && now_ms.saturating_sub(at) <= window_ms
            })
    })
}

/// The start gate: effective mode `mock`, a target, and every target pod's `:pulls` field
/// (pod -> `<mode> <ms>`) says `mock` within the window.
pub fn gate(
    mode: Mode,
    targets: &[Target],
    pulls: &BTreeMap<String, String>,
    now_ms: u64,
    window_ms: u64,
) -> Gate {
    if mode != Mode::Mock {
        Gate::WaitingForMock
    } else if targets.is_empty() {
        Gate::WaitingForTargets
    } else if targets.iter().all(|t| {
        pulls
            .get(&t.pod)
            .is_some_and(|f| pulled_mock(f, now_ms, window_ms))
    }) {
        Gate::Open
    } else {
        Gate::WaitingForMock
    }
}

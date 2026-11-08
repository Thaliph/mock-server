//! Mode convergence (spec §7.4): how many pods follow the effective mode of their deployment.

use std::collections::BTreeMap;

use crate::Mode;

/// The pods of a deployment by what their last egress answer carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Convergence {
    /// Pods within the window whose last answer carried the effective mode.
    pub pods: u64,
    /// Pods within the window whose last answer carried the other mode.
    pub other_pods: u64,
    /// Fields that do not read as `<mode> <ms>` or lie too far in the future; in neither count.
    pub unreadable: u64,
}

/// How far in the future a `:pulls` timestamp may be (clock skew between replicas).
const SKEW_MS: u64 = 60_000;

/// Counts the `:pulls` fields (pod to `<mode> <ms>`): a field at or after `now_ms - window_ms`
/// and at most a minute ahead of `now_ms` counts for its mode; an older one is in neither count.
pub fn convergence(
    effective: Mode,
    pulls: &BTreeMap<String, String>,
    window_ms: u64,
    now_ms: u64,
) -> Convergence {
    let mut found = Convergence::default();
    for field in pulls.values() {
        let read = field.split_once(' ').and_then(|(mode, at)| {
            let mode = [Mode::Discovery, Mode::Mock]
                .into_iter()
                .find(|m| m.as_str() == mode)?;
            Some((mode, at.parse::<u64>().ok()?))
        });
        match read {
            Some((_, at)) if at > now_ms.saturating_add(SKEW_MS) => found.unreadable += 1,
            Some((_, at)) if now_ms.saturating_sub(at) > window_ms => {}
            Some((mode, _)) if mode == effective => found.pods += 1,
            Some(_) => found.other_pods += 1,
            None => found.unreadable += 1,
        }
    }
    found
}

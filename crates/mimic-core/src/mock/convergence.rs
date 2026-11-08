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

/// Counts the `:pulls` fields (pod to `<mode> <ms>`).
pub fn convergence(
    _effective: Mode,
    _pulls: &BTreeMap<String, String>,
    _window_ms: u64,
    _now_ms: u64,
) -> Convergence {
    Convergence::default()
}

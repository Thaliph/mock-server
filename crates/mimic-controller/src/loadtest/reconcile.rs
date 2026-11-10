//! One pass of the reconciler.

use super::{Reconciler, Test};

impl Reconciler {
    /// One pass at `now_ms` (Unix milliseconds).
    pub async fn tick(&self, _now_ms: u64) {}

    /// A `MimicLoadTest` was deleted: its Job goes with it (owner reference) and its slot is
    /// released.
    pub async fn deleted(&self, _test: &Test) {}
}

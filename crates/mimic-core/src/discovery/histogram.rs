//! Latency histogram with fixed log buckets (spec §6, DSC-10): 4 per doubling from 1 ms to
//! 131 072 ms, plus an overflow bucket. Sparse, integer-only, so merging is exact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Index of the overflow bucket (values above 131 072 ms).
pub const OVERFLOW: u8 = 69;

/// Bucket of a latency in milliseconds. Bucket `i` closes at `2^(i/4)` ms (bucket 0 holds
/// 0 and 1 ms); the overflow bucket is `OVERFLOW`.
pub fn bucket_index(_ms: u64) -> u8 {
    todo!()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    buckets: BTreeMap<u8, u64>,
}

impl Histogram {
    pub fn observe(&mut self, _ms: u64) {
        todo!()
    }

    pub fn merge(&mut self, _other: &Histogram) {
        todo!()
    }

    /// Non-empty buckets, by index.
    pub fn buckets(&self) -> &BTreeMap<u8, u64> {
        &self.buckets
    }

    pub fn count(&self) -> u64 {
        self.buckets.values().sum()
    }
}

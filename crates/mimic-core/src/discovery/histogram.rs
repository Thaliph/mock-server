//! Latency histogram with fixed log buckets (spec §6, DSC-10): 4 per doubling from 1 ms to
//! 131 072 ms, plus an overflow bucket. Sparse, integer-only, so merging is exact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::check::Violation;

/// Index of the overflow bucket (values above 131 072 ms).
pub const OVERFLOW: u8 = 69;

/// Bucket of a latency in milliseconds. Bucket `i` closes at `2^(i/4)` ms (bucket 0 holds
/// 0 and 1 ms); the overflow bucket is `OVERFLOW`.
pub fn bucket_index(ms: u64) -> u8 {
    if ms <= 1 {
        return 0;
    }
    if ms > 1 << 17 {
        return OVERFLOW;
    }
    // Smallest i with ms <= 2^(i/4), i.e. ms^4 <= 2^i, in exact integer arithmetic.
    let fourth = u128::from(ms).pow(4);
    let bits = 128 - (fourth - 1).leading_zeros();
    bits as u8
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Histogram {
    buckets: BTreeMap<u8, u64>,
}

impl Histogram {
    pub fn observe(&mut self, ms: u64) {
        let n = self.buckets.entry(bucket_index(ms)).or_default();
        *n = n.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (bucket, n) in &other.buckets {
            let mine = self.buckets.entry(*bucket).or_default();
            *mine = mine.saturating_add(*n);
        }
    }

    /// Non-empty buckets, by index.
    pub fn buckets(&self) -> &BTreeMap<u8, u64> {
        &self.buckets
    }

    pub fn count(&self) -> u64 {
        self.buckets
            .values()
            .fold(0, |n, c| u64::saturating_add(n, *c))
    }

    /// Every bucket index is one of the fixed buckets.
    pub(super) fn check(&self) -> Result<(), Violation> {
        match self.buckets.keys().any(|b| *b > OVERFLOW) {
            true => Err(Violation::Bucket),
            false => Ok(()),
        }
    }
}

//! Latency histogram with fixed log buckets (spec §6, DSC-10): 4 per doubling from 1 ms to
//! 131 072 ms, plus an overflow bucket. Sparse, integer-only, so merging is exact.

use crate::compact::SortedMap;

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
    buckets: SortedMap<u8, u64>,
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
    pub fn buckets(&self) -> &SortedMap<u8, u64> {
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

/// Where a quantile falls: the upper bound of its bucket, or the overflow bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Upper bound of the bucket in milliseconds, rounded up.
    Ms(u64),
    Overflow,
}

impl Histogram {
    /// The `percent` quantile (1..=100): the bucket holding the observation of rank
    /// `ceil(count * percent / 100)`. `None` without data.
    pub fn quantile(&self, percent: u64) -> Option<Bound> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let rank = (u128::from(count) * u128::from(percent.clamp(1, 100)))
            .div_ceil(100)
            .max(1);
        let mut seen = 0u128;
        for (bucket, n) in &self.buckets {
            seen += u128::from(*n);
            if seen >= rank {
                return Some(match *bucket {
                    b if b >= OVERFLOW => Bound::Overflow,
                    b => Bound::Ms(upper_bound(b)),
                });
            }
        }
        Some(Bound::Overflow)
    }
}

/// A delay in the bucket it falls in, as that bucket's largest whole number of milliseconds
/// (spec §6.4): 0 stays 0, a delay over 131 072 ms is 131 072. Idempotent, and never below its
/// input up to 131 072, so a sample shows the pace of a stream and no exact time.
pub fn bucketed(ms: u64) -> u64 {
    match ms {
        0 => 0,
        m if m > 1 << 17 => 1 << 17,
        m => {
            let bucket = bucket_index(m);
            // The largest `x` with `x^4 <= 2^bucket`.
            let limit = 1u128 << bucket;
            let (mut low, mut high) = (1u64, 1u64 << 17);
            while low < high {
                let mid = low + (high - low).div_ceil(2);
                if u128::from(mid).pow(4) <= limit {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            low
        }
    }
}

/// Upper bound of bucket `i` in milliseconds, rounded up: the smallest `m` with `m^4 >= 2^i`.
fn upper_bound(bucket: u8) -> u64 {
    let (mut low, mut high) = (1u64, 1 << 17);
    while low < high {
        let mid = low + (high - low) / 2;
        if u128::from(mid).pow(4) >= 1u128.checked_shl(u32::from(bucket)).unwrap_or(u128::MAX) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

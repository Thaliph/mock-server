//! Latency histogram (spec §7.8 "Results"): 16 buckets per doubling.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Buckets per doubling: a bound is at most 2^(1/16), 4.4 % above the values it holds.
const PER_DOUBLING: u32 = 16;
/// The last bucket with a bound: the first one whose bound reaches 300 s. Slower values go in
/// the overflow bucket, reported with this bound.
const LAST: u16 = 451;
const OVERFLOW: u16 = LAST + 1;

/// Upper bounds, in microseconds, of buckets `0..=LAST`: `ceil(2^(b/16))`.
fn bounds() -> &'static [u64] {
    static BOUNDS: OnceLock<Vec<u64>> = OnceLock::new();
    BOUNDS.get_or_init(|| {
        (0..=u32::from(LAST))
            .map(|b| {
                if b % PER_DOUBLING == 0 {
                    1u64 << (b / PER_DOUBLING)
                } else {
                    (f64::from(b) / f64::from(PER_DOUBLING)).exp2().ceil() as u64
                }
            })
            .collect()
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Histogram {
    counts: BTreeMap<u16, u64>,
}

impl Histogram {
    /// The bucket of a latency in microseconds.
    pub fn bucket(micros: u64) -> u16 {
        let at = bounds().partition_point(|bound| *bound < micros);
        u16::try_from(at).map_or(OVERFLOW, |b| b.min(OVERFLOW))
    }

    /// The upper bound of a bucket, in microseconds.
    pub fn bound(bucket: u16) -> u64 {
        let b = &bounds();
        b[usize::from(bucket.min(LAST))]
    }

    pub fn record(&mut self, micros: u64) {
        let n = self.counts.entry(Self::bucket(micros)).or_default();
        *n = n.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (bucket, count) in &other.counts {
            let n = self.counts.entry((*bucket).min(OVERFLOW)).or_default();
            *n = n.saturating_add(*count);
        }
    }

    pub fn count(&self) -> u64 {
        self.counts.values().fold(0, |n, c| n.saturating_add(*c))
    }

    /// The upper bound of the bucket holding the `percent`-th percentile; 0 when empty.
    pub fn percentile(&self, percent: u64) -> u64 {
        let total = self.count();
        let rank = (u128::from(percent.min(100)) * u128::from(total))
            .div_ceil(100)
            .max(1);
        let mut seen = 0u128;
        for (bucket, count) in &self.counts {
            seen += u128::from(*count);
            if seen >= rank {
                return Self::bound(*bucket);
            }
        }
        0
    }

    /// The upper bound of the highest bucket; 0 when empty.
    pub fn max(&self) -> u64 {
        self.counts
            .keys()
            .next_back()
            .map_or(0, |b| Self::bound(*b))
    }
}

//! Latency histogram (spec §7.8 "Results"): 16 buckets per doubling.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Histogram {
    counts: BTreeMap<u16, u64>,
}

impl Histogram {
    /// The bucket of a latency in microseconds.
    pub fn bucket(micros: u64) -> u16 {
        todo!()
    }

    /// The upper bound of a bucket, in microseconds.
    pub fn bound(bucket: u16) -> u64 {
        todo!()
    }

    pub fn record(&mut self, micros: u64) {
        todo!()
    }

    pub fn merge(&mut self, other: &Histogram) {
        todo!()
    }

    pub fn count(&self) -> u64 {
        todo!()
    }

    /// The upper bound of the bucket holding the `percent`-th percentile; 0 when empty.
    pub fn percentile(&self, percent: u64) -> u64 {
        todo!()
    }

    /// The upper bound of the highest bucket; 0 when empty.
    pub fn max(&self) -> u64 {
        todo!()
    }
}

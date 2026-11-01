//! The discovery store (spec §7.3): the only code that reads or writes the `mimic:v1:` keys.
// Called by the push route (step 4 of F5).
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use mimic_core::discovery::report::{Limits, Report};
use prometheus_client::registry::Registry;

/// A namespace, deployment or pod name that is not DNS-1123.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError;

/// The keys of one deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    pub report: String,
    pub meta: String,
    pub pods: String,
    pub seq: String,
    pub ns_index: String,
    pub workloads: String,
}

pub fn keys(_namespace: &str, _deployment: &str) -> Result<Keys, NameError> {
    unimplemented!()
}

/// A session and sequence that passed the format check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqId {
    pub session: String,
    pub sequence: u64,
}

impl SeqId {
    pub fn parse(_session: &str, _sequence: u64) -> Result<SeqId, NameError> {
        unimplemented!()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Apply,
    Duplicate,
}

/// `stored`: the last applied sequence of the pod and session; `sequence`: the push's.
pub fn decide(_stored: Option<u64>, _sequence: Option<u64>) -> Decision {
    unimplemented!()
}

/// A checked, re-redacted and stamped report with who sent it.
pub struct Push {
    pub pod: String,
    pub seq: Option<SeqId>,
    pub report: Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// Invalid name: `400`.
    Name,
    /// Too many deployments in the namespace: `507`.
    Deployments,
    /// Valkey unavailable, too many conflicts or an unreadable stored value: `503`.
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub ttl: Duration,
    pub limits: Limits,
    pub max_deployments: usize,
}

#[derive(Clone, Default)]
pub struct StoreMetrics;

impl StoreMetrics {
    pub fn register(_registry: &mut Registry) -> StoreMetrics {
        unimplemented!()
    }
}

pub struct Store;

impl Store {
    pub fn new(_client: redis::Client, _config: StoreConfig, _metrics: StoreMetrics) -> Arc<Store> {
        unimplemented!()
    }

    pub async fn push(
        self: &Arc<Self>,
        _namespace: &str,
        _deployment: &str,
        _push: Push,
    ) -> Result<Outcome, StoreError> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests;

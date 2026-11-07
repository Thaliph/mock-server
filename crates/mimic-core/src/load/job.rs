//! The objects of a run: the generator Job and its NetworkPolicy (spec §7.8 "Generator").

use std::collections::BTreeMap;

use serde_json::Value;

/// The name of a run's Job: `mimic-load-<first 12 hex of the uid>`.
pub fn job_name(uid: &str) -> String {
    todo!()
}

pub struct JobInput<'a> {
    pub namespace: &'a str,
    /// The `MimicLoadTest`.
    pub name: &'a str,
    pub uid: &'a str,
    pub deployment: &'a str,
    pub concurrency: u64,
    pub duration_ms: u64,
    pub pod_concurrency: u64,
    pub start_timeout_ms: u64,
    pub report_interval_ms: u64,
    pub max_plan: usize,
    pub image: &'a str,
    pub resources: &'a Value,
    pub controller_url: &'a str,
    pub controller_ca: &'a str,
    pub secret_name: Option<&'a str>,
}

pub fn job(input: &JobInput<'_>) -> Value {
    todo!()
}

pub struct PolicyInput<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub uid: &'a str,
    pub deployment: &'a str,
    /// Ports of the plan's endpoints.
    pub ports: &'a [u16],
    pub controller_namespace: &'a str,
    pub controller_labels: &'a BTreeMap<String, String>,
    pub controller_port: u16,
    pub dns_namespace: &'a str,
    pub dns_labels: &'a BTreeMap<String, String>,
    /// `loadTest.metricsFrom`: NetworkPolicy peers allowed on the admin port.
    pub metrics_from: &'a [Value],
}

pub fn network_policy(input: &PolicyInput<'_>) -> Value {
    todo!()
}

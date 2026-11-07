//! Validation of a test's parameters against the controller's limits (spec §7.8 "Lifecycle").

/// The numbers of a `MimicLoadTest` spec, durations in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub concurrency: u64,
    pub rate: Option<u64>,
    pub duration_ms: u64,
    pub ramp_up_ms: u64,
    pub timeout_ms: u64,
}

/// `MIMIC_LOAD_MAX_CONCURRENCY`, `MIMIC_LOAD_MAX_RATE`, `MIMIC_LOAD_MAX_DURATION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_concurrency: u64,
    pub max_rate: u64,
    pub max_duration_ms: u64,
}

/// The field at fault and a message naming it and the limit; no value from the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    pub message: String,
}

pub fn validate(params: &Params, limits: &Limits) -> Result<(), Invalid> {
    todo!()
}

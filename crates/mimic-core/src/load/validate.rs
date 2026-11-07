//! Validation of a test's parameters against the controller's limits (spec §7.8 "Lifecycle").

/// The CRD's floor of `spec.duration`.
const MIN_DURATION_MS: u64 = 10_000;
/// The CRD's bounds of `spec.timeout`.
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 60_000;

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

/// `10s`, `1500ms`.
fn show(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

fn invalid(field: &'static str, message: String) -> Result<(), Invalid> {
    Err(Invalid { field, message })
}

/// Checks the CRD's floors and the controller's `MIMIC_LOAD_*` limits; the first field at fault.
pub fn validate(params: &Params, limits: &Limits) -> Result<(), Invalid> {
    if !(1..=limits.max_concurrency).contains(&params.concurrency) {
        return invalid(
            "spec.concurrency",
            format!(
                "spec.concurrency must be 1 to {} (MIMIC_LOAD_MAX_CONCURRENCY)",
                limits.max_concurrency
            ),
        );
    }
    if params
        .rate
        .is_some_and(|r| !(1..=limits.max_rate).contains(&r))
    {
        return invalid(
            "spec.rate",
            format!(
                "spec.rate must be 1 to {} (MIMIC_LOAD_MAX_RATE)",
                limits.max_rate
            ),
        );
    }
    if !(MIN_DURATION_MS..=limits.max_duration_ms).contains(&params.duration_ms) {
        return invalid(
            "spec.duration",
            format!(
                "spec.duration must be {} to {} (MIMIC_LOAD_MAX_DURATION)",
                show(MIN_DURATION_MS),
                show(limits.max_duration_ms)
            ),
        );
    }
    if params.ramp_up_ms > params.duration_ms {
        return invalid(
            "spec.rampUp",
            "spec.rampUp must not exceed spec.duration".to_string(),
        );
    }
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&params.timeout_ms) {
        return invalid(
            "spec.timeout",
            format!(
                "spec.timeout must be {} to {}",
                show(MIN_TIMEOUT_MS),
                show(MAX_TIMEOUT_MS)
            ),
        );
    }
    Ok(())
}

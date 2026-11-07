//! The schedule of a run (spec §7.8 "Schedule").

/// What one generator pod runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Share {
    pub concurrency: u64,
    pub rate: Option<u64>,
}

/// Generator pods of a run: `ceil(concurrency / pod_concurrency)`.
pub fn pods(concurrency: u64, pod_concurrency: u64) -> u64 {
    todo!()
}

/// The share of each pod, remainders to the lowest indices.
pub fn split(concurrency: u64, rate: Option<u64>, pod_concurrency: u64) -> Vec<Share> {
    todo!()
}

/// Sends due by `elapsed_ms` in the open model, cumulative.
pub fn sends_due(rate: u64, ramp_up_ms: u64, duration_ms: u64, elapsed_ms: u64) -> u64 {
    todo!()
}

/// Closed model workers at `elapsed_ms`: 0 from `duration_ms` on.
pub fn workers(concurrency: u64, ramp_up_ms: u64, duration_ms: u64, elapsed_ms: u64) -> u64 {
    todo!()
}

/// What to do at one tick of an open-model pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub send: u64,
    pub missed: u64,
}

/// Open model pacer of one pod.
#[derive(Debug, Clone)]
pub struct Pacer {
    rate: u64,
    ramp_up_ms: u64,
    duration_ms: u64,
    concurrency: u64,
    issued: u64,
}

impl Pacer {
    pub fn new(rate: u64, ramp_up_ms: u64, duration_ms: u64, concurrency: u64) -> Self {
        Pacer {
            rate,
            ramp_up_ms,
            duration_ms,
            concurrency,
            issued: 0,
        }
    }

    /// Sends due since the last tick; those that find the concurrency in flight are missed.
    pub fn tick(&mut self, elapsed_ms: u64, in_flight: u64) -> Tick {
        todo!()
    }
}

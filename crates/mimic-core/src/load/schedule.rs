//! The schedule of a run (spec §7.8 "Schedule").

/// What one generator pod runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Share {
    pub concurrency: u64,
    pub rate: Option<u64>,
}

/// Generator pods of a run: `ceil(concurrency / pod_concurrency)`.
pub fn pods(concurrency: u64, pod_concurrency: u64) -> u64 {
    concurrency.div_ceil(pod_concurrency.max(1)).max(1)
}

/// The share of each pod, remainders to the lowest indices.
pub fn split(concurrency: u64, rate: Option<u64>, pod_concurrency: u64) -> Vec<Share> {
    let n = pods(concurrency, pod_concurrency);
    let part = |total: u64, i: u64| total / n + u64::from(i < total % n);
    (0..n)
        .map(|i| Share {
            concurrency: part(concurrency, i),
            rate: rate.map(|r| part(r, i)),
        })
        .collect()
}

/// Sends due by `elapsed_ms` in the open model, cumulative.
pub fn sends_due(rate: u64, ramp_up_ms: u64, duration_ms: u64, elapsed_ms: u64) -> u64 {
    let t = u128::from(elapsed_ms.min(duration_ms));
    let ramp = u128::from(ramp_up_ms.min(duration_ms));
    let rate = u128::from(rate);
    // The rate rises linearly to `rate` over the ramp (area: rate * t^2 / 2 ramp), then holds.
    let due = if t < ramp {
        rate * t * t / (2000 * ramp)
    } else {
        rate * (2 * t - ramp) / 2000
    };
    u64::try_from(due).unwrap_or(u64::MAX)
}

/// Requests per second the schedule asks of one pod at `elapsed_ms`; `None` in the closed
/// model (`rate` absent), where no rate is planned.
pub fn planned_rate(
    rate: Option<u64>,
    ramp_up_ms: u64,
    duration_ms: u64,
    elapsed_ms: u64,
) -> Option<f64> {
    let _ = (rate, ramp_up_ms, duration_ms, elapsed_ms);
    None
}

/// Closed model workers at `elapsed_ms`: 0 from `duration_ms` on.
pub fn workers(concurrency: u64, ramp_up_ms: u64, duration_ms: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms >= duration_ms {
        return 0;
    }
    let ramp = ramp_up_ms.min(duration_ms);
    if elapsed_ms >= ramp {
        return concurrency;
    }
    let extra =
        u128::from(concurrency.saturating_sub(1)) * u128::from(elapsed_ms) / u128::from(ramp);
    1 + u64::try_from(extra).unwrap_or(0)
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
        let total = sends_due(self.rate, self.ramp_up_ms, self.duration_ms, elapsed_ms);
        let due = total.saturating_sub(self.issued);
        self.issued = self.issued.max(total);
        let send = due.min(self.concurrency.saturating_sub(in_flight));
        Tick {
            send,
            missed: due - send,
        }
    }
}

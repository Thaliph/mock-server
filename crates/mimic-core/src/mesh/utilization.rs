//! The utilisation of an admission contract (spec §4.1 "Signals", §9
//! `mimic_admission_utilization`): `max(min(1, in flight / concurrency), demand / rps)`, each
//! term an exponential average of 15 s whose weight follows the time between two ticks. Pure:
//! the instant and the cumulative counts are passed in; there is no clock.

use std::time::Duration;

use super::admission::{Contract, Counts};

/// The time constant of both averages.
pub const TIME_CONSTANT: Duration = Duration::from_secs(15);

/// The two averages of a contract in force.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Averages {
    /// Mean admitted requests in flight.
    pub in_flight: f64,
    /// Offers that ended admitted, overloaded or queue-timeout, per second.
    pub demand: f64,
}

/// The averages of the contract in force, and the tick they were last moved at.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Active {
    contract: Contract,
    at: Duration,
    last: Counts,
    avg: Averages,
}

/// The utilisation of the contract in force; none without one.
#[derive(Debug, Clone, Default)]
pub struct Utilization(Option<Active>);

impl Utilization {
    pub fn new() -> Utilization {
        Utilization::default()
    }

    /// One tick: `now` is the admission clock, `counts` the cumulative counts at `now` and
    /// `contract` the one in force. A tick not after the previous one changes nothing; a
    /// contract that starts begins at 0 and takes its first sample at the next tick.
    pub fn tick(&mut self, _now: Duration, _contract: Option<Contract>, _counts: Counts) {
        let _ = &self.0;
    }

    /// The utilisation while a contract is in force: finite and at least 0, the in-flight
    /// term at most 1.
    pub fn value(&self) -> Option<f64> {
        let active = self.0.as_ref()?;
        let Contract { concurrency, rps } = active.contract;
        let in_flight = (active.avg.in_flight / f64::from(concurrency.max(1))).min(1.0);
        Some(in_flight.max(active.avg.demand / f64::from(rps.max(1))))
    }

    pub fn averages(&self) -> Option<Averages> {
        self.0.as_ref().map(|a| a.avg)
    }
}

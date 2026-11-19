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
    pub fn tick(&mut self, now: Duration, contract: Option<Contract>, counts: Counts) {
        let Some(contract) = contract else {
            self.0 = None;
            return;
        };
        let Some(active) = self.0.as_mut() else {
            self.0 = Some(Active {
                contract,
                at: now,
                last: counts,
                avg: Averages {
                    in_flight: 0.0,
                    demand: 0.0,
                },
            });
            return;
        };
        if now <= active.at {
            return;
        }
        let dt = (now - active.at).as_secs_f64();
        let alpha = -f64::exp_m1(-dt / TIME_CONSTANT.as_secs_f64());
        let last = &active.last;
        let in_flight = counts.in_flight_ns.saturating_sub(last.in_flight_ns) as f64 / (dt * 1e9);
        // `queued` and `cancelled` are not demand: a queued offer ends in one of the others.
        let offers = [
            counts
                .admitted_at_once
                .saturating_sub(last.admitted_at_once),
            counts
                .admitted_from_queue
                .saturating_sub(last.admitted_from_queue),
            counts.overloaded.saturating_sub(last.overloaded),
            counts.queue_timeout.saturating_sub(last.queue_timeout),
        ];
        let demand = offers.iter().map(|&n| n as f64).sum::<f64>() / dt;
        let avg = &mut active.avg;
        avg.in_flight += alpha * (in_flight - avg.in_flight);
        avg.demand += alpha * (demand - avg.demand);
        active.contract = contract;
        active.at = now;
        active.last = counts;
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

//! Admission contract of one pod (spec §4.1 "Admission contract", §7 `spec.admission`): at most
//! `concurrency` requests in flight and `rps` admissions per second, a FIFO queue of
//! `⌈concurrency / 10⌉` places where a request waits at most [`QUEUE_WAIT`], and the counts
//! #454 reads. Pure: the sidecar's monotonic clock is passed in; there is no I/O and no random
//! source. A clock that goes back counts as the latest instant seen and frees nothing.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Largest `admission.concurrency` (the smallest is 1).
pub const MAX_CONCURRENCY: u32 = 100_000;
/// Largest `admission.rps` (the smallest is 1).
pub const MAX_RPS: u32 = 1_000_000;
/// The longest wait in the queue (owner decision, not configurable).
pub const QUEUE_WAIT: Duration = Duration::from_millis(150);

/// One token of the bucket, in nano-tokens.
const UNIT: u64 = 1_000_000_000;

/// `spec.admission` (§7); step 2 makes it the view's `admission`.
/// JSON (sorted keys): `{"concurrency":64,"rps":200}`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub concurrency: u32,
    pub rps: u32,
}

impl Contract {
    pub fn in_bounds(&self) -> bool {
        todo!()
    }

    pub fn burst(&self) -> u32 {
        todo!()
    }

    pub fn queue_size(&self) -> u32 {
        todo!()
    }
}

/// A queued offer; increasing, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(u64);

/// One admitted request. Not `Clone`: it is released at most once. Dropping it releases
/// nothing (the state is pure): the sidecar's `Slot` calls [`Admission::release`], also for an
/// admitted entry whose waiter left.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Permit {
    epoch: u64,
}

/// Why a request was refused (the `x-mimic-error` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The queue was full at the offer.
    Overloaded,
    /// The request waited [`QUEUE_WAIT`] in the queue.
    QueueTimeout,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Overloaded => "overloaded",
            Refusal::QueueTimeout => "queue-timeout",
        }
    }
}

/// What [`Admission::offer`] decided. `Refused` is `Overloaded` only.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    Admitted(Permit),
    Queued(Ticket),
    Refused(Refusal),
}

/// How a queued entry ended. `Refused` is `QueueTimeout` only.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Admitted(Permit),
    Refused(Refusal),
}

/// A queued entry that a call resolved.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub ticket: Ticket,
    pub verdict: Verdict,
}

/// ADM-07, the input of #454. Every field is cumulative except the first two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub in_flight: u32,
    pub depth: u32,
    pub admitted_at_once: u64,
    pub queued: u64,
    pub admitted_from_queue: u64,
    pub overloaded: u64,
    pub queue_timeout: u64,
    pub cancelled: u64,
    /// ∫ in_flight dt, in nanoseconds.
    pub in_flight_ns: u128,
}

/// One place of the queue (24 bytes).
#[derive(Debug, Clone)]
struct Entry {
    ticket: u64,
    deadline: Duration,
}

/// The admission state of one pod.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    contract: Option<Contract>,
    /// Changes with each presence change of the contract: older permits release nothing.
    epoch: u64,
    in_flight: u32,
    /// The bucket, in nano-tokens.
    level: u64,
    /// The latest instant seen.
    last: Duration,
    /// Deadlines never decrease, tickets increase.
    queue: VecDeque<Entry>,
    next_ticket: u64,
    /// The cumulative fields (`in_flight` and `depth` are read from the state).
    counts: Counts,
}

impl Admission {
    pub fn new() -> Admission {
        Admission::default()
    }

    pub fn contract(&self) -> Option<Contract> {
        todo!()
    }

    pub fn set_contract(&mut self, _c: Option<Contract>, _now: Duration, _out: &mut Vec<Resolved>) {
        todo!()
    }

    pub fn offer(&mut self, _now: Duration, _out: &mut Vec<Resolved>) -> Offer {
        todo!()
    }

    pub fn release(&mut self, _permit: Permit, _now: Duration, _out: &mut Vec<Resolved>) {
        todo!()
    }

    pub fn cancel(&mut self, _ticket: Ticket) -> bool {
        todo!()
    }

    pub fn apply(&mut self, _now: Duration, _out: &mut Vec<Resolved>) {
        todo!()
    }

    pub fn next_instant(&self) -> Option<Duration> {
        todo!()
    }

    pub fn counts(&self, _now: Duration) -> Counts {
        todo!()
    }
}

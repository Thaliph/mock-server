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
    /// Whether every field is inside its bound (the reader of a view uses it).
    pub fn in_bounds(&self) -> bool {
        (1..=MAX_CONCURRENCY).contains(&self.concurrency) && (1..=MAX_RPS).contains(&self.rps)
    }

    /// The tokens the bucket holds at most: `max(1, ⌈rps / 10⌉)`.
    pub fn burst(&self) -> u32 {
        self.rps.div_ceil(10).max(1)
    }

    /// The places of the queue: `max(1, ⌈concurrency / 10⌉)`.
    pub fn queue_size(&self) -> u32 {
        self.concurrency.div_ceil(10).max(1)
    }

    /// The contract with each field brought into its bound, so that no value divides by zero.
    fn clamped(self) -> Contract {
        Contract {
            concurrency: self.concurrency.clamp(1, MAX_CONCURRENCY),
            rps: self.rps.clamp(1, MAX_RPS),
        }
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
    /// No contract: every offer is admitted.
    pub fn new() -> Admission {
        Admission::default()
    }

    pub fn contract(&self) -> Option<Contract> {
        self.contract
    }

    /// Follows the view's `admission`. A new contract starts with in-flight 0 and a full
    /// bucket; new values keep the requests in flight and the entries queued, with the tokens
    /// capped at the new burst; removing the contract admits every entry still queued.
    pub fn set_contract(&mut self, c: Option<Contract>, now: Duration, out: &mut Vec<Resolved>) {
        self.advance(now);
        self.apply_queue(out);
        match (self.contract, c.map(Contract::clamped)) {
            (None, None) => {}
            (Some(old), Some(new)) if old == new => {}
            (Some(_), Some(new)) => {
                self.contract = Some(new);
                self.level = self.level.min(u64::from(new.burst()) * UNIT);
                self.apply_queue(out);
            }
            (None, Some(new)) => {
                self.epoch = self.epoch.wrapping_add(1);
                self.in_flight = 0;
                self.level = u64::from(new.burst()) * UNIT;
                self.contract = Some(new);
            }
            (Some(_), None) => {
                let epoch = self.epoch.wrapping_add(1);
                for entry in self.queue.drain(..) {
                    self.counts.admitted_from_queue += 1;
                    out.push(Resolved {
                        ticket: Ticket(entry.ticket),
                        verdict: Verdict::Admitted(Permit { epoch }),
                    });
                }
                self.epoch = epoch;
                self.in_flight = 0;
                self.contract = None;
            }
        }
    }

    /// A request whose head is whole. The queue is applied first, so a new request never
    /// passes one still waiting.
    pub fn offer(&mut self, now: Duration, out: &mut Vec<Resolved>) -> Offer {
        self.advance(now);
        self.apply_queue(out);
        let Some(contract) = self.contract else {
            self.counts.admitted_at_once += 1;
            return Offer::Admitted(Permit { epoch: self.epoch });
        };
        if self.queue.is_empty() && self.can_admit(contract) {
            self.take_slot();
            self.counts.admitted_at_once += 1;
            return Offer::Admitted(Permit { epoch: self.epoch });
        }
        if self.queue.len() >= contract.queue_size() as usize {
            self.counts.overloaded += 1;
            return Offer::Refused(Refusal::Overloaded);
        }
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.saturating_add(1);
        self.queue.push_back(Entry {
            ticket,
            deadline: self.last.saturating_add(QUEUE_WAIT),
        });
        self.counts.queued += 1;
        Offer::Queued(Ticket(ticket))
    }

    /// The end of an admitted request. A permit of an earlier contract releases nothing.
    pub fn release(&mut self, permit: Permit, now: Duration, out: &mut Vec<Resolved>) {
        self.advance(now);
        if self.contract.is_some() && permit.epoch == self.epoch {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.apply_queue(out);
    }

    /// The client left while queued: `true` when the ticket was still queued (its place is
    /// freed), `false` when it was resolved or is unknown.
    pub fn cancel(&mut self, ticket: Ticket) -> bool {
        match self.queue.binary_search_by_key(&ticket.0, |e| e.ticket) {
            Ok(index) => {
                self.queue.remove(index);
                self.counts.cancelled += 1;
                true
            }
            Err(_) => false,
        }
    }

    /// Resolves the entries due at `now`: the expired ones, then the admissible ones.
    pub fn apply(&mut self, now: Duration, out: &mut Vec<Resolved>) {
        self.advance(now);
        self.apply_queue(out);
    }

    /// The next instant at which a decision can change: the next token while the head of the
    /// queue waits for one, else its deadline; `None` for an empty queue. Always later than the
    /// latest instant seen.
    pub fn next_instant(&self) -> Option<Duration> {
        let head = self.queue.front()?;
        match self.contract {
            Some(contract) if self.level < UNIT => {
                let wait = (UNIT - self.level).div_ceil(u64::from(contract.rps));
                let token = self.last.saturating_add(Duration::from_nanos(wait));
                Some(token.min(head.deadline))
            }
            _ => Some(head.deadline),
        }
    }

    /// The counts at `now` (the integral is extended to it); reads only.
    pub fn counts(&self, now: Duration) -> Counts {
        let elapsed = now.saturating_sub(self.last).as_nanos();
        Counts {
            in_flight: self.in_flight,
            depth: u32::try_from(self.queue.len()).unwrap_or(u32::MAX),
            in_flight_ns: self
                .counts
                .in_flight_ns
                .saturating_add(u128::from(self.in_flight) * elapsed),
            ..self.counts
        }
    }

    /// Moves the clock to `max(now, last)`: the bucket refills and the integral grows.
    fn advance(&mut self, now: Duration) {
        let elapsed = now.saturating_sub(self.last).as_nanos();
        if elapsed == 0 {
            return;
        }
        self.counts.in_flight_ns = self
            .counts
            .in_flight_ns
            .saturating_add(u128::from(self.in_flight) * elapsed);
        if let Some(contract) = self.contract {
            let full = u128::from(contract.burst()) * u128::from(UNIT);
            let level = u128::from(self.level).saturating_add(elapsed * u128::from(contract.rps));
            self.level = u64::try_from(level.min(full)).unwrap_or(u64::MAX);
        }
        self.last = now;
    }

    /// The queue at `last`: the expired heads time out, then the heads are admitted while a
    /// slot and a token are free. Only the head is read.
    fn apply_queue(&mut self, out: &mut Vec<Resolved>) {
        while let Some(head) = self.queue.front()
            && head.deadline <= self.last
        {
            let ticket = Ticket(head.ticket);
            self.queue.pop_front();
            self.counts.queue_timeout += 1;
            out.push(Resolved {
                ticket,
                verdict: Verdict::Refused(Refusal::QueueTimeout),
            });
        }
        let Some(contract) = self.contract else {
            return;
        };
        while !self.queue.is_empty() && self.can_admit(contract) {
            let Some(head) = self.queue.pop_front() else {
                break;
            };
            self.take_slot();
            self.counts.admitted_from_queue += 1;
            out.push(Resolved {
                ticket: Ticket(head.ticket),
                verdict: Verdict::Admitted(Permit { epoch: self.epoch }),
            });
        }
    }

    fn can_admit(&self, contract: Contract) -> bool {
        self.in_flight < contract.concurrency && self.level >= UNIT
    }

    fn take_slot(&mut self) {
        self.in_flight += 1;
        self.level -= UNIT;
    }
}

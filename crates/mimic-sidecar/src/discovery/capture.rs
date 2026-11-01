//! The capture tap of the HTTP/1 relay (spec §6): heads and body prefixes of an exchange are
//! copied into a [`Capture`] within a global byte budget, then handed to the recorder thread
//! through a bounded queue. Nothing here waits: what does not fit is dropped and counted.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;

use mimic_core::discovery::report::Direction;
use mimic_core::proxy::http1::Captured;

use super::metrics::DiscoveryMetrics;

/// Bytes captured in flight and queued, against `MIMIC_DISCOVERY_CAPTURE_BUDGET`.
pub(super) struct Budget {
    used: AtomicUsize,
    limit: usize,
}

impl Budget {
    pub(super) fn new(limit: usize) -> Budget {
        Budget {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_add(&self, n: usize) -> bool {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|total| *total <= self.limit)
            })
            .is_ok()
    }

    fn sub(&self, n: usize) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }
}

/// Bytes taken from the budget, given back when dropped (the record was processed, dropped
/// or abandoned).
struct Reservation {
    budget: Arc<Budget>,
    held: usize,
}

impl Reservation {
    fn grow(&mut self, n: usize) -> bool {
        let ok = self.budget.try_add(n);
        if ok {
            self.held += n;
        }
        ok
    }

    fn shrink(&mut self, n: usize) {
        self.budget.sub(n);
        self.held -= n;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.sub(self.held);
    }
}

/// What the tap kept of one body.
#[derive(Debug)]
pub(super) enum Part {
    /// The body did not complete (the exchange was cut): not counted.
    Absent,
    /// No body bytes.
    Empty,
    /// The whole body, at most 256 KiB.
    Data(Vec<u8>),
    /// More than 256 KiB.
    TooLarge,
    /// The capture budget was full.
    Budget,
}

fn part(total: Option<u64>, skipped: bool, body: Vec<u8>) -> Part {
    match total {
        None => Part::Absent,
        Some(0) => Part::Empty,
        Some(_) if skipped => Part::Budget,
        Some(n) if n > body.len() as u64 => Part::TooLarge,
        Some(_) => Part::Data(body),
    }
}

/// One exchange, ready for the recorder.
pub(super) struct Record {
    pub(super) direction: Direction,
    pub(super) port: u16,
    /// The normalised authority (spec §4.1), when the request named one.
    pub(super) authority: Option<String>,
    pub(super) status: u16,
    pub(super) head_ms: u64,
    pub(super) request_head: Vec<u8>,
    pub(super) response_head: Vec<u8>,
    pub(super) request: Part,
    pub(super) response: Part,
    _reservation: Reservation,
}

/// What the relay knows of an exchange that the capture does not.
pub(crate) struct Meta {
    pub(crate) port: u16,
    pub(crate) authority: Option<String>,
    pub(crate) status: u16,
    pub(crate) head_ms: u64,
    /// The exchange became a tunnel (`101`): no bodies.
    pub(crate) tunnel: bool,
}

/// The bytes of one exchange being relayed.
pub(crate) struct Capture {
    reservation: Reservation,
    request_head: Vec<u8>,
    response_head: Vec<u8>,
    request_body: Vec<u8>,
    response_body: Vec<u8>,
    request_skipped: bool,
    response_skipped: bool,
    /// A head did not fit in the budget: the exchange is dropped.
    head_lost: bool,
    request_total: Option<u64>,
    response_total: Option<u64>,
}

impl Capture {
    pub(crate) fn push(&mut self, captured: Captured) {
        match captured {
            Captured::RequestHead(bytes) => {
                if self.reservation.grow(bytes.len()) {
                    self.request_head = bytes;
                } else {
                    self.head_lost = true;
                }
            }
            Captured::ResponseHead(bytes) => {
                if self.reservation.grow(bytes.len()) {
                    self.response_head = bytes;
                } else {
                    self.head_lost = true;
                }
            }
            Captured::RequestBody(bytes) => {
                body(
                    &mut self.reservation,
                    &mut self.request_body,
                    &mut self.request_skipped,
                    &bytes,
                );
            }
            Captured::ResponseBody(bytes) => {
                body(
                    &mut self.reservation,
                    &mut self.response_body,
                    &mut self.response_skipped,
                    &bytes,
                );
            }
        }
    }

    pub(crate) fn request_end(&mut self, total: u64) {
        self.request_total = Some(total);
    }

    pub(crate) fn response_end(&mut self, total: u64) {
        self.response_total = Some(total);
    }
}

/// Appends `bytes` to `body` while the budget allows; the first refusal frees the body.
fn body(reservation: &mut Reservation, body: &mut Vec<u8>, skipped: &mut bool, bytes: &[u8]) {
    if *skipped {
        return;
    }
    if reservation.grow(bytes.len()) {
        body.extend_from_slice(bytes);
    } else {
        *skipped = true;
        reservation.shrink(body.len());
        *body = Vec::new();
    }
}

/// The relay's side of discovery: cheap to clone, never blocks.
#[derive(Clone)]
pub struct Tap {
    queue: SyncSender<Record>,
    budget: Arc<Budget>,
    metrics: DiscoveryMetrics,
}

impl Tap {
    pub(super) fn new(
        queue: SyncSender<Record>,
        budget: Arc<Budget>,
        metrics: DiscoveryMetrics,
    ) -> Tap {
        Tap {
            queue,
            budget,
            metrics,
        }
    }

    /// Starts the capture of an exchange.
    pub(crate) fn capture(&self) -> Capture {
        Capture {
            reservation: Reservation {
                budget: self.budget.clone(),
                held: 0,
            },
            request_head: Vec::new(),
            response_head: Vec::new(),
            request_body: Vec::new(),
            response_body: Vec::new(),
            request_skipped: false,
            response_skipped: false,
            head_lost: false,
            request_total: None,
            response_total: None,
        }
    }

    /// Queues the exchange for the recorder; a full queue or a lost head drops it, counted.
    pub(crate) fn submit(&self, capture: Capture, meta: Meta) {
        let direction = Direction::Inbound;
        if capture.head_lost {
            self.metrics.exchange("inbound", "dropped");
            return;
        }
        let bodies = !meta.tunnel;
        let record = Record {
            direction,
            port: meta.port,
            authority: meta.authority,
            status: meta.status,
            head_ms: meta.head_ms,
            request_head: capture.request_head,
            response_head: capture.response_head,
            request: if bodies {
                part(
                    capture.request_total,
                    capture.request_skipped,
                    capture.request_body,
                )
            } else {
                Part::Absent
            },
            response: if bodies {
                part(
                    capture.response_total,
                    capture.response_skipped,
                    capture.response_body,
                )
            } else {
                Part::Absent
            },
            _reservation: capture.reservation,
        };
        if self.queue.try_send(record).is_err() {
            self.metrics.exchange("inbound", "dropped");
        }
    }
}

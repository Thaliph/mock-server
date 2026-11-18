//! The admission of the pod's inbound requests (spec §4.1 "Admission contract"): the pure
//! state of `mimic_core::mesh::admission` behind one lock, the wait of a queued request, and
//! one timer task per pod, alive while the queue is not empty, that decides the queue at the
//! instants the state gives.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use mimic_core::mesh::admission::{
    Admission, Contract, Counts, Offer, Permit, Refusal, Resolved, Ticket, Verdict,
};
use tokio::sync::{Notify, oneshot};

/// The admission state of this pod (spec §4.1 "Admission contract"): shared by every inbound
/// connection, cheap to clone.
#[derive(Clone)]
pub(crate) struct Admitter(Arc<Shared>);

struct Shared {
    /// Short sections only: never held across an `await`, an I/O, a spawn or a `Slot` drop.
    inner: Mutex<Inner>,
    /// The clock of the state: `now` is the time since this instant.
    epoch: Instant,
    /// The timer task reads `next_instant()` again.
    wake: Notify,
}

struct Inner {
    admission: Admission,
    /// Reused by every call.
    out: Vec<Resolved>,
    waiters: BTreeMap<Ticket, oneshot::Sender<Verdict>>,
    /// A timer task runs.
    driving: bool,
}

impl Inner {
    /// Hands each resolved entry to its waiter. A permit whose waiter is gone is released at
    /// once, which may admit the next entries: bounded by the queue.
    fn dispatch(&mut self, now: Duration) {
        let mut out = std::mem::take(&mut self.out);
        let mut more = Vec::new();
        while !out.is_empty() {
            for resolved in out.drain(..) {
                let unsent = match self.waiters.remove(&resolved.ticket) {
                    Some(waiter) => waiter.send(resolved.verdict).err(),
                    None => Some(resolved.verdict),
                };
                if let Some(Verdict::Admitted(permit)) = unsent {
                    self.admission.release(permit, now, &mut more);
                }
            }
            std::mem::swap(&mut out, &mut more);
        }
        self.out = out;
    }
}

impl Admitter {
    /// No contract: every request is admitted.
    pub(crate) fn new() -> Admitter {
        Admitter(Arc::new(Shared {
            inner: Mutex::new(Inner {
                admission: Admission::new(),
                out: Vec::new(),
                waiters: BTreeMap::new(),
                driving: false,
            }),
            epoch: Instant::now(),
            wake: Notify::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Duration {
        self.0.epoch.elapsed()
    }

    /// Follows the view's `admission` (spec §4.1 "Contract changes").
    pub(crate) fn set_contract(&self, contract: Option<Contract>) {
        {
            let mut guard = self.lock();
            let now = self.now();
            let inner = &mut *guard;
            inner.admission.set_contract(contract, now, &mut inner.out);
            inner.dispatch(now);
        }
        self.0.wake.notify_one();
    }

    /// The contract in force.
    #[allow(dead_code)] // The HTTP/1 hold reads it per head (#453 step 4).
    pub(crate) fn contract(&self) -> Option<Contract> {
        self.lock().admission.contract()
    }

    /// Offers a request, then waits (at most 150 ms) when it is queued. Cancel-safe: dropping
    /// the future cancels the entry, or releases a permit that was already sent to it.
    pub(crate) async fn admit(&self) -> Result<Slot, Refusal> {
        let (ticket, rx, spawn) = {
            let mut guard = self.lock();
            let now = self.now();
            let inner = &mut *guard;
            let offer = inner.admission.offer(now, &mut inner.out);
            inner.dispatch(now);
            match offer {
                Offer::Admitted(permit) => {
                    return Ok(Slot {
                        admitter: self.clone(),
                        permit: Some(permit),
                    });
                }
                Offer::Refused(refusal) => return Err(refusal),
                Offer::Queued(ticket) => {
                    let (tx, rx) = oneshot::channel();
                    inner.waiters.insert(ticket, tx);
                    (ticket, rx, !std::mem::replace(&mut inner.driving, true))
                }
            }
        };
        if spawn {
            tokio::spawn(drive(self.0.clone()));
        } else {
            self.0.wake.notify_one();
        }
        let mut waiting = Waiting {
            admitter: self,
            ticket,
            rx: Some(rx),
        };
        let verdict = match waiting.rx.as_mut() {
            Some(rx) => rx.await,
            None => return Err(Refusal::QueueTimeout),
        };
        // Resolved: nothing is left to cancel.
        waiting.rx = None;
        match verdict {
            Ok(Verdict::Admitted(permit)) => Ok(Slot {
                admitter: self.clone(),
                permit: Some(permit),
            }),
            Ok(Verdict::Refused(refusal)) => Err(refusal),
            Err(_) => Err(Refusal::QueueTimeout),
        }
    }

    /// The counts of the state now (spec ADM-07).
    pub(crate) fn counts(&self) -> Counts {
        self.lock().admission.counts(self.now())
    }

    fn release(&self, permit: Permit) {
        let mut guard = self.lock();
        let now = self.now();
        let inner = &mut *guard;
        inner.admission.release(permit, now, &mut inner.out);
        inner.dispatch(now);
    }
}

/// One admitted request; dropping it releases its permit, whichever way the exchange ends.
#[must_use]
pub(crate) struct Slot {
    admitter: Admitter,
    permit: Option<Permit>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.admitter.release(permit);
        }
    }
}

/// A queued request's wait: dropped before its verdict (the stream was reset), it frees the
/// place, or releases the permit sent to it just before.
struct Waiting<'a> {
    admitter: &'a Admitter,
    ticket: Ticket,
    rx: Option<oneshot::Receiver<Verdict>>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        let mut guard = self.admitter.lock();
        let now = self.admitter.now();
        let inner = &mut *guard;
        inner.waiters.remove(&self.ticket);
        if inner.admission.cancel(self.ticket) {
            return;
        }
        if let Ok(Verdict::Admitted(permit)) = rx.try_recv() {
            inner.admission.release(permit, now, &mut inner.out);
            inner.dispatch(now);
        }
    }
}

/// The timer of the queue: applies the state at each instant it gives, until the queue is
/// empty.
async fn drive(shared: Arc<Shared>) {
    loop {
        let wake_at = {
            let mut guard = shared.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let now = shared.epoch.elapsed();
            let inner = &mut *guard;
            inner.admission.apply(now, &mut inner.out);
            inner.dispatch(now);
            match inner.admission.next_instant() {
                None => {
                    inner.driving = false;
                    return;
                }
                Some(at) => shared
                    .epoch
                    .checked_add(at)
                    .unwrap_or_else(|| Instant::now() + Duration::from_millis(1)),
            }
        };
        tokio::select! {
            () = tokio::time::sleep_until(wake_at.into()) => {}
            () = shared.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::time::{Duration, Instant};

    use mimic_core::mesh::admission::{Contract, Refusal};

    use super::Admitter;

    fn with(concurrency: u32, rps: u32) -> Admitter {
        let admitter = Admitter::new();
        admitter.set_contract(Some(Contract { concurrency, rps }));
        admitter
    }

    /// Polls `fut` until `cond` holds (at most 1 s), then gives it back unresolved.
    async fn until(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}");
    }

    async fn within<T>(limit: Duration, fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(limit, fut).await.expect("in time")
    }

    #[tokio::test]
    async fn adm_16_admit_without_contract_is_immediate_and_a_queued_admit_times_out_by_the_timer()
    {
        let free = Admitter::new();
        let slots: Vec<_> = (0..1000)
            .map(|_| {
                let fut = pin!(free.admit());
                match futures_now(fut) {
                    Some(Ok(slot)) => slot,
                    other => panic!("not admitted at once: {:?}", other.map(|r| r.err())),
                }
            })
            .collect();
        assert_eq!(free.counts().in_flight, 0);
        drop(slots);

        let admitter = with(1, 1000);
        let _s1 = within(Duration::from_millis(100), admitter.admit())
            .await
            .expect("the first is admitted");
        let t0 = Instant::now();
        let second = tokio::spawn({
            let admitter = admitter.clone();
            async move { admitter.admit().await.err() }
        });
        until("the second is queued", || admitter.counts().depth == 1).await;
        let t1 = Instant::now();
        let third = within(Duration::from_secs(2), admitter.admit()).await;
        assert_eq!(third.err(), Some(Refusal::Overloaded));
        assert!(
            t1.elapsed() <= Duration::from_millis(5),
            "{:?}",
            t1.elapsed()
        );
        let second = within(Duration::from_secs(2), second).await.unwrap();
        assert_eq!(second, Some(Refusal::QueueTimeout));
        let waited = t0.elapsed();
        assert!(
            (Duration::from_millis(150)..=Duration::from_millis(250)).contains(&waited),
            "{waited:?}"
        );
    }

    /// The output of `fut` when it is ready at its first poll.
    fn futures_now<F: std::future::Future>(fut: std::pin::Pin<&mut F>) -> Option<F::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match fut.poll(&mut cx) {
            std::task::Poll::Ready(out) => Some(out),
            std::task::Poll::Pending => None,
        }
    }

    #[tokio::test]
    async fn adm_17_the_token_limit_alone_admits_the_queued_one_at_the_next_token() {
        let admitter = with(1000, 10);
        let _s1 = within(Duration::from_millis(100), admitter.admit())
            .await
            .expect("the first is admitted");
        let t0 = Instant::now();
        let second = within(Duration::from_secs(2), admitter.admit()).await;
        let waited = t0.elapsed();
        assert!(second.is_ok(), "{:?}", second.err());
        assert!(
            (Duration::from_millis(95)..Duration::from_millis(150)).contains(&waited),
            "{waited:?}"
        );
        assert_eq!(admitter.counts().in_flight, 2);
    }

    #[tokio::test]
    async fn adm_18_a_dropped_slot_admits_the_queued_one_and_a_late_permit_is_released() {
        // A slot dropped: the queued one is admitted at once.
        let admitter = with(1, 1000);
        let s1 = admitter.admit().await.expect("admitted");
        let mut fut = Box::pin(admitter.admit());
        assert!(futures_now(fut.as_mut()).is_none(), "queued");
        assert_eq!(admitter.counts().depth, 1);
        let t0 = Instant::now();
        drop(s1);
        let s2 = within(Duration::from_secs(1), fut).await.expect("admitted");
        assert!(
            t0.elapsed() <= Duration::from_millis(10),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(admitter.counts().in_flight, 1);
        drop(s2);
        assert_eq!(admitter.counts().in_flight, 0);

        // The race: the permit is sent to a waiter dropped before it reads it.
        let s1 = admitter.admit().await.expect("admitted");
        let mut fut = Box::pin(admitter.admit());
        assert!(futures_now(fut.as_mut()).is_none(), "queued");
        drop(s1);
        drop(fut);
        assert_eq!(
            admitter.counts().in_flight,
            0,
            "the late permit is released"
        );
        let mut next = Box::pin(admitter.admit());
        assert!(
            matches!(futures_now(next.as_mut()), Some(Ok(_))),
            "admitted at once"
        );

        // A cancel: the place is freed and counted as cancelled.
        let admitter = with(1, 1000);
        let _s1 = admitter.admit().await.expect("admitted");
        let mut fut = Box::pin(admitter.admit());
        assert!(futures_now(fut.as_mut()).is_none(), "queued");
        drop(fut);
        let counts = admitter.counts();
        assert_eq!((counts.depth, counts.cancelled), (0, 1));
        let mut next = Box::pin(admitter.admit());
        assert!(
            futures_now(next.as_mut()).is_none(),
            "queued, not overloaded"
        );
        assert_eq!(admitter.counts().overloaded, 0);
    }
}

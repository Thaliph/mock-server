//! The admission of the pod's inbound requests (spec §4.1 "Admission contract").

use mimic_core::mesh::admission::{Contract, Counts, Refusal};

/// The admission state of this pod: shared by every inbound connection, cheap to clone.
#[derive(Clone)]
pub(crate) struct Admitter;

impl Admitter {
    pub(crate) fn new() -> Admitter {
        Admitter
    }

    pub(crate) fn set_contract(&self, _contract: Option<Contract>) {}

    #[allow(dead_code)]
    pub(crate) fn contract(&self) -> Option<Contract> {
        todo!()
    }

    pub(crate) async fn admit(&self) -> Result<Slot, Refusal> {
        todo!()
    }

    pub(crate) fn counts(&self) -> Counts {
        todo!()
    }
}

/// One admitted request.
#[must_use]
pub(crate) struct Slot;

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

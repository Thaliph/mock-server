//! CTL-07 and SHR-18: the body budget is held until the store write ends, also when the
//! client goes away first (the write runs in a task of the store, not in the handler).

use super::*;

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timeout waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shr_18_a_client_that_disconnects_keeps_the_budget_until_the_write_ends() {
    let s = setup(Options::default());
    let body = payload(SESSION, 1, &report_of(&["/a"]));
    let length = body.len() as u64;
    // No write starts while the slots are held: the push waits in the store's queue.
    let slots = s.api.store.hold_writers().await;
    let client = {
        let router = s.router.clone();
        let token = token(&s.ns, "pay", "t");
        tokio::spawn(async move {
            send(
                &router,
                PEER,
                Method::POST,
                Some(&token),
                "pay",
                Some(body.len()),
                Body::from(body),
            )
            .await
        })
    };
    until("the push to be queued", || s.api.store.queued() == 1).await;
    assert_eq!(s.api.budget.used(), length);
    // The client disconnects: the handler future is dropped, the push stays queued.
    client.abort();
    let _ = client.await;
    assert_eq!(
        s.api.budget.used(),
        length,
        "the budget share was released while the write is still pending"
    );
    drop(slots);
    until("the write to end", || s.api.budget.used() == 0).await;
    assert!(
        stored_report(&s, "pay").await.is_some(),
        "applied all the same"
    );
}

//! ADM-51: one reason in three places. The `x-mimic-error` of an answer of the sidecar, the
//! access line's `reason` and the label of its metric are the same `Reason::as_str` (spec §9).

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use common::admission::{App, call, contract, inbound};
use common::faults::{abort_on, status};
use common::mesh_pods::{H1Client, Pod, Svc, cluster_ip, mesh_state, meshed};
use common::{Logs, capture_logs, closed_port, fixture_mesh, h2_client, metric, until};
use mimic_core::mesh::reason::Reason;

fn cart(pods: &[SocketAddr]) -> Svc {
    Svc::new("shop/cart", "127.0.0.1", 80, pods)
}

/// The `x-mimic-error` of a response head.
fn error_of(head: &str) -> String {
    head.lines()
        .find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case("x-mimic-error")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_else(|| panic!("no x-mimic-error: {head}"))
}

/// The `reason` of the access lines of the log (`n` in all), in their order.
async fn reasons(logs: &Logs, n: usize) -> Vec<Option<String>> {
    logs.access_exactly(n)
        .await
        .iter()
        .map(|entry| entry["reason"].as_str().map(str::to_string))
        .collect()
}

/// The outbound header, access `reason` and `mimic_mesh_requests_total` label agree on `want`:
/// the access lines `seen.0..seen.1` are the case's, and only the answer's has a `reason`.
async fn assert_outbound(
    logs: &Logs,
    f: &common::Fixture,
    head: &str,
    seen: (usize, usize),
    want: Reason,
) {
    let header = error_of(head);
    assert_eq!(header, want.as_str(), "{head}");
    let case: Vec<String> = reasons(logs, seen.1).await[seen.0..]
        .iter()
        .flatten()
        .cloned()
        .collect();
    assert_eq!(case, [want.as_str()], "the access reasons of the case");
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &[&format!("reason=\"{header}\"")]
        ),
        1.0,
        "{scrape}"
    );
}

#[tokio::test]
async fn adm_51_one_reason_in_the_header_the_access_line_and_the_label() {
    let logs = capture_logs();
    let mut seen = 0;

    // Inbound: overloaded and queue-timeout, contract 1/1000, the slot held.
    let app = App::h2c(1000).await;
    let state = meshed(Some("cart-7d9f-x2k"), |_| {});
    state.hold(contract(1, 1000), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    let mut client = h2_client(f.addr).await;
    let held = {
        let mut sender = client.clone();
        tokio::spawn(async move { call(&mut sender, "/held", &[]).await })
    };
    until("the app holds the first request", || {
        app.counts.in_flight() >= 1
    })
    .await;
    let queued = {
        let mut sender = client.clone();
        tokio::spawn(async move { call(&mut sender, "/queued", &[]).await })
    };
    until("the second stream is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    let overloaded = call(&mut client, "/overloaded", &[]).await;
    let queued = queued.await.unwrap();
    assert_eq!(held.await.unwrap().status, 200);
    seen += 3;
    let entries = logs.access_exactly(seen).await;
    for (answer, path, want) in [
        (&overloaded, "/overloaded", Reason::Overloaded),
        (&queued, "/queued", Reason::QueueTimeout),
    ] {
        assert_eq!(answer.status, 503, "{answer:?}");
        assert_eq!(answer.error(), Some(want.as_str()), "{answer:?}");
        let entry = entries
            .iter()
            .find(|e| e["path"] == path)
            .unwrap_or_else(|| panic!("no access line for {path}: {entries:?}"));
        assert_eq!(entry["reason"], want.as_str(), "{entry}");
    }
    state.admission_tick();
    let scrape = f.metrics.encode();
    for want in [Reason::Overloaded, Reason::QueueTimeout] {
        assert_eq!(
            metric(
                &scrape,
                "mimic_admission_requests_total{",
                &[&format!("result=\"{}\"", want.as_str())]
            ),
            1.0,
            "{scrape}"
        );
    }

    // no-endpoint: a Service port without endpoints.
    let state = mesh_state(&[cart(&[])], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert_eq!(status(&head), 503, "{head}");
    assert_outbound(&logs, &f, &head, (seen, seen + 1), Reason::NoEndpoint).await;
    seen += 1;

    // breaker-open: maxRequests 1, one request held by a slow pod.
    let pod = Pod::http1("slow", 1000).await;
    let state = mesh_state(&[cart(&[pod.addr]).breaker(1, 1024)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let held = tokio::spawn({
        let addr = f.addr;
        async move { H1Client::connect(addr).await.get("/held").await }
    });
    until("the slot is held", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert_eq!(status(&head), 503, "{head}");
    let (held, _) = held.await.unwrap();
    assert_eq!(status(&held), 200, "{held}");
    assert_outbound(&logs, &f, &head, (seen, seen + 2), Reason::BreakerOpen).await;
    seen += 2;

    // fault-injected: an abort of 100 % with status 418.
    let pod = Pod::http1("pod", 0).await;
    let faults = format!("[{}]", abort_on("/", 100, 418));
    let state = mesh_state(&[cart(&[pod.addr]).faults(&faults)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert_eq!(status(&head), 418, "{head}");
    assert_outbound(&logs, &f, &head, (seen, seen + 1), Reason::FaultInjected).await;
    seen += 1;

    // connect-failed: every pod refuses connections.
    let pods = [closed_port().await, closed_port().await];
    let state = mesh_state(&[cart(&pods)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert_eq!(status(&head), 503, "{head}");
    assert_outbound(&logs, &f, &head, (seen, seen + 1), Reason::ConnectFailed).await;
}

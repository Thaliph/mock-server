//! MCK-18, the capture budget of the mocked path (spec §8 "Bounds"): a request body that does
//! not fit `MIMIC_DISCOVERY_CAPTURE_BUDGET` is matched as absent, and the bytes reserved for a
//! body are given back when the client aborts. Every wait has a deadline.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::mock_rig::{AUTHORITY, Ex, Rig, record, rig_with_budget};
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::report::Report;
use mimic_sidecar::discovery::CaptureBudget;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// `POST /orders`, recorded without a request body: a request whose body is matched as
/// absent is `exact`, one whose body fields are kept shares none with the sample and is
/// `fallback`.
fn orders_set() -> EgressSet {
    let mut report = Report::new(None);
    for _ in 0..3 {
        record(
            &mut report,
            &Ex {
                method: "POST",
                path: "/orders",
                status: 200,
                head_ms: 5,
                grpc: false,
                resp_body: Some("{\"ok\":true}"),
            },
        );
    }
    EgressSet::project(&report)
}

fn json(len: usize) -> Vec<u8> {
    let head = b"{\"note\":\"";
    let tail = b"\"}";
    let mut out = head.to_vec();
    out.resize(len - tail.len(), b'a');
    out.extend_from_slice(tail);
    out
}

async fn read_head(s: &mut TcpStream) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(n > 0, "closed before the answer");
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

fn tier(head: &str) -> String {
    head.lines()
        .find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case("x-mimic-match")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_else(|| panic!("no x-mimic-match in {head}"))
}

async fn post(addr: SocketAddr, framing: &str, body: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /orders HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Type: application/json\r\n{framing}\r\n\r\n"
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    read_head(&mut s).await
}

fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

async fn used_is(budget: &CaptureBudget, want: usize, within: Duration) {
    let end = Instant::now() + within;
    while budget.used() != want {
        assert!(
            Instant::now() < end,
            "budget holds {}, wanted {want}",
            budget.used()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn rig_budget(limit: usize) -> (Rig, CaptureBudget) {
    let budget = CaptureBudget::new(limit);
    let rig = rig_with_budget(orders_set(), 100, 1024, Some(budget.clone())).await;
    (rig, budget)
}

#[tokio::test]
async fn mck_18_a_body_that_fits_the_budget_is_matched_and_the_budget_is_given_back() {
    let (rig, budget) = rig_budget(1 << 20).await;
    let head = post(rig.addr, "Content-Length: 2048", &json(2048)).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(tier(&head), "fallback");
    used_is(&budget, 0, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn mck_18_a_declared_body_over_the_budget_is_matched_as_absent() {
    let (rig, budget) = rig_budget(1024).await;
    let head = post(rig.addr, "Content-Length: 2048", &json(2048)).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(tier(&head), "exact");
    used_is(&budget, 0, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn mck_18_a_chunked_body_that_outgrows_the_budget_is_matched_as_absent() {
    let (rig, budget) = rig_budget(1024).await;
    let body = json(1400);
    let (a, b) = body.split_at(700);
    let head = post(rig.addr, "Transfer-Encoding: chunked", &chunked(&[a, b])).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(tier(&head), "exact");
    used_is(&budget, 0, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn mck_18_the_reservation_of_a_body_is_released_when_the_client_aborts() {
    let (rig, budget) = rig_budget(1 << 20).await;
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    let head = format!(
        "POST /orders HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n"
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(&json(50_000)[..40_000]).await.unwrap();
    // The declared length is reserved at once, in full.
    used_is(&budget, 100_000, Duration::from_secs(5)).await;
    drop(s);
    used_is(&budget, 0, Duration::from_secs(1)).await;
    assert_eq!(rig.counter("mimic_mock_pending"), 0);
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["result"], "client_abort");
}

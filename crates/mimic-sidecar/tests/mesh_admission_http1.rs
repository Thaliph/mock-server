//! ADM-15, ADM-17 to ADM-25, ADM-27 and ADM-30 on HTTP/1: the inbound side of a meshed sidecar
//! holds each request head whole under a contract, admits, queues or refuses it, keeps its slot
//! until its response is complete, and keeps the connection after a refusal when the request
//! allows it (spec §4.1 "Admission contract"). An inbound sidecar in process in front of an
//! HTTP/1 app that records every byte.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::admission::{App, Http1App, contract, full_queue, inbound, meshed, no_contract};
use common::mesh_pods::view_services;
use common::prx_corpus::corpus;
use common::{
    Fixture, capture_logs, counted, echo_server, fixture, fixture_mesh_on, metric, read_all, until,
};
use mimic_runtime::config::Env;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use mimic_sidecar::mesh::MeshState;
use mimic_sidecar::probes::{self, Probes};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt;

const POD: &str = "cart-7d9f-x2k";
const ORIGIN: &str = "inbound; pod=cart-7d9f-x2k";
const TIMEOUT: Duration = Duration::from_secs(10);

/// A refusal on a kept connection (spec §4.1, ADM-20): the four fields, no `connection` field.
fn kept(error: &str, app: SocketAddr, origin: &str) -> String {
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: {error}\r\nx-mimic-endpoint: {app}\r\nx-mimic-origin: {origin}\r\ncontent-length: 0\r\n\r\n"
    )
}

/// A refusal that ends its connection: the four fields and `connection: close`.
fn closed(error: &str, app: SocketAddr, origin: &str) -> String {
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: {error}\r\nx-mimic-endpoint: {app}\r\nx-mimic-origin: {origin}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n").into_bytes()
}

/// An app answering after `delay_ms` and a meshed sidecar holding `view` in front of it.
async fn rig(view: mimic_core::mesh::view::View, delay_ms: u64) -> (Http1App, MeshState, Fixture) {
    let app = App::http1(delay_ms).await;
    let state = meshed(Some(POD), |_| {});
    state.hold(view, "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    (app, state, f)
}

/// A new connection to the sidecar, once the sidecar dialled the app for it.
async fn connect(f: &Fixture, app: &Http1App) -> TcpStream {
    let before = app.counts.accepted();
    let c = TcpStream::connect(f.addr).await.unwrap();
    c.set_nodelay(true).unwrap();
    until("the sidecar dials the app", || {
        app.counts.accepted() > before
    })
    .await;
    c
}

/// A held slot: `GET /slow` on its own connection, held by the app (its delay is long).
async fn hold_slot(f: &Fixture, app: &Http1App) -> TcpStream {
    let mut c = connect(f, app).await;
    c.write_all(&get("/slow")).await.unwrap();
    until("the app holds /slow", || app.in_flight() == 1).await;
    c
}

/// Reads until `needle` is in what was read; everything read so far.
async fn read_until(c: &mut TcpStream, got: &mut Vec<u8>, needle: &[u8]) {
    tokio::time::timeout(TIMEOUT, async {
        while !got.windows(needle.len()).any(|w| w == needle) {
            let mut buf = [0u8; 4096];
            let n = c.read(&mut buf).await.expect("no reset");
            assert!(
                n > 0,
                "the connection ended: {}",
                String::from_utf8_lossy(got)
            );
            got.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("read in time");
}

/// Reads one answer with an empty body: its head, and no byte more.
async fn one_answer(c: &mut TcpStream) -> String {
    let mut got = Vec::new();
    read_until(c, &mut got, b"\r\n\r\n").await;
    String::from_utf8(got).unwrap()
}

/// Sends `request` on a new connection and reads one answer with an empty body (a refusal on a
/// kept connection); the bytes, the time from the write, and the connection.
async fn refused_on_new(
    f: &Fixture,
    app: &Http1App,
    request: &[u8],
) -> (String, Duration, TcpStream) {
    let mut c = connect(f, app).await;
    let start = Instant::now();
    c.write_all(request).await.unwrap();
    let got = one_answer(&mut c).await;
    (got, start.elapsed(), c)
}

/// `GET path` on a new connection: its `200` and the time it took.
async fn ok_on_new(f: &Fixture, app: &Http1App, path: &str) -> Duration {
    let mut c = connect(f, app).await;
    let start = Instant::now();
    c.write_all(&get(path)).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut c, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK, "{path}");
    start.elapsed()
}

fn spawn_refused(
    f: &Fixture,
    app: &Http1App,
    path: &'static str,
) -> JoinHandle<(String, Duration, TcpStream)> {
    let (addr, accepted) = (f.addr, app.counts.accepted.clone());
    tokio::spawn(async move {
        let before = accepted.load(Ordering::SeqCst);
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        until("the sidecar dials the app", || {
            accepted.load(Ordering::SeqCst) > before
        })
        .await;
        let start = Instant::now();
        c.write_all(&get(path)).await.unwrap();
        let got = one_answer(&mut c).await;
        (got, start.elapsed(), c)
    })
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[tokio::test]
async fn adm_15_http1_refusals_have_the_fields_and_the_app_gets_no_byte() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 1000).await;
    let mut held = hold_slot(&f, &app).await;
    let second = spawn_refused(&f, &app, "/r2");
    until("the second is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    let (third, took, _c) = refused_on_new(&f, &app, &get("/r3")).await;
    assert_eq!(third, kept("overloaded", app.addr, ORIGIN));
    assert!(took <= ms(50), "{took:?}");
    let (second, took, mut b) = second.await.unwrap();
    assert_eq!(second, kept("queue-timeout", app.addr, ORIGIN));
    assert!((ms(150)..=ms(400)).contains(&took), "{took:?}");
    assert!(!app.got("/r2") && !app.got("/r3"), "{}", app.raw_text());
    assert_eq!(&app.per_conn()[1..], [0, 0]);

    // A body still coming when the answer is written is read and dropped; the client's FIN
    // ends the connection (no reset).
    let mut d = connect(&f, &app).await;
    d.write_all(b"POST /r5 HTTP/1.1\r\nHost: a\r\ncontent-length: 30000\r\n\r\n")
        .await
        .unwrap();
    let (mut reader, mut writer) = d.into_split();
    let body = tokio::spawn(async move {
        for _ in 0..3 {
            if writer.write_all(&[b'x'; 10_000]).await.is_err() {
                return;
            }
            tokio::time::sleep(ms(60)).await;
        }
        let _ = writer.shutdown().await;
    });
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, reader.read_to_end(&mut got))
        .await
        .expect("the end in time")
        .expect("a FIN, not a reset");
    assert_eq!(
        String::from_utf8(got).unwrap(),
        kept("queue-timeout", app.addr, ORIGIN)
    );
    body.await.unwrap();
    assert!(
        !app.got("/r5") && !app.got("xxxxxxxx"),
        "{}",
        app.raw_text()
    );

    // B's connection goes on once the slot is free.
    let mut got = Vec::new();
    read_until(&mut held, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
    app.counts.delay_ms.store(0, Ordering::SeqCst);
    b.write_all(&get("/r4")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut b, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
}

#[tokio::test]
async fn adm_15_http1_an_ipv6_endpoint_and_no_pod_name() {
    let _logs = capture_logs();
    let Ok(listener) = TcpListener::bind("[::1]:0").await else {
        return;
    };
    let app = Http1App::serve(listener, 1000);
    let state = meshed(None, |_| {});
    state.hold(contract(1, 1000), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    let _held = hold_slot(&f, &app).await;
    let (answer, _, _c) = refused_on_new(&f, &app, &get("/r2")).await;
    assert_eq!(answer, kept("queue-timeout", app.addr, "inbound"));
    assert!(answer.contains(&format!("x-mimic-endpoint: [::1]:{}\r\n", app.addr.port())));
}

/// One answer on a keep-alive connection: the status, the `x-mimic-error`, whether it closes.
async fn answer_of(c: &mut TcpStream, got: &mut Vec<u8>) -> Option<(u16, Option<String>, bool)> {
    loop {
        if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&got[..p]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length: ")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            if got.len() >= p + 4 + length {
                got.drain(..p + 4 + length);
                let status = head.split(' ').nth(1)?.parse().ok()?;
                let error = head
                    .lines()
                    .find_map(|l| l.strip_prefix("x-mimic-error: ").map(str::to_string));
                return Some((status, error, head.contains("\r\nconnection: close")));
            }
        }
        let mut buf = [0u8; 4096];
        match tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("an answer in time")
        {
            Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
            _ => return None,
        }
    }
}

#[derive(Default, Debug)]
struct Tally {
    ok: usize,
    overloaded: usize,
    timeouts: usize,
    /// Ended before an answer, or reset after a refusal.
    lost: usize,
    other: usize,
}

#[tokio::test]
async fn adm_17_http1_twice_the_concurrency_never_exceeds_it_and_refuses_the_rest() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(4, 1000), 200).await;
    let end = Instant::now() + Duration::from_secs(3);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let addr = f.addr;
        tasks.push(tokio::spawn(async move {
            let mut tally = Tally::default();
            let mut conn: Option<(TcpStream, Vec<u8>)> = None;
            while Instant::now() < end {
                let (c, got) = match conn.as_mut() {
                    Some(open) => open,
                    None => conn.insert((TcpStream::connect(addr).await.unwrap(), Vec::new())),
                };
                c.write_all(&get("/")).await.unwrap();
                match answer_of(c, got).await {
                    None => {
                        tally.lost += 1;
                        conn = None;
                    }
                    Some((200, None, _)) => tally.ok += 1,
                    Some((503, Some(error), close)) => {
                        match error.as_str() {
                            "overloaded" => tally.overloaded += 1,
                            "queue-timeout" => tally.timeouts += 1,
                            _ => tally.other += 1,
                        }
                        // A refused `GET` keeps its connection (ADM-20).
                        if close {
                            if read_all(c).await.is_err() {
                                tally.lost += 1;
                            }
                            conn = None;
                        }
                        tokio::time::sleep(ms(1)).await;
                    }
                    Some(_) => tally.other += 1,
                }
            }
            tally
        }));
    }
    let mut all = Tally::default();
    for task in tasks {
        let t = task.await.unwrap();
        all.ok += t.ok;
        all.overloaded += t.overloaded;
        all.timeouts += t.timeouts;
        all.lost += t.lost;
        all.other += t.other;
    }
    assert_eq!(app.max_in_flight(), 4, "{all:?}");
    assert_eq!((all.lost, all.other), (0, 0), "{all:?}");
    assert!(all.overloaded > 0 && all.timeouts > 0, "{all:?}");
    assert_eq!(all.ok, app.counts.requests(), "{all:?}");
}

#[tokio::test]
async fn adm_17_http1_twice_the_rate_is_admitted_at_the_rate() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1000, 50), 0).await;
    let start = Instant::now();
    let end = start + Duration::from_secs(4);
    let mut tasks = Vec::new();
    for i in 0..20u64 {
        let addr = f.addr;
        tasks.push(tokio::spawn(async move {
            let first = tokio::time::Instant::from_std(start) + ms(i * 10);
            let mut ticks = tokio::time::interval_at(first, ms(200));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let (mut sent, mut refused, mut last) = (0usize, 0usize, start);
            let mut conn: Option<(TcpStream, Vec<u8>)> = None;
            loop {
                ticks.tick().await;
                if Instant::now() >= end {
                    break;
                }
                let (c, got) = match conn.as_mut() {
                    Some(open) => open,
                    None => conn.insert((TcpStream::connect(addr).await.unwrap(), Vec::new())),
                };
                c.write_all(&get("/")).await.unwrap();
                let answer = answer_of(c, got).await;
                sent += 1;
                last = Instant::now();
                match answer {
                    Some((200, None, _)) => {}
                    Some((503, Some(_), close)) => {
                        refused += 1;
                        if close {
                            conn = None;
                        }
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            (sent, refused, last)
        }));
    }
    let (mut sent, mut refused, mut last) = (0, 0, start);
    for task in tasks {
        let (s, r, l) = task.await.unwrap();
        (sent, refused) = (sent + s, refused + r);
        last = last.max(l);
    }
    let t = (last - start).as_secs_f64();
    let admitted = app.counts.requests();
    assert!(
        admitted as f64 <= 5.0 + (50.0 * t).floor(),
        "{admitted} in {t:.3} s"
    );
    assert!(
        admitted as f64 >= (0.9 * 50.0 * t).ceil(),
        "{admitted} in {t:.3} s"
    );
    assert_eq!(admitted + refused, sent);
}

#[tokio::test]
async fn adm_18_http1_a_chunked_response_holds_the_slot_until_its_last_chunk() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    a.write_all(&get("/chunked")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\n").await;
    let t0 = Instant::now();
    tokio::time::sleep_until((t0 + ms(200)).into()).await;
    let (b, _, _b) = refused_on_new(&f, &app, &get("/r2")).await;
    assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN));
    read_until(&mut a, &mut got, b"0\r\n\r\n").await;
    let took = ok_on_new(&f, &app, "/r3").await;
    assert!(took < ms(100), "{took:?}");
}

#[tokio::test]
async fn adm_18_http1_an_sse_response_holds_the_slot_and_a_client_reset_frees_it() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    a.write_all(&get("/sse")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"data: 1\n\n").await;
    let t0 = Instant::now();
    for (at, path) in [(200, "/r2"), (1200, "/r3")] {
        tokio::time::sleep_until((t0 + ms(at)).into()).await;
        let (b, _, _b) = refused_on_new(&f, &app, &get(path)).await;
        assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN), "{path}");
    }
    socket2::SockRef::from(&a)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(a);
    let took = ok_on_new(&f, &app, "/r4").await;
    assert!(took < ms(100), "{took:?}");
}

#[tokio::test]
async fn adm_18_http1_an_app_that_closes_without_answering_frees_the_slot() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    a.write_all(&get("/close")).await.unwrap();
    let _ = read_all(&mut a).await;
    let took = ok_on_new(&f, &app, "/r2").await;
    assert!(took < ms(100), "{took:?}");
}

#[tokio::test]
async fn adm_18_http1_a_close_delimited_response_frees_the_slot_at_the_app_close() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    let t0 = Instant::now();
    a.write_all(&get("/cd")).await.unwrap();
    until("the app has /cd", || app.got("/cd")).await;
    let (b, _, _b) = refused_on_new(&f, &app, &get("/r2")).await;
    assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN));
    assert!(t0.elapsed() < ms(300) + ms(150), "B was decided during A");
    let body = read_all(&mut a).await.expect("a FIN");
    assert!(body.ends_with(b"until the app closes"));
    let took = ok_on_new(&f, &app, "/r3").await;
    assert!(took < ms(100), "{took:?}");
}

const UPGRADE: &[u8] =
    b"GET /ws HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";

#[tokio::test]
async fn adm_19_a_101_ends_the_count_and_a_declined_upgrade_ends_it_at_its_response_end() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    a.write_all(UPGRADE).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\n").await;
    assert!(got.starts_with(b"HTTP/1.1 101 "));
    a.write_all(b"ping").await.unwrap();
    read_until(&mut a, &mut got, b"ping").await;
    let took = ok_on_new(&f, &app, "/r2").await;
    assert!(took < ms(100), "{took:?}");

    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    let t0 = Instant::now();
    a.write_all(
        b"GET /ws-no HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
    )
    .await
    .unwrap();
    tokio::time::sleep_until((t0 + ms(100)).into()).await;
    let (b, _, _b) = refused_on_new(&f, &app, &get("/r2")).await;
    assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN));
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\nno").await;
    let took = ok_on_new(&f, &app, "/r3").await;
    assert!(took < ms(100), "{took:?}");
    a.write_all(&get("/r4")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
}

#[tokio::test]
async fn adm_19_a_connect_answered_200_ends_the_count_at_its_head() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut a = connect(&f, &app).await;
    a.write_all(b"CONNECT app.test:443 HTTP/1.1\r\nHost: app.test:443\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\n").await;
    assert!(got.starts_with(b"HTTP/1.1 200 "));
    a.write_all(b"hi").await.unwrap();
    read_until(&mut a, &mut got, b"hi").await;
    let took = ok_on_new(&f, &app, "/r2").await;
    assert!(took < ms(100), "{took:?}");
}

/// Writes `bytes` one every 100 ms; `before_last` runs just before the last one.
async fn drip(c: &mut TcpStream, bytes: &[u8], before_last: impl FnOnce()) {
    let (last, rest) = bytes.split_last().unwrap();
    for b in rest {
        c.write_all(&[*b]).await.unwrap();
        tokio::time::sleep(ms(100)).await;
    }
    before_last();
    c.write_all(&[*last]).await.unwrap();
}

#[tokio::test]
async fn adm_22_a_dripped_head_holds_no_slot_and_is_decided_whole() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let head = b"GET /drip HTTP/1.1\r\nHost: a\r\n\r\n";
    let mut a = connect(&f, &app).await;
    let raw = app.raw.clone();
    let dripping = tokio::spawn(async move {
        drip(&mut a, head, || {
            let raw = raw.lock().unwrap();
            assert!(
                !raw.windows(5).any(|w| w == b"/drip"),
                "a piece was forwarded"
            );
        })
        .await;
        a
    });
    tokio::time::sleep(ms(300)).await;
    let took = ok_on_new(&f, &app, "/r2").await;
    assert!(took < ms(100), "{took:?}");
    let mut a = dripping.await.unwrap();
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
    assert!(
        app.raw_text()
            .contains("GET /drip HTTP/1.1\r\nHost: a\r\n\r\n")
    );
}

#[tokio::test]
async fn adm_22_a_thousand_incomplete_heads_take_no_slot_and_no_place() {
    let _logs = capture_logs();
    mimic_sidecar::listener::raise_fd_limit();
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let app = Http1App::serve(listener, 0);
    let state = meshed(Some(POD), |_| {});
    state.hold(contract(1, 1000), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    let mut open = Vec::new();
    for _ in 0..10 {
        for _ in 0..100 {
            let mut c = TcpStream::connect(f.addr).await.unwrap();
            c.write_all(b"GET /h HTTP/1.1\r\nHost: a\r\n")
                .await
                .unwrap();
            open.push(c);
        }
        tokio::task::yield_now().await;
    }
    until("1000 app connections", || app.counts.accepted() == 1000).await;
    let counts = state.admission_counts_for_test();
    assert_eq!((counts.in_flight, counts.depth), (0, 0));
    let took = ok_on_new(&f, &app, "/r").await;
    assert!(took < ms(100), "{took:?}");
    assert!(!app.got("/h"));
}

#[tokio::test]
async fn adm_22_unjudged_heads_are_decided_and_hold_their_slot_until_the_connection_ends() {
    let _logs = capture_logs();
    let big = {
        let mut head = b"GET /b HTTP/1.1\r\n".to_vec();
        while head.len() < 64 * 1024 {
            head.extend_from_slice(b"x-pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        head
    };
    let cases: Vec<(&str, Vec<u8>, bool)> = vec![
        ("stalled", b"GET /s HTTP/1.1\r\nx: 1".to_vec(), false),
        ("64 KiB", big, false),
        (
            "bad line",
            b"GET /c HTTP/1.1\r\nBad Header : x\r\n\r\n".to_vec(),
            false,
        ),
        ("cut", b"GET /d HTTP/1.1\r\nx: 1".to_vec(), true),
    ];
    for (name, head, cut) in cases {
        // Admitted: forwarded unchanged, and the slot is held while the connection is open.
        let (app, state, f) = {
            let app = App::http1(0).await;
            let state = meshed(Some(POD), |_| {});
            state.hold(contract(1, 1000), "\"1-a\"");
            let f = inbound(app.addr, &state, |s| s.with_detect_stall(ms(300))).await;
            (app, state, f)
        };
        let mut a = connect(&f, &app).await;
        a.write_all(&head).await.unwrap();
        if cut {
            a.shutdown().await.unwrap();
        }
        let raw = app.raw.clone();
        let want = head.clone();
        until(&format!("{name}: the app has the head"), || {
            raw.lock().unwrap().len() >= want.len()
        })
        .await;
        assert!(
            app.raw.lock().unwrap().starts_with(&head),
            "{name}: changed"
        );
        if cut {
            // The app closes on the FIN: the connection ends, and its slot with it.
            let _ = read_all(&mut a).await;
        } else {
            let (b, _, _b) = refused_on_new(&f, &app, &get("/r2")).await;
            assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN), "{name}");
            assert_eq!(state.admission_counts_for_test().in_flight, 1, "{name}");
            drop(a);
        }
        let took = ok_on_new(&f, &app, "/r3").await;
        assert!(took < ms(100), "{name}: {took:?}");

        // Refused: never kept (ADM-20), the 503 with `connection: close`, and no byte of the
        // head in the app.
        let (app, _state, f) = {
            let app = App::http1(1000).await;
            let state = meshed(Some(POD), |_| {});
            state.hold(contract(1, 1000), "\"1-a\"");
            let f = inbound(app.addr, &state, |s| s.with_detect_stall(ms(300))).await;
            (app, state, f)
        };
        let _held = hold_slot(&f, &app).await;
        let mut a = connect(&f, &app).await;
        a.write_all(&head).await.unwrap();
        if cut {
            a.shutdown().await.unwrap();
        }
        let got = read_all(&mut a).await.expect("a FIN");
        assert_eq!(
            String::from_utf8(got).unwrap(),
            closed("queue-timeout", app.addr, ORIGIN),
            "{name}"
        );
        assert_eq!(app.per_conn()[1], 0, "{name}: a byte reached the app");
    }
}

/// A TLS server with a self-signed certificate for `localhost` that answers 204 to anything,
/// and the certificate.
async fn https_app() -> (
    SocketAddr,
    tokio_rustls::rustls::pki_types::CertificateDer<'static>,
) {
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let der = cert.cert.der().clone();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![der.clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = tls.read(&mut buf).await;
                let _ = tls
                    .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (addr, der)
}

#[tokio::test]
async fn adm_23_tls_opaque_tcp_and_probes_are_not_metered() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 1000).await;
    let _held = hold_slot(&f, &app).await;
    app.counts.delay_ms.store(0, Ordering::SeqCst);
    // (1) Inbound TLS is a tunnel.
    let (tls_app, cert) = https_app().await;
    let tls_side = inbound(tls_app, &state, |s| s).await;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = TcpStream::connect(tls_side.addr).await.unwrap();
    let mut tls = tokio::time::timeout(
        TIMEOUT,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(
            tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        ),
    )
    .await
    .expect("a handshake in time")
    .expect("a handshake");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut answer)).await;
    assert!(answer.starts_with(b"HTTP/1.1 204 "), "{answer:?}");
    // (2) Opaque TCP is a tunnel.
    let echo = echo_server().await;
    let echo_side = inbound(echo, &state, |s| s).await;
    let mut c = TcpStream::connect(echo_side.addr).await.unwrap();
    c.write_all(b"HELO\r\n").await.unwrap();
    let mut back = [0u8; 6];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut back))
        .await
        .expect("echoed in time")
        .unwrap();
    assert_eq!(&back, b"HELO\r\n");
    // (3) A probe is served by the admin port.
    let json = format!(
        r#"{{"web/readyz":{{"path":"/p","port":{},"scheme":"HTTP","timeoutSeconds":1}}}}"#,
        app.addr.port()
    );
    let env = Env::from_pairs([
        ("MIMIC_APP_PROBES".to_string(), json),
        ("MIMIC_POD_IP".to_string(), "127.0.0.1".to_string()),
    ]);
    let router = probes::router(Arc::new(Probes::from_env(&env).unwrap()));
    let request = axum::http::Request::get("/app-health/web/readyz")
        .body(axum::body::Body::empty())
        .unwrap();
    let status = router.oneshot(request).await.unwrap().status().as_u16();
    assert_eq!(status, 200);
    assert_eq!(state.admission_counts_for_test().in_flight, 1);
}

/// Sends one PRX-15 conversation through `f` to the destination listener `l`: what the client
/// got and what the destination received.
async fn converse(
    f: &Fixture,
    l: TcpListener,
    steps: Vec<(Vec<u8>, Vec<u8>)>,
) -> (Vec<u8>, Vec<u8>) {
    let server = tokio::spawn({
        let steps = steps.clone();
        async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut received = Vec::new();
            let mut expected = 0;
            for (req, resp) in steps {
                expected += req.len();
                while received.len() < expected {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap();
                    assert!(n > 0, "destination: client closed early");
                    received.extend_from_slice(&buf[..n]);
                }
                s.write_all(&resp).await.unwrap();
            }
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
            received.extend_from_slice(&rest);
            received
        }
    });
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    for (req, _) in &steps {
        if !req.is_empty() {
            c.write_all(req).await.unwrap();
        }
    }
    let got = read_all(&mut c).await.expect("a FIN");
    drop(c);
    let received = tokio::time::timeout(TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    (got, received)
}

/// A head whose fields are dripped one byte every 100 ms reaches the app byte by byte: what was
/// sent is there before the blank line that ends it. (Protocol detection keeps the request line
/// until it is whole, with or without a contract.)
async fn reaches_byte_by_byte(f: &Fixture, app: &Http1App) {
    let mut c = connect(f, app).await;
    let line = b"GET /seen HTTP/1.1\r\n";
    let before = app.raw.lock().unwrap().len();
    c.write_all(line).await.unwrap();
    for b in b"Host: a\r\n" {
        c.write_all(&[*b]).await.unwrap();
        tokio::time::sleep(ms(100)).await;
    }
    until("the head reaches the app before its end", || {
        app.raw.lock().unwrap().len() >= before + line.len() + 9
    })
    .await;
    c.write_all(b"\r\n").await.unwrap();
    let mut got = Vec::new();
    read_until(&mut c, &mut got, b"\r\n\r\nok").await;
}

/// 6 concurrent `/` through `f`, all answered, all in the app at once.
async fn six_at_once(f: &Fixture, app: &Http1App) {
    app.counts.max_in_flight.store(0, Ordering::SeqCst);
    app.counts.delay_ms.store(200, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for i in 0..6 {
        let addr = f.addr;
        tasks.push(tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&get(&format!("/six{i}"))).await.unwrap();
            let mut got = Vec::new();
            read_until(&mut c, &mut got, b"\r\n\r\nok").await;
            got
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), OK);
    }
    assert_eq!(app.max_in_flight(), 6);
    app.counts.delay_ms.store(0, Ordering::SeqCst);
}

#[tokio::test]
async fn adm_23_without_a_contract_or_with_the_mesh_off_nothing_is_held() {
    let _logs = capture_logs();
    // (1) A view without a contract: the PRX-15 corpus is byte-exact both ways.
    let state = meshed(Some(POD), |_| {});
    state.hold(no_contract(), "\"1-a\"");
    for conversation in corpus() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = inbound(l.local_addr().unwrap(), &state, |s| s).await;
        let (got, received) = converse(&f, l, conversation.steps.clone()).await;
        let sent: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(r, _)| r.clone())
            .collect();
        let answered: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(_, r)| r.clone())
            .collect();
        assert_eq!(got, answered, "{}: client side", conversation.name);
        assert_eq!(received, sent, "{}: destination side", conversation.name);
    }
    let app = App::http1(0).await;
    let f = inbound(app.addr, &state, |s| s).await;
    reaches_byte_by_byte(&f, &app).await;
    six_at_once(&f, &app).await;
    // (2) The mesh off.
    let app = App::http1(0).await;
    let addr = app.addr;
    let off = fixture(Direction::Inbound, move |_| Some(addr)).await;
    reaches_byte_by_byte(&off, &app).await;
    six_at_once(&off, &app).await;
    // (2) A view of `"mesh": false`.
    let state = meshed(Some(POD), |_| {});
    let mut view = view_services(&[]);
    view.mesh = false;
    state.hold(view, "\"1-a\"");
    let app = App::http1(0).await;
    let f = inbound(app.addr, &state, |s| s).await;
    reaches_byte_by_byte(&f, &app).await;
    six_at_once(&f, &app).await;
}

#[tokio::test]
async fn adm_24_http1_removing_the_contract_stops_holding_heads() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 0).await;
    state.hold(no_contract(), "\"2-a\"");
    reaches_byte_by_byte(&f, &app).await;
}

#[tokio::test]
async fn adm_21_a_refusal_comes_after_the_whole_earlier_response() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut c = connect(&f, &app).await;
    let mut both = get("/chunked");
    both.extend_from_slice(&get("/r2"));
    c.write_all(&both).await.unwrap();
    let refused = kept("queue-timeout", app.addr, ORIGIN);
    let mut got = Vec::new();
    read_until(&mut c, &mut got, refused.as_bytes()).await;
    let chunked = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\na\r\n1\r\nb\r\n1\r\nc\r\n0\r\n\r\n";
    assert_eq!(
        String::from_utf8(got).unwrap(),
        format!("{chunked}{refused}")
    );
    assert!(app.got("/chunked") && !app.got("/r2"), "{}", app.raw_text());
    // The connection goes on.
    c.write_all(&get("/r3")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut c, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
}

#[tokio::test]
async fn adm_30_on_shutdown_a_queued_request_is_decided_within_its_150_ms() {
    let _logs = capture_logs();
    // The slot is held past the queue time: the queued one is refused. B is a second request
    // on a kept connection, which looks idle to the codec while it is held.
    let (app, state, f) = rig(contract(1, 1000), 0).await;
    let mut b = connect(&f, &app).await;
    b.write_all(&get("/b1")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut b, &mut got, b"\r\n\r\nok").await;
    app.counts.delay_ms.store(1000, Ordering::SeqCst);
    let mut a = hold_slot(&f, &app).await;
    let started = Instant::now();
    let written = Instant::now();
    b.write_all(&get("/r2")).await.unwrap();
    until("B is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    f.shared.shutdown();
    // Kept after the refusal, then closed idle by the shutdown: a FIN.
    let b = read_all(&mut b).await.expect("a FIN after the answer");
    let took = written.elapsed();
    assert_eq!(
        String::from_utf8(b).unwrap(),
        kept("queue-timeout", app.addr, ORIGIN)
    );
    assert!((ms(150)..=ms(400)).contains(&took), "{took:?}");
    let got = read_all(&mut a).await.expect("a FIN after the answer");
    assert_eq!(got, OK);
    assert!(started.elapsed() < ms(1500), "{:?}", started.elapsed());

    // The slot frees first: the queued one is admitted.
    let (app, state, f) = rig(contract(1, 1000), 100).await;
    let _a = hold_slot(&f, &app).await;
    let mut b = connect(&f, &app).await;
    let written = Instant::now();
    b.write_all(&get("/r2")).await.unwrap();
    // Queued, unless the slot freed already (A takes 100 ms).
    until("B is queued or admitted", || {
        state.admission_counts_for_test().depth == 1 || app.counts.requests() == 2
    })
    .await;
    f.shared.shutdown();
    let got = read_all(&mut b).await.expect("a FIN after the answer");
    assert_eq!(got, OK);
    let arrived = app.counts.arrivals()[1];
    assert!(
        arrived.saturating_duration_since(written) < ms(150),
        "{:?}",
        arrived.saturating_duration_since(written)
    );
}

// --- ADM-20: the connection after a refusal ---------------------------------------------

/// A new connection, once the sidecar dialled the app for it, and the index of its app
/// connection in `per_conn`.
async fn connect_indexed(f: &Fixture, app: &Http1App) -> (TcpStream, usize) {
    let c = connect(f, app).await;
    (c, app.counts.accepted() - 1)
}

/// A body of `n` bytes that starts with a marker the app must never see.
fn body(n: usize) -> Vec<u8> {
    let mut body = b"adm20-body-".to_vec();
    body.resize(n, b'x');
    body
}

fn post(path: &str, length: usize) -> Vec<u8> {
    format!("POST {path} HTTP/1.1\r\nHost: cart.shop.svc\r\ncontent-length: {length}\r\n\r\n")
        .into_bytes()
}

#[tokio::test]
async fn adm_20_a_refused_request_without_a_large_body_keeps_its_connection() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 400).await;
    let overloaded = kept("overloaded", app.addr, ORIGIN);
    for i in 1..=4 {
        let (mut r, index) = connect_indexed(&f, &app).await;
        let (mut held, _filler) = full_queue(&f, &app, &state).await;
        let path = format!("/r{i}");
        // (k1) no body; (k2) 1024 bytes with the head; (k3) the same, 500 ms after the answer;
        // (k4) 65536 bytes with the head.
        let (head, rest, late) = match i {
            1 => (get(&path), Vec::new(), false),
            2 => (post(&path, 1024), body(1024), false),
            3 => (post(&path, 1024), body(1024), true),
            _ => (post(&path, 65536), body(65536), false),
        };
        let mut first = head;
        if !late {
            first.extend_from_slice(&rest);
        }
        let written = Instant::now();
        r.write_all(&first).await.unwrap();
        let answer = one_answer(&mut r).await;
        let took = written.elapsed();
        assert_eq!(answer, overloaded, "{path}");
        assert!(took <= ms(50), "{path}: {took:?}");
        if late {
            tokio::time::sleep(ms(500)).await;
            r.write_all(&rest).await.unwrap();
        }
        let (status, _) = held.read().await.expect("the held answer");
        assert!(status.starts_with("HTTP/1.1 200 "), "{status}");
        let next = get(&format!("/n{i}"));
        r.write_all(&next).await.unwrap();
        let mut got = Vec::new();
        read_until(&mut r, &mut got, b"\r\n\r\nok").await;
        assert_eq!(got, OK, "{path}");
        assert_eq!(
            app.per_conn()[index],
            next.len(),
            "{path}: R's app connection"
        );
    }
    for path in ["/r1", "/r2", "/r3", "/r4", "adm20-body-"] {
        assert!(!app.got(path), "{path} reached the app: {}", app.raw_text());
    }
}

#[tokio::test]
async fn adm_20_a_body_that_never_comes_closes_the_connection_with_a_fin_after_1_s() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 400).await;
    let (mut r, index) = connect_indexed(&f, &app).await;
    let _queue = full_queue(&f, &app, &state).await;
    r.write_all(&post("/r5", 1024)).await.unwrap();
    let answer = one_answer(&mut r).await;
    let answered = Instant::now();
    assert_eq!(answer, kept("overloaded", app.addr, ORIGIN));
    let end = tokio::time::timeout(Duration::from_secs(3), read_all(&mut r))
        .await
        .expect("the connection ends within 3 s");
    let took = answered.elapsed();
    assert_eq!(end.expect("a FIN, not a reset"), b"");
    assert!((ms(1000)..=ms(1500)).contains(&took), "{took:?}");
    assert!(!app.got("/r5"), "{}", app.raw_text());
    assert_eq!(app.per_conn()[index], 0);
}

#[tokio::test]
async fn adm_20_the_other_requests_are_answered_connection_close_then_fin() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 400).await;
    let refused = closed("overloaded", app.addr, ORIGIN);
    let cases: Vec<(&str, Vec<u8>, &[u8])> = vec![
        ("content-length 65537", post("/c1", 65537), b""),
        (
            "chunked",
            b"POST /c2 HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            b"5\r\nhello\r\n0\r\n\r\n",
        ),
        (
            "expect",
            b"POST /c3 HTTP/1.1\r\nHost: cart.shop.svc\r\nExpect: 100-continue\r\ncontent-length: 10\r\n\r\n".to_vec(),
            b"",
        ),
        (
            "http/1.0",
            b"GET /c4 HTTP/1.0\r\nHost: cart.shop.svc\r\n\r\n".to_vec(),
            b"",
        ),
        (
            "connection close",
            b"GET /c5 HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: close\r\n\r\n".to_vec(),
            b"",
        ),
        (
            "content-length and transfer-encoding",
            b"POST /c6 HTTP/1.1\r\nHost: cart.shop.svc\r\ncontent-length: 5\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            b"",
        ),
    ];
    for (name, head, later) in cases {
        let (mut r, index) = connect_indexed(&f, &app).await;
        let (mut held, _filler) = full_queue(&f, &app, &state).await;
        r.write_all(&head).await.unwrap();
        if !later.is_empty() {
            tokio::time::sleep(ms(50)).await;
            r.write_all(later).await.unwrap();
        }
        let got = read_all(&mut r).await.expect("a FIN, not a reset");
        assert_eq!(String::from_utf8(got).unwrap(), refused, "{name}");
        assert_eq!(app.per_conn()[index], 0, "{name}: a byte reached the app");
        held.read().await.expect("the held answer");
    }
}

// --- ADM-21: pipelining ------------------------------------------------------------------

/// Reads exactly `n` bytes.
async fn read_n(c: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut got = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut got))
        .await
        .expect("read in time")
        .expect("no reset");
    got
}

#[tokio::test]
async fn adm_21_pipelined_the_refusal_follows_the_first_answer_and_the_connection_goes_on() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 300).await;
    let mut c = connect(&f, &app).await;
    let mut both = get("/p1");
    both.extend_from_slice(&get("/p2"));
    c.write_all(&both).await.unwrap();
    let refused = kept("queue-timeout", app.addr, ORIGIN);
    let mut got = Vec::new();
    read_until(&mut c, &mut got, refused.as_bytes()).await;
    let want = [OK, refused.as_bytes()].concat();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want)
    );
    assert!(app.got("/p1") && !app.got("/p2"), "{}", app.raw_text());
    c.write_all(&get("/p3")).await.unwrap();
    let mut got = Vec::new();
    read_until(&mut c, &mut got, b"\r\n\r\nok").await;
    assert_eq!(got, OK);
}

/// `GET /q1` and `GET /q2` in one write through a sidecar holding `view`: the two answers, and
/// how long the app took to have both heads.
async fn pipelined(view: mimic_core::mesh::view::View) -> (Vec<u8>, Duration) {
    let (app, _state, f) = rig(view, 300).await;
    let mut c = connect(&f, &app).await;
    let mut both = get("/q1");
    both.extend_from_slice(&get("/q2"));
    let written = Instant::now();
    c.write_all(&both).await.unwrap();
    until("both heads reach the app", || {
        app.got("GET /q1 ") && app.got("GET /q2 ")
    })
    .await;
    let reached = written.elapsed();
    let got = read_n(&mut c, 2 * OK.len()).await;
    (got, reached)
}

#[tokio::test]
async fn adm_21_with_two_slots_both_pipelined_requests_reach_the_app_byte_for_byte() {
    let _logs = capture_logs();
    let (admitted, reached) = pipelined(contract(2, 1000)).await;
    assert!(reached < ms(100), "{reached:?}");
    let (plain, _) = pipelined(no_contract()).await;
    assert_eq!(admitted, plain);
    let state = meshed(Some(POD), |_| {});
    state.hold(contract(1000, 100_000), "\"1-a\"");
    for conversation in corpus() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = inbound(l.local_addr().unwrap(), &state, |s| s).await;
        let (got, received) = converse(&f, l, conversation.steps.clone()).await;
        let sent: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(r, _)| r.clone())
            .collect();
        let answered: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(_, r)| r.clone())
            .collect();
        assert_eq!(got, answered, "{}: client side", conversation.name);
        assert_eq!(received, sent, "{}: destination side", conversation.name);
    }
}

// --- ADM-25: the app's forged fields and the sidecar's own refusals ----------------------

#[tokio::test]
async fn adm_25_http1_forged_fields_are_stripped_and_own_refusals_keep_theirs() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 400).await;
    let mut a = connect(&f, &app).await;
    a.write_all(&get("/forge")).await.unwrap();
    until("the app has /forge", || app.got("/forge")).await;
    let (b, _, _b) = refused_on_new(&f, &app, &get("/b")).await;
    assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN));
    let a = one_answer(&mut a).await;
    assert_eq!(
        a,
        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n"
    );
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_stripped_headers_total", &[]),
        2.0,
        "{scrape}"
    );
}

// --- ADM-27: the access line, RED and discovery of a refusal -----------------------------

/// The lines of `text` a component wrote at `WARN` or `INFO` (not the access log, not the test's
/// own log sync).
fn component_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["level"] == "WARN" || v["level"] == "INFO")
        .filter(|v| v["target"] != "access" && v["target"] != "sync")
        .collect()
}

#[tokio::test]
async fn adm_27_http1_each_refusal_writes_one_line_counts_5xx_and_synthetic() {
    let logs = capture_logs();
    let app = App::http1(400).await;
    let state = meshed(Some(POD), |_| {});
    state.hold(contract(1, 1000), "\"1-a\"");
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let addr = app.addr;
    let f = fixture_mesh_on(
        Direction::Inbound,
        Arc::new(move |_| Ok(addr)),
        &state,
        Some(&discovery),
        |s| s,
    )
    .await;
    let mut a = connect(&f, &app).await;
    a.write_all(&get("/ok")).await.unwrap();
    until("the app holds /ok", || app.in_flight() == 1).await;
    logs.settled().await;
    let before = logs.text().len();
    let b = spawn_refused(&f, &app, "/b?q=1");
    until("B is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    let (c, _, _c) = refused_on_new(
        &f,
        &app,
        b"POST /c HTTP/1.1\r\nHost: cart.shop.svc\r\ncontent-length: 0\r\n\r\n",
    )
    .await;
    assert_eq!(c, kept("overloaded", app.addr, ORIGIN));
    let (b, _, _b) = b.await.unwrap();
    assert_eq!(b, kept("queue-timeout", app.addr, ORIGIN));
    logs.settled().await;
    let between = component_lines(&logs.text()[before..]);
    assert!(between.is_empty(), "{between:?}");
    let mut got = Vec::new();
    read_until(&mut a, &mut got, b"\r\n\r\nok").await;

    let lines = logs.access_exactly(3).await;
    let line = |path: &str| {
        lines
            .iter()
            .find(|l| l["path"] == path)
            .unwrap_or_else(|| panic!("no line for {path}: {lines:?}"))
    };
    let b = line("/b");
    assert_eq!(
        (&b["status"], &b["reason"], &b["method"], &b["protocol"]),
        (
            &serde_json::json!(503),
            &serde_json::json!("queue-timeout"),
            &serde_json::json!("GET"),
            &serde_json::json!("http1")
        ),
        "{b}"
    );
    assert_eq!(
        (
            &b["direction"],
            &b["result"],
            &b["bytes_in"],
            &b["bytes_out"]
        ),
        (
            &serde_json::json!("inbound"),
            &serde_json::json!("ok"),
            &serde_json::json!(0),
            &serde_json::json!(0)
        ),
        "{b}"
    );
    assert!(b["duration_ms"].as_u64().unwrap() >= 150, "{b}");
    assert!(b.get("upstream").is_none(), "{b}");
    let c = line("/c");
    assert_eq!(
        (&c["status"], &c["reason"], &c["method"]),
        (
            &serde_json::json!(503),
            &serde_json::json!("overloaded"),
            &serde_json::json!("POST")
        ),
        "{c}"
    );
    assert!(c["duration_ms"].as_u64().unwrap() <= 50, "{c}");
    assert!(c.get("upstream").is_none(), "{c}");
    let a = line("/ok");
    assert_eq!(a["status"], 200, "{a}");
    assert!(a.get("reason").is_none(), "{a}");
    // The authority is read as for an admitted request.
    assert_eq!(b["authority"], a["authority"], "{b}");

    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_http_requests_total{",
            &["direction=\"inbound\"", "status_class=\"5xx\""]
        ),
        2.0,
        "{scrape}"
    );
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"synthetic\"} 2",
    )
    .await;
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    let report = discovery.report();
    let templates: Vec<String> = report.endpoints().into_iter().map(|e| e.template).collect();
    assert_eq!(templates, ["/ok"]);
}

/// A secret-looking value that must reach no log line (gitleaks allowlists the test files).
const CANARY: &str = "adm27-canary-5c1e7f";

#[tokio::test]
async fn adm_27_a_refusal_line_carries_no_header_value_query_or_body() {
    use http_body_util::{BodyExt, Full};
    let logs = capture_logs();
    // HTTP/1.
    let (app, _state, f) = rig(contract(1, 1000), 400).await;
    let _held = hold_slot(&f, &app).await;
    let secret_body = format!("{{\"password\":\"{CANARY}\"}}");
    let request = format!(
        "POST /p?token={CANARY} HTTP/1.1\r\nHost: cart.shop.svc\r\nx-api-key: {CANARY}\r\ncontent-length: {}\r\n\r\n{secret_body}",
        secret_body.len()
    );
    let (answer, _, _c) = refused_on_new(&f, &app, request.as_bytes()).await;
    assert_eq!(answer, kept("queue-timeout", app.addr, ORIGIN));

    // h2c.
    let app = App::h2c(400).await;
    let state = meshed(Some(POD), |_| {});
    state.hold(contract(1, 1000), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (sender, connection) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .handshake::<_, Full<bytes::Bytes>>(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(connection);
    let mut first = sender.clone();
    let _held = tokio::spawn(async move {
        let request = hyper::Request::get("http://cart.shop.svc/slow")
            .body(Full::new(bytes::Bytes::new()))
            .unwrap();
        first.send_request(request).await.map(|r| r.status())
    });
    until("the app holds the first stream", || app.in_flight() == 1).await;
    let request = hyper::Request::post(format!("http://cart.shop.svc/p?token={CANARY}"))
        .header("x-api-key", CANARY)
        .body(Full::new(bytes::Bytes::from(secret_body)))
        .unwrap();
    let mut second = sender.clone();
    let response = tokio::time::timeout(TIMEOUT, second.send_request(request))
        .await
        .expect("answered in time")
        .expect("an answer");
    assert_eq!(response.status(), 503);
    let _ = response.into_body().collect().await;

    logs.settled().await;
    let refusals: Vec<(String, String)> = logs
        .access()
        .iter()
        .filter(|l| l["path"] == "/p")
        .map(|l| (l["protocol"].to_string(), l["reason"].to_string()))
        .collect();
    let want = |protocol: &str| (format!("\"{protocol}\""), "\"queue-timeout\"".to_string());
    assert_eq!(refusals, [want("http1"), want("h2")]);
    let text = logs.text();
    assert!(
        !text.contains(CANARY),
        "a log line carries the query, a header or the body"
    );
}

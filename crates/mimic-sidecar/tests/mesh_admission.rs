//! ADM-16 to ADM-18, ADM-24 and ADM-28 on `h2c`: the inbound side of a meshed sidecar admits
//! each stream under the contract of its view (spec §4.1 "Admission contract"). An inbound
//! sidecar in process in front of an `h2c` app that counts.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use common::admission::{
    ANSWER_TIMEOUT, Answer, App, call, contract, inbound, meshed, no_contract, refusal_fields,
};
use common::faults::no_series;
use common::{Fixture, H2Sender, Logs, capture_logs, h2_client, until};
use http_body_util::BodyExt;
use hyper::Request;
use mimic_runtime::config::Env;
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::task::JoinHandle;

const ORIGIN: &str = "inbound; pod=cart-7d9f-x2k";

/// An app answering after `delay_ms`, and a meshed sidecar in front of it holding `view`.
async fn rig(view: mimic_core::mesh::view::View, delay_ms: u64) -> (App, MeshState, Fixture) {
    let app = App::h2c(delay_ms).await;
    let state = meshed(Some("cart-7d9f-x2k"), |_| {});
    state.hold(view, "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    (app, state, f)
}

/// Sends `/` on a clone of `sender` in a task.
fn spawn_call(sender: &H2Sender, path: &'static str) -> JoinHandle<Answer> {
    let mut sender = sender.clone();
    tokio::spawn(async move { call(&mut sender, path, &[]).await })
}

/// A held slot: a `/` the app holds (its delay is set long before), sent first.
async fn hold_slot(app: &App, sender: &H2Sender) -> JoinHandle<Answer> {
    let before = app.counts.requests();
    let held = spawn_call(sender, "/");
    until("the app holds the first request", || {
        app.counts.requests() > before && app.in_flight() >= 1
    })
    .await;
    held
}

fn assert_refused(answer: &Answer, error: &str, app: std::net::SocketAddr) {
    assert_eq!(answer.status, 503, "{answer:?}");
    assert_eq!(
        answer.headers,
        refusal_fields(error, app, ORIGIN),
        "{answer:?}"
    );
    assert!(answer.body.is_empty(), "{answer:?}");
}

/// The lines of `message` so far.
async fn lines(logs: &Logs, message: &str) -> Vec<serde_json::Value> {
    logs.settled().await;
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == message)
        .collect()
}

#[tokio::test]
async fn adm_16_h2c_refusals_have_the_four_fields_and_the_connection_goes_on() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 1000).await;
    let mut client = h2_client(f.addr).await;
    let first = hold_slot(&app, &client).await;
    let second = spawn_call(&client, "/");
    until("the second stream is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    let third = call(&mut client, "/", &[]).await;
    assert_refused(&third, "overloaded", app.addr);
    assert!(third.elapsed <= Duration::from_millis(50), "{third:?}");
    let second = second.await.unwrap();
    assert_refused(&second, "queue-timeout", app.addr);
    assert!(
        (Duration::from_millis(150)..=Duration::from_millis(400)).contains(&second.elapsed),
        "{second:?}"
    );
    let first = first.await.unwrap();
    assert_eq!(first.status, 200);
    app.counts.delay_ms.store(0, Ordering::SeqCst);
    let fourth = call(&mut client, "/", &[]).await;
    assert_eq!(fourth.status, 200, "{fourth:?}");
    assert_eq!(
        app.counts.requests(),
        2,
        "the refused streams never reach the app"
    );
    assert!(no_series(
        &f.metrics.encode(),
        "mimic_mesh_inbound_refusals_total"
    ));
}

#[tokio::test]
async fn adm_16_h2c_a_refused_grpc_request_gets_the_same_answer_without_grpc_status() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 1000).await;
    let mut client = h2_client(f.addr).await;
    let _first = hold_slot(&app, &client).await;
    let _second = spawn_call(&client, "/");
    until("the second stream is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    let request = Request::post("http://cart.shop.svc/shop.Cart/Get")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let (head, trailers) = tokio::time::timeout(ANSWER_TIMEOUT, async {
        let response = client.send_request(request).await.expect("an answer");
        let (head, body) = response.into_parts();
        let collected = body.collect().await.expect("a whole body");
        (head, collected.trailers().cloned())
    })
    .await
    .expect("answered in time");
    assert_eq!(head.status.as_u16(), 503);
    let mut headers: Vec<(String, String)> = head
        .headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    headers.sort();
    assert_eq!(headers, refusal_fields("overloaded", app.addr, ORIGIN));
    assert_eq!(trailers, None, "no grpc-status, no grpc-message");
    assert_eq!(app.counts.requests(), 1);
}

#[tokio::test]
async fn adm_17_h2c_twice_the_concurrency_never_exceeds_it_and_refuses_the_rest() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(4, 1000), 200).await;
    let client = h2_client(f.addr).await;
    let end = Instant::now() + Duration::from_secs(3);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let mut sender = client.clone();
        tasks.push(tokio::spawn(async move {
            let (mut ok, mut overloaded, mut timeouts, mut errors, mut other) = (0, 0, 0, 0, 0);
            while Instant::now() < end {
                let request = Request::get("http://cart.shop.svc/")
                    .body(http_body_util::Empty::<Bytes>::new())
                    .unwrap();
                let answered = tokio::time::timeout(ANSWER_TIMEOUT, async {
                    let response = sender.send_request(request).await?;
                    let status = response.status().as_u16();
                    let error = response
                        .headers()
                        .get("x-mimic-error")
                        .map(|v| v.to_str().unwrap().to_string());
                    response.into_body().collect().await?;
                    Ok::<_, hyper::Error>((status, error))
                })
                .await
                .expect("answered in time");
                let admitted = matches!(answered, Ok((200, _)));
                match answered {
                    Ok((200, None)) => ok += 1,
                    Ok((503, Some(e))) if e == "overloaded" => overloaded += 1,
                    Ok((503, Some(e))) if e == "queue-timeout" => timeouts += 1,
                    Ok(_) => other += 1,
                    Err(_) => errors += 1,
                }
                if !admitted {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            (ok, overloaded, timeouts, errors, other)
        }));
    }
    let (mut ok, mut overloaded, mut timeouts, mut errors, mut other) = (0, 0, 0, 0, 0);
    for task in tasks {
        let (a, b, c, d, e) = task.await.unwrap();
        (ok, overloaded, timeouts, errors, other) =
            (ok + a, overloaded + b, timeouts + c, errors + d, other + e);
    }
    assert_eq!(
        app.max_in_flight(),
        4,
        "the app never has more than 4 in flight"
    );
    assert_eq!(
        (errors, other),
        (0, 0),
        "every request is answered 200 or refused"
    );
    assert!(overloaded > 0 && timeouts > 0, "{overloaded} {timeouts}");
    assert_eq!(ok, app.counts.requests());
}

#[tokio::test]
async fn adm_17_h2c_twice_the_rate_is_admitted_at_the_rate() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1000, 50), 0).await;
    let start = Instant::now();
    let end = start + Duration::from_secs(4);
    let mut tasks = Vec::new();
    for i in 0..20u64 {
        let mut sender = h2_client(f.addr).await;
        tasks.push(tokio::spawn(async move {
            let first = tokio::time::Instant::from_std(start) + Duration::from_millis(i * 10);
            let mut ticks = tokio::time::interval_at(first, Duration::from_millis(200));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let (mut sent, mut refused, mut last) = (0usize, 0usize, start);
            loop {
                ticks.tick().await;
                if Instant::now() >= end {
                    break;
                }
                let answer = call(&mut sender, "/", &[]).await;
                sent += 1;
                last = Instant::now();
                match answer.status {
                    200 => {}
                    503 if answer.error().is_some() => refused += 1,
                    status => panic!("unexpected {status}: {answer:?}"),
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
        "{admitted} admitted in {t:.3} s"
    );
    assert!(
        admitted as f64 >= (0.9 * 50.0 * t).ceil(),
        "{admitted} admitted in {t:.3} s"
    );
    assert_eq!(admitted + refused, sent, "the rest are refused");
}

#[tokio::test]
async fn adm_18_h2c_a_response_with_trailers_frees_the_slot_at_the_trailers() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 0).await;
    let mut client = h2_client(f.addr).await;
    let request = Request::get("http://cart.shop.svc/trailers")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let a = tokio::time::timeout(ANSWER_TIMEOUT, client.send_request(request))
        .await
        .expect("a head in time")
        .expect("a head");
    assert_eq!(a.status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = call(&mut client, "/", &[]).await;
    assert_refused(&b, "queue-timeout", app.addr);
    let collected = tokio::time::timeout(ANSWER_TIMEOUT, a.into_body().collect())
        .await
        .expect("the body in time")
        .expect("a whole body");
    assert_eq!(
        collected.trailers().map(|t| t["x-t"].clone()),
        Some("1".parse().unwrap())
    );
    assert_eq!(state.admission_counts_for_test().in_flight, 0);
    let c = call(&mut client, "/", &[]).await;
    assert_eq!(c.status, 200, "{c:?}");
    assert!(c.elapsed < Duration::from_millis(100), "{c:?}");
}

#[tokio::test]
async fn adm_18_h2c_an_sse_stream_holds_the_slot_and_a_stream_reset_frees_it() {
    let _logs = capture_logs();
    let (app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut events = h2_client(f.addr).await;
    let request = Request::get("http://cart.shop.svc/sse")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let mut a = tokio::time::timeout(ANSWER_TIMEOUT, async {
        let response = events.send_request(request).await.expect("a head");
        let mut body = response.into_body();
        let frame = body.frame().await.expect("a frame").expect("an event");
        assert_eq!(frame.into_data().unwrap(), Bytes::from("data: 1\n\n"));
        body
    })
    .await
    .expect("the first event in time");
    let mut client = h2_client(f.addr).await;
    let t0 = Instant::now();
    for at in [200, 1200] {
        tokio::time::sleep_until((t0 + Duration::from_millis(at)).into()).await;
        let b = call(&mut client, "/", &[]).await;
        assert_refused(&b, "queue-timeout", app.addr);
    }
    // The event stream is still open: dropping its body resets the stream.
    assert!(
        tokio::time::timeout(Duration::from_millis(10), a.frame())
            .await
            .is_err()
    );
    drop(a);
    let c = call(&mut client, "/", &[]).await;
    assert_eq!(c.status, 200, "{c:?}");
    assert!(c.elapsed < Duration::from_millis(100), "{c:?}");
}

#[tokio::test]
async fn adm_18_h2c_an_app_that_closes_without_answering_frees_the_slot() {
    let _logs = capture_logs();
    let (_app, _state, f) = rig(contract(1, 1000), 0).await;
    let mut first = h2_client(f.addr).await;
    let request = Request::get("http://cart.shop.svc/close")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let a = tokio::time::timeout(ANSWER_TIMEOUT, first.send_request(request))
        .await
        .expect("an end in time");
    assert!(a.is_err(), "the stream is reset: {a:?}");
    let mut client = h2_client(f.addr).await;
    let c = call(&mut client, "/", &[]).await;
    assert_eq!(c.status, 200, "{c:?}");
    assert!(c.elapsed < Duration::from_millis(100), "{c:?}");
}

#[tokio::test]
async fn adm_24_a_view_that_adds_a_contract_applies_to_the_next_stream() {
    let _logs = capture_logs();
    let (app, state, f) = rig(no_contract(), 300).await;
    let mut client = h2_client(f.addr).await;
    let three: Vec<_> = (0..3).map(|_| spawn_call(&client, "/")).collect();
    for answer in three {
        assert_eq!(answer.await.unwrap().status, 200);
    }
    assert_eq!(app.max_in_flight(), 3);
    state.hold(contract(1, 1000), "\"2-a\"");
    app.counts.delay_ms.store(1000, Ordering::SeqCst);
    let _held = hold_slot(&app, &client).await;
    let next = call(&mut client, "/", &[]).await;
    assert_refused(&next, "queue-timeout", app.addr);
}

#[tokio::test]
async fn adm_24_a_lower_contract_cuts_nothing_and_refuses_until_below_it() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(4, 1000), 500).await;
    let mut client = h2_client(f.addr).await;
    let four: Vec<_> = (0..4).map(|_| spawn_call(&client, "/")).collect();
    until("4 in flight", || app.in_flight() == 4).await;
    app.counts.delay_ms.store(0, Ordering::SeqCst);
    state.hold(contract(1, 1000), "\"2-a\"");
    let refused = call(&mut client, "/", &[]).await;
    assert_refused(&refused, "queue-timeout", app.addr);
    for answer in four {
        assert_eq!(answer.await.unwrap().status, 200);
    }
    let after = call(&mut client, "/", &[]).await;
    assert_eq!(after.status, 200, "{after:?}");
    assert!(after.elapsed < Duration::from_millis(100), "{after:?}");
}

#[tokio::test]
async fn adm_24_a_higher_or_removed_contract_admits_the_queued_stream_at_once() {
    let _logs = capture_logs();
    for (name, new) in [("raised", contract(3, 1000)), ("removed", no_contract())] {
        let (app, state, f) = rig(contract(1, 1000), 1000).await;
        let client = h2_client(f.addr).await;
        let held = hold_slot(&app, &client).await;
        app.counts.delay_ms.store(0, Ordering::SeqCst);
        let queued = spawn_call(&client, "/");
        until("B is queued", || {
            state.admission_counts_for_test().depth == 1
        })
        .await;
        let t = Instant::now();
        state.hold(new, "\"2-a\"");
        let b = queued.await.unwrap();
        assert_eq!(b.status, 200, "{name}: {b:?}");
        let arrived = app.counts.arrivals()[1];
        assert!(
            arrived.saturating_duration_since(t) < Duration::from_millis(50),
            "{name}: B arrived {:?} after the view",
            arrived.saturating_duration_since(t)
        );
        if name == "removed" {
            assert_eq!(held.await.unwrap().status, 200);
            app.counts.max_in_flight.store(0, Ordering::SeqCst);
            app.counts.delay_ms.store(300, Ordering::SeqCst);
            let three: Vec<_> = (0..3).map(|_| spawn_call(&client, "/")).collect();
            for answer in three {
                assert_eq!(answer.await.unwrap().status, 200);
            }
            assert!(app.max_in_flight() >= 3, "later streams are not held");
        }
    }
}

#[tokio::test]
async fn adm_24_a_stale_view_keeps_its_contract() {
    let logs = capture_logs();
    let app = App::h2c(1000).await;
    let state = meshed(Some("cart-7d9f-x2k"), |c| {
        c.stale_after = Duration::from_millis(200);
    });
    state.hold(contract(1, 1000), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    state.tick();
    assert_eq!(
        lines(&logs, "mesh view stale").await.len(),
        1,
        "the view is stale"
    );
    let mut client = h2_client(f.addr).await;
    let _held = hold_slot(&app, &client).await;
    let next = call(&mut client, "/", &[]).await;
    assert_refused(&next, "queue-timeout", app.addr);
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `<dir>/state/mesh.json` holding `view`, fetched now.
fn persist(dir: &std::path::Path, view: &mimic_core::mesh::view::View) {
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(
        dir.join("state").join("mesh.json"),
        format!(
            "{{\"fetchedAt\":{},\"view\":{}}}",
            unix_now(),
            view.to_json()
        ),
    )
    .unwrap();
}

/// A meshed state of `MIMIC_MESH=on` with no controller, its state file in `dir`.
fn from_env(dir: &std::path::Path, vars: &[(&str, &str)]) -> MeshState {
    let env = Env::from_pairs(
        [("MIMIC_MESH", "on"), ("MIMIC_POD_NAME", "cart-7d9f-x2k")]
            .iter()
            .chain(vars)
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    MeshState::new(&MeshConfig::from_env(&env, dir).unwrap())
}

#[tokio::test]
async fn adm_24_a_restart_with_only_the_persisted_view_enforces_its_contract() {
    let _logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    persist(dir.path(), &contract(1, 1000));
    let state = from_env(dir.path(), &[]);
    state.restore().await;
    let app = App::h2c(1000).await;
    let f = inbound(app.addr, &state, |s| s).await;
    let mut client = h2_client(f.addr).await;
    let _held = hold_slot(&app, &client).await;
    let next = call(&mut client, "/", &[]).await;
    assert_refused(&next, "queue-timeout", app.addr);
}

#[tokio::test]
async fn adm_28_without_a_view_one_warning_after_the_startup_wait_and_nothing_held() {
    let logs = capture_logs();
    let app = App::h2c(200).await;
    let t0 = Instant::now();
    let state = meshed(Some("cart-7d9f-x2k"), |c| {
        c.startup_wait = Duration::from_millis(300);
    });
    tokio::spawn(state.clone().run_age(std::future::pending()));
    let f = inbound(app.addr, &state, |s| s).await;
    let client = h2_client(f.addr).await;
    let six: Vec<_> = (0..6).map(|_| spawn_call(&client, "/")).collect();
    tokio::time::sleep_until((t0 + Duration::from_millis(150)).into()).await;
    assert!(
        lines(&logs, "admission contract unknown").await.is_empty(),
        "no warning before the startup wait"
    );
    for answer in six {
        assert_eq!(answer.await.unwrap().status, 200);
    }
    assert_eq!(app.max_in_flight(), 6, "nothing is held");
    tokio::time::sleep_until((t0 + Duration::from_secs(1)).into()).await;
    let warned = lines(&logs, "admission contract unknown").await;
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["level"], "WARN");
    assert_eq!(warned[0]["target"], "mesh");
    tokio::time::sleep_until((t0 + Duration::from_millis(1600)).into()).await;
    assert_eq!(lines(&logs, "admission contract unknown").await.len(), 1);
    state.hold(contract(1, 1000), "\"1-a\"");
    app.counts.delay_ms.store(1000, Ordering::SeqCst);
    let mut client = client;
    let _held = hold_slot(&app, &client).await;
    let next = call(&mut client, "/", &[]).await;
    assert_refused(&next, "queue-timeout", app.addr);
}

#[tokio::test]
async fn adm_28_a_persisted_or_timely_view_writes_no_warning() {
    let logs = capture_logs();
    let wait = [("MIMIC_MESH_STARTUP_WAIT", "300ms")];
    // (a) A persisted view without a contract.
    let dir = tempfile::tempdir().unwrap();
    persist(dir.path(), &no_contract());
    let t0 = Instant::now();
    let state = from_env(dir.path(), &wait);
    state.restore().await;
    tokio::spawn(state.clone().run_age(std::future::pending()));
    // (b) No file, a view held at 100 ms.
    let empty = tempfile::tempdir().unwrap();
    let other = from_env(empty.path(), &wait);
    tokio::spawn(other.clone().run_age(std::future::pending()));
    tokio::time::sleep_until((t0 + Duration::from_millis(100)).into()).await;
    other.hold(no_contract(), "\"1-a\"");
    tokio::time::sleep_until((t0 + Duration::from_millis(900)).into()).await;
    assert!(lines(&logs, "admission contract unknown").await.is_empty());
}

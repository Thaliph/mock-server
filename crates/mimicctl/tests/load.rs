//! Critères LOD-09 à LOD-11 de la spec (§5.9) : `mimicctl load` contre un serveur local.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_core::load::{Corpus, Criteria, Saturation, verdict};
use mimic_core::model::{Body, Direction, Exchange};
use mimicctl::load::{
    Options, Plan, Stop, Target, isolation, parse_duration, parse_ratio, pod_usage, run,
    selected_pods,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

fn exchange(path: &str, at: u64) -> Exchange {
    Exchange {
        id: at.to_string(),
        direction: Direction::Inbound,
        started_at_ms: at,
        scheme: "http".into(),
        authority: "svc:8080".into(),
        method: "POST".into(),
        path: path.into(),
        query: vec![("q".into(), "a b".into())],
        req_headers: vec![],
        req_body: Body::captured(b"ping".to_vec()),
        status: 200,
        resp_headers: vec![],
        resp_body: Body::Empty,
        ttfb_us: 1,
        total_us: 1,
        trace_id: None,
        redactions: vec![],
    }
}

fn corpus() -> Arc<Corpus> {
    Arc::new(Corpus::new(&[exchange("/a", 1), exchange("/b", 2)], &[]))
}

/// Compteurs du serveur : requêtes reçues, en cours, maximum en cours, bodies corrects.
#[derive(Default)]
struct Seen {
    total: AtomicU64,
    in_flight: AtomicU64,
    max_in_flight: AtomicU64,
    good: AtomicU64,
}

/// Serveur HTTP/1.1 local : au plus `slots` requêtes traitées à la fois, chacune `work`.
async fn server(slots: usize, work: Duration) -> (SocketAddr, Arc<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let gate = Arc::new(Semaphore::new(slots));
    let s = seen.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (seen, gate) = (s.clone(), gate.clone());
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let (seen, gate) = (seen.clone(), gate.clone());
                    async move {
                        use http_body_util::BodyExt;
                        seen.total.fetch_add(1, Ordering::SeqCst);
                        let now = seen.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        seen.max_in_flight.fetch_max(now, Ordering::SeqCst);
                        let ok = req.method() == "POST"
                            && req.uri().query() == Some("q=a%20b")
                            && req.headers().get("host").is_some();
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        if ok && body == "ping" {
                            seen.good.fetch_add(1, Ordering::SeqCst);
                        }
                        {
                            let _permit = gate.acquire().await.unwrap();
                            tokio::time::sleep(work).await;
                        }
                        seen.in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from("pong"))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, seen)
}

fn options() -> Options {
    Options {
        timeout: Duration::from_secs(5),
        criteria: Criteria {
            max_error_rate: 0.01,
            max_p99_us: None,
            plateau: 0.1,
        },
        sample_interval: Duration::from_secs(15),
        sampler: None,
        progress: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lod_09_fixed_concurrency_sends_exactly_n_requests_with_at_most_c_in_flight() {
    let (addr, seen) = server(64, Duration::from_millis(2)).await;
    let target = Target::parse(&format!("http://{addr}"), false).unwrap();
    let plan = Plan::Fixed {
        concurrency: 4,
        stop: Stop::Requests(200),
    };
    let out = run(corpus(), &target, &plan, &options()).await;
    assert_eq!(seen.total.load(Ordering::SeqCst), 200);
    assert_eq!(seen.good.load(Ordering::SeqCst), 200);
    assert_eq!(seen.max_in_flight.load(Ordering::SeqCst), 4);
    assert_eq!(out.steps.len(), 1);
    let s = &out.steps[0];
    assert_eq!(
        (s.concurrency, s.requests, s.ok, s.errors),
        (4, 200, 200, 0)
    );
    assert!(s.p50_us >= 2_000, "{s:?}");
    assert!(s.rps > 0.0);

    // Cible injoignable : erreurs comptées, le tir se termine.
    let dead = Target::parse("http://127.0.0.1:9", false).unwrap();
    let plan = Plan::Fixed {
        concurrency: 2,
        stop: Stop::Requests(10),
    };
    let out = run(corpus(), &dead, &plan, &options()).await;
    assert_eq!(out.steps[0].errors, 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lod_11_ramp_stops_at_the_first_saturated_step() {
    let (addr, _) = server(4, Duration::from_millis(5)).await;
    let target = Target::parse(&format!("http://{addr}/"), false).unwrap();
    let plan = Plan::Ramp {
        steps: mimic_core::load::ramp(1, 2.0, 64).unwrap(),
        step_duration: Duration::from_millis(400),
    };
    let out = run(corpus(), &target, &plan, &options()).await;
    let concurrency: Vec<u32> = out.steps.iter().map(|s| s.concurrency).collect();
    assert_eq!(concurrency, [1, 2, 4, 8], "{:?}", out.steps);
    let v = verdict(&out.steps, &options().criteria);
    assert_eq!(
        v.saturated,
        Some((3, Saturation::Plateau)),
        "{:?}",
        out.steps
    );
    assert_eq!(v.capacity, Some(2), "{:?}", out.steps);
}

#[test]
fn lod_10_isolation_requires_mock_and_all_selected_pods_aligned() {
    let w = |mode: &str, selected: u64, aligned: u64| json!({"desired": {"mode": mode}, "selected_pods": selected, "aligned": aligned});
    let err = isolation(&w("record", 2, 2)).unwrap_err();
    assert!(err.contains("record"), "{err}");
    let err = isolation(&w("mock-partial", 2, 2)).unwrap_err();
    assert!(err.contains("mock-partial"), "{err}");
    let err = isolation(&w("mock", 2, 1)).unwrap_err();
    assert!(err.contains("1/2"), "{err}");
    assert!(isolation(&w("mock", 0, 0)).is_err());
    assert!(isolation(&w("mock", 2, 2)).is_ok());
    assert!(isolation(&json!({})).is_err());

    let pods = json!({"pods": [
        {"instance": "p1", "selected": true},
        {"instance": "p2", "selected": false},
        {"instance": "p3", "selected": true},
    ]});
    assert_eq!(selected_pods(&pods), ["p1", "p3"]);
}

#[test]
fn lod_08_pod_metrics_and_cli_values() {
    let m = json!({
        "metadata": {"name": "checkout-7d9f-abcde"},
        "containers": [
            {"name": "app", "usage": {"cpu": "250m", "memory": "128Mi"}},
            {"name": "mimic", "usage": {"cpu": "1500000n", "memory": "20Mi"}},
        ],
    });
    let (pod, containers) = pod_usage(&m).unwrap();
    assert_eq!(pod, "checkout-7d9f-abcde");
    assert_eq!(containers[0], ("app".into(), 0.25, 134_217_728));
    assert_eq!(containers[1].0, "mimic");
    assert_eq!(pod_usage(&json!({})), None);

    assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
    assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    assert!(parse_duration("5").is_err());
    assert!(parse_duration("0s").is_err());
    assert_eq!(parse_ratio("1%").unwrap(), 0.01);
    assert_eq!(parse_ratio("0.3").unwrap(), 0.3);
    assert!(parse_ratio("-1").is_err());
    assert!(parse_ratio("abc").is_err());

    assert!(Target::parse("https://checkout:8443", false).is_err());
    assert!(Target::parse("checkout:8080", false).is_err());
    let t = Target::parse("http://checkout.perf:8080/api/", false).unwrap();
    assert_eq!(t.uri("/orders?x=1"), "/api/orders?x=1");
    let t = Target::parse("http://checkout.perf", true).unwrap();
    assert_eq!(t.uri("/orders"), "http://checkout.perf/orders");
}

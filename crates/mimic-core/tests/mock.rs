//! Critères d'acceptation MCK-* de la spec (§5.3 et §11.2), au niveau du moteur.

mod common;

use common::{ExchangeExt, exchange, sequence};
use mimic_core::mock::{MatchKind, MockConfig, MockDecision, MockEngine, MockRequest, Strategy};
use mimic_core::model::{Body, Direction, Exchange, Header};
use serde_json::{Value, json};

const API: &str = "api.saas.io:443";

/// Enregistrement d'un appel sortant vers la dépendance.
fn out(method: &str, path_and_query: &str) -> Exchange {
    let mut e = exchange(method, path_and_query);
    e.direction = Direction::Outbound;
    e.scheme = "https".into();
    e.authority = API.into();
    e
}

fn engine(recordings: Vec<Exchange>, config: MockConfig) -> MockEngine {
    MockEngine::new(sequence(recordings), config)
}

fn req(method: &str, path_and_query: &str) -> MockRequest {
    let e = out(method, path_and_query);
    MockRequest {
        authority: e.authority,
        method: e.method,
        path: e.path,
        query: e.query,
        headers: vec![],
        body: vec![],
    }
}

fn matched(d: MockDecision) -> mimic_core::mock::MockResponse {
    match d {
        MockDecision::Matched(r) => r,
        MockDecision::Unmatched => panic!("aucune correspondance"),
    }
}

fn body_json(r: &mimic_core::mock::MockResponse) -> Value {
    serde_json::from_slice(r.body().bytes().unwrap()).unwrap()
}

fn percentile(mut v: Vec<u64>, p: f64) -> u64 {
    v.sort_unstable();
    v[((p * v.len() as f64).ceil() as usize).max(1) - 1]
}

#[test]
fn mck_01_exact_replay_of_status_headers_and_body() {
    let rec = out("GET", "/v1/a")
        .status(201)
        .resp_json(json!({"ok": true}))
        .with_resp_header("x-request-id", "r1")
        .with_resp_header("connection", "keep-alive")
        .with_resp_header("transfer-encoding", "chunked")
        .with_resp_header("content-length", "11");
    let m = engine(vec![rec.clone()], MockConfig::default());
    let r = matched(m.respond(&req("GET", "/v1/a")));
    assert_eq!(r.status, 201);
    assert_eq!(r.body(), &rec.resp_body);
    assert_eq!(r.kind, MatchKind::Exact);
    assert_eq!(r.score, 1.0);
    let names: Vec<_> = r.headers().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["content-type", "x-request-id"]);
}

#[test]
fn mck_02_identifier_segments_match_by_template() {
    let rec = out("GET", "/v1/customers/1").resp_json(json!({"id": 1}));
    let m = engine(vec![rec], MockConfig::default());
    let r = matched(m.respond(&req("GET", "/v1/customers/2")));
    assert_eq!(r.kind, MatchKind::Template);
    assert!(r.score < 1.0);
    assert_eq!(
        m.respond(&req("GET", "/v1/customers/me")),
        MockDecision::Unmatched
    );
}

#[test]
fn mck_03_unknown_authority_is_unmatched() {
    let m = engine(vec![out("GET", "/v1/a")], MockConfig::default());
    let mut r = req("GET", "/v1/a");
    r.authority = "other.io:443".into();
    assert_eq!(m.respond(&r), MockDecision::Unmatched);
}

#[test]
fn mck_04_latency_follows_recorded_distribution() {
    let recs = (50..=150)
        .map(|ms| out("GET", "/slow").latency_ms(ms))
        .collect();
    let m = engine(recs, MockConfig::default());
    let lat: Vec<u64> = (0..1_000)
        .map(|_| matched(m.respond(&req("GET", "/slow"))).latency_us)
        .collect();
    let p50 = percentile(lat, 0.5);
    assert!((90_000..=110_000).contains(&p50), "p50 = {p50} µs");
}

#[test]
fn mck_05_latency_factor_scales_replay() {
    let recs = (50..=150)
        .map(|ms| out("GET", "/slow").latency_ms(ms))
        .collect();
    let config = MockConfig {
        latency_factor: 2.0,
        ..MockConfig::default()
    };
    let m = engine(recs, config);
    let lat: Vec<u64> = (0..1_000)
        .map(|_| matched(m.respond(&req("GET", "/slow"))).latency_us)
        .collect();
    let p50 = percentile(lat, 0.5);
    assert!((180_000..=220_000).contains(&p50), "p50 = {p50} µs");

    let config = MockConfig {
        latency_factor: 0.0,
        ..MockConfig::default()
    };
    let m = engine(vec![out("GET", "/slow").latency_ms(100)], config);
    assert_eq!(matched(m.respond(&req("GET", "/slow"))).latency_us, 0);
}

#[test]
fn mck_06_sequence_replays_in_recording_order() {
    let recs = vec![
        out("GET", "/cart").resp_json(json!({"items": 0})),
        out("GET", "/cart").resp_json(json!({"items": 1})),
        out("GET", "/cart").resp_json(json!({"items": 2})),
    ];
    let config = MockConfig {
        strategy: Strategy::Sequence,
        ..MockConfig::default()
    };
    let m = engine(recs, config);
    let seen: Vec<Value> = (0..4)
        .map(|_| body_json(&matched(m.respond(&req("GET", "/cart"))))["items"].clone())
        .collect();
    assert_eq!(seen, vec![json!(0), json!(1), json!(2), json!(0)]);
}

#[test]
fn mck_06b_round_robin_rotates_over_equivalent_recordings() {
    let recs = vec![
        out("GET", "/r").resp_json(json!({"n": 0})),
        out("GET", "/r").resp_json(json!({"n": 1})),
    ];
    let config = MockConfig {
        strategy: Strategy::RoundRobin,
        ..MockConfig::default()
    };
    let m = engine(recs, config);
    let seen: Vec<Value> = (0..3)
        .map(|_| body_json(&matched(m.respond(&req("GET", "/r"))))["n"].clone())
        .collect();
    assert_eq!(seen, vec![json!(0), json!(1), json!(0)]);
}

#[test]
fn best_strategy_prefers_most_recent_among_equals() {
    let recs = vec![
        out("GET", "/b").resp_json(json!({"v": "old"})),
        out("GET", "/b").resp_json(json!({"v": "new"})),
    ];
    let m = engine(recs, MockConfig::default());
    assert_eq!(
        body_json(&matched(m.respond(&req("GET", "/b"))))["v"],
        "new"
    );
}

#[test]
fn mck_12_best_prefers_most_frequent_response_among_copies() {
    // Le même appel vu par trois sidecars : deux fois 200, une fois 500, le 500 en dernier.
    let recs = vec![
        out("GET", "/users/7").resp_json(json!({"name": "a"})),
        out("GET", "/users/7").resp_json(json!({"name": "a"})),
        out("GET", "/users/7")
            .status(500)
            .resp_json(json!({"error": "boom"})),
    ];
    let m = engine(recs, MockConfig::default());
    let r = matched(m.respond(&req("GET", "/users/7")));
    assert_eq!(r.status, 200);
    assert_eq!(body_json(&r)["name"], "a");
}

#[test]
fn mck_07_weighted_status_reproduces_error_rate() {
    let mut recs: Vec<_> = (0..98)
        .map(|_| out("POST", "/charge").status(200))
        .collect();
    recs.extend((0..2).map(|_| out("POST", "/charge").status(429)));
    let config = MockConfig {
        strategy: Strategy::WeightedStatus,
        ..MockConfig::default()
    };
    let m = engine(recs, config);
    let throttled = (0..10_000)
        .filter(|_| matched(m.respond(&req("POST", "/charge"))).status == 429)
        .count();
    assert!(
        (150..=250).contains(&throttled),
        "429 = {throttled} / 10000"
    );
}

#[test]
fn mck_08_content_encoding_is_kept() {
    let rec = out("GET", "/gz")
        .resp_json(json!({"a": 1}))
        .with_resp_header("content-encoding", "gzip");
    let m = engine(vec![rec], MockConfig::default());
    let r = matched(m.respond(&req("GET", "/gz")));
    assert!(
        r.headers()
            .any(|h| h.name == "content-encoding" && h.value == "gzip")
    );
    assert_eq!(body_json(&r), json!({"a": 1}));
}

#[test]
fn mck_09_query_values_drive_the_choice() {
    let recs = vec![
        out("GET", "/list?page=1").resp_json(json!({"page": 1})),
        out("GET", "/list?page=2").resp_json(json!({"page": 2})),
        out("GET", "/list?page=3").resp_json(json!({"page": 3})),
    ];
    let m = engine(recs, MockConfig::default());
    let r = matched(m.respond(&req("GET", "/list?page=2")));
    assert_eq!(body_json(&r)["page"], 2);
    assert_eq!(r.kind, MatchKind::Exact);
    let r = matched(m.respond(&req("GET", "/list?page=9")));
    assert_eq!(r.kind, MatchKind::Fuzzy);
}

#[test]
fn mck_09b_request_body_drives_the_choice() {
    let recs = vec![
        out("POST", "/q")
            .req_json(json!({"sku": "A"}))
            .resp_json(json!({"r": "A"})),
        out("POST", "/q")
            .req_json(json!({"sku": "B"}))
            .resp_json(json!({"r": "B"})),
    ];
    let m = engine(recs, MockConfig::default());
    let mut r = req("POST", "/q");
    r.headers = vec![Header::new("content-type", "application/json")];
    r.body = serde_json::to_vec(&json!({"sku": "A"})).unwrap();
    assert_eq!(body_json(&matched(m.respond(&r)))["r"], "A");
}

#[test]
fn match_headers_are_compared_when_configured() {
    let recs = vec![
        out("GET", "/v")
            .with_req_header("x-api-version", "1")
            .resp_json(json!({"v": 1})),
        out("GET", "/v")
            .with_req_header("x-api-version", "2")
            .resp_json(json!({"v": 2})),
    ];
    let config = MockConfig {
        match_headers: vec!["x-api-version".into()],
        ..MockConfig::default()
    };
    let m = engine(recs, config);
    let mut r = req("GET", "/v");
    r.headers = vec![Header::new("X-Api-Version", "1")];
    assert_eq!(body_json(&matched(m.respond(&r)))["v"], 1);
}

#[test]
fn mck_10_method_and_authority_are_mandatory() {
    let m = engine(vec![out("GET", "/x")], MockConfig::default());
    assert_eq!(m.respond(&req("POST", "/x")), MockDecision::Unmatched);
    assert_eq!(m.respond(&req("GET", "/y")), MockDecision::Unmatched);
}

#[test]
fn mck_11_same_seed_same_decisions() {
    let recs: Vec<_> = (0..20)
        .map(|i| {
            out("GET", "/s")
                .status(if i % 4 == 0 { 500 } else { 200 })
                .latency_ms(10 + i)
        })
        .collect();
    let config = MockConfig {
        strategy: Strategy::WeightedStatus,
        seed: 42,
        ..MockConfig::default()
    };
    let run = || {
        let m = engine(recs.clone(), config.clone());
        (0..50)
            .map(|_| {
                let r = matched(m.respond(&req("GET", "/s")));
                (r.status, r.latency_us)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn inbound_and_bodyless_recordings_are_not_served() {
    let mut inbound = out("GET", "/in");
    inbound.direction = Direction::Inbound;
    let mut skipped = out("GET", "/skip").resp_json(json!({}));
    skipped.resp_body = Body::Skipped { size: 10 };
    let m = engine(vec![inbound, skipped], MockConfig::default());
    assert_eq!(m.respond(&req("GET", "/in")), MockDecision::Unmatched);
    assert_eq!(m.respond(&req("GET", "/skip")), MockDecision::Unmatched);
}

#[test]
fn bodyless_recordings_still_feed_latency() {
    let full = out("GET", "/l").resp_json(json!({})).latency_ms(10);
    let mut slow = out("GET", "/l").resp_json(json!({})).latency_ms(1_000);
    slow.resp_body = Body::Skipped { size: 2 };
    let recs: Vec<_> = std::iter::once(full)
        .chain(std::iter::repeat_n(slow, 99))
        .collect();
    let m = engine(recs, MockConfig::default());
    let lat: Vec<u64> = (0..200)
        .map(|_| matched(m.respond(&req("GET", "/l"))).latency_us)
        .collect();
    assert!(percentile(lat, 0.5) >= 1_000_000);
}

#[test]
fn engine_reports_its_content() {
    let m = engine(
        vec![out("GET", "/a"), out("GET", "/b")],
        MockConfig::default(),
    );
    assert_eq!(m.len(), 2);
    assert_eq!(m.authorities(), vec![API.to_string()]);
}

#[test]
fn mck_13_only_replayable_records_of_the_operation_are_compared() {
    let mut recs: Vec<Exchange> = (1..=3)
        .map(|i| {
            out("GET", &format!("/users/{i}"))
                .resp_json(json!({"n": i}))
                .latency_ms(1)
        })
        .collect();
    for i in 0..10_000 {
        let mut e = out("GET", &format!("/users/{}", 100 + i))
            .resp_json(json!({}))
            .latency_ms(50);
        e.resp_body = Body::Skipped { size: 2 };
        recs.push(e);
    }
    recs.extend((0..5_000).map(|i| out("GET", &format!("/orders/{i}/lines")).resp_json(json!({}))));
    let m = engine(recs, MockConfig::default());
    let r = req("GET", "/users/9");
    assert_eq!(m.candidates(&r), 3);
    let lat: Vec<u64> = (0..500)
        .map(|_| matched(m.respond(&r)).latency_us)
        .collect();
    assert!(lat.contains(&50_000), "durées sans body ignorées");
    assert_eq!(percentile(lat, 0.5), 50_000);
    assert_eq!(m.candidates(&req("GET", "/orders/1/lines")), 5_000);
    assert_eq!(m.candidates(&req("GET", "/users/me")), 0);
    assert_eq!(m.respond(&req("GET", "/users/me")), MockDecision::Unmatched);
}

#[test]
fn mck_14_engine_is_shared_between_threads() {
    fn shared<T: Send + Sync>(_: &T) {}
    const THREADS: usize = 8;
    const PER_THREAD: usize = 1_000;
    let mut recs: Vec<Exchange> = (0..4)
        .map(|i| {
            out("GET", "/rr")
                .resp_json(json!({"n": i}))
                .latency_ms(10 + i)
        })
        .collect();
    recs.extend((0..20).map(|i| {
        out("GET", "/w")
            .status(if i % 5 == 0 { 503 } else { 200 })
            .latency_ms(1 + i)
    }));
    recs.push(out("GET", "/best").resp_json(json!({"ok": 1})));
    recs.push(
        out("GET", "/best")
            .resp_json(json!({"ok": 1}))
            .latency_ms(9),
    );
    // Réponses servies (code, latence, échange), triées : l'ordre d'arrivée ne compte pas.
    let run = |strategy: Strategy, path: &'static str, concurrent: bool| {
        let config = MockConfig {
            strategy,
            seed: 7,
            ..MockConfig::default()
        };
        let m = engine(recs.clone(), config);
        shared(&m);
        let one = |m: &MockEngine| {
            let r = matched(m.respond(&req("GET", path)));
            (r.status, r.latency_us, r.exchange_id.clone())
        };
        let mut served: Vec<(u16, u64, String)> = if concurrent {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|_| s.spawn(|| (0..PER_THREAD).map(|_| one(&m)).collect::<Vec<_>>()))
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap())
                    .collect()
            })
        } else {
            (0..THREADS * PER_THREAD).map(|_| one(&m)).collect()
        };
        served.sort();
        served
    };
    assert_eq!(
        run(Strategy::Best, "/best", true),
        run(Strategy::Best, "/best", false)
    );
    let weighted = run(Strategy::WeightedStatus, "/w", true);
    assert!(weighted.iter().any(|r| r.0 == 503));
    assert_eq!(weighted, run(Strategy::WeightedStatus, "/w", false));
    let mut per: std::collections::BTreeMap<String, usize> = Default::default();
    for (_, _, id) in run(Strategy::RoundRobin, "/rr", true) {
        *per.entry(id).or_default() += 1;
    }
    assert_eq!(per.len(), 4, "{per:?}");
    assert!(
        per.values().all(|n| *n == THREADS * PER_THREAD / 4),
        "{per:?}"
    );
}

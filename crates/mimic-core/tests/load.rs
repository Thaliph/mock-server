//! Critères LOD-* de la spec (§5.9) : tir de charge par rejeu des requêtes enregistrées.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::load::{
    Corpus, Criteria, Histogram, PodSample, Report, Saturation, StepResult, aggregate, parse_cpu,
    parse_header, parse_memory, ramp, recommend, render_markdown, render_text, summarize, verdict,
};
use mimic_core::model::{Body, Direction, Exchange};

fn inbound(path: &str, at: u64, id: &str) -> Exchange {
    exchange("GET", path).at(at, id)
}

#[test]
fn lod_01_request_replays_method_path_query_body_without_connection_or_redacted_headers() {
    let mut post = exchange("POST", "/orders?coupon=a b&x=%&k")
        .at(1, "1")
        .with_req_header("authorization", "<redacted:sha256:0123abcd>")
        .with_req_header("connection", "keep-alive")
        .with_req_header("host", "checkout:8080")
        .with_req_header("x-tenant", "t1")
        .with_req_header("X-Tenant", "t2");
    post.req_body = Body::captured(vec![0xff, 0x00, 0x10]);
    let mut outbound = inbound("/out", 2, "2");
    outbound.direction = Direction::Outbound;
    let mut truncated = inbound("/big", 3, "3");
    truncated.req_body = Body::Truncated {
        size: 10_000,
        prefix: vec![1, 2, 3],
    };
    let extra = vec![parse_header("Authorization: Bearer t").unwrap()];

    let corpus = Corpus::new(&[post, outbound, truncated], &extra);
    assert_eq!(corpus.len(), 1);
    assert_eq!(corpus.truncated(), 1);
    let r = corpus.nth(0);
    assert_eq!(r.method, "POST");
    assert_eq!(r.path_and_query, "/orders?coupon=a%20b&x=%25&k=");
    assert_eq!(r.body, vec![0xff, 0x00, 0x10]);
    assert_eq!(
        r.headers,
        [
            ("authorization".to_string(), "Bearer t".to_string()),
            ("x-tenant".to_string(), "t1".to_string()),
            ("x-tenant".to_string(), "t2".to_string()),
        ]
    );
    assert!(parse_header("sans-deux-points").is_err());
    assert!(parse_header(": vide").is_err());
}

#[test]
fn lod_02_requests_cycle_in_recording_order() {
    let list = [
        inbound("/c", 30, "c"),
        inbound("/a", 10, "a"),
        inbound("/b", 20, "b"),
    ];
    let corpus = Corpus::new(&list, &[]);
    let paths: Vec<&str> = (0..7)
        .map(|n| corpus.nth(n).path_and_query.as_str())
        .collect();
    assert_eq!(paths, ["/a", "/b", "/c", "/a", "/b", "/c", "/a"]);
    assert!(Corpus::new(&[] as &[Exchange], &[]).is_empty());
}

#[test]
fn lod_03_ramp_multiplies_up_to_max_included() {
    assert_eq!(ramp(1, 2.0, 20).unwrap(), [1, 2, 4, 8, 16, 20]);
    assert_eq!(ramp(4, 3.0, 4).unwrap(), [4]);
    assert_eq!(ramp(3, 1.5, 10).unwrap(), [3, 5, 8, 10]);
    assert!(ramp(1, 1.0, 10).is_err());
    assert!(ramp(0, 2.0, 10).is_err());
    assert!(ramp(8, 2.0, 4).is_err());
}

/// Rang le plus proche, sur des valeurs triées.
fn exact(sorted: &[u64], q: f64) -> u64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn close(got: u64, want: u64) {
    let err = (got as f64 - want as f64).abs() / want as f64;
    assert!(err < 0.01, "{got} au lieu de {want} ({:.3} %)", err * 100.0);
}

#[test]
fn lod_04_histogram_quantiles_within_one_percent_and_merge() {
    // Latences de 1 µs à 60 s, réparties sur toutes les décades.
    let mut values: Vec<u64> = (0..20_000u64)
        .map(|i| {
            let x = (i * 7919) % 20_000;
            (10f64.powf(x as f64 / 20_000.0 * 7.78)) as u64 + 1
        })
        .collect();
    let (left, right) = values.split_at(7_000);
    let (mut a, mut b, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
    left.iter().for_each(|v| a.record(*v));
    right.iter().for_each(|v| b.record(*v));
    values.iter().for_each(|v| all.record(*v));
    a.merge(&b);
    assert_eq!(a, all);
    values.sort_unstable();
    assert_eq!(a.count(), 20_000);
    for q in [0.5, 0.95, 0.99] {
        close(a.quantile(q), exact(&values, q));
    }
    assert_eq!(a.max(), *values.last().unwrap());
    assert!(*values.last().unwrap() >= 59_000_000);
    assert_eq!(Histogram::new().quantile(0.99), 0);
}

fn step(concurrency: u32, rps: f64, error_rate: f64, p99_us: u64) -> StepResult {
    let duration_us = 10_000_000;
    let total = (rps * 10.0 / (1.0 - error_rate)).round() as u64;
    let errors = total - (rps * 10.0).round() as u64;
    let mut latency = Histogram::new();
    for _ in 0..total {
        latency.record(p99_us);
    }
    StepResult {
        concurrency,
        duration_us,
        ok: total - errors,
        errors,
        latency,
        samples: vec![],
    }
}

fn criteria() -> Criteria {
    Criteria {
        max_error_rate: 0.01,
        max_p99_us: None,
        plateau: 0.05,
    }
}

#[test]
fn lod_05_errors_saturate_the_step() {
    let steps: Vec<_> = [
        step(1, 100.0, 0.0, 1_000),
        step(2, 190.0, 0.0, 1_000),
        step(4, 370.0, 0.03, 1_000),
    ]
    .iter()
    .map(summarize)
    .collect();
    let v = verdict(&steps, &criteria());
    assert_eq!(v.capacity, Some(1));
    assert_eq!(v.saturated, Some((2, Saturation::Errors)));
}

#[test]
fn lod_06_p99_above_slo_saturates_the_step() {
    let steps: Vec<_> = [
        step(1, 100.0, 0.0, 50_000),
        step(2, 190.0, 0.0, 120_000),
        step(4, 370.0, 0.0, 400_000),
    ]
    .iter()
    .map(summarize)
    .collect();
    let c = Criteria {
        max_p99_us: Some(200_000),
        ..criteria()
    };
    let v = verdict(&steps, &c);
    assert_eq!(v.capacity, Some(1));
    assert_eq!(v.saturated, Some((2, Saturation::Latency)));
}

#[test]
fn lod_07_throughput_plateau_saturates_the_step() {
    let steps: Vec<_> = [
        step(1, 100.0, 0.0, 1_000),
        step(2, 195.0, 0.0, 1_000),
        step(4, 200.0, 0.0, 1_000),
    ]
    .iter()
    .map(summarize)
    .collect();
    let v = verdict(&steps, &criteria());
    assert_eq!(v.capacity, Some(1));
    assert_eq!(v.saturated, Some((2, Saturation::Plateau)));
    // Premier palier saturé : pas de capacité.
    let first = vec![summarize(&step(1, 100.0, 0.5, 1_000))];
    let v = verdict(&first, &criteria());
    assert_eq!(v.capacity, None);
    assert_eq!(v.saturated, Some((0, Saturation::Errors)));
}

fn sample(at_s: u64, pod: &str, app_cpu: &str, app_mem: &str) -> PodSample {
    PodSample {
        at_us: at_s * 1_000_000,
        pod: pod.into(),
        containers: vec![
            (
                "app".into(),
                parse_cpu(app_cpu).unwrap(),
                parse_memory(app_mem).unwrap(),
            ),
            ("mimic".into(), 2.0, 1 << 30),
            ("istio-proxy".into(), 3.0, 1 << 30),
        ],
    }
}

#[test]
fn lod_08_quantities_aggregation_and_recommendation() {
    assert_eq!(parse_cpu("250m"), Some(0.25));
    assert_eq!(parse_cpu("1"), Some(1.0));
    assert_eq!(parse_cpu("1500000n"), Some(0.0015));
    assert_eq!(parse_cpu("2500u"), Some(0.0025));
    assert_eq!(parse_cpu("abc"), None);
    assert_eq!(parse_memory("128Mi"), Some(134_217_728));
    assert_eq!(parse_memory("1Gi"), Some(1_073_741_824));
    assert_eq!(parse_memory("500k"), Some(500_000));
    assert_eq!(parse_memory("12345"), Some(12_345));
    assert_eq!(parse_memory("2M"), Some(2_000_000));
    assert_eq!(parse_memory("x"), None);

    // Palier de 60 s : le relevé à 10 s (premier tiers) est ignoré.
    let samples = vec![
        sample(10, "p1", "4", "4Gi"),
        sample(30, "p1", "400m", "100Mi"),
        sample(45, "p1", "600m", "120Mi"),
        sample(30, "p2", "500m", "90Mi"),
    ];
    let usage = aggregate(&samples, 60_000_000).unwrap();
    assert_eq!(usage.pods, 2);
    // Moyenne par pod (0,5 et 0,5), puis entre pods ; maximum de mémoire.
    assert!((usage.cpu_cores - 0.5).abs() < 1e-9, "{usage:?}");
    assert_eq!(usage.memory_bytes, 120 * 1024 * 1024);
    assert_eq!(aggregate(&samples[..1], 60_000_000), None);

    let mut s = summarize(&step(4, 800.0, 0.0, 1_000));
    s.usage = Some(usage);
    let r = recommend(&s, 0.3, 0.2).unwrap();
    assert_eq!(r.cpu_millicores, 650);
    assert_eq!(r.memory_mib, 144);
    assert!((r.rps_per_pod - 400.0).abs() < 1e-9);
    assert!((r.rps_per_core.unwrap() - 800.0).abs() < 1e-9);
    s.usage = None;
    assert_eq!(recommend(&s, 0.3, 0.2), None);
}

#[test]
fn lod_12_reports_are_deterministic_and_json_reads_back() {
    let report = || {
        let steps: Vec<_> = [
            step(1, 100.0, 0.0, 1_000),
            step(2, 195.0, 0.0, 2_000),
            step(4, 200.0, 0.0, 9_000),
        ]
        .iter()
        .map(summarize)
        .collect();
        Report::new(
            "http://checkout.perf:8080",
            3,
            1,
            steps,
            &criteria(),
            0.3,
            0.2,
            vec!["aucun relevé de ressources".into()],
        )
    };
    let (a, b) = (report(), report());
    assert_eq!(render_text(&a), render_text(&b));
    assert_eq!(render_markdown(&a), render_markdown(&b));
    let json = serde_json::to_string(&a).unwrap();
    assert_eq!(json, serde_json::to_string(&b).unwrap());
    let back: Report = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);

    let text = render_text(&a);
    assert!(text.contains("http://checkout.perf:8080"), "{text}");
    assert!(text.contains("capacité"), "{text}");
    assert!(text.contains("plateau"), "{text}");
    assert!(text.contains("aucun relevé de ressources"), "{text}");
    let md = render_markdown(&a);
    assert!(md.contains("| palier |"), "{md}");
}

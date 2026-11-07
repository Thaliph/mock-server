//! LDT-03, LDT-04 and LDT-05: schedule, results, validation, gate and the run's objects.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde_json::{Value, json};

use super::gate::{Gate, Target, gate, targets};
use super::histogram::Histogram;
use super::job::{JobInput, PolicyInput, job, job_name, network_policy};
use super::plan::{Endpoint, Plan};
use super::schedule::{Pacer, Share, pods, sends_due, split, workers};
use super::summary::{Class, ErrorKind, PodResults, Stats, summary};
use super::validate::{Limits, Params, validate};
use crate::Mode;

/// Largest relative error of a bucket bound: 2^(1/16).
const STEP: f64 = 1.044_274_5;

mod schedule_tests {
    use super::*;

    #[test]
    fn ldt_03_open_model_ramps_then_holds_the_rate() {
        // Rate 100, rampUp 10 s, duration 20 s.
        let due = |t| sends_due(100, 10_000, 20_000, t);
        assert!(due(10_000).abs_diff(500) <= 1, "{}", due(10_000));
        assert!((due(20_000) - due(10_000)).abs_diff(1000) <= 1);
        assert_eq!(due(0), 0);
        // None after the duration.
        assert_eq!(due(25_000), due(20_000));
        assert_eq!(due(3_600_000), due(20_000));
        // Without ramp the rate is constant.
        assert_eq!(sends_due(100, 0, 20_000, 1_000), 100);
    }

    #[test]
    fn ldt_03_a_pacer_sends_what_is_due_and_nothing_after_the_duration() {
        let mut p = Pacer::new(100, 10_000, 20_000, 1_000);
        let (mut first, mut second, mut late) = (0, 0, 0);
        for t in (10..=30_000u64).step_by(10) {
            let tick = p.tick(t, 0);
            assert_eq!(tick.missed, 0);
            match t {
                ..=10_000 => first += tick.send,
                10_001..=20_000 => second += tick.send,
                _ => late += tick.send,
            }
        }
        assert!(first.abs_diff(500) <= 1, "{first}");
        assert!(second.abs_diff(1000) <= 1, "{second}");
        assert_eq!(late, 0);
    }

    #[test]
    fn ldt_03_a_send_due_with_the_concurrency_in_flight_is_missed_not_delayed() {
        let mut p = Pacer::new(100, 0, 20_000, 2);
        // 100 sends due at 1 s, everything in flight: all missed.
        let t = p.tick(1_000, 2);
        assert_eq!((t.send, t.missed), (0, 100));
        // One slot free: one send, the rest missed.
        let t = p.tick(2_000, 1);
        assert_eq!((t.send, t.missed), (1, 99));
        // Nothing is carried over: with free slots only the new sends are due.
        let t = p.tick(2_010, 0);
        assert_eq!((t.send, t.missed), (1, 0));
    }

    #[test]
    fn ldt_03_closed_model_workers_rise_from_one_to_the_concurrency() {
        let w = |t| workers(10, 10_000, 60_000, t);
        assert_eq!(w(0), 1);
        assert_eq!(w(5_000), 5);
        assert_eq!(w(10_000), 10);
        assert_eq!(w(59_999), 10);
        assert_eq!(w(60_000), 0);
        assert_eq!(workers(10, 0, 60_000, 0), 10);
        assert_eq!(workers(1, 10_000, 60_000, 3_000), 1);
    }

    #[test]
    fn ldt_03_a_run_is_split_over_ceil_concurrency_over_pod_concurrency_pods() {
        assert_eq!(pods(300, 128), 3);
        assert_eq!(pods(128, 128), 1);
        assert_eq!(pods(129, 128), 2);
        let s = split(300, Some(1001), 128);
        let share = |c, r| Share {
            concurrency: c,
            rate: Some(r),
        };
        assert_eq!(s, [share(100, 334), share(100, 334), share(100, 333)]);
        let s = split(10, None, 4);
        assert_eq!(
            s.iter().map(|s| (s.concurrency, s.rate)).collect::<Vec<_>>(),
            [(4, None), (3, None), (3, None)]
        );
    }
}

/// A deterministic stream (SplitMix64).
fn stream(mut state: u64) -> impl FnMut() -> u64 {
    move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn histogram(values: &[u64]) -> Histogram {
    let mut h = Histogram::default();
    for v in values {
        h.record(*v);
    }
    h
}

fn endpoint(method: &str, template: &str) -> Endpoint {
    Endpoint {
        method: method.into(),
        port: 8080,
        template: template.into(),
        host: None,
        weight: 1,
        volatile: vec![],
        samples: vec![],
    }
}

fn plan2() -> Plan {
    Plan {
        credentials: vec![],
        endpoints: vec![endpoint("GET", "/a"), endpoint("POST", "/b/{b_id}")],
    }
}

mod results_tests {
    use super::*;

    #[test]
    fn ldt_04_a_bucket_bound_is_at_most_4_4_percent_above_its_values() {
        let mut next = stream(1);
        let mut values: Vec<u64> = (0..=2_000).collect();
        values.extend((0..5_000).map(|_| 1 + next() % 300_000_000));
        values.extend((0..28).map(|i| 1u64 << i));
        for v in values {
            let b = Histogram::bucket(v);
            let bound = Histogram::bound(b);
            assert!(bound >= v, "value {v} above its bound {bound}");
            assert!(bound as f64 <= v.max(1) as f64 * STEP, "{v} -> {bound}");
        }
    }

    #[test]
    fn ldt_04_percentiles_of_100000_latencies_are_within_4_4_percent() {
        let mut next = stream(42);
        // Log-uniform between 10 µs and 10 s.
        let mut values: Vec<u64> = (0..100_000)
            .map(|_| {
                let x = (next() % 1_000_000) as f64 / 1_000_000.0;
                (10.0 * 1_000_000f64.powf(x)) as u64
            })
            .collect();
        let h = histogram(&values);
        values.sort_unstable();
        assert_eq!(h.count(), 100_000);
        for p in [50u64, 90, 99] {
            let exact = values[(p as usize * values.len()).div_ceil(100) - 1];
            let got = h.percentile(p);
            assert!(got >= exact, "p{p}: {got} < {exact}");
            assert!(got as f64 <= exact as f64 * STEP, "p{p}: {got} vs {exact}");
        }
        // `max` is the bound of the highest bucket.
        let top = *values.last().unwrap_or(&0);
        assert_eq!(h.max(), Histogram::bound(Histogram::bucket(top)));
        assert_eq!(Histogram::default().max(), 0);
        assert_eq!(Histogram::default().percentile(99), 0);
    }

    #[test]
    fn ldt_04_merging_is_commutative_associative_and_equals_the_union() {
        let mut next = stream(9);
        let mut set = |n: usize| -> Vec<u64> { (0..n).map(|_| next() % 5_000_000).collect() };
        let (a, b, c) = (set(300), set(500), set(7));
        let (ha, hb, hc) = (histogram(&a), histogram(&b), histogram(&c));
        let merged = |x: &Histogram, y: &Histogram| {
            let mut m = x.clone();
            m.merge(y);
            m
        };
        assert_eq!(merged(&ha, &hb), merged(&hb, &ha));
        assert_eq!(
            merged(&merged(&ha, &hb), &hc),
            merged(&ha, &merged(&hb, &hc))
        );
        let union: Vec<u64> = a.iter().chain(&b).chain(&c).copied().collect();
        assert_eq!(merged(&merged(&ha, &hb), &hc), histogram(&union));
    }

    fn pod(seed: u64) -> PodResults {
        let mut next = stream(seed);
        let mut r = PodResults {
            endpoints: vec![Stats::default(), Stats::default()],
        };
        for _ in 0..200 {
            let i = (next() % 2) as usize;
            match next() % 7 {
                0 => r.endpoints[i].error(ErrorKind::Timeout),
                1 => r.endpoints[i].missed(1 + next() % 3),
                _ => r.endpoints[i].response(200 + (next() % 4) as u16 * 100, 1 + next() % 90_000),
            }
        }
        r
    }

    #[test]
    fn ldt_04_the_summary_of_two_pods_is_the_summary_of_their_union() {
        let (a, b) = (pod(1), pod(2));
        let mut union = a.clone();
        union.merge(&b);
        let plan = plan2();
        assert_eq!(
            summary(&plan, &[a.clone(), b.clone()], 10_000),
            summary(&plan, &[union], 10_000)
        );
        assert_eq!(
            summary(&plan, &[a.clone(), b.clone()], 10_000),
            summary(&plan, &[b, a], 10_000)
        );
    }

    #[test]
    fn ldt_04_rps_is_requests_times_1000_over_elapsed_rounded() {
        let mut s = Stats::default();
        for _ in 0..7 {
            s.response(200, 100);
        }
        let pods = [PodResults {
            endpoints: vec![s, Stats::default()],
        }];
        let sum = summary(&plan2(), &pods, 2_000);
        assert_eq!(sum.total.requests, 7);
        assert_eq!(sum.total.rps, 4); // 3.5 rounds up
        assert_eq!(sum.endpoints[0].row.rps, 4);
        assert_eq!(sum.endpoints[1].row.rps, 0);
        assert_eq!(summary(&plan2(), &pods, 3_000).total.rps, 2); // 2.33
        assert_eq!(summary(&plan2(), &pods, 0).total.rps, 0);
    }

    #[test]
    fn ldt_04_statuses_and_errors_fall_in_the_listed_classes_only() {
        let mut s = Stats::default();
        for status in [200, 204, 301, 404, 503, 599] {
            s.response(status, 10);
        }
        // Not a final status: a protocol error, not a status.
        s.response(100, 10);
        s.response(600, 10);
        s.response(0, 10);
        assert_eq!(
            s.statuses,
            BTreeMap::from([
                (Class::C2xx, 2),
                (Class::C3xx, 1),
                (Class::C4xx, 1),
                (Class::C5xx, 2)
            ])
        );
        assert_eq!(s.errors, BTreeMap::from([(ErrorKind::Protocol, 3)]));
        assert_eq!(s.requests, 9);
        assert_eq!(s.latency.count(), 6);
        s.error(ErrorKind::Connect);
        s.missed(4);
        assert_eq!((s.requests, s.missed), (10, 4));
        let json = serde_json::to_value(&s).unwrap_or(Value::Null);
        let keys: Vec<&String> = json["statuses"].as_object().map_or(vec![], |m| m.keys().collect());
        assert_eq!(keys, ["2xx", "3xx", "4xx", "5xx"]);
        // A class outside the list is refused when read.
        let bad = json!({"requests": 1, "missed": 0, "statuses": {"6xx": 1}, "errors": {}, "latency": {}});
        assert!(serde_json::from_value::<Stats>(bad).is_err());
        let bad = json!({"requests": 1, "missed": 0, "statuses": {}, "errors": {"dns": 1}, "latency": {}});
        assert!(serde_json::from_value::<Stats>(bad).is_err());
    }

    /// Every object key and string of a JSON value.
    fn words(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    out.push(k.clone());
                    words(v, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|v| words(v, out)),
            Value::String(s) => out.push(s.clone()),
            _ => {}
        }
    }

    #[test]
    fn ldt_04_the_serialisation_is_deterministic_and_holds_names_and_numbers_only() {
        let pods = [pod(3), pod(4)];
        let sum = summary(&plan2(), &pods, 12_345);
        let a = serde_json::to_string(&sum).unwrap_or_default();
        let b = serde_json::to_string(&summary(&plan2(), &pods, 12_345)).unwrap_or_default();
        assert!(!a.is_empty());
        assert_eq!(a, b);
        let value: Value = serde_json::from_str(&a).unwrap_or(Value::Null);
        let mut seen = Vec::new();
        words(&value, &mut seen);
        let allowed = [
            "total", "endpoints", "method", "port", "template", "requests", "missed", "statuses",
            "errors", "rps", "latencyMicros", "p50", "p90", "p99", "max", "GET", "POST", "/a",
            "/b/{b_id}", "2xx", "3xx", "4xx", "5xx", "timeout", "connect", "reset", "protocol",
            "refused", "unverified",
        ];
        for w in &seen {
            assert!(allowed.contains(&w.as_str()), "unexpected {w:?}");
        }
        // Round trip.
        assert_eq!(serde_json::from_str::<crate::load::summary::Summary>(&a).ok(), Some(sum));
    }
}

fn limits() -> Limits {
    Limits {
        max_concurrency: 256,
        max_rate: 2000,
        max_duration_ms: 30 * 60_000,
    }
}

fn params() -> Params {
    Params {
        concurrency: 10,
        rate: Some(200),
        duration_ms: 300_000,
        ramp_up_ms: 30_000,
        timeout_ms: 10_000,
    }
}

fn slice(items: Value) -> k8s_openapi::api::discovery::v1::EndpointSlice {
    serde_json::from_value(json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {"name": "s", "namespace": "ns"},
        "addressType": "IPv4",
        "endpoints": items,
    }))
    .expect("a slice")
}

fn ep(pod: &str, ip: &str, ready: Option<bool>) -> Value {
    let mut e = json!({
        "addresses": [ip],
        "targetRef": {"kind": "Pod", "name": pod, "namespace": "ns"},
    });
    if let Some(r) = ready {
        e["conditions"] = json!({"ready": r});
    }
    e
}

fn target(pod: &str, ip: &str) -> Target {
    Target {
        pod: pod.into(),
        ip: ip.parse::<IpAddr>().expect("an ip"),
    }
}

mod validation_tests {
    use super::*;

    #[test]
    fn ldt_05_a_value_above_its_limit_is_invalid_with_the_field_name() {
        assert_eq!(validate(&params(), &limits()), Ok(()));
        let bad = |p: Params| validate(&p, &limits()).err();
        let field = |p: Params| bad(p).map(|e| e.field);
        assert_eq!(
            field(Params { concurrency: 257, ..params() }),
            Some("spec.concurrency")
        );
        assert_eq!(
            field(Params { rate: Some(2001), ..params() }),
            Some("spec.rate")
        );
        assert_eq!(
            field(Params { duration_ms: 30 * 60_000 + 1, ..params() }),
            Some("spec.duration")
        );
        assert_eq!(
            field(Params { ramp_up_ms: 300_001, ..params() }),
            Some("spec.rampUp")
        );
        assert_eq!(
            field(Params { timeout_ms: 60_001, ..params() }),
            Some("spec.timeout")
        );
        // The message names the field and the limit.
        let e = bad(Params { rate: Some(2001), ..params() }).expect("invalid");
        assert!(e.message.contains("spec.rate") && e.message.contains("MIMIC_LOAD_MAX_RATE"));
        // The CRD floors.
        assert_eq!(field(Params { concurrency: 0, ..params() }), Some("spec.concurrency"));
        assert_eq!(field(Params { rate: Some(0), ..params() }), Some("spec.rate"));
        assert_eq!(field(Params { duration_ms: 9_999, ramp_up_ms: 0, ..params() }), Some("spec.duration"));
        assert_eq!(field(Params { timeout_ms: 99, ..params() }), Some("spec.timeout"));
        // The edges are valid; the closed model has no rate.
        let edge = Params {
            concurrency: 256,
            rate: Some(2000),
            duration_ms: 30 * 60_000,
            ramp_up_ms: 30 * 60_000,
            timeout_ms: 100,
        };
        assert_eq!(validate(&edge, &limits()), Ok(()));
        assert_eq!(validate(&Params { rate: None, ..params() }, &limits()), Ok(()));
    }
}

mod gate_tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;
    const WINDOW: u64 = 300_000;

    fn pulls(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn two() -> Vec<Target> {
        vec![target("a-6d4c8f9b7-xxxxx", "10.0.0.1"), target("a-6d4c8f9b7-zzzzz", "10.0.0.2")]
    }

    #[test]
    fn ldt_05_the_gate_opens_only_in_mock_with_every_target_pulling_mock() {
        let fresh = format!("mock {}", NOW - 1_000);
        let ok = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh), ("a-6d4c8f9b7-zzzzz", &fresh)]);
        assert_eq!(gate(Mode::Mock, &two(), &ok, NOW, WINDOW), Gate::Open);
        // Effective mode discovery.
        assert_eq!(gate(Mode::Discovery, &two(), &ok, NOW, WINDOW), Gate::WaitingForMock);
        // No target.
        assert_eq!(gate(Mode::Mock, &[], &ok, NOW, WINDOW), Gate::WaitingForTargets);
        // A pod still in discovery.
        let d = format!("discovery {}", NOW - 1_000);
        let one_discovery = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh), ("a-6d4c8f9b7-zzzzz", &d)]);
        assert_eq!(gate(Mode::Mock, &two(), &one_discovery, NOW, WINDOW), Gate::WaitingForMock);
        // An expired field.
        let old = format!("mock {}", NOW - WINDOW - 1);
        let expired = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh), ("a-6d4c8f9b7-zzzzz", &old)]);
        assert_eq!(gate(Mode::Mock, &two(), &expired, NOW, WINDOW), Gate::WaitingForMock);
        // On the edge of the window it still counts.
        let edge = format!("mock {}", NOW - WINDOW);
        let at_edge = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh), ("a-6d4c8f9b7-zzzzz", &edge)]);
        assert_eq!(gate(Mode::Mock, &two(), &at_edge, NOW, WINDOW), Gate::Open);
        // No field at all, and a field that does not parse.
        let missing = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh)]);
        assert_eq!(gate(Mode::Mock, &two(), &missing, NOW, WINDOW), Gate::WaitingForMock);
        let junk = pulls(&[("a-6d4c8f9b7-xxxxx", &fresh), ("a-6d4c8f9b7-zzzzz", "mock")]);
        assert_eq!(gate(Mode::Mock, &two(), &junk, NOW, WINDOW), Gate::WaitingForMock);
        // A pod that is not a target does not matter.
        let extra = pulls(&[
            ("a-6d4c8f9b7-xxxxx", &fresh),
            ("a-6d4c8f9b7-zzzzz", &fresh),
            ("a-6d4c8f9b7-ggggg", &d),
        ]);
        assert_eq!(gate(Mode::Mock, &two(), &extra, NOW, WINDOW), Gate::Open);
    }

    #[test]
    fn ldt_05_targets_are_ready_pods_of_the_deployment_one_per_pod() {
        let slices = [
            slice(json!([
                ep("a-6d4c8f9b7-zzzzz", "10.0.0.2", Some(true)),
                ep("a-6d4c8f9b7-xxxxx", "10.0.0.1", None),
                // Another deployment whose name starts with `a-`.
                ep("a-b-6d4c8f9b7-xxxxx", "10.0.0.9", Some(true)),
                // Not ready.
                ep("a-6d4c8f9b7-ggggg", "10.0.0.3", Some(false)),
                // Not a pod.
                {"addresses": ["10.0.0.4"], "targetRef": {"kind": "Node", "name": "a-6d4c8f9b7-hhhhh"}},
                {"addresses": ["10.0.0.5"]},
            ])),
            // The same pod behind another Service, and a second address of a pod.
            slice(json!([
                ep("a-6d4c8f9b7-xxxxx", "10.0.0.1", Some(true)),
                {"addresses": ["10.0.0.6", "10.0.0.7"], "targetRef": {"kind": "Pod", "name": "a-6d4c8f9b7-jjjjj"}},
            ])),
        ];
        assert_eq!(
            targets(&slices, "a"),
            [
                target("a-6d4c8f9b7-jjjjj", "10.0.0.6"),
                target("a-6d4c8f9b7-xxxxx", "10.0.0.1"),
                target("a-6d4c8f9b7-zzzzz", "10.0.0.2"),
            ]
        );
        assert_eq!(targets(&slices, "a-b"), [target("a-b-6d4c8f9b7-xxxxx", "10.0.0.9")]);
        assert!(targets(&[], "a").is_empty());
    }
}

const UID: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

fn resources() -> Value {
    json!({"requests": {"cpu": "500m", "memory": "128Mi"}, "limits": {"cpu": "2", "memory": "256Mi"}})
}

fn input<'a>(resources: &'a Value, secret: Option<&'a str>) -> JobInput<'a> {
    JobInput {
        namespace: "shop",
        name: "load-1",
        uid: UID,
        deployment: "payment",
        concurrency: 300,
        duration_ms: 300_000,
        pod_concurrency: 128,
        start_timeout_ms: 300_000,
        report_interval_ms: 5_000,
        max_plan: 8 << 20,
        image: "ghcr.io/thaliph/mimic@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        resources,
        controller_url: "https://mimic-controller.mimic-system.svc:8444",
        controller_ca: "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        secret_name: secret,
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The checks of the Job `ValidatingAdmissionPolicy` of §11, in Rust (the policy itself is
/// installed by the chart, LDT-17).
fn job_admitted(job: &Value, image: &str) -> Result<(), String> {
    let pod = &job["spec"]["template"]["spec"];
    let label = |v: &Value, k: &str| v["metadata"]["labels"][k].as_str().map(str::to_string);
    for l in [
        "app.kubernetes.io/managed-by",
        "mimic.platform.io/load-test",
    ] {
        label(job, l).ok_or(format!("job label {l}"))?;
        label(&job["spec"]["template"], l).ok_or(format!("pod label {l}"))?;
    }
    if label(&job["spec"]["template"], "mimic.platform.io/inject").as_deref() != Some("disabled") {
        return Err("inject label".into());
    }
    let containers = pod["containers"].as_array().ok_or("containers")?;
    if containers.len() != 1 {
        return Err("one container".into());
    }
    for forbidden in ["initContainers", "ephemeralContainers", "nodeName", "tolerations", "hostAliases", "hostNetwork", "hostPID", "hostIPC", "hostUsers", "shareProcessNamespace"] {
        if !pod[forbidden].is_null() && pod[forbidden] != json!(false) {
            return Err(format!("forbidden {forbidden}"));
        }
    }
    let c = &containers[0];
    if c["image"] != image || c["command"] != json!(["/usr/local/bin/mimic-load"]) || !c["args"].is_null() || !c["envFrom"].is_null() {
        return Err("image, command or args".into());
    }
    let allowed = [
        "MIMIC_LOAD_RUN", "MIMIC_LOAD_DEPLOYMENT", "MIMIC_CONTROLLER_URL", "MIMIC_CONTROLLER_CA",
        "MIMIC_LOAD_TOKEN_FILE", "MIMIC_LOAD_CREDENTIALS_DIR", "MIMIC_LOAD_REPORT_INTERVAL",
        "MIMIC_LOAD_MAX_PLAN",
    ];
    for e in c["env"].as_array().ok_or("env")? {
        let name = e["name"].as_str().unwrap_or_default();
        if !allowed.contains(&name) || !e["value"].is_string() || !e["valueFrom"].is_null() {
            return Err(format!("env {name}"));
        }
    }
    if pod["automountServiceAccountToken"] != json!(false) || pod["serviceAccountName"] != json!("default") {
        return Err("service account".into());
    }
    let sc = &c["securityContext"];
    if sc["runAsNonRoot"] != json!(true)
        || sc["runAsUser"] != json!(65532)
        || sc["allowPrivilegeEscalation"] != json!(false)
        || sc["readOnlyRootFilesystem"] != json!(true)
        || sc["capabilities"] != json!({"drop": ["ALL"]})
        || sc["privileged"] == json!(true)
        || pod["securityContext"]["seccompProfile"]["type"] != json!("RuntimeDefault")
    {
        return Err("security context".into());
    }
    let mut secrets = 0;
    for v in pod["volumes"].as_array().ok_or("volumes")? {
        if let Some(p) = v["projected"].as_object() {
            let sources = p["sources"].as_array().ok_or("sources")?;
            let [src] = sources.as_slice() else { return Err("one source".into()) };
            let t = &src["serviceAccountToken"];
            if t["audience"] != json!("mimic.platform.io/load") || t["expirationSeconds"].as_u64().is_none_or(|s| s > 600) {
                return Err("token".into());
            }
        } else if v["secret"].is_object() {
            secrets += 1;
            let m = c["volumeMounts"].as_array().ok_or("mounts")?.iter().find(|m| m["name"] == v["name"]);
            if m.is_none_or(|m| m["mountPath"] != json!("/mimic/credentials") || m["readOnly"] != json!(true)) {
                return Err("secret mount".into());
            }
        } else {
            return Err("volume kind".into());
        }
    }
    if secrets > 1 {
        return Err("secret volumes".into());
    }
    Ok(())
}

fn env_of(job: &Value) -> BTreeMap<String, String> {
    job["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| (e["name"].as_str().unwrap_or_default().to_string(), e["value"].as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

mod object_tests {
    use super::*;

    #[test]
    fn ldt_05_the_job_for_concurrency_300_has_the_shape_of_the_spec() {
        let r = resources();
        let j = job(&input(&r, None));
        assert_eq!(j["apiVersion"], "batch/v1");
        assert_eq!(j["kind"], "Job");
        assert_eq!(job_name(UID), "mimic-load-7c9e66797425");
        assert_eq!(j["metadata"]["name"], "mimic-load-7c9e66797425");
        assert_eq!(j["metadata"]["namespace"], "shop");
        let s = &j["spec"];
        assert_eq!(s["completionMode"], "Indexed");
        assert_eq!(s["parallelism"], 3);
        assert_eq!(s["completions"], 3);
        assert_eq!(s["backoffLimit"], 0);
        // duration + MIMIC_LOAD_START_TIMEOUT + 60 s.
        assert_eq!(s["activeDeadlineSeconds"], 300 + 300 + 60);
        assert_eq!(s["ttlSecondsAfterFinished"], 3600);
        let owner = &j["metadata"]["ownerReferences"][0];
        assert_eq!(owner["apiVersion"], "mimic.platform.io/v1alpha1");
        assert_eq!(owner["kind"], "MimicLoadTest");
        assert_eq!(owner["name"], "load-1");
        assert_eq!(owner["uid"], UID);
        assert_eq!(owner["blockOwnerDeletion"], false);
        assert_eq!(j["metadata"]["labels"]["mimic.platform.io/load-test"], "load-1");
        assert_eq!(j["metadata"]["labels"]["app.kubernetes.io/managed-by"], "mimic");
        let t = &s["template"];
        assert_eq!(t["metadata"]["labels"]["mimic.platform.io/inject"], "disabled");
        assert_eq!(t["metadata"]["labels"]["mimic.platform.io/load-test"], "load-1");
        assert_eq!(t["spec"]["restartPolicy"], "Never");
        assert_eq!(t["spec"]["automountServiceAccountToken"], false);
        let c = &t["spec"]["containers"][0];
        assert_eq!(c["name"], "mimic-load");
        assert_eq!(c["resources"], r);
        assert_eq!(c["securityContext"]["runAsUser"], 65532);
        // The projected token, and no secret volume without credentials.
        let volumes = t["spec"]["volumes"].as_array().expect("volumes");
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0]["projected"]["sources"][0]["serviceAccountToken"]["audience"], "mimic.platform.io/load");
        assert_eq!(volumes[0]["projected"]["sources"][0]["serviceAccountToken"]["expirationSeconds"], 600);
        // Literal env of §10.6 names.
        let env = env_of(&j);
        assert_eq!(env["MIMIC_LOAD_RUN"], format!("shop/load-1/{UID}"));
        assert_eq!(env["MIMIC_LOAD_DEPLOYMENT"], "payment");
        assert_eq!(env["MIMIC_CONTROLLER_URL"], "https://mimic-controller.mimic-system.svc:8444");
        assert!(env["MIMIC_CONTROLLER_CA"].starts_with("-----BEGIN CERTIFICATE-----"));
        assert_eq!(env["MIMIC_LOAD_TOKEN_FILE"], "/var/run/secrets/mimic.platform.io/load-token");
        assert_eq!(env["MIMIC_LOAD_REPORT_INTERVAL"], "5s");
        assert_eq!(env["MIMIC_LOAD_MAX_PLAN"], (8 << 20).to_string());
        assert!(!env.contains_key("MIMIC_LOAD_CREDENTIALS_DIR"));
        assert_eq!(job_admitted(&j, input(&r, None).image), Ok(()));
        // Same input, same object.
        assert_eq!(j, job(&input(&r, None)));
    }

    #[test]
    fn ldt_05_the_secret_volume_exists_only_with_credentials() {
        let r = resources();
        let j = job(&input(&r, Some("payment-load")));
        let pod = &j["spec"]["template"]["spec"];
        let secret = pod["volumes"]
            .as_array()
            .expect("volumes")
            .iter()
            .find(|v| v["secret"].is_object())
            .expect("a secret volume");
        assert_eq!(secret["secret"]["secretName"], "payment-load");
        assert_eq!(secret["secret"]["optional"], false);
        assert_eq!(secret["secret"]["defaultMode"], 0o400);
        let mount = pod["containers"][0]["volumeMounts"]
            .as_array()
            .expect("mounts")
            .iter()
            .find(|m| m["name"] == secret["name"])
            .expect("a mount");
        assert_eq!(mount["mountPath"], "/mimic/credentials");
        assert_eq!(mount["readOnly"], true);
        assert_eq!(env_of(&j)["MIMIC_LOAD_CREDENTIALS_DIR"], "/mimic/credentials");
        assert_eq!(job_admitted(&j, input(&r, None).image), Ok(()));
    }

    #[test]
    fn ldt_05_the_pod_count_follows_the_concurrency_and_the_deadline_the_durations() {
        let r = resources();
        let mut i = input(&r, None);
        i.concurrency = 128;
        i.duration_ms = 10_500;
        i.start_timeout_ms = 5_000;
        let j = job(&i);
        assert_eq!(j["spec"]["parallelism"], 1);
        assert_eq!(j["spec"]["completions"], 1);
        // 10.5 s rounds up.
        assert_eq!(j["spec"]["activeDeadlineSeconds"], 11 + 5 + 60);
        i.report_interval_ms = 1_500;
        assert_eq!(env_of(&job(&i))["MIMIC_LOAD_REPORT_INTERVAL"], "1500ms");
    }

    #[test]
    fn ldt_05_the_admission_policy_refuses_what_the_spec_forbids() {
        let r = resources();
        let image = input(&r, None).image;
        let ok = job(&input(&r, Some("s")));
        let with = |path: &str, v: Value| {
            let mut j = ok.clone();
            if let Some(slot) = j.pointer_mut(path) {
                *slot = v;
            }
            j
        };
        let p = "/spec/template/spec";
        assert!(job_admitted(&with("/spec/template/spec/containers/0/image", json!("x:latest")), image).is_err());
        assert!(job_admitted(&with(&format!("{p}/automountServiceAccountToken"), json!(true)), image).is_err());
        assert!(job_admitted(&with(&format!("{p}/containers/0/args"), json!(["--x"])), image).is_err());
        assert!(job_admitted(&with(&format!("{p}/volumes/0/projected/sources/0/serviceAccountToken/audience"), json!("")), image).is_err());
        assert!(job_admitted(&with(&format!("{p}/volumes/0/projected/sources/0/serviceAccountToken/expirationSeconds"), json!(3600)), image).is_err());
    }

    fn policy<'a>(
        ports: &'a [u16],
        controller: &'a BTreeMap<String, String>,
        dns: &'a BTreeMap<String, String>,
        metrics: &'a [Value],
    ) -> PolicyInput<'a> {
        PolicyInput {
            namespace: "shop",
            name: "load-1",
            uid: UID,
            deployment: "payment",
            ports,
            controller_namespace: "mimic-system",
            controller_labels: controller,
            controller_port: 8444,
            dns_namespace: "kube-system",
            dns_labels: dns,
            metrics_from: metrics,
        }
    }

    #[test]
    fn ldt_05_the_network_policy_reaches_the_deployment_the_controller_and_dns_only() {
        let controller = labels(&[("app.kubernetes.io/name", "mimic-controller")]);
        let dns = labels(&[("k8s-app", "kube-dns")]);
        let n = network_policy(&policy(&[8080, 9090, 8080], &controller, &dns, &[]));
        assert_eq!(n["apiVersion"], "networking.k8s.io/v1");
        assert_eq!(n["kind"], "NetworkPolicy");
        assert_eq!(n["metadata"]["namespace"], "shop");
        assert_eq!(n["metadata"]["name"], "mimic-load-7c9e66797425");
        assert_eq!(n["metadata"]["ownerReferences"][0]["uid"], UID);
        assert_eq!(n["metadata"]["ownerReferences"][0]["blockOwnerDeletion"], false);
        assert_eq!(n["metadata"]["labels"]["mimic.platform.io/load-test"], "load-1");
        let spec = &n["spec"];
        assert_eq!(spec["podSelector"]["matchLabels"], json!({"mimic.platform.io/load-test": "load-1"}));
        assert_eq!(spec["policyTypes"], json!(["Ingress", "Egress"]));
        // No ingress rule without a metrics source.
        assert_eq!(spec["ingress"].as_array().map_or(0, Vec::len), 0);
        let egress = spec["egress"].as_array().expect("egress");
        assert_eq!(egress.len(), 3);
        // The deployment's pods on the plan's ports (once each) and 15100.
        assert_eq!(egress[0]["to"], json!([{"podSelector": {"matchLabels": {"mimic.platform.io/deployment": "payment"}}}]));
        let ports: Vec<u64> = egress[0]["ports"].as_array().expect("ports").iter().map(|p| p["port"].as_u64().unwrap_or(0)).collect();
        assert_eq!(ports, [8080, 9090, 15100]);
        assert!(egress[0]["ports"].as_array().expect("ports").iter().all(|p| p["protocol"] == "TCP"));
        // The controller on 8444.
        assert_eq!(
            egress[1],
            json!({
                "to": [{
                    "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "mimic-system"}},
                    "podSelector": {"matchLabels": {"app.kubernetes.io/name": "mimic-controller"}},
                }],
                "ports": [{"protocol": "TCP", "port": 8444}],
            })
        );
        // DNS on 53, UDP and TCP.
        assert_eq!(
            egress[2],
            json!({
                "to": [{
                    "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}},
                    "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}},
                }],
                "ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}],
            })
        );
    }

    #[test]
    fn ldt_05_the_metrics_source_may_reach_the_admin_port_only() {
        let controller = labels(&[("app", "c")]);
        let dns = labels(&[("k8s-app", "kube-dns")]);
        let from = [json!({"namespaceSelector": {"matchLabels": {"name": "monitoring"}}})];
        let n = network_policy(&policy(&[8080], &controller, &dns, &from));
        assert_eq!(
            n["spec"]["ingress"],
            json!([{"from": from, "ports": [{"protocol": "TCP", "port": 15100}]}])
        );
        // Same input, same object.
        assert_eq!(n, network_policy(&policy(&[8080], &controller, &dns, &from)));
    }
}

//! ADM-53 to ADM-55: the admission signals of the inbound sidecar (spec §4.1 "Signals", §9
//! `mimic_admission_*`) and the INFO `admission contract changed` (§7.9 "Admission").

mod common;

use std::time::{Duration, Instant};

use common::admission::{
    App, call, contract, from_env, hold_slot, inbound, no_contract, persist, rig, spawn_call,
};
use common::faults::absent;
use common::mesh_pods::meshed;
use common::{capture_logs, h2_client, metric, until};
use mimic_sidecar::mesh::MeshState;

const CHANGED: &str = "admission contract changed";

/// The `# HELP`, `# TYPE` and `# UNIT` lines of §9, in their order.
const DESCRIPTORS: [&str; 13] = [
    "# HELP mimic_admission_requests Requests offered to the admission contract, by result",
    "# TYPE mimic_admission_requests counter",
    "# HELP mimic_admission_in_flight_seconds Integral of the admitted requests in flight over time, in seconds",
    "# TYPE mimic_admission_in_flight_seconds counter",
    "# UNIT mimic_admission_in_flight_seconds seconds",
    "# HELP mimic_admission_in_flight Admitted requests in flight",
    "# TYPE mimic_admission_in_flight gauge",
    "# HELP mimic_admission_queue_depth Requests waiting in the admission queue",
    "# TYPE mimic_admission_queue_depth gauge",
    "# HELP mimic_admission_limit Admission contract in force, by kind",
    "# TYPE mimic_admission_limit gauge",
    "# HELP mimic_admission_utilization Share of the admission contract used, smoothed over 15 s, refused demand included",
    "# TYPE mimic_admission_utilization gauge",
];

/// The ticks of the state run (its 100 ms timer) until the test ends.
fn ticks(state: &MeshState) {
    tokio::spawn(state.clone().run_age(std::future::pending()));
}

fn requests(scrape: &str, result: &str) -> f64 {
    metric(
        scrape,
        "mimic_admission_requests_total{",
        &[&format!("result=\"{result}\"")],
    )
}

fn utilization(scrape: &str) -> f64 {
    metric(scrape, "mimic_admission_utilization ", &[])
}

/// The lines of the four gauges in a scrape.
fn gauge_lines(scrape: &str) -> Vec<&str> {
    scrape
        .lines()
        .filter(|l| {
            [
                "mimic_admission_in_flight ",
                "mimic_admission_queue_depth",
                "mimic_admission_limit",
                "mimic_admission_utilization",
            ]
            .iter()
            .any(|g| l.starts_with(g))
        })
        .collect()
}

#[tokio::test]
async fn adm_53_counts_and_gauges_after_admissions_and_refusals() {
    let _logs = capture_logs();
    let (app, state, f) = rig(contract(2, 1000), 300).await;
    ticks(&state);
    let mut client = h2_client(f.addr).await;
    let r1 = spawn_call(&client, "/");
    let r2 = spawn_call(&client, "/");
    until("two requests at the app", || app.counts.in_flight() == 2).await;
    let r3 = spawn_call(&client, "/");
    until("R3 is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    state.admission_tick();
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_admission_in_flight ", &[]),
        2.0,
        "{scrape}"
    );
    assert_eq!(metric(&scrape, "mimic_admission_queue_depth ", &[]), 1.0);
    let limit = |kind: &str| {
        metric(
            &scrape,
            "mimic_admission_limit{",
            &[&format!("kind=\"{kind}\"")],
        )
    };
    assert_eq!((limit("concurrency"), limit("rps")), (2.0, 1000.0));
    let r4 = call(&mut client, "/", &[]).await;
    assert_eq!(r4.error(), Some("overloaded"), "{r4:?}");
    let r3 = r3.await.unwrap();
    assert_eq!(r3.error(), Some("queue-timeout"), "{r3:?}");
    // R5 is queued while the slots are still held, and reset by its client.
    let r5 = spawn_call(&client, "/");
    until("R5 is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    r5.abort();
    until("R5 is cancelled", || {
        state.admission_counts_for_test().cancelled == 1
    })
    .await;
    assert_eq!(r1.await.unwrap().status, 200);
    assert_eq!(r2.await.unwrap().status, 200);
    let r6 = call(&mut client, "/", &[]).await;
    assert_eq!(r6.status, 200, "{r6:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let scrape = f.metrics.encode();
    for (result, n) in [
        ("admitted", 3.0),
        ("queued", 2.0),
        ("overloaded", 1.0),
        ("queue-timeout", 1.0),
        ("cancelled", 1.0),
    ] {
        assert_eq!(requests(&scrape, result), n, "{result}: {scrape}");
    }
    let seconds = metric(&scrape, "mimic_admission_in_flight_seconds_total", &[]);
    assert!((0.75..=1.05).contains(&seconds), "{seconds}");
    let descriptors: Vec<&str> = scrape
        .lines()
        .filter(|l| l.starts_with('#') && l.contains(" mimic_admission_"))
        .collect();
    assert_eq!(descriptors, DESCRIPTORS);
    for sample in scrape.lines().filter(|l| l.starts_with("mimic_admission_")) {
        let series = sample.rsplit_once(' ').map_or(sample, |(s, _)| s);
        if let Some((_, labels)) = series.split_once('{') {
            let labels = labels.trim_end_matches('}');
            let one = !labels.contains(',');
            assert!(
                one && (labels.starts_with("result=\"") || labels.starts_with("kind=\"")),
                "{sample}"
            );
        }
    }
}

#[tokio::test]
async fn adm_53_no_family_before_a_contract_and_the_gauges_leave_with_it() {
    let _logs = capture_logs();
    let app = App::h2c(0).await;
    let state = meshed(Some("cart-7d9f-x2k"), |_| {});
    state.hold(no_contract(), "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    ticks(&state);
    let mut client = h2_client(f.addr).await;
    for _ in 0..3 {
        assert_eq!(call(&mut client, "/", &[]).await.status, 200);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let scrape = f.metrics.encode();
    assert!(!scrape.contains("mimic_admission_"), "{scrape}");

    state.hold(contract(2, 1000), "\"2-a\"");
    let scrape = f.metrics.encode();
    for result in [
        "admitted",
        "queued",
        "overloaded",
        "queue-timeout",
        "cancelled",
    ] {
        let series = format!("mimic_admission_requests_total{{result=\"{result}\"}} 0");
        assert!(scrape.lines().any(|l| l == series), "{series}: {scrape}");
    }
    assert_eq!(gauge_lines(&scrape).len(), 5, "{scrape}");
    assert_eq!(call(&mut client, "/", &[]).await.status, 200);

    state.hold(no_contract(), "\"3-a\"");
    let scrape = f.metrics.encode();
    assert!(gauge_lines(&scrape).is_empty(), "{scrape}");
    assert_eq!(requests(&scrape, "admitted"), 1.0, "{scrape}");
    for _ in 0..3 {
        assert_eq!(call(&mut client, "/", &[]).await.status, 200);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let scrape = f.metrics.encode();
    assert_eq!(requests(&scrape, "admitted"), 1.0, "{scrape}");
    assert!(gauge_lines(&scrape).is_empty(), "{scrape}");
}

#[tokio::test]
async fn adm_54_utilization_rises_over_15_s_and_decays_after_the_load() {
    let _logs = capture_logs();
    let (_app, state, f) = rig(contract(1000, 20), 0).await;
    ticks(&state);
    let client = h2_client(f.addr).await;
    let began = Instant::now();
    let load = tokio::spawn({
        let client = client.clone();
        async move {
            let mut every = tokio::time::interval(Duration::from_millis(25));
            while began.elapsed() < Duration::from_secs(20) {
                every.tick().await;
                drop(spawn_call(&client, "/"));
            }
        }
    });
    let mut scraped = Vec::new();
    for s in 10..=20u64 {
        tokio::time::sleep_until((began + Duration::from_secs(s)).into()).await;
        scraped.push(utilization(&f.metrics.encode()));
    }
    load.await.unwrap();
    for pair in scraped.windows(2) {
        let (u, next) = (pair[0], pair[1]);
        assert!(next >= u - 0.02, "{scraped:?}");
        assert!((next - u).abs() <= 0.1, "{scraped:?}");
    }
    let at_stop = scraped[scraped.len() - 1];
    assert!(at_stop > 1.0 && at_stop <= 1.6, "{scraped:?}");
    tokio::time::sleep_until((began + Duration::from_secs(25)).into()).await;
    let after = utilization(&f.metrics.encode());
    assert!(
        (0.6 * at_stop..=0.85 * at_stop).contains(&after),
        "{after} after {at_stop}"
    );
}

#[tokio::test]
async fn adm_54_the_in_flight_term_is_capped_by_the_concurrency() {
    let _logs = capture_logs();
    let (_app, state, f) = rig(contract(4, 100_000), 200).await;
    ticks(&state);
    let client = h2_client(f.addr).await;
    let began = Instant::now();
    let clients: Vec<_> = (0..8)
        .map(|_| {
            let mut sender = client.clone();
            tokio::spawn(async move {
                while began.elapsed() < Duration::from_secs(5) {
                    call(&mut sender, "/", &[]).await;
                }
            })
        })
        .collect();
    let mut scraped = Vec::new();
    for half in 1..=10u32 {
        tokio::time::sleep_until((began + Duration::from_millis(500) * half).into()).await;
        scraped.push(utilization(&f.metrics.encode()));
    }
    for c in clients {
        c.await.unwrap();
    }
    assert!(scraped.iter().all(|u| *u <= 1.0), "{scraped:?}");
    assert!(scraped[scraped.len() - 1] > 0.2, "{scraped:?}");
}

#[tokio::test]
async fn adm_55_set_changed_kept_and_removed_contracts_write_one_line_each() {
    let logs = capture_logs();
    let state = meshed(Some("cart-7d9f-x2k"), |_| {});
    for (view, etag) in [
        (no_contract(), "\"1-a\""),
        (contract(2, 100), "\"2-a\""),
        (contract(2, 100), "\"3-a\""),
        (contract(4, 100), "\"4-a\""),
        (no_contract(), "\"5-a\""),
    ] {
        state.hold(view, etag);
    }
    let changed = logs.messages(CHANGED).await;
    assert_eq!(changed.len(), 3, "{changed:?}");
    for entry in &changed {
        assert_eq!(
            (&entry["level"], &entry["target"]),
            (&"INFO".into(), &"mesh".into()),
            "{entry}"
        );
    }
    let fields = |entry: &serde_json::Value| {
        ["concurrency", "rps", "previous_concurrency", "previous_rps"]
            .map(|k| entry.get(k).and_then(serde_json::Value::as_u64))
    };
    assert_eq!(fields(&changed[0]), [Some(2), Some(100), None, None]);
    assert_eq!(
        fields(&changed[1]),
        [Some(4), Some(100), Some(2), Some(100)]
    );
    assert_eq!(fields(&changed[2]), [None, None, Some(4), Some(100)]);
    assert!(absent(&changed[0], "previous_concurrency"));
    assert!(absent(&changed[2], "concurrency"));
    let object = changed[0].as_object().unwrap();
    assert!(!object.contains_key("previous_concurrency") && !object.contains_key("previous_rps"));
    let object = changed[2].as_object().unwrap();
    assert!(!object.contains_key("concurrency") && !object.contains_key("rps"));
    let etags: Vec<&str> = changed.iter().filter_map(|e| e["etag"].as_str()).collect();
    assert_eq!(etags, ["\"2-a\"", "\"4-a\"", "\"5-a\""]);
}

#[tokio::test]
async fn adm_55_a_restart_with_a_persisted_contract_writes_one_line() {
    let logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    persist(dir.path(), &contract(2, 100));
    let state = from_env(dir.path(), &[]);
    state.restore().await;
    let changed = logs.messages(CHANGED).await;
    assert_eq!(changed.len(), 1, "{changed:?}");
    let entry = &changed[0];
    assert_eq!(
        (&entry["concurrency"], &entry["rps"]),
        (&2.into(), &100.into()),
        "{entry}"
    );
    assert_eq!(entry["etag"], state.etag().unwrap(), "{entry}");
}

#[tokio::test]
async fn adm_55_a_thousand_refusals_write_no_component_line() {
    let logs = capture_logs();
    let (app, state, f) = rig(contract(1, 1000), 5000).await;
    ticks(&state);
    let held_by = h2_client(f.addr).await;
    let _held = hold_slot(&app, &held_by).await;
    let unknown = logs.messages("admission contract unknown").await.len();
    logs.settled().await;
    let before = logs.text().len();
    let mut calls = Vec::new();
    for _ in 0..5 {
        let client = h2_client(f.addr).await;
        calls.extend((0..200).map(|_| spawn_call(&client, "/")));
    }
    for c in calls {
        assert_eq!(c.await.unwrap().status, 503);
    }
    logs.settled().await;
    let text = logs.text();
    let new: Vec<serde_json::Value> = text[before..]
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    let others: Vec<&serde_json::Value> = new
        .iter()
        .filter(|v| v["target"] != "access" && v["target"] != "sync")
        .collect();
    assert!(others.is_empty(), "{others:?}");
    let access = new.iter().filter(|v| v["target"] == "access").count();
    assert_eq!(access, 1000);
    assert_eq!(
        logs.messages("admission contract unknown").await.len(),
        unknown
    );
    assert_eq!(app.counts.requests(), 1, "only the held one");
}

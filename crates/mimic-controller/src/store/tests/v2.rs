//! F8 step 5: version 2 pushes and the `:outbound` key (SHR-09, SHR-13, SHR-14).

use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;

fn outbound_exchange(report: &mut Report, authority: &str, path: &str, times: usize) {
    for _ in 0..times {
        report.record_at(
            ScopeKey::Authority(authority.to_string()),
            &Via {
                tls: true,
                sni: Some("api.example.com"),
                dst: None,
            },
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Outbound,
                port: 443,
                method: "GET",
                path,
                status: 200,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
}

/// A push with `inbound` paths and `outbound` `(authority, path, times)` exchanges.
pub(super) fn push_v2(
    pod: &str,
    seq: u64,
    inbound: &[&str],
    outbound: &[(&str, &str, usize)],
) -> Push {
    let mut report = Report::new(Some(pod.to_string()));
    for path in inbound {
        exchange(&mut report, path, 200);
    }
    for (authority, path, times) in outbound {
        outbound_exchange(&mut report, authority, path, *times);
    }
    report.stamp(today());
    Push {
        pod: pod.to_string(),
        seq: Some(SeqId::parse(SESSION, seq).expect("valid seq")),
        report,
    }
}

#[test]
fn shr_09_the_outbound_key_name_is_byte_exact() {
    let k = keys("shop", "pay.v2").expect("valid names");
    assert_eq!(k.outbound, "mimic:v1:{shop/pay.v2}:outbound");
}

#[tokio::test]
async fn shr_09_a_version_2_push_is_split_into_report_and_outbound() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("shr09s"), "pay");
    let plain = unique("shr09p");
    let out = [("api.example.com:443", "/v1/x", 3)];
    let got = store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a", "/b"], &out))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    // The same inbound pushes without the outbound part: what F7 stores.
    let got = store
        .push(&plain, dep, push_v2("pod-a", 1, &["/a", "/b"], &[]))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));

    let k = keys(&ns, dep).expect("keys");
    let f7 = keys(&plain, dep).expect("keys");
    let mut c = conn(None).await;
    let report: String = cmd(&mut c, &["GET", &k.report]).await;
    let f7_report: String = cmd(&mut c, &["GET", &f7.report]).await;
    // Exchanges differ by the outbound ones only: compare the parts.
    let merged = push_v2("pod-a", 1, &["/a", "/b"], &out).report;
    let (inbound_part, outbound_part) = merge_bounded(Report::new(None), merged, &LIMITS)
        .report
        .split();
    assert!(report.starts_with("{\"version\":1,"));
    assert_eq!(report, inbound_part.to_json().unwrap());
    assert_eq!(report, f7_report);
    let outbound: String = cmd(&mut c, &["GET", &k.outbound]).await;
    assert!(outbound.starts_with("{\"version\":2,"), "{outbound}");
    assert_eq!(outbound, outbound_part.unwrap().to_json().unwrap());
    // No `:outbound` key without outbound data: the F7 key set.
    let exists: i64 = cmd(&mut c, &["EXISTS", &f7.outbound]).await;
    assert_eq!(exists, 0);

    let inbound_n: String = cmd(&mut c, &["HGET", &k.meta, "endpoints_inbound"]).await;
    let outbound_n: String = cmd(&mut c, &["HGET", &k.meta, "endpoints_outbound"]).await;
    assert_eq!((inbound_n.as_str(), outbound_n.as_str()), ("2", "1"));
    let exchanges: String = cmd(&mut c, &["HGET", &k.meta, "exchanges"]).await;
    assert_eq!(exchanges, "5");
    let bytes: String = cmd(&mut c, &["HGET", &k.meta, "report_bytes"]).await;
    assert_eq!(bytes, (report.len() + outbound.len()).to_string());
    // The key expires like the others and is a string.
    let thirty_days = i64::try_from(30 * DAY * 1000).expect("fits");
    let ttl: i64 = cmd(&mut c, &["PTTL", &k.outbound]).await;
    assert!(ttl > thirty_days - 60_000 && ttl <= thirty_days, "{ttl}");
    let ty: String = cmd(&mut c, &["TYPE", &k.outbound]).await;
    assert_eq!(ty, "string");
    let ttl: i64 = cmd(&mut c, &["PTTL", &k.report]).await;
    assert!(ttl > thirty_days - 60_000 && ttl <= thirty_days, "{ttl}");
}

#[tokio::test]
async fn shr_09_a_duplicate_leaves_both_keys_unchanged_and_versions_merge() {
    let (store, _) = replica(None, config(Duration::from_secs(3600)));
    let (ns, dep) = (unique("shr09d"), "pay");
    let first = || push_v2("pod-a", 1, &["/a"], &[("a.example.com:443", "/x", 2)]);
    assert_eq!(store.push(&ns, dep, first()).await, Ok(Outcome::Applied));
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let before: (String, String) = (
        cmd(&mut c, &["GET", &k.report]).await,
        cmd(&mut c, &["GET", &k.outbound]).await,
    );
    assert_eq!(store.push(&ns, dep, first()).await, Ok(Outcome::Duplicate));
    let after: (String, String) = (
        cmd(&mut c, &["GET", &k.report]).await,
        cmd(&mut c, &["GET", &k.outbound]).await,
    );
    assert_eq!(before, after);
    // A version 1 push (an F7 sidecar) next to a version 2 one: the outbound part stays.
    let got = store
        .push(&ns, dep, push_of("pod-b", Some(1), &[("/b", 200)]))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let outbound: String = cmd(&mut c, &["GET", &k.outbound]).await;
    assert_eq!(outbound, before.1);
    // The reader sees the union.
    let union = store.report(&ns, dep).await.expect("report");
    let union = Report::from_json(std::str::from_utf8(&union).unwrap()).expect("report");
    assert_eq!(union.version(), 2);
    assert_eq!(union.exchanges(), 4);
    assert_eq!(union.outbound_exchanges(), 2);
}

#[tokio::test]
async fn shr_13_the_reader_returns_the_stored_bytes_without_outbound_and_the_join_with_it() {
    let (store, _) = replica(None, config(Duration::from_secs(3600)));
    let (ns, dep) = (unique("shr13j"), "pay");
    let got = store
        .push(&ns, dep, push_of("p", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let stored: String = cmd(&mut c, &["GET", &k.report]).await;
    assert_eq!(store.report(&ns, dep).await.unwrap(), stored.into_bytes());
    let out = [("a.example.com:443", "/x", 1)];
    let got = store.push(&ns, dep, push_v2("p", 2, &[], &out)).await;
    assert_eq!(got, Ok(Outcome::Applied));
    let inbound: String = cmd(&mut c, &["GET", &k.report]).await;
    let outbound: String = cmd(&mut c, &["GET", &k.outbound]).await;
    let join = Report::join(
        Report::from_json(&inbound).unwrap(),
        Some(Report::from_json(&outbound).unwrap()),
    );
    assert_eq!(
        store.report(&ns, dep).await.unwrap(),
        join.to_json().unwrap().into_bytes()
    );
}

#[tokio::test]
async fn shr_09_authority_limits_count_and_warn_once_per_deployment() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let mut cfg = config(Duration::from_secs(3600));
    cfg.limits.max_authorities = 2;
    let (store, metrics) = replica(None, cfg);
    let (ns, dep) = (unique("shr09a"), "pay");
    let two = [
        ("a.example.com:443", "/x", 5),
        ("b.example.com:443", "/x", 5),
    ];
    let got = store.push(&ns, dep, push_v2("p", 1, &[], &two)).await;
    assert_eq!(got, Ok(Outcome::Applied));
    // A new authority with less traffic is refused.
    let c_push = [("c.example.com:443", "/x", 1)];
    let got = store.push(&ns, dep, push_v2("p", 2, &[], &c_push)).await;
    assert_eq!(got, Ok(Outcome::Applied));
    let selector = "kind=\"authority\"";
    assert_eq!(counter(&metrics, "store_refused_total", selector), 1.0);
    // One with more traffic evicts the least stored one (ties: the larger key).
    let d_push = [("d.example.com:443", "/x", 9)];
    let got = store.push(&ns, dep, push_v2("p", 3, &[], &d_push)).await;
    assert_eq!(got, Ok(Outcome::Applied));
    assert_eq!(counter(&metrics, "store_evicted_total", selector), 1.0);
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let outbound: String = cmd(&mut c, &["GET", &k.outbound]).await;
    assert!(outbound.contains("a.example.com:443") && outbound.contains("d.example.com:443"));
    assert!(!outbound.contains("b.example.com:443"));
    assert_eq!(log_lines(&capture, "store limit reached"), 1);
    assert_eq!(log_lines(&capture, "store evicted"), 1);
}

#[tokio::test]
async fn shr_09_an_unreadable_outbound_value_is_never_overwritten_and_names_its_key() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let (store, _) = replica(None, config(Duration::from_secs(3600)));
    let (ns, dep) = (unique("shr09u"), "pay");
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    // Not JSON, then an `:outbound` that holds an inbound scope.
    let mut with_inbound = push_of("p", None, &[("/a", 200)]).report;
    with_inbound.stamp(today());
    for bad in ["{not json".to_string(), with_inbound.to_json().unwrap()] {
        let _: () = cmd(&mut c, &["SET", &k.outbound, &bad]).await;
        let out = [("a.example.com:443", "/x", 1)];
        let got = store.push(&ns, dep, push_v2("p", 1, &["/a"], &out)).await;
        assert_eq!(got, Err(StoreError::Unavailable));
        let stored: String = cmd(&mut c, &["GET", &k.outbound]).await;
        assert_eq!(stored, bad);
        let exists: i64 = cmd(&mut c, &["EXISTS", &k.report]).await;
        assert_eq!(exists, 0);
        // The reader refuses it too.
        assert_eq!(
            store.report(&ns, dep).await,
            Err(ReadError::NotFound),
            "no :report: nothing to read"
        );
    }
    let lines: Vec<serde_json::Value> = String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == "stored report unreadable")
        .collect();
    assert!(lines.len() >= 2, "{lines:?}");
    assert!(lines.iter().all(|l| l["key"] == "outbound"), "{lines:?}");
}

#[tokio::test]
async fn shr_09_a_report_key_that_is_not_version_1_is_never_overwritten() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let (store, _) = replica(None, config(Duration::from_secs(3600)));
    let (ns, dep) = (unique("shr09v"), "pay");
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let mut v2 = Report::new(None);
    outbound_exchange(&mut v2, "a.example.com:443", "/x", 1);
    let v2 = v2.to_json().unwrap();
    let _: () = cmd(&mut c, &["SET", &k.report, &v2]).await;
    let got = store
        .push(&ns, dep, push_of("p", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(got, Err(StoreError::Unavailable));
    let stored: String = cmd(&mut c, &["GET", &k.report]).await;
    assert_eq!(stored, v2);
    assert_eq!(store.report(&ns, dep).await, Err(ReadError::Unreadable));
    assert!(log_lines(&capture, "stored report unreadable") >= 1);
}

#[tokio::test]
async fn shr_09_a_stored_outbound_part_expires_when_its_scopes_do() {
    let (store, _) = replica(None, config(Duration::from_secs(10 * DAY)));
    let (ns, dep) = (unique("shr09e"), "pay");
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    // An outbound part last seen long ago, a fresh inbound push: the stale scope goes and
    // the key with it.
    let mut old = Report::new(None);
    outbound_exchange(&mut old, "a.example.com:443", "/x", 1);
    old.stamp(today() - 100);
    let (_, outbound) = old.split();
    let _: () = cmd(
        &mut c,
        &["SET", &k.outbound, &outbound.unwrap().to_json().unwrap()],
    )
    .await;
    let got = store
        .push(&ns, dep, push_of("p", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let exists: i64 = cmd(&mut c, &["EXISTS", &k.outbound]).await;
    assert_eq!(exists, 0);
}

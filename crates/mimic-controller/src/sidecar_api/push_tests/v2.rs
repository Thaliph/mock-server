//! F8 step 5: version 2 pushes through the push route (SHR-01, SHR-09, SHR-14).

use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;

const AUTHORITY: &str = "api.example.com:443";

fn outbound_report(authorities: &[&str]) -> Report {
    let mut report = Report::new(Some(pod_of("pay")));
    exchange(&mut report, "/in", 200);
    for authority in authorities {
        report.record_at(
            ScopeKey::Authority((*authority).to_string()),
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
                path: "/v1/x",
                status: 200,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    report
}

async fn stored_outbound(s: &Setup, deployment: &str) -> Option<String> {
    let key = keys(&s.ns, deployment).unwrap().outbound;
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn shr_09_a_version_2_push_is_applied_and_split() {
    let s = setup(Options::default());
    let report = outbound_report(&[AUTHORITY]);
    assert_eq!(report.version(), 2);
    let answer = push(&s, "pay", payload(SESSION, 1, &report)).await;
    assert_eq!(
        answer.status,
        200,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    assert_eq!(answer.json(), json!({"applied": true}));
    let inbound = stored_report(&s, "pay").await.unwrap();
    assert!(inbound.starts_with("{\"version\":1,"), "{inbound}");
    assert!(!inbound.contains(AUTHORITY));
    let outbound = stored_outbound(&s, "pay").await.unwrap();
    assert!(outbound.starts_with("{\"version\":2,"), "{outbound}");
    assert!(outbound.contains(AUTHORITY));
    // A duplicate changes nothing.
    let again = push(&s, "pay", payload(SESSION, 1, &report)).await;
    assert_eq!(again.json(), json!({"applied": false}));
    assert_eq!(stored_outbound(&s, "pay").await, Some(outbound));
    // A version 1 push still works, and so does the next version 2 one.
    let v1 = push(&s, "pay", payload(SESSION_2, 1, &report_of(&["/b"]))).await;
    assert_eq!(v1.json(), json!({"applied": true}));
    let v2 = push(
        &s,
        "pay",
        payload(SESSION, 2, &outbound_report(&["b.example.com:443"])),
    )
    .await;
    assert_eq!(v2.json(), json!({"applied": true}));
    assert!(
        stored_outbound(&s, "pay")
            .await
            .unwrap()
            .contains("b.example.com:443")
    );
}

/// The envelope of a version 2 report edited by `edit`.
fn edited(edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut body = envelope(SESSION, 1, &outbound_report(&[AUTHORITY]));
    edit(&mut body["report"]);
    serde_json::to_vec(&body).unwrap()
}

#[tokio::test]
async fn shr_09_each_violation_of_the_outbound_check_is_a_400_with_its_reason() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options::default());
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "authority",
            edited(|r| {
                let scope = r["scopes"]["outbound"][AUTHORITY].take();
                r["scopes"]["outbound"] = json!({ "A.example:80": scope });
            }),
            "authority",
        ),
        (
            "version 1 with outbound",
            edited(|r| r["version"] = json!(1)),
            "version",
        ),
        (
            "tunnel kind",
            edited(|r| r["scopes"]["outbound"][AUTHORITY]["tunnels"] = json!({"udp": 1})),
            "tunnels",
        ),
        (
            "scheme",
            edited(|r| {
                r["scopes"]["outbound"][AUTHORITY]["origin"]["schemes"] = json!({"ftp": 1});
            }),
            "origin",
        ),
    ];
    let mut want = Vec::new();
    for (name, body, reason) in cases {
        let answer = push(&s, "pay", body).await;
        assert_eq!(answer.status, 400, "{name}");
        want.push(json!(reason));
    }
    let got: Vec<Value> = lines(&capture)
        .into_iter()
        .filter(|l| l["target"] == "sidecar_api" && l["endpoint"] == ROUTE)
        .map(|l| l["reason"].clone())
        .collect();
    assert_eq!(got, want);
    assert_eq!(stored_report(&s, "pay").await, None);
    assert_eq!(stored_outbound(&s, "pay").await, None);
}

#[tokio::test]
async fn shr_09_more_authorities_than_the_limit_is_507_push_limit_and_nothing_is_stored() {
    let s = setup(Options {
        max_authorities: 2,
        ..Options::default()
    });
    let three = outbound_report(&[
        "a.example.com:443",
        "b.example.com:443",
        "c.example.com:443",
    ]);
    let answer = push(&s, "pay", payload(SESSION, 1, &three)).await;
    assert_eq!(answer.status, 507);
    assert_eq!(stored_report(&s, "pay").await, None);
    assert_eq!(stored_outbound(&s, "pay").await, None);
    let two = outbound_report(&["a.example.com:443", "b.example.com:443"]);
    assert_eq!(push(&s, "pay", payload(SESSION, 1, &two)).await.status, 200);
}

/// The largest `last_seen` in a JSON value.
fn latest(value: &Value) -> Option<u64> {
    match value {
        Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| {
                if k == "last_seen" {
                    v.as_u64()
                } else {
                    latest(v)
                }
            })
            .max(),
        Value::Array(items) => items.iter().filter_map(latest).max(),
        _ => None,
    }
}

#[tokio::test]
async fn shr_09_the_controller_stamps_every_push_over_a_hostile_last_seen() {
    let s = setup(Options::default());
    let far = today() + 10_000;
    let hostile = |report: &mut Value| {
        // Every place a `last_seen` can sit: scope, endpoint and sample, in both directions.
        fn set(value: &mut Value, day: u64) {
            match value {
                Value::Object(map) => {
                    if map.contains_key("last_seen") {
                        map.insert("last_seen".into(), json!(day));
                    }
                    for v in map.values_mut() {
                        set(v, day);
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(|v| set(v, day)),
                _ => {}
            }
        }
        report["scopes"]["outbound"][AUTHORITY]["last_seen"] = json!(far);
        set(report, far);
    };
    let body = edited(hostile);
    assert!(latest(&serde_json::from_slice::<Value>(&body).unwrap()["report"]) == Some(far));
    let answer = push(&s, "pay", body).await;
    assert_eq!(
        answer.status,
        200,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    for stored in [
        stored_outbound(&s, "pay").await.unwrap(),
        stored_report(&s, "pay").await.unwrap(),
    ] {
        let stored: Value = serde_json::from_str(&stored).unwrap();
        assert!(latest(&stored).is_none_or(|day| day <= today()), "{stored}");
    }
    let stored: Value = serde_json::from_str(&stored_outbound(&s, "pay").await.unwrap()).unwrap();
    assert_eq!(latest(&stored), Some(today()));
}

#[tokio::test]
async fn shr_09_the_push_line_carries_the_report_version() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options::default());
    let v2 = push(
        &s,
        "pay",
        payload(SESSION, 1, &outbound_report(&[AUTHORITY])),
    )
    .await;
    assert_eq!(v2.status, 200);
    let v1 = push(&s, "pay", payload(SESSION, 2, &report_of(&["/a"]))).await;
    assert_eq!(v1.status, 200);
    let seen: Vec<Value> = lines(&capture)
        .into_iter()
        .filter(|l| l["target"] == "sidecar_api" && l["endpoint"] == ROUTE)
        .collect();
    let versions: Vec<_> = seen.iter().map(|l| l["version"].clone()).collect();
    assert_eq!(versions, [json!(2), json!(1)]);
}

#[tokio::test]
async fn shr_09_a_hostile_last_seen_does_not_keep_stored_data_alive_past_the_ttl() {
    let s = setup(Options::default());
    let far = today() + 10_000;
    let body = edited(|report| {
        fn set(value: &mut Value, day: u64) {
            match value {
                Value::Object(map) => {
                    if map.contains_key("last_seen") {
                        map.insert("last_seen".into(), json!(day));
                    }
                    map.values_mut().for_each(|v| set(v, day));
                }
                Value::Array(items) => items.iter_mut().for_each(|v| set(v, day)),
                _ => {}
            }
        }
        set(report, far);
    });
    assert_eq!(push(&s, "pay", body).await.status, 200);
    let inbound = Report::from_json(&stored_report(&s, "pay").await.unwrap()).unwrap();
    let outbound = Report::from_json(&stored_outbound(&s, "pay").await.unwrap()).unwrap();
    let mut stored = Report::join(inbound, Some(outbound));
    assert!(!stored.endpoints().is_empty());
    // The TTL is 30 days: one day past it, everything the push brought is expired.
    mimic_core::discovery::store::drop_expired(&mut stored, today() + 31, 30);
    assert!(stored.endpoints().is_empty());
    assert_eq!(stored.authorities(), 0);
}

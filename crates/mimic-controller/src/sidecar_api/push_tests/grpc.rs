//! F14 (#461): version 3 pushes and the two formats of the egress set (spec §6.4, §7.3, §7.7):
//! GRP-24.

use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;
use mimic_core::grpc::frame::encode_frame;
use mimic_core::grpc::wire::{Field, Message, Value as Wire};

use super::egress::{pull, request, stored_outbound};
use super::v2::AUTHORITY;
use super::*;
use crate::store::{keys, parse_outbound};

const JWT: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";

fn framed() -> Vec<u8> {
    let message = Message {
        fields: vec![
            Field {
                number: 1,
                value: Wire::Str("alice".into()),
            },
            Field {
                number: 2,
                value: Wire::Str(JWT.into()),
            },
        ],
    };
    encode_frame(false, &message.encode().unwrap()).unwrap()
}

/// An inbound and an outbound gRPC call, with trailers.
fn grpc_report() -> Report {
    grpc_report_of("/pkg.Users/Get", true)
}

fn grpc_report_of(path: &str, outbound: bool) -> Report {
    let mut report = Report::new(Some(pod_of("pay")));
    let body = framed();
    let headers = [("content-type", "application/grpc")];
    let trailers = [("grpc-status", "0"), ("x-trace-bin", "AAEC")];
    for (direction, key) in [
        (Direction::Inbound, ScopeKey::Port(50051)),
        (Direction::Outbound, ScopeKey::Authority(AUTHORITY.into())),
    ]
    .into_iter()
    .filter(|(d, _)| outbound || *d == Direction::Inbound)
    {
        report.record_at(
            key,
            &Via {
                tls: false,
                sni: None,
                dst: None,
            },
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction,
                port: 50051,
                method: "POST",
                path,
                status: 200,
                head_ms: 3,
                host: None,
                grpc: true,
                observation: Observation {
                    query: "",
                    req_headers: &headers,
                    req_body: Some(&body),
                    resp_headers: &headers,
                    resp_body: Some(&body),
                    resp_trailers: &trailers,
                },
            },
        );
    }
    report
}

#[tokio::test]
async fn grp_24_a_version_3_push_is_applied_split_and_stored_redacted() {
    let s = setup(Options::default());
    let report = grpc_report();
    assert_eq!(report.version(), 3);
    let answer = push(&s, "pay", payload(SESSION, 1, &report)).await;
    assert_eq!(
        answer.status,
        200,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    let inbound = stored_report(&s, "pay").await.unwrap();
    let outbound = stored_outbound(&s, "pay").await.unwrap();
    let overlay = stored_grpc(&s, "pay").await.unwrap();
    // The parts are in the formats of F13 (GRP-30); the messages and trailers are in `:grpc`.
    assert!(inbound.starts_with("{\"version\":1,"), "{inbound}");
    assert!(outbound.starts_with("{\"version\":2,"), "{outbound}");
    for stored in [&inbound, &outbound] {
        assert!(!stored.contains("\"trailers\"") && !stored.contains("mimic-jwt"));
    }
    assert!(overlay.contains("mimic-jwt") && overlay.contains("\"trailers\""));
    for stored in [&inbound, &outbound, &overlay] {
        assert!(!stored.contains(JWT) && !stored.contains("AAEC"));
    }
    // A version 1 and a version 2 push (the form of F13) are applied as before.
    let v1 = push(&s, "pay", payload(SESSION_2, 1, &report_of(&["/b"]))).await;
    assert_eq!(v1.json(), json!({"applied": true}));
    let v2 = push(
        &s,
        "pay",
        payload(
            SESSION_2,
            2,
            &super::v2::outbound_report(&["b.example.com:443"]),
        ),
    )
    .await;
    assert_eq!(v2.json(), json!({"applied": true}));
    // The report route answers the union, version 3, with the messages.
    let merged = s.api.store.report(&s.ns, "pay").await.unwrap();
    let merged = String::from_utf8(merged).unwrap();
    assert!(merged.starts_with("{\"version\":3,"), "{merged}");
    assert!(merged.contains("mimic-jwt"));
    // A message that is not redacted does not pass the check: the sidecar redacts first.
    let forged = payload(SESSION, 3, &report).clone();
    let text = String::from_utf8(forged).unwrap().replace("alice", "bob");
    assert_eq!(push(&s, "pay", text.into_bytes()).await.status, 400);
}

#[tokio::test]
async fn grp_24_the_two_formats_are_served_with_their_own_etags() {
    let s = setup(Options::default());
    push(&s, "pay", payload(SESSION, 1, &grpc_report())).await;
    let tok = token(&s.ns, "pay", "t");
    let get = |query: &'static str, held: Option<String>| {
        let (router, tok) = (s.router.clone(), tok.clone());
        async move {
            request(
                &router,
                Method::GET,
                Some(&tok),
                "pay",
                query,
                held.as_deref(),
            )
            .await
        }
    };
    let two = get("?format=2", None).await;
    let one = get("?format=1", None).await;
    assert_eq!((two.status, one.status), (200, 200));
    assert!(two.text().contains("\"format\":2") && two.text().contains("\"trailers\""));
    assert!(one.text().contains("\"format\":1"));
    assert!(
        !one.text().contains("trailers") && !one.text().contains("\"media\":\"application/grpc\"")
    );
    assert!(two.etag().starts_with("\"2-") && one.etag().starts_with("\"1-"));
    // The same digest, two entries: neither answers for the other.
    assert_eq!(get("?format=2", Some(two.etag())).await.status, 304);
    assert_eq!(get("?format=1", Some(one.etag())).await.status, 304);
    assert_eq!(get("?format=1", Some(two.etag())).await.status, 200);
    assert_eq!(get("?format=2", Some(one.etag())).await.status, 200);
    let again = get("?format=1", None).await;
    assert_eq!(again.text(), one.text());
    // What a sidecar up to F13 reads: the format 1 body reads with the check of format 1.
    let set = mimic_core::discovery::egress::EgressSet::from_json(&one.text()).unwrap();
    assert_eq!(set.format, 1);
    for query in ["?format=3", "?format=0", "?format=", ""] {
        assert_eq!(get(query, None).await.status, 400, "{query}");
    }
    // The pull helper of the earlier steps asks for format 1.
    assert_eq!(pull(&s, "pay", None).await.text(), one.text());
}

async fn stored_grpc(s: &Setup, deployment: &str) -> Option<String> {
    let key = keys(&s.ns, deployment).unwrap().grpc;
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn grp_30_everything_written_is_read_by_the_rules_of_f13() {
    let s = setup(Options::default());
    assert_eq!(
        push(&s, "pay", payload(SESSION, 1, &grpc_report()))
            .await
            .status,
        200
    );
    // `:report` is read as version 1 and `:outbound` as version 2 holding no inbound scope: the
    // rules of `read_part` of F13; neither holds a gRPC part or an unknown field.
    let inbound = stored_report(&s, "pay").await.unwrap();
    let outbound = stored_outbound(&s, "pay").await.unwrap();
    let inbound = Report::parse_checked(&inbound, usize::MAX, usize::MAX).unwrap();
    assert_eq!(inbound.version(), 1);
    let outbound = parse_outbound(outbound.as_bytes()).expect("read by the rules of F13");
    assert_eq!(outbound.version(), 2);
    // `:grpc` has the lifetime of the parts, and its digest is in `:meta`.
    let k = keys(&s.ns, "pay").unwrap();
    let mut c = conn().await;
    let ttl: i64 = redis::cmd("PTTL")
        .arg(&k.grpc)
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(ttl > 0);
    let digest: String = redis::cmd("HGET")
        .arg(&k.meta)
        .arg("grpc_digest")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(digest.len(), 64);
    // A push without gRPC data keeps the messages.
    assert_eq!(
        push(&s, "pay", payload(SESSION_2, 1, &report_of(&["/b"])))
            .await
            .status,
        200
    );
    let merged = String::from_utf8(s.api.store.report(&s.ns, "pay").await.unwrap()).unwrap();
    assert!(merged.contains("mimic-jwt") && merged.contains("\"trailers\""));
    // A workload without a gRPC sample has no `:grpc` and no digest.
    assert_eq!(
        push(&s, "web", payload(SESSION, 1, &report_of(&["/b"])))
            .await
            .status,
        200
    );
    assert!(stored_grpc(&s, "web").await.is_none());
}

#[tokio::test]
async fn grp_30_the_etag_of_format_2_follows_the_overlay_alone() {
    let s = setup(Options::default());
    push(&s, "pay", payload(SESSION, 1, &grpc_report())).await;
    let tok = token(&s.ns, "pay", "t");
    let get = |query: &'static str| {
        let (router, tok) = (s.router.clone(), tok.clone());
        async move { request(&router, Method::GET, Some(&tok), "pay", query, None).await }
    };
    let (two, one) = (get("?format=2").await, get("?format=1").await);
    // Only `:grpc` changes: another inbound gRPC path.
    push(
        &s,
        "pay",
        payload(SESSION_2, 1, &grpc_report_of("/pkg.Users/List", false)),
    )
    .await;
    let (two_after, one_after) = (get("?format=2").await, get("?format=1").await);
    assert_ne!(two.etag(), two_after.etag());
    assert_eq!(one.etag(), one_after.etag());
    assert!(two_after.text().contains("\"trailers\""));
    assert!(!one_after.text().contains("trailers"));
}

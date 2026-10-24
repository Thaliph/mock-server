//! Critère MCK-29 de la spec (§5.3, §7) : trailers de réponse redactés, gardés dans le
//! snapshot et sa forme de mock.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::model::{Body, Direction, Header};
use mimic_core::redact::{RedactConfig, Redactor};
use mimic_core::snapshot::{MockSnapshot, Snapshot, SnapshotOptions, build};

fn grpc() -> mimic_core::model::Exchange {
    let mut e = exchange("POST", "/shop.Quotes/Get")
        .with_req_header("content-type", "application/grpc")
        .with_resp_header("content-type", "application/grpc")
        .at(1, "g1");
    e.direction = Direction::Outbound;
    e.authority = "quotes.shop:443".into();
    e.req_body = Body::captured(b"\0\0\0\0\x02hi".to_vec());
    e.resp_body = Body::captured(b"\0\0\0\0\x02ok".to_vec());
    e.resp_trailers = vec![
        Header::new("grpc-status", "0"),
        Header::new("grpc-message", "OK"),
        Header::new("x-auth-token", "secret-de-trailer"),
    ];
    e
}

#[test]
fn mck_29_trailers_are_redacted_and_kept_in_snapshots() {
    let mut e = grpc();
    Redactor::new(RedactConfig::default(), b"sel".to_vec()).redact_with(&mut e, true);
    let value = |name: &str| {
        e.resp_trailers
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.value.clone())
            .unwrap()
    };
    assert_eq!(value("grpc-status"), "0");
    assert_eq!(value("grpc-message"), "OK");
    assert!(value("x-auth-token").starts_with("<redacted:sha256:"));
    assert!(
        e.redactions
            .iter()
            .any(|m| m.location == "response.trailer.x-auth-token"),
        "{:?}",
        e.redactions
    );

    let snapshot = build([&e], &SnapshotOptions::default());
    let reread = Snapshot::from_jsonl(&snapshot.to_jsonl(), &snapshot.id).unwrap();
    assert_eq!(reread.exchanges[0].resp_trailers, e.resp_trailers);
    let form = MockSnapshot::from_snapshot(&reread);
    let form = MockSnapshot::from_json(&form.to_json(), &snapshot.id).unwrap();
    assert_eq!(form.records[0].resp_trailers, e.resp_trailers);

    // Sans trailers : pas de champ, donc les mêmes identifiants de snapshot qu'avant.
    let mut plain = grpc();
    plain.resp_trailers.clear();
    let line = serde_json::to_string(&plain).unwrap();
    assert!(!line.contains("resp_trailers"), "{line}");
}

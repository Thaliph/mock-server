//! GRP-19 to GRP-23 and GRP-26 (#461): recording unary gRPC messages in samples, report
//! version 3 and the egress formats.

use mimic_core::discovery::egress::{EgressSet, Refusal, etag};
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::openapi;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_core::grpc::frame::encode_frame;
use mimic_core::grpc::record::{Failure, first_message};
use mimic_core::grpc::wire::{Field, Message, Value};
use proptest::prelude::*;
use serde_json::Value as Json;

const JWT: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const GHP: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

fn f(number: u32, value: Value) -> Field {
    Field { number, value }
}

fn message(fields: Vec<Field>) -> Message {
    Message { fields }
}

fn frame(m: &Message) -> Vec<u8> {
    encode_frame(false, &m.encode().expect("encodes")).expect("frames")
}

/// 32 distinct bytes: a random-key stand-in.
fn blob() -> Vec<u8> {
    (0u8..32)
        .map(|b| b.wrapping_mul(7).wrapping_add(3))
        .collect()
}

/// A message holding a secret of every kind the redaction knows, and a neutral field.
fn canary() -> Message {
    message(vec![
        f(1, Value::Str("alice".into())),
        f(2, Value::Str(JWT.into())),
        f(
            3,
            Value::Message(message(vec![f(1, Value::Str(GHP.into()))])),
        ),
        f(4, Value::Bytes(blob())),
        f(5, Value::Varint(42)),
    ])
}

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 64,
        max_authorities: 16,
        sample_budget: 1 << 20,
    }
}

struct Call<'a> {
    request: Vec<u8>,
    response: Vec<u8>,
    req_headers: Vec<(&'a str, &'a str)>,
    resp_headers: Vec<(&'a str, &'a str)>,
    trailers: Vec<(&'a str, &'a str)>,
}

impl Default for Call<'_> {
    fn default() -> Self {
        Call {
            request: frame(&canary()),
            response: frame(&canary()),
            req_headers: vec![("content-type", "application/grpc")],
            resp_headers: vec![("content-type", "application/grpc")],
            trailers: vec![("grpc-status", "0"), ("grpc-message", "ok")],
        }
    }
}

fn record(report: &mut Report, direction: Direction, call: &Call<'_>) {
    let exchange = Exchange {
        direction,
        port: 50051,
        method: "POST",
        path: "/pkg.Users/Get",
        status: 200,
        head_ms: 2,
        host: None,
        grpc: true,
        observation: Observation {
            query: "",
            req_headers: &call.req_headers,
            req_body: Some(&call.request),
            resp_headers: &call.resp_headers,
            resp_body: Some(&call.response),
            resp_trailers: &call.trailers,
            resp_marks: &[],
        },
    };
    let via = Via {
        tls: false,
        sni: None,
        dst: None,
    };
    let scope = match direction {
        Direction::Inbound => ScopeKey::Port(50051),
        Direction::Outbound => ScopeKey::Authority("users.example:80".into()),
    };
    report.record_at(scope, &via, &limits(), &Redactor::default(), &exchange);
}

fn recorded(call: &Call<'_>) -> Report {
    let mut report = Report::new(None);
    record(&mut report, Direction::Inbound, call);
    report
}

fn plain_report() -> Report {
    let mut report = Report::new(None);
    let exchange = Exchange {
        direction: Direction::Inbound,
        port: 8080,
        method: "GET",
        path: "/a",
        status: 200,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: Observation {
            query: "",
            req_headers: &[],
            req_body: None,
            resp_headers: &[("content-type", "application/json")],
            resp_body: Some(b"{\"a\":1}"),
            resp_trailers: &[],
            resp_marks: &[],
        },
    };
    let via = Via {
        tls: false,
        sni: None,
        dst: None,
    };
    report.record_at(
        ScopeKey::Port(8080),
        &via,
        &limits(),
        &Redactor::default(),
        &exchange,
    );
    report
}

fn json(report: &Report) -> Json {
    serde_json::from_str(&report.to_json().expect("serialises")).expect("json")
}

fn samples(report: &Json) -> Vec<&Json> {
    let mut out = Vec::new();
    fn walk<'a>(v: &'a Json, out: &mut Vec<&'a Json>) {
        match v {
            Json::Object(map) => {
                if let Some(Json::Object(samples)) = map.get("samples") {
                    out.extend(samples.values());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Json::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(report, &mut out);
    out
}

#[test]
fn grp_19_first_message_of_identity_gzip_and_several_frames() {
    let m = canary();
    let one = frame(&m);
    assert_eq!(first_message(None, &one).expect("reads"), m);
    let mut three = one.clone();
    three.extend(frame(&message(vec![f(1, Value::Varint(9))])));
    three.extend(frame(&message(vec![f(1, Value::Varint(8))])));
    assert_eq!(first_message(Some("identity"), &three).expect("reads"), m);
    // A trailing partial frame is ignored.
    let mut cut = one.clone();
    cut.extend([0, 0, 0, 0, 9, 1]);
    assert_eq!(first_message(None, &cut).expect("reads"), m);
    // gzip.
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&m.encode().expect("encodes")).expect("writes");
    let packed = encode_frame(true, &gz.finish().expect("finishes")).expect("frames");
    assert_eq!(first_message(Some("gzip"), &packed).expect("reads"), m);
}

#[test]
fn grp_19_failures() {
    let one = frame(&canary());
    let packed = encode_frame(true, b"zzzz").expect("frames");
    assert_eq!(first_message(Some("br"), &packed), Err(Failure::Encoding));
    assert_eq!(first_message(Some("zstd"), &packed), Err(Failure::Encoding));
    // A frame announcing 256 KiB + 1, from its header.
    let mut big = vec![0];
    big.extend(((256 * 1024 + 1) as u32).to_be_bytes());
    assert_eq!(first_message(None, &big), Err(Failure::TooLarge));
    // A gzip payload inflating past 256 KiB.
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&vec![0u8; 256 * 1024 + 1]).expect("writes");
    let bomb = encode_frame(true, &gz.finish().expect("finishes")).expect("frames");
    assert_eq!(first_message(Some("gzip"), &bomb), Err(Failure::TooLarge));
    // 4097 fields.
    let many = message((0..4097).map(|_| f(1, Value::Varint(1))).collect());
    assert_eq!(first_message(None, &frame(&many)), Err(Failure::TooLarge));
    // Opaque.
    let mut bad_flag = one.clone();
    bad_flag[0] = 2;
    assert_eq!(first_message(None, &bad_flag), Err(Failure::Opaque));
    assert_eq!(
        first_message(None, &one[..one.len() - 1]),
        Err(Failure::Opaque)
    );
    assert_eq!(first_message(None, &[]), Err(Failure::Opaque));
    assert_eq!(
        first_message(None, &encode_frame(false, &[0xff]).expect("frames")),
        Err(Failure::Opaque)
    );
}

proptest! {
    #[test]
    fn grp_19_no_input_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300),
                              encoding in proptest::option::of("[a-z]{0,6}")) {
        let _ = first_message(encoding.as_deref(), &bytes);
    }
}

#[test]
fn grp_20_sample_holds_the_redacted_message_and_the_trailers() {
    let call = Call {
        req_headers: vec![
            ("content-type", "application/grpc"),
            ("authorization", "Bearer abc"),
            ("x-note-bin", "AAEC"),
        ],
        trailers: vec![
            ("grpc-status", "0"),
            ("grpc-message", "ok"),
            ("x-request-id", "r-1"),
            ("x-trace-bin", "AAEC"),
        ],
        ..Call::default()
    };
    let report = recorded(&call);
    let text = report.to_json().expect("serialises");
    for secret in [JWT, GHP, "AAEC"] {
        assert!(!text.contains(secret), "a secret reached the report");
    }
    let value = json(&report);
    let all = samples(&value);
    assert_eq!(all.len(), 1);
    let sample = all[0];
    for side in ["request", "response"] {
        let body = &sample[side]["body"];
        assert_eq!(body["media"], "application/grpc", "{side}");
        let message = &body["value"];
        assert_eq!(message["1"], "alice");
        assert_eq!(message["2"], "mimic-jwt");
        assert_eq!(message["3"]["1"], "mimic-token");
        assert_eq!(message["4"], "mimic-secret");
        assert_eq!(message["5"], 42);
    }
    let headers = &sample["request"]["headers"];
    assert!(headers.to_string().contains("Bearer mimic-token"));
    assert!(headers.to_string().contains("mimic-secret"));
    assert_eq!(
        sample["response"]["trailers"],
        serde_json::json!([
            ["grpc-status", "0"],
            ["grpc-message", "ok"],
            ["x-request-id", "r-1"],
            ["x-trace-bin", "mimic-secret"]
        ])
    );
    // No frame: neither the length prefix nor a base64 of the payload.
    let payload = call.request[5..].to_vec();
    let b64 = base64_of(&payload);
    assert!(!text.contains(&b64));
    assert_eq!(report.check(64, 16), Ok(()));
    assert_eq!(Report::from_json(&text).expect("reads"), report);
}

fn base64_of(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
        }
        out.extend(std::iter::repeat_n('=', 3 - chunk.len()));
    }
    out
}

#[test]
fn grp_20_outbound_exchange_alike() {
    let mut report = Report::new(None);
    record(&mut report, Direction::Outbound, &Call::default());
    let text = report.to_json().expect("serialises");
    assert!(!text.contains(JWT) && !text.contains(GHP));
    let value = json(&report);
    let sample = samples(&value)[0].clone();
    assert_eq!(sample["response"]["body"]["media"], "application/grpc");
    assert_eq!(sample["response"]["trailers"][0][0], "grpc-status");
}

#[test]
fn grp_20_gzip_messages_read_alike() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&canary().encode().expect("encodes"))
        .expect("writes");
    let packed = encode_frame(true, &gz.finish().expect("finishes")).expect("frames");
    let call = Call {
        request: packed,
        req_headers: vec![
            ("content-type", "application/grpc"),
            ("grpc-encoding", "gzip"),
        ],
        ..Call::default()
    };
    let value = json(&recorded(&call));
    assert_eq!(
        samples(&value)[0]["request"]["body"]["value"]["2"],
        "mimic-jwt"
    );
}

#[test]
fn grp_20_other_media_and_non_messages_leave_the_body_out() {
    for media in [
        "application/grpc+json",
        "application/grpc-web",
        "application/grpc-web+proto",
    ] {
        let call = Call {
            req_headers: vec![("content-type", media)],
            resp_headers: vec![("content-type", media)],
            ..Call::default()
        };
        let value = json(&recorded(&call));
        let sample = samples(&value)[0].clone();
        assert!(sample["request"]["body"].is_null(), "{media}");
        assert!(sample["response"]["body"].is_null(), "{media}");
    }
    let call = Call {
        request: encode_frame(false, &[0xff]).expect("frames"),
        ..Call::default()
    };
    let value = json(&recorded(&call));
    assert!(samples(&value)[0]["request"]["body"].is_null());
}

#[test]
fn grp_20_a_sample_without_grpc_parts_is_what_it_was() {
    let text = plain_report().to_json().expect("serialises");
    assert!(!text.contains("trailers"));
    assert!(text.contains("\"version\":1"));
}

#[test]
fn grp_21_version_3_rule() {
    let grpc = recorded(&Call::default());
    assert_eq!(grpc.version(), 3);
    assert!(
        grpc.to_json()
            .expect("serialises")
            .contains("\"version\":3")
    );
    assert!(
        !grpc
            .to_json()
            .expect("serialises")
            .contains("outbound_exchanges")
    );
    let mut both = recorded(&Call::default());
    record(&mut both, Direction::Outbound, &Call::default());
    assert_eq!(both.version(), 3);
    assert!(
        both.to_json()
            .expect("serialises")
            .contains("outbound_exchanges")
    );
    assert_eq!(both.check(64, 16), Ok(()));
    assert_eq!(
        Report::from_json(&both.to_json().expect("serialises")).expect("reads"),
        both
    );
    // Neither is a plain version 1 or 2 report.
    let wrong = both
        .to_json()
        .expect("serialises")
        .replace("\"version\":3", "\"version\":2");
    assert!(Report::from_json(&wrong).is_err());
    assert_eq!(plain_report().version(), 1);
}

#[test]
fn grp_21_merge_in_either_order_and_without_grpc() {
    let plain = plain_report();
    let grpc = recorded(&Call::default());
    let mut a = plain.clone();
    a.merge(&limits(), grpc.clone());
    let mut b = grpc.clone();
    b.merge(&limits(), plain.clone());
    assert_eq!(a.version(), 3);
    assert_eq!(a.to_json().expect("a"), b.to_json().expect("b"));
    // Without the gRPC parts it is the plain report again, version 1.
    let stripped = a.without_grpc();
    assert_eq!(stripped.version(), 1);
    let text = stripped.to_json().expect("serialises");
    assert!(!text.contains("\"trailers\"") && !text.contains("\"media\":\"application/grpc\""));
    assert_eq!(stripped.check(64, 16), Ok(()));
    // With an outbound scope: version 2.
    let mut both = recorded(&Call::default());
    record(&mut both, Direction::Outbound, &Call::default());
    assert_eq!(both.without_grpc().version(), 2);
    assert_eq!(both.without_grpc().check(64, 16), Ok(()));
}

#[test]
fn grp_21_split_and_join() {
    let mut both = recorded(&Call::default());
    record(&mut both, Direction::Outbound, &Call::default());
    let (inbound, outbound) = both.split();
    let outbound = outbound.expect("outbound part");
    assert_eq!((inbound.version(), outbound.version()), (3, 3));
    assert_eq!(Report::join(inbound, Some(outbound)), both);
    // A part without gRPC parts is version 1 or 2 as before.
    let mut mixed = plain_report();
    record(&mut mixed, Direction::Outbound, &Call::default());
    let (inbound, outbound) = mixed.split();
    assert_eq!(inbound.version(), 1);
    assert_eq!(outbound.expect("outbound part").version(), 3);
}

#[test]
fn grp_22_check_and_re_redaction() {
    let report = recorded(&Call::default());
    let text = report.to_json().expect("serialises");
    // 65 trailers.
    let mut value: Json = serde_json::from_str(&text).expect("json");
    let pairs: Vec<Json> = (0..65)
        .map(|i| serde_json::json!([format!("x-{i}"), "v"]))
        .collect();
    set_trailers(&mut value, Json::Array(pairs));
    let refused = Report::parse_checked(&value.to_string(), 64, 16);
    assert!(refused.is_err(), "65 trailers");
    // A media type that is not gRPC on a gRPC body.
    let text2 = text.replace(
        "\"media\":\"application/grpc\"",
        "\"media\":\"application/grpc-web\"",
    );
    assert!(Report::parse_checked(&text2, 64, 16).is_err());
}

fn set_trailers(v: &mut Json, new: Json) {
    match v {
        Json::Object(map) => {
            if map.contains_key("trailers") {
                map.insert("trailers".into(), new.clone());
            }
            map.values_mut().for_each(|v| set_trailers(v, new.clone()));
        }
        Json::Array(items) => items.iter_mut().for_each(|v| set_trailers(v, new.clone())),
        _ => {}
    }
}

#[test]
fn grp_22_a_faulty_sidecar_cannot_store_a_secret_in_a_message() {
    // A sample whose message holds raw secrets, as a sidecar that did not redact would push.
    let report = recorded(&Call::default());
    let mut value = json(&report);
    let raw = serde_json::json!({
        "1": "alice", "2": JWT, "3": {"1": GHP},
        "4": {"$bytes": base64_of(&blob())}, "5": 42
    });
    replace_bodies(&mut value, &raw);
    let mut pushed = Report::from_json(&value.to_string()).expect("reads");
    pushed.re_redact(&Redactor::default());
    let text = pushed.to_json().expect("serialises");
    assert!(!text.contains(JWT) && !text.contains(GHP) && !text.contains(&base64_of(&blob())));
    assert!(text.contains("mimic-jwt") && text.contains("mimic-token"));
    // Idempotent.
    let mut again = pushed.clone();
    again.re_redact(&Redactor::default());
    assert_eq!(again, pushed);
    // A body that is not a message.
    let mut value = json(&report);
    replace_bodies(&mut value, &serde_json::json!("not a message"));
    let mut pushed = Report::from_json(&value.to_string()).expect("reads");
    pushed.re_redact(&Redactor::default());
    assert!(
        pushed
            .to_json()
            .expect("serialises")
            .contains("\"value\":\"mimic-secret\"")
    );
}

fn replace_bodies(v: &mut Json, new: &Json) {
    match v {
        Json::Object(map) => {
            if map.get("media").is_some_and(|m| m == "application/grpc")
                && map.contains_key("value")
            {
                map.insert("value".into(), new.clone());
            }
            map.values_mut().for_each(|v| replace_bodies(v, new));
        }
        Json::Array(items) => items.iter_mut().for_each(|v| replace_bodies(v, new)),
        _ => {}
    }
}

fn outbound_report() -> Report {
    let mut report = Report::new(None);
    record(&mut report, Direction::Outbound, &Call::default());
    report
}

#[test]
fn grp_23_the_two_formats() {
    let report = outbound_report();
    let two = EgressSet::project(&report);
    assert_eq!(two.format, 2);
    let text2 = two.to_json().expect("serialises");
    assert!(text2.contains("\"trailers\"") && text2.contains("application/grpc"));
    let one = EgressSet::project_as(&report, 1);
    assert_eq!(one.format, 1);
    let text1 = one.to_json().expect("serialises");
    assert!(!text1.contains("trailers") && !text1.contains("\"media\":\"application/grpc\""));
    // Both read back byte for byte, and the format 1 set is what the report without gRPC parts gives.
    assert_eq!(
        EgressSet::from_json(&text1)
            .expect("reads")
            .to_json()
            .expect("serialises"),
        text1
    );
    assert_eq!(
        EgressSet::from_json(&text2)
            .expect("reads")
            .to_json()
            .expect("serialises"),
        text2
    );
    let plain = EgressSet::project_as(&report.without_grpc(), 2);
    let mut as_one = plain.clone();
    as_one.format = 1;
    assert_eq!(as_one.to_json().expect("serialises"), text1);
}

#[test]
fn grp_23_formats_read_and_refused() {
    let two = EgressSet::project(&outbound_report());
    let text = two.to_json().expect("serialises");
    for format in [0, 3] {
        let other = text.replace("\"format\":2", &format!("\"format\":{format}"));
        assert_eq!(EgressSet::from_json(&other), Err(Refusal::Format));
    }
    // A format 1 set that holds a gRPC part.
    let wrong = text.replace("\"format\":2", "\"format\":1");
    assert_eq!(EgressSet::from_json(&wrong), Err(Refusal::Malformed));
    assert_ne!(etag(1, "ab"), etag(2, "ab"));
}

#[test]
fn grp_23_projections_are_canonical() {
    let a = EgressSet::project(&outbound_report()).to_json();
    let b = EgressSet::project(&outbound_report()).to_json();
    assert_eq!(a.expect("a"), b.expect("b"));
}

#[test]
fn grp_26_openapi_shows_the_message() {
    let report = recorded(&Call::default());
    let doc = openapi::document(&report, "ns", "dep", 1, "inbound-50051").expect("document");
    let op = &doc["paths"]["/pkg.Users/Get"]["post"];
    let request = &op["requestBody"]["content"]["application/grpc"];
    assert!(
        request["schema"]["properties"]["1"].is_object(),
        "{request}"
    );
    assert_eq!(request["examples"].as_object().map(|e| e.len()), Some(1));
    let response = &op["responses"]["200"]["content"]["application/grpc"];
    assert!(
        response["schema"]["properties"]["2"].is_object(),
        "{response}"
    );
    let text = doc.to_string();
    assert!(!text.contains(JWT) && !text.contains(GHP) && !text.contains("trailers"));
}

fn set_in_samples(v: &mut Json, f: &dyn Fn(&mut Json)) {
    match v {
        Json::Object(map) => {
            if let Some(Json::Object(samples)) = map.get_mut("samples") {
                samples.values_mut().for_each(f);
            }
            map.values_mut().for_each(|v| set_in_samples(v, f));
        }
        Json::Array(items) => items.iter_mut().for_each(|v| set_in_samples(v, f)),
        _ => {}
    }
}

#[test]
fn grp_22_raw_trailers_and_bin_values_are_redacted_whatever_the_request() {
    let report = recorded(&Call::default());
    let mut value = json(&report);
    // A sample pushed with raw metadata, and a request that is JSON, not gRPC.
    set_in_samples(&mut value, &|sample| {
        sample["request"]["headers"] = serde_json::json!([
            ["content-type", "application/json"],
            ["x-note-bin", "QUJD"],
            ["x-neutral", "kept"]
        ]);
        sample["response"]["trailers"] = serde_json::json!([
            ["x-trace-bin", "QUJD"],
            ["x-token", JWT],
            ["grpc-status", "0"]
        ]);
    });
    let mut pushed = Report::from_json(&value.to_string()).expect("reads");
    pushed.re_redact(&Redactor::default());
    let text = pushed.to_json().expect("serialises");
    assert!(!text.contains(JWT) && !text.contains("QUJD"));
    let after = json(&pushed);
    let sample = samples(&after)[0].clone();
    assert_eq!(
        sample["response"]["trailers"],
        serde_json::json!([
            ["x-trace-bin", "mimic-secret"],
            ["x-token", "mimic-token"],
            ["grpc-status", "0"]
        ])
    );
    assert!(sample["request"]["headers"].to_string().contains("kept"));
    let mut again = pushed.clone();
    again.re_redact(&Redactor::default());
    assert_eq!(again, pushed);
}

fn both_ways() -> Report {
    let mut report = recorded(&Call::default());
    record(&mut report, Direction::Outbound, &Call::default());
    report.merge(&limits(), plain_report());
    report
}

#[test]
fn grp_30_the_stored_parts_are_in_the_formats_of_f13() {
    use mimic_core::discovery::overlay::GrpcOverlay;
    let full = both_ways();
    assert_eq!(full.version(), 3);
    let (stripped, overlay) = full.clone().split_grpc();
    assert!(stripped.version() <= 2);
    let (inbound, outbound) = stripped.clone().into_split();
    let outbound = outbound.expect("an outbound part");
    for part in [inbound.to_json().unwrap(), outbound.to_json().unwrap()] {
        assert!(!part.contains("\"trailers\"") && !part.contains("\"media\":\"application/grpc\""));
        assert!(!part.contains("\"version\":3"));
    }
    assert_eq!((inbound.version(), outbound.version()), (1, 2));
    // The overlay is JSON with the full samples; it reads back, and applying it gives the report.
    assert!(!overlay.is_empty());
    let text = overlay.to_json().expect("serialises");
    assert!(text.contains("\"trailers\"") && !text.contains(JWT));
    let back = GrpcOverlay::from_json(&text).expect("reads");
    assert_eq!(back.to_json().unwrap(), text);
    let mut joined = stripped.clone();
    joined.apply_grpc(back);
    assert_eq!(joined.to_json().unwrap(), full.to_json().unwrap());
    // Without gRPC samples the overlay is empty, and applying it changes nothing.
    let (plain, none) = plain_report().split_grpc();
    assert!(none.is_empty());
    let mut same = plain.clone();
    same.apply_grpc(none);
    assert_eq!(same, plain);
    // An entry for an endpoint that is gone is dropped; another version is refused.
    let mut gone = Report::new(None);
    gone.apply_grpc(GrpcOverlay::from_json(&text).unwrap());
    assert_eq!(gone.version(), 1);
    assert!(GrpcOverlay::from_json(&text.replacen("\"version\":1", "\"version\":2", 1)).is_err());
}

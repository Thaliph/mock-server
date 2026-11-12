//! GRP-38 to GRP-40 (#464): server-streaming gRPC recorded as a sequence of messages with
//! bucketed delays.

use mimic_core::discovery::check::Violation;
use mimic_core::discovery::egress::{EgressSet, Refusal};
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_core::grpc::frame::encode_frame;
use mimic_core::grpc::record::{Mark, bucketed, first_message, single_message, steps};
use mimic_core::grpc::wire::{Field, Message, Value};
use serde_json::Value as Json;

const JWT: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";

fn num(n: u64) -> Message {
    Message {
        fields: vec![Field {
            number: 1,
            value: Value::Varint(n),
        }],
    }
}

fn text(s: &str) -> Message {
    Message {
        fields: vec![Field {
            number: 1,
            value: Value::Str(s.into()),
        }],
    }
}

fn frame(m: &Message) -> Vec<u8> {
    encode_frame(false, &m.encode().expect("encodes")).expect("frames")
}

/// The body of `n` frames, and the end offset of each.
fn body(messages: &[Message]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::new();
    let mut ends = Vec::new();
    for m in messages {
        out.extend(frame(m));
        ends.push(out.len());
    }
    (out, ends)
}

fn marks(pairs: &[(usize, u64)]) -> Vec<Mark> {
    pairs.iter().map(|&(end, ms)| Mark { end, ms }).collect()
}

fn five() -> Vec<Message> {
    (1..=5).map(num).collect()
}

fn delays(found: &[mimic_core::grpc::record::Step]) -> Vec<u64> {
    found.iter().map(|s| s.delay_ms).collect()
}

#[test]
fn grp_38_steps_are_the_messages_after_the_first_with_bucketed_delays() {
    let (bytes, ends) = body(&five());
    let marks = marks(&[
        (ends[0], 0),
        (ends[1], 50),
        (ends[2], 100),
        (ends[3], 100),
        (ends[4], 400),
    ]);
    let found = steps(None, &bytes, &marks);
    assert_eq!(delays(&found), [53, 53, 0, 304]);
    let messages: Vec<Message> = found.into_iter().map(|s| s.message).collect();
    assert_eq!(messages, five()[1..]);
    assert_eq!(first_message(None, &bytes).expect("reads"), num(1));
}

#[test]
fn grp_38_chunks_that_hold_several_frames_or_part_of_one() {
    let (bytes, ends) = body(&five());
    // One chunk holds everything.
    let one = marks(&[(bytes.len(), 70)]);
    assert_eq!(delays(&steps(None, &bytes, &one)), [0, 0, 0, 0]);
    // The second message is split over two chunks and arrives with the last of them.
    let split = marks(&[
        (ends[0], 10),
        (ends[0] + 2, 40),
        (ends[1], 110),
        (bytes.len(), 110),
    ]);
    assert_eq!(delays(&steps(None, &bytes, &split)), [107, 0, 0, 0]);
    // No marks: no delay.
    assert_eq!(delays(&steps(None, &bytes, &[])), [0, 0, 0, 0]);
}

#[test]
fn grp_38_at_most_63_steps() {
    let many: Vec<Message> = (0..70).map(num).collect();
    let (bytes, _) = body(&many);
    let found = steps(None, &bytes, &[]);
    assert_eq!(found.len(), 63);
    assert_eq!(found[62].message, num(63));
}

#[test]
fn grp_38_encodings() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&num(2).encode().expect("encodes"))
        .expect("writes");
    let packed = encode_frame(true, &gz.finish().expect("finishes")).expect("frames");
    let mut bytes = frame(&num(1));
    bytes.extend(&packed);
    bytes.extend(frame(&num(3)));
    let found = steps(Some("gzip"), &bytes, &[]);
    let messages: Vec<Message> = found.into_iter().map(|s| s.message).collect();
    assert_eq!(messages, [num(2), num(3)]);
    // Under an encoding that is not decoded the compressed message ends the list.
    assert!(steps(Some("br"), &bytes, &[]).is_empty());
}

#[test]
fn grp_38_what_cannot_be_read_ends_the_list() {
    let first = frame(&num(1));
    let second = frame(&num(2));
    let fourth = frame(&num(4));
    // A payload that is not a message.
    let mut bytes = [first.clone(), second.clone()].concat();
    bytes.extend(encode_frame(false, &[0xff]).expect("frames"));
    bytes.extend(&fourth);
    assert_eq!(steps(None, &bytes, &[]).len(), 1);
    // A bad flag.
    let mut bytes = [first.clone(), second.clone()].concat();
    bytes.extend([7, 0, 0, 0, 0]);
    bytes.extend(&fourth);
    assert_eq!(steps(None, &bytes, &[]).len(), 1);
    // A partial last frame.
    let mut bytes = [first.clone(), second].concat();
    bytes.extend([0, 0, 0, 0, 9, 1]);
    assert_eq!(steps(None, &bytes, &[]).len(), 1);
    // The first message is not one: nothing is a step.
    let mut bytes = encode_frame(false, &[0xff]).expect("frames");
    bytes.extend(&fourth);
    assert!(steps(None, &bytes, &[]).is_empty());
    assert!(steps(None, &first, &[]).is_empty());
}

#[test]
fn grp_38_bucketed_delays() {
    for (ms, want) in [
        (0, 0),
        (1, 1),
        (2, 2),
        (50, 53),
        (100, 107),
        (300, 304),
        (5, 5),
        (6, 6),
        (7, 8),
    ] {
        assert_eq!(bucketed(ms), want, "{ms}");
    }
    for ms in [131_073, 1 << 40, u64::MAX] {
        assert_eq!(bucketed(ms), 131_072);
    }
    for ms in (0..2000).chain([4000, 65_535, 100_000, 131_072]) {
        let b = bucketed(ms);
        assert!(b >= ms, "{ms} gives {b}");
        assert_eq!(bucketed(b), b, "{ms}");
    }
}

#[test]
fn grp_38_a_single_message() {
    let one = frame(&num(1));
    assert!(single_message(&one));
    assert!(single_message(&frame(&Message { fields: vec![] })));
    assert!(!single_message(&[]));
    assert!(!single_message(&[one.clone(), one.clone()].concat()));
    assert!(!single_message(&one[..one.len() - 1]));
    assert!(!single_message(&[3, 0, 0, 0, 0]));
    let mut more = one.clone();
    more.push(0);
    assert!(!single_message(&more));
}

// --- recording -------------------------------------------------------------------------

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 64,
        max_authorities: 16,
        sample_budget: 1 << 20,
    }
}

fn record_with(
    report: &mut Report,
    direction: Direction,
    request: &[u8],
    response: &[u8],
    marks: &[Mark],
) {
    let exchange = Exchange {
        direction,
        port: 50051,
        method: "POST",
        path: "/pkg.Users/Watch",
        status: 200,
        head_ms: 2,
        host: None,
        grpc: true,
        observation: Observation {
            query: "",
            req_headers: &[("content-type", "application/grpc")],
            req_body: Some(request),
            resp_headers: &[("content-type", "application/grpc")],
            resp_body: Some(response),
            resp_trailers: &[("grpc-status", "0")],
            resp_marks: marks,
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

fn recorded(request: &[u8], response: &[u8], marks: &[Mark]) -> Report {
    let mut report = Report::new(None);
    record_with(&mut report, Direction::Inbound, request, response, marks);
    report
}

fn json(report: &Report) -> Json {
    serde_json::from_str(&report.to_json().expect("serialises")).expect("json")
}

fn samples(report: &Json) -> Vec<Json> {
    let mut out = Vec::new();
    fn walk(v: &Json, out: &mut Vec<Json>) {
        match v {
            Json::Object(map) => {
                if let Some(Json::Object(samples)) = map.get("samples") {
                    out.extend(samples.values().cloned());
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

fn stream_of(report: &Report) -> Json {
    let all = samples(&json(report));
    assert_eq!(all.len(), 1);
    all[0]["response"]["stream"].clone()
}

fn request() -> Vec<u8> {
    frame(&text("watch"))
}

fn paced() -> (Vec<u8>, Vec<Mark>) {
    let (bytes, ends) = body(&five());
    let marks = marks(&[
        (ends[0], 0),
        (ends[1], 50),
        (ends[2], 100),
        (ends[3], 100),
        (ends[4], 400),
    ]);
    (bytes, marks)
}

#[test]
fn grp_39_a_server_stream_is_the_first_message_and_steps() {
    let (bytes, marks) = paced();
    let report = recorded(&request(), &bytes, &marks);
    assert_eq!(
        stream_of(&report),
        serde_json::json!([
            {"delay_ms": 53, "message": {"1": 2}},
            {"delay_ms": 53, "message": {"1": 3}},
            {"delay_ms": 0, "message": {"1": 4}},
            {"delay_ms": 304, "message": {"1": 5}},
        ])
    );
    assert_eq!(
        samples(&json(&report))[0]["response"]["body"]["value"],
        serde_json::json!({"1": 1})
    );
    assert_eq!(report.version(), 3);
    assert_eq!(report.check(64, 16), Ok(()));
    let back = Report::from_json(&report.to_json().expect("serialises")).expect("reads");
    assert_eq!(back, report);
}

#[test]
fn grp_39_a_unary_call_has_no_stream() {
    let report = recorded(&request(), &frame(&num(1)), &[]);
    let all = samples(&json(&report));
    assert!(all[0]["response"].get("stream").is_none());
}

#[test]
fn grp_39_steps_are_redacted_like_the_first_message() {
    let secret = Message {
        fields: vec![
            Field {
                number: 1,
                value: Value::Str(JWT.into()),
            },
            Field {
                number: 2,
                value: Value::Str("ghp_abcdefghijklmnopqrstuvwxyz0123456789".into()),
            },
            Field {
                number: 3,
                value: Value::Bytes(
                    (0u8..32)
                        .map(|b| b.wrapping_mul(7).wrapping_add(3))
                        .collect(),
                ),
            },
            Field {
                number: 4,
                value: Value::Varint(42),
            },
        ],
    };
    let (bytes, _) = body(&[num(1), secret.clone(), secret]);
    let report = recorded(&request(), &bytes, &[]);
    let text = report.to_json().expect("serialises");
    assert!(!text.contains(JWT) && !text.contains("ghp_") && !text.contains("AAcOFRwj"));
    let stream = stream_of(&report);
    assert_eq!(stream[0]["message"]["1"], "mimic-jwt");
    assert_eq!(stream[1]["message"]["2"], "mimic-token");
    assert_eq!(stream[1]["message"]["3"], "mimic-secret");
    assert_eq!(stream[1]["message"]["4"], 42);
}

#[test]
fn grp_39_client_and_bidirectional_streams_are_recorded_by_their_first_message() {
    let (bytes, marks) = paced();
    let two = [request(), request()].concat();
    let report = recorded(&two, &bytes, &marks);
    let all = samples(&json(&report));
    assert_eq!(
        all[0]["response"]["body"]["value"],
        serde_json::json!({"1": 1})
    );
    assert!(all[0]["response"].get("stream").is_none());
    // No request message either.
    let report = recorded(&[], &bytes, &marks);
    assert!(
        samples(&json(&report))[0]["response"]
            .get("stream")
            .is_none()
    );
}

#[test]
fn grp_39_a_message_that_cannot_be_read_ends_the_list() {
    let mut bytes = [frame(&num(1)), frame(&num(2))].concat();
    bytes.extend(encode_frame(false, &[0xff]).expect("frames"));
    bytes.extend(frame(&num(4)));
    let report = recorded(&request(), &bytes, &[]);
    assert_eq!(stream_of(&report).as_array().map(Vec::len), Some(1));
}

#[test]
fn grp_39_at_most_64_messages_and_256_kib() {
    for count in [64, 65, 100] {
        let many: Vec<Message> = (0..count).map(num).collect();
        let (bytes, _) = body(&many);
        let report = recorded(&request(), &bytes, &[]);
        assert_eq!(stream_of(&report).as_array().map(Vec::len), Some(63));
    }
    // 70 000 bytes that are not text, three times: the base64 of two fit in 256 KiB.
    let big = Message {
        fields: vec![Field {
            number: 1,
            value: Value::Bytes(vec![0xff; 70_000]),
        }],
    };
    let (bytes, _) = body(&[big.clone(), big.clone(), big]);
    let report = recorded(&request(), &bytes, &[]);
    assert_eq!(stream_of(&report).as_array().map(Vec::len), Some(1));
    assert_eq!(report.check(64, 16), Ok(()));
}

#[test]
fn grp_39_streams_are_samples_of_their_own() {
    let (bytes, ends) = body(&five());
    let at = |ms: u64| marks(&[(ends[0], 0), (ends[1], ms), (ends[4], ms)]);
    let mut report = Report::new(None);
    record_with(&mut report, Direction::Inbound, &request(), &bytes, &at(50));
    record_with(&mut report, Direction::Inbound, &request(), &bytes, &at(50));
    assert_eq!(samples(&json(&report)).len(), 1);
    record_with(
        &mut report,
        Direction::Inbound,
        &request(),
        &bytes,
        &at(400),
    );
    assert_eq!(samples(&json(&report)).len(), 2);
    let (shorter, _) = body(&five()[..4]);
    record_with(
        &mut report,
        Direction::Inbound,
        &request(),
        &shorter,
        &at(50),
    );
    assert_eq!(samples(&json(&report)).len(), 3);
    assert_eq!(report.check(64, 16), Ok(()));
}

// --- check, re-redaction, formats --------------------------------------------------------

fn set_samples(v: &mut Json, f: &dyn Fn(&mut Json)) {
    match v {
        Json::Object(map) => {
            if let Some(Json::Object(samples)) = map.get_mut("samples") {
                samples.values_mut().for_each(f);
            }
            map.values_mut().for_each(|v| set_samples(v, f));
        }
        Json::Array(items) => items.iter_mut().for_each(|v| set_samples(v, f)),
        _ => {}
    }
}

fn streamed() -> Report {
    let (bytes, marks) = paced();
    recorded(&request(), &bytes, &marks)
}

fn with_sample(f: &dyn Fn(&mut Json)) -> Result<(), Violation> {
    let mut value = json(&streamed());
    set_samples(&mut value, f);
    Report::parse_checked(&value.to_string(), 64, 16).map(|_| ())
}

#[test]
fn grp_40_check() {
    assert_eq!(with_sample(&|_| {}), Ok(()));
    let step = |ms: u64| serde_json::json!({"delay_ms": ms, "message": {"1": 1}});
    // 64 steps.
    let sixty_four: Vec<Json> = (0..64).map(|_| step(0)).collect();
    assert_eq!(
        with_sample(&|s| s["response"]["stream"] = Json::Array(sixty_four.clone())),
        Err(Violation::Malformed)
    );
    // A delay that is no bucket bound.
    assert_eq!(
        with_sample(&|s| s["response"]["stream"][0]["delay_ms"] = 51.into()),
        Err(Violation::Malformed)
    );
    // A stream without a gRPC response body.
    assert_eq!(
        with_sample(&|s| s["response"]["body"] = Json::Null),
        Err(Violation::Malformed)
    );
    // Messages over 256 KiB in all, each within it.
    let long = "a".repeat(140_000);
    let big = serde_json::json!({"delay_ms": 0, "message": {"1": long}});
    assert_eq!(
        with_sample(&|s| {
            s["response"]["body"]["value"] = serde_json::json!({"1": long});
            s["response"]["stream"] = Json::Array(vec![big.clone()]);
        }),
        Err(Violation::BodySize)
    );
    // A `bytes` that is not the size of the content.
    assert_eq!(
        with_sample(&|s| s["bytes"] = 5.into()),
        Err(Violation::SampleBytes)
    );
    // Another field in a step.
    assert_eq!(
        with_sample(&|s| s["response"]["stream"][0]["x"] = 1.into()),
        Err(Violation::UnknownField)
    );
}

#[test]
fn grp_40_re_redaction() {
    let mut value = json(&streamed());
    set_samples(&mut value, &|s| {
        s["response"]["stream"][0]["message"] = serde_json::json!({"1": JWT});
        s["response"]["stream"][1]["message"] = serde_json::json!("not a message");
    });
    let mut pushed = Report::from_json(&value.to_string()).expect("reads");
    pushed.re_redact(&Redactor::default());
    let text = pushed.to_json().expect("serialises");
    assert!(!text.contains(JWT));
    let stream = stream_of(&pushed);
    assert_eq!(stream[0]["message"]["1"], "mimic-jwt");
    assert_eq!(stream[1]["message"], "mimic-secret");
    let mut again = pushed.clone();
    again.re_redact(&Redactor::default());
    assert_eq!(again, pushed);
}

#[test]
fn grp_40_without_grpc_and_the_formats() {
    let mut report = Report::new(None);
    let (bytes, marks) = paced();
    record_with(&mut report, Direction::Outbound, &request(), &bytes, &marks);
    assert_eq!(report.version(), 3);
    let stripped = report.without_grpc();
    assert!(
        !stripped
            .to_json()
            .expect("serialises")
            .contains("\"stream\"")
    );
    assert_eq!(stripped.check(64, 16), Ok(()));
    let two = EgressSet::project(&report).to_json().expect("serialises");
    assert!(two.contains("\"stream\""));
    let one = EgressSet::project_as(&report, 1)
        .to_json()
        .expect("serialises");
    assert!(!one.contains("stream") && !one.contains("trailers"));
    // As the set of a report without gRPC parts.
    let plain = EgressSet::project_as(&stripped, 2);
    let mut as_one = plain;
    as_one.format = 1;
    assert_eq!(as_one.to_json().expect("serialises"), one);
    // A set of format 1 that holds a stream is refused, one of format 2 is read back.
    let wrong = two.replace("\"format\":2", "\"format\":1");
    assert_eq!(EgressSet::from_json(&wrong), Err(Refusal::Malformed));
    assert_eq!(
        EgressSet::from_json(&two)
            .expect("reads")
            .to_json()
            .expect("serialises"),
        two
    );
}

#[test]
fn grp_40_samples_that_differ_by_their_stream_only_are_one_in_format_1() {
    let (bytes, ends) = body(&five());
    let at = |ms: u64| marks(&[(ends[0], 0), (ends[1], ms), (ends[4], ms)]);
    let mut report = Report::new(None);
    record_with(
        &mut report,
        Direction::Outbound,
        &request(),
        &bytes,
        &at(50),
    );
    record_with(
        &mut report,
        Direction::Outbound,
        &request(),
        &bytes,
        &at(400),
    );
    let full = EgressSet::project(&report).to_json().expect("serialises");
    assert_eq!(full.matches("\"head_ms\"").count(), 2);
    let one = EgressSet::project_as(&report, 1)
        .to_json()
        .expect("serialises");
    assert_eq!(one.matches("\"head_ms\"").count(), 1);
}

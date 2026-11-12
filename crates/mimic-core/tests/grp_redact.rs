//! GRP-12 to GRP-17 (#460): redaction of gRPC metadata and decoded messages.

use mimic_core::discovery::redact::Redactor;
use mimic_core::grpc::redact::{Direction, FieldNames, FieldRule, GrpcRedactor};
use mimic_core::grpc::wire::{Field, MAX_DEPTH, MAX_FIELDS, Message, Value, WireError};
use proptest::prelude::*;

const JWT: &str =
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CARD: &str = "4111111111111111";

fn f(number: u32, value: Value) -> Field {
    Field { number, value }
}
fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}
fn msg(fields: Vec<Field>) -> Message {
    Message { fields }
}
fn nested(fields: Vec<Field>) -> Value {
    Value::Message(msg(fields))
}
fn plain() -> GrpcRedactor {
    GrpcRedactor::new(Redactor::default(), Vec::new())
}
fn run(r: &GrpcRedactor, m: &Message) -> Message {
    r.message("p.S/M", Direction::Request, m, &FieldNames::new())
        .expect("redacts")
}
fn deep(levels: usize) -> Message {
    let mut m = msg(vec![f(1, Value::Varint(1))]);
    for _ in 1..levels {
        m = msg(vec![f(1, Value::Message(m))]);
    }
    m
}

#[test]
fn grp_12_metadata_names_and_bin_values() {
    let r = GrpcRedactor::new(Redactor::new(["x-tenant"]), Vec::new());
    assert_eq!(
        r.metadata("authorization", "Bearer abc"),
        "Bearer mimic-token"
    );
    assert_eq!(r.metadata("x-api-key", "abc"), "mimic-api-key");
    assert_eq!(r.metadata("x-request-id", "r-1"), "r-1");
    assert_eq!(r.metadata("user-agent", "grpc-go/1.5"), "grpc-go/1.5");
    assert_eq!(r.metadata("trace-bin", "AAEC"), "mimic-secret");
    assert_eq!(r.metadata("X-Note-Bin", "AAEC"), "mimic-secret");
    assert_eq!(r.metadata("authorization-bin", "AAEC"), "mimic-auth");
    assert_eq!(r.metadata("x-tenant", "acme"), "mimic-secret");
}

#[test]
fn grp_13_value_detectors_at_any_depth() {
    let pem = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----";
    let m = msg(vec![
        f(1, s(JWT)),
        f(
            2,
            nested(vec![f(1, s(pem)), f(2, nested(vec![f(1, s(CARD))]))]),
        ),
        f(3, s("ghp_abcdefghijklmnopqrstuvwxyz0123456789")),
        f(3, s("AKIAIOSFODNN7EXAMPLE")),
        f(3, s("fine")),
        f(4, Value::Varint(4_111_111_111_111_111)),
        f(5, Value::Varint(4_111_111_111_111_112)),
    ]);
    let out = run(&plain(), &m);
    let expected = msg(vec![
        f(1, s("mimic-jwt")),
        f(
            2,
            nested(vec![
                f(1, s("mimic-private-key")),
                f(2, nested(vec![f(1, s("mimic-card"))])),
            ]),
        ),
        f(3, s("mimic-token")),
        f(3, s("mimic-api-key")),
        f(3, s("fine")),
        f(4, s("mimic-card")),
        f(5, Value::Varint(4_111_111_111_111_112)),
    ]);
    assert_eq!(out, expected);
    let clean = msg(vec![
        f(1, s("hello")),
        f(2, Value::Fixed32(7)),
        f(3, Value::Bytes(vec![1, 2, 3])),
        f(4, nested(vec![f(1, Value::Varint(9))])),
    ]);
    assert_eq!(run(&plain(), &clean), clean);
}

fn distinct(n: u8) -> String {
    (0..n).map(|i| char::from(b'a' + i)).collect()
}

#[test]
fn grp_14_entropy_thresholds() {
    let m = msg(vec![
        f(1, s(&distinct(16))),
        f(2, s(&distinct(22))),
        f(3, s(&distinct(32))),
        f(4, s(&distinct(15))),
        f(5, s("the quick brown fox")),
        f(6, s("a quick brown fox jumps over the lazy dog")),
        f(7, s("ORDER_STATUS_PENDING")),
        f(8, s(&"a".repeat(4096))),
    ]);
    let out = run(&plain(), &m);
    for i in 0..3 {
        assert_eq!(out.fields[i].value, s("mimic-secret"));
    }
    for i in 3..8 {
        assert_eq!(out.fields[i].value, m.fields[i].value);
    }

    let all: Vec<u8> = (0..=255u8).collect();
    let m = msg(vec![
        f(1, Value::Bytes((0..16).collect())),
        f(2, Value::Bytes((0..32).collect())),
        f(3, Value::Bytes(all)),
        f(4, Value::Bytes(vec![9; 16])),
        f(5, Value::Bytes((0..15).collect())),
    ]);
    let out = run(&plain(), &m);
    for i in 0..3 {
        assert_eq!(out.fields[i].value, s("mimic-secret"));
    }
    for i in 3..5 {
        assert_eq!(out.fields[i].value, m.fields[i].value);
    }
}

#[test]
fn grp_13_detectors_on_bytes_and_fixed64() {
    let pem = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----";
    let cases: [(&str, &str); 3] = [
        (JWT, "mimic-jwt"),
        (CARD, "mimic-card"),
        (pem, "mimic-private-key"),
    ];
    for (text, placeholder) in cases {
        let bytes = text.as_bytes();
        let variants = [
            bytes.to_vec(),
            [&[0xff][..], bytes].concat(),
            [bytes, &[0xfe][..]].concat(),
            [&[0xc3][..], bytes, &[0x80, 0xff][..]].concat(),
        ];
        for v in variants {
            assert!(std::str::from_utf8(&v).is_err() || v == bytes);
            let out = run(&plain(), &msg(vec![f(1, Value::Bytes(v))]));
            assert_eq!(out.fields[0].value, s(placeholder));
        }
    }
    let kept = Value::Bytes([&[0xff][..], b"hello"].concat());
    assert_eq!(
        run(&plain(), &msg(vec![f(1, kept.clone())])).fields[0].value,
        kept
    );
    let out = run(
        &plain(),
        &msg(vec![
            f(1, Value::Fixed64(4_111_111_111_111_111)),
            f(2, Value::Fixed64(4_111_111_111_111_112)),
            f(3, Value::Fixed64(7)),
        ]),
    );
    assert_eq!(out.fields[0].value, s("mimic-card"));
    assert_eq!(out.fields[1].value, Value::Fixed64(4_111_111_111_111_112));
    assert_eq!(out.fields[2].value, Value::Fixed64(7));
}

fn rule(method: &str, direction: Direction, path: &str) -> FieldRule {
    FieldRule::new(method, direction, path).expect("a rule")
}

fn tree() -> Message {
    msg(vec![
        f(1, s("keep")),
        f(3, nested(vec![f(1, s("a")), f(1, s("b")), f(2, s("keep"))])),
        f(3, nested(vec![f(1, nested(vec![f(1, Value::Varint(1))]))])),
    ])
}

#[test]
fn grp_15_field_rules_by_number() {
    let r = GrpcRedactor::new(
        Redactor::default(),
        vec![rule("p.S/M", Direction::Request, "3.1")],
    );
    let out = r
        .message("p.S/M", Direction::Request, &tree(), &FieldNames::new())
        .expect("redacts");
    let expected = msg(vec![
        f(1, s("keep")),
        f(
            3,
            nested(vec![
                f(1, s("mimic-secret")),
                f(1, s("mimic-secret")),
                f(2, s("keep")),
            ]),
        ),
        f(3, nested(vec![f(1, s("mimic-secret"))])),
    ]);
    assert_eq!(out, expected);
    for (method, direction) in [
        ("p.S/Other", Direction::Request),
        ("p.S/M", Direction::Response),
    ] {
        let same = r
            .message(method, direction, &tree(), &FieldNames::new())
            .expect("redacts");
        assert_eq!(same, tree());
    }
    let none = GrpcRedactor::new(
        Redactor::default(),
        vec![
            rule("p.S/M", Direction::Request, "3.1.1.1"),
            rule("p.S/M", Direction::Request, "9"),
            rule("p.S/M", Direction::Request, "2.1"),
        ],
    );
    let same = none
        .message("p.S/M", Direction::Request, &tree(), &FieldNames::new())
        .expect("redacts");
    assert_eq!(same, tree());
}

#[test]
fn grp_15_field_rules_by_name_and_validation() {
    let mut names = FieldNames::new();
    names.insert(vec![3], "user".to_string());
    names.insert(vec![3, 2], "email".to_string());
    let r = GrpcRedactor::new(
        Redactor::default(),
        vec![rule("p.S/M", Direction::Request, "user.email")],
    );
    let m = msg(vec![f(3, nested(vec![f(2, s("a@b.c")), f(1, s("k"))]))]);
    let out = r
        .message("p.S/M", Direction::Request, &m, &names)
        .expect("redacts");
    assert_eq!(
        out,
        msg(vec![f(
            3,
            nested(vec![f(2, s("mimic-secret")), f(1, s("k"))])
        )])
    );
    let unnamed = r
        .message("p.S/M", Direction::Request, &m, &FieldNames::new())
        .expect("redacts");
    assert_eq!(unnamed, m);
    let sub = GrpcRedactor::new(
        Redactor::default(),
        vec![rule("p.S/M", Direction::Request, "3")],
    );
    let out = sub
        .message("p.S/M", Direction::Request, &m, &names)
        .expect("redacts");
    assert_eq!(out, msg(vec![f(3, s("mimic-secret"))]));
    for bad in ["", ".1", "1.", "1..2", "0", "536870912", "a b", "-1"] {
        assert!(FieldRule::new("p.S/M", Direction::Request, bad).is_err());
    }
    assert!(FieldRule::new("", Direction::Request, "1").is_err());
}

#[test]
fn grp_16_field_names_feed_the_name_rules() {
    let mut names = FieldNames::new();
    names.insert(vec![2], "password".to_string());
    names.insert(vec![3, 1], "refreshToken".to_string());
    names.insert(vec![4], "note".to_string());
    let m = msg(vec![
        f(2, s("x")),
        f(2, Value::Varint(12345)),
        f(2, Value::Bytes(vec![1, 2])),
        f(2, nested(vec![f(1, s("deep"))])),
        f(3, nested(vec![f(1, s("t")), f(2, s("u"))])),
        f(4, s("kept")),
    ]);
    let out = plain()
        .message("p.S/M", Direction::Response, &m, &names)
        .expect("redacts");
    let expected = msg(vec![
        f(2, s("mimic-password")),
        f(2, s("mimic-password")),
        f(2, s("mimic-password")),
        f(2, s("mimic-password")),
        f(3, nested(vec![f(1, s("mimic-token")), f(2, s("u"))])),
        f(4, s("kept")),
    ]);
    assert_eq!(out, expected);
    let unnamed = run(&plain(), &m);
    assert_eq!(unnamed, m);
}

#[test]
fn grp_17_bounds_are_errors_not_nulls() {
    let r = plain();
    let names = FieldNames::new();
    let call = |m: &Message| r.message("p.S/M", Direction::Request, m, &names);
    assert!(call(&deep(MAX_DEPTH)).is_ok());
    assert_eq!(call(&deep(MAX_DEPTH + 1)), Err(WireError::TooDeep));
    let huge = deep(10_000);
    assert_eq!(call(&huge), Err(WireError::TooDeep));
    // Dropped one level at a time: the test itself must not recurse on the drop.
    let mut m = huge;
    while let Some(Field {
        value: Value::Message(inner),
        ..
    }) = m.fields.pop()
    {
        m = inner;
    }
    let wide = msg((0..=MAX_FIELDS).map(|_| f(1, Value::Varint(1))).collect());
    assert_eq!(call(&wide), Err(WireError::TooManyFields));
    let at_bound = msg((0..MAX_FIELDS).map(|_| f(1, Value::Varint(1))).collect());
    assert!(call(&at_bound).is_ok());
}

#[test]
fn grp_17_idempotent_and_independent_of_the_secret() {
    let r = plain();
    let a = msg(vec![
        f(1, s("ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
        f(2, s(JWT)),
    ]);
    let b = msg(vec![
        f(
            1,
            s("ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ),
        f(2, s(JWT)),
    ]);
    let (ra, rb) = (run(&r, &a), run(&r, &b));
    assert_eq!(ra.encode().expect("encodes"), rb.encode().expect("encodes"));
    assert_eq!(run(&r, &ra), ra);
    assert!(ra.encode().is_ok());
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<u64>().prop_map(Value::Varint),
        any::<u32>().prop_map(Value::Fixed32),
        any::<u64>().prop_map(Value::Fixed64),
        ".{0,40}".prop_map(Value::Str),
        proptest::collection::vec(any::<u8>(), 0..300).prop_map(Value::Bytes),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        proptest::collection::vec((1u32..8, inner), 0..4).prop_map(|fields| {
            Value::Message(Message {
                fields: fields
                    .into_iter()
                    .map(|(number, value)| Field { number, value })
                    .collect(),
            })
        })
    })
}

proptest! {
    #[test]
    fn grp_17_random_messages(fields in proptest::collection::vec((1u32..8, value()), 0..6)) {
        let m = Message { fields: fields.into_iter().map(|(number, value)| Field { number, value }).collect() };
        let r = GrpcRedactor::new(Redactor::default(), vec![FieldRule::new("p.S/M", Direction::Request, "1.2").expect("a rule")]);
        let names = FieldNames::new();
        if let Ok(out) = r.message("p.S/M", Direction::Request, &m, &names) {
            prop_assert_eq!(r.message("p.S/M", Direction::Request, &out, &names), Ok(out.clone()));
            prop_assert!(out.encode().is_ok() || m.encode().is_err());
        }
    }
}

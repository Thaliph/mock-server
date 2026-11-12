//! GRP-04 to GRP-08 (#459): the schema-less wire-format reader, its bounds, the canonical JSON
//! and the round trip.

use mimic_core::grpc::wire::{
    Field, MAX_DEPTH, MAX_FIELD_NUMBER, MAX_FIELDS, MAX_PAYLOAD, Message, Value, WireError,
};
use proptest::prelude::*;
use serde_json::json;

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn tag(number: u32, wire: u8) -> Vec<u8> {
    varint(u64::from(number) << 3 | u64::from(wire))
}

fn var(number: u32, v: u64) -> Vec<u8> {
    [tag(number, 0), varint(v)].concat()
}

fn len(number: u32, payload: &[u8]) -> Vec<u8> {
    [
        tag(number, 2),
        varint(payload.len() as u64),
        payload.to_vec(),
    ]
    .concat()
}

fn decode_json(bytes: &[u8]) -> serde_json::Value {
    Message::decode(bytes).expect("decodes").to_json()
}

fn only(bytes: &[u8]) -> Value {
    let mut fields = Message::decode(bytes).expect("decodes").fields;
    assert_eq!(fields.len(), 1);
    fields.remove(0).value
}

#[test]
fn grp_04_every_wire_type_is_read() {
    for v in [0u64, 1, 300, u64::MAX] {
        assert_eq!(only(&var(1, v)), Value::Varint(v));
    }
    assert_eq!(varint(u64::MAX).len(), 10);
    let fixed32 = [tag(2, 5), vec![1, 2, 3, 4]].concat();
    assert_eq!(only(&fixed32), Value::Fixed32(0x0403_0201));
    let fixed64 = [tag(3, 1), vec![1, 0, 0, 0, 0, 0, 0, 0x80]].concat();
    assert_eq!(only(&fixed64), Value::Fixed64(0x8000_0000_0000_0001));
    assert_eq!(only(&len(4, b"abc")), Value::Str("abc".into()));
    let m = Message::decode(&[var(1, 7), len(2, b"x")].concat()).expect("decodes");
    assert_eq!(m.fields.len(), 2);
    assert_eq!(m.fields[0].number, 1);
    assert_eq!(m.fields[1].number, 2);
}

#[test]
fn grp_04_field_numbers_are_bounded() {
    assert_eq!(MAX_FIELD_NUMBER, (1 << 29) - 1);
    let m = Message::decode(&var(MAX_FIELD_NUMBER, 1)).expect("decodes");
    assert_eq!(m.fields[0].number, MAX_FIELD_NUMBER);
    assert!(Message::decode(&var(0, 1)).is_err());
    assert!(Message::decode(&var(MAX_FIELD_NUMBER + 1, 1)).is_err());
    // A tag that does not fit 32 bits.
    assert!(Message::decode(&[varint(1 << 40), vec![0]].concat()).is_err());
}

#[test]
fn grp_04_malformed_values_are_errors() {
    let overlong = [tag(1, 0), vec![0xff; 10], vec![0x01]].concat();
    assert_eq!(Message::decode(&overlong), Err(WireError::BadVarint));
    let overflow = [tag(1, 0), vec![0xff; 9], vec![0x02]].concat();
    assert_eq!(Message::decode(&overflow), Err(WireError::BadVarint));
    let truncated = [
        [tag(1, 0), vec![0x80]].concat(),
        [tag(1, 1), vec![0; 7]].concat(),
        [tag(1, 5), vec![0; 3]].concat(),
        [tag(1, 2), varint(5), b"abc".to_vec()].concat(),
        vec![0x80],
    ];
    for (i, bytes) in truncated.iter().enumerate() {
        assert!(Message::decode(bytes).is_err(), "truncated case {i}");
    }
    // A length beyond the payload and beyond u32.
    assert!(Message::decode(&[tag(1, 2), varint(u64::MAX)].concat()).is_err());
    // Groups, and the unassigned wire types 6 and 7.
    for wire in [3u8, 4, 6, 7] {
        assert!(Message::decode(&tag(1, wire)).is_err(), "wire {wire}");
    }
    assert_eq!(
        Message::decode(&[tag(1, 3), tag(1, 4)].concat()),
        Err(WireError::Group)
    );
}

#[test]
fn grp_05_empty_value_is_an_empty_string() {
    assert_eq!(only(&len(1, b"")), Value::Str(String::new()));
    assert_eq!(decode_json(&len(1, b"")), json!({"1": ""}));
}

#[test]
fn grp_05_a_payload_that_parses_is_a_message_even_when_utf8() {
    // 0x08 0x01 is valid UTF-8 and the field 1 = 1.
    let value = only(&len(1, &[0x08, 0x01]));
    assert_eq!(
        value,
        Value::Message(Message {
            fields: vec![Field {
                number: 1,
                value: Value::Varint(1)
            }]
        })
    );
    assert_eq!(decode_json(&len(2, &var(1, 5))), json!({"2": {"1": 5}}));
}

#[test]
fn grp_05_text_that_is_not_a_message_is_a_string() {
    // 'h' = tag of field 13, wire 0 (fine), then 'e' = 0x65: varint 101; 'l' = tag 13 wire 4,
    // a group: not a message.
    assert_eq!(only(&len(1, b"hello")), Value::Str("hello".into()));
    assert_eq!(
        only(&len(1, "héllo".as_bytes())),
        Value::Str("héllo".into())
    );
}

#[test]
fn grp_05_invalid_utf8_that_does_not_parse_is_bytes() {
    assert_eq!(
        only(&len(1, &[0xff, 0xfe, 0xfd])),
        Value::Bytes(vec![0xff, 0xfe, 0xfd])
    );
    // A group tag, and a truncated field, are not messages.
    assert_eq!(only(&len(1, &[0x0b, 0xff])), Value::Bytes(vec![0x0b, 0xff]));
    assert_eq!(
        only(&len(1, &[0x0a, 0x05, 0xff])),
        Value::Bytes(vec![0x0a, 0x05, 0xff])
    );
}

#[test]
fn grp_05_a_repeated_field_renders_as_an_array() {
    let bytes = [var(1, 1), var(1, 2), len(1, b"a")].concat();
    assert_eq!(Message::decode(&bytes).expect("decodes").fields.len(), 3);
    assert_eq!(decode_json(&bytes), json!({"1": [1, 2, "a"]}));
}

fn nested(levels: usize, innermost: &[u8]) -> Vec<u8> {
    let mut bytes = innermost.to_vec();
    for _ in 0..levels {
        bytes = len(1, &bytes);
    }
    bytes
}

fn depth_of(message: &Message) -> usize {
    message
        .fields
        .iter()
        .map(|f| match &f.value {
            Value::Message(m) => 1 + depth_of(m),
            _ => 1,
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn grp_06_nesting_stops_at_depth_32() {
    assert_eq!(MAX_DEPTH, 32);
    // The top-level message is level 1: 31 wrappers around a varint field make 32 messages.
    let at_limit = Message::decode(&nested(MAX_DEPTH - 1, &var(2, 9))).expect("decodes");
    let mut level = 1;
    let mut cursor = &at_limit;
    while let Some(Field {
        value: Value::Message(inner),
        ..
    }) = cursor.fields.first()
    {
        level += 1;
        cursor = inner;
    }
    assert_eq!(level, MAX_DEPTH);
    assert_eq!(cursor.fields[0].value, Value::Varint(9));
    // One more wrapper: the deepest payload is no longer read as a message.
    let beyond = Message::decode(&nested(MAX_DEPTH, &var(2, 9))).expect("decodes");
    let mut cursor = &beyond;
    let mut level = 1;
    while let Some(Field {
        value: Value::Message(inner),
        ..
    }) = cursor.fields.first()
    {
        level += 1;
        cursor = inner;
    }
    assert_eq!(level, MAX_DEPTH);
    assert!(matches!(
        cursor.fields[0].value,
        Value::Str(_) | Value::Bytes(_)
    ));
}

#[test]
fn grp_06_a_very_deep_input_does_not_overflow_the_stack() {
    // As many wrappers as 256 KiB hold (a few bytes each): thousands of levels.
    let mut bytes = var(2, 1);
    loop {
        let next = len(1, &bytes);
        if next.len() > MAX_PAYLOAD {
            break;
        }
        bytes = next;
    }
    let message = Message::decode(&bytes).expect("decodes");
    assert!(depth_of(&message) <= MAX_DEPTH + 1);
    // Groups opening 100 000 deep.
    let groups = tag(1, 3).repeat(100_000);
    assert!(Message::decode(&groups).is_err());
}

#[test]
fn grp_06_4096_fields_are_read_and_4097_are_an_error() {
    assert_eq!(MAX_FIELDS, 4096);
    let many = |n: usize| var(1, 1).repeat(n);
    assert_eq!(
        Message::decode(&many(MAX_FIELDS))
            .expect("decodes")
            .fields
            .len(),
        MAX_FIELDS
    );
    assert_eq!(
        Message::decode(&many(MAX_FIELDS + 1)),
        Err(WireError::TooManyFields)
    );
    // The budget is shared by the tree: 4095 fields + a nested payload of 2 fields is over,
    // so the payload stays a string.
    let tree = [many(MAX_FIELDS - 1), len(2, &many(2))].concat();
    let message = Message::decode(&tree).expect("decodes");
    assert_eq!(message.fields.len(), MAX_FIELDS);
    assert!(matches!(
        message.fields[MAX_FIELDS - 1].value,
        Value::Str(_)
    ));
    let fits = [many(MAX_FIELDS - 3), len(2, &many(2))].concat();
    let message = Message::decode(&fits).expect("decodes");
    assert!(matches!(
        message.fields[MAX_FIELDS - 3].value,
        Value::Message(_)
    ));
    // A nested payload that alone exceeds the budget stays bytes.
    let big = len(1, &many(MAX_FIELDS + 1));
    assert!(matches!(only(&big), Value::Str(_) | Value::Bytes(_)));
}

#[test]
fn grp_06_256_kib_is_read_and_one_more_byte_is_an_error() {
    assert_eq!(MAX_PAYLOAD, 256 * 1024);
    let exact = len(1, &vec![b'a'; MAX_PAYLOAD - 4]);
    assert_eq!(exact.len(), MAX_PAYLOAD);
    assert!(Message::decode(&exact).is_ok());
    let over = len(1, &vec![b'a'; MAX_PAYLOAD - 3]);
    assert_eq!(over.len(), MAX_PAYLOAD + 1);
    assert_eq!(Message::decode(&over), Err(WireError::TooLarge));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]
    #[test]
    fn grp_06_no_input_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(m) = Message::decode(&bytes) {
            let text = m.to_json_string();
            prop_assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
            prop_assert!(m.encode().is_ok());
        }
    }
}

#[test]
fn grp_07_canonical_json_keys_fields_by_number() {
    let bytes = [len(1, b"abc"), len(2, &var(1, 5))].concat();
    let message = Message::decode(&bytes).expect("decodes");
    assert_eq!(message.to_json_string(), r#"{"1":"abc","2":{"1":5}}"#);
    assert_eq!(message.to_json(), json!({"1": "abc", "2": {"1": 5}}));
}

#[test]
fn grp_07_other_types_are_tagged_and_keys_are_in_numeric_order() {
    let bytes = [
        var(10, 1),
        var(2, u64::MAX),
        [tag(3, 5), vec![1, 0, 0, 0]].concat(),
        [tag(4, 1), vec![2, 0, 0, 0, 0, 0, 0, 0]].concat(),
        len(5, &[0xff, 0x00]),
        len(6, b"\xc3\xa9\"\n"),
    ]
    .concat();
    let message = Message::decode(&bytes).expect("decodes");
    assert_eq!(
        message.to_json_string(),
        concat!(
            r#"{"2":18446744073709551615,"3":{"$fixed32":1},"4":{"$fixed64":2},"#,
            r#""5":{"$bytes":"/wA="},"6":"é\"\n","10":1}"#
        )
    );
    assert_eq!(message.to_json_string(), message.to_json_string());
    assert_eq!(Message::default().to_json_string(), "{}");
}

#[test]
fn grp_08_round_trip_of_a_canonical_encoding() {
    let bytes = [
        var(1, 150),
        len(2, b"text"),
        len(3, &[var(1, 1), len(2, &[0xff, 0xfe])].concat()),
        [tag(4, 5), vec![9, 8, 7, 6]].concat(),
        [tag(5, 1), vec![1; 8]].concat(),
        var(6, 1),
        var(6, 2),
        len(7, b""),
    ]
    .concat();
    let message = Message::decode(&bytes).expect("decodes");
    let back = Message::from_json(&message.to_json()).expect("reads");
    assert_eq!(back.encode().expect("encodes"), bytes);
    assert_eq!(message.encode().expect("encodes"), bytes);
}

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u64>().prop_map(Value::Varint),
        any::<u32>().prop_map(Value::Fixed32),
        any::<u64>().prop_map(Value::Fixed64),
        // A leading 0x0b is a group tag: text is never read as a message (which could reorder).
        "[a-zA-Z0-9 é]{0,24}"
            .prop_map(|s| { Value::Str(if s.is_empty() { s } else { format!("\u{b}{s}") }) }),
        // A leading 0xff is invalid UTF-8 and a tag of wire type 7: never read as a message.
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(|mut b| {
            b.insert(0, 0xff);
            Value::Bytes(b)
        }),
    ]
}

fn message(depth: u32) -> BoxedStrategy<Message> {
    let value = if depth == 0 {
        leaf().boxed()
    } else {
        prop_oneof![3 => leaf(), 1 => message(depth - 1).prop_map(Value::Message)].boxed()
    };
    proptest::collection::vec((1u32..40, value), 0..6)
        .prop_map(|mut fields| {
            fields.sort_by_key(|(n, _)| *n);
            Message {
                fields: fields
                    .into_iter()
                    .map(|(number, value)| Field { number, value })
                    .collect(),
            }
        })
        .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]
    #[test]
    fn grp_08_round_trip_of_random_trees(tree in message(3)) {
        let bytes = tree.encode().expect("encodes");
        let decoded = Message::decode(&bytes).expect("decodes");
        let json = decoded.to_json();
        let back = Message::from_json(&json).expect("reads");
        prop_assert_eq!(back.encode().expect("encodes"), bytes.clone());
        // The text form reads back to the same value.
        let text: serde_json::Value = serde_json::from_str(&decoded.to_json_string()).expect("json");
        prop_assert_eq!(text, json);
    }
}

#[test]
fn grp_08_from_json_refuses_what_the_decoder_never_writes() {
    let bad = [
        json!(1),
        json!("a"),
        json!(null),
        json!([1]),
        json!({"0": 1}),
        json!({"01": 1}),
        json!({"a": 1}),
        json!({"536870912": 1}),
        json!({"1": -1}),
        json!({"1": 1.5}),
        json!({"1": true}),
        json!({"1": null}),
        json!({"1": []}),
        json!({"1": [[1]]}),
        json!({"1": {"$other": 1}}),
        json!({"1": {"$fixed32": 4294967296u64}}),
        json!({"1": {"$fixed32": 1, "2": 1}}),
        json!({"1": {"$bytes": "!!"}}),
        json!({"1": {"$bytes": 5}}),
    ];
    for (i, json) in bad.iter().enumerate() {
        assert!(Message::from_json(json).is_err(), "bad case {i}");
    }
}

#[test]
fn grp_08_from_json_bounds_depth_and_size() {
    let mut deep = json!({"2": 1});
    for _ in 0..MAX_DEPTH - 1 {
        deep = json!({ "1": deep });
    }
    assert!(Message::from_json(&deep).is_ok());
    let deeper = json!({ "1": deep });
    assert!(Message::from_json(&deeper).is_err());
    let too_big = json!({"1": "a".repeat(MAX_PAYLOAD)});
    let message = Message::from_json(&too_big).expect("reads");
    assert_eq!(message.encode(), Err(WireError::TooLarge));
}

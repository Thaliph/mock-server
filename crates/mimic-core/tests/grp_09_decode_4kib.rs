//! GRP-09 (#459): a recorded 4 KiB message holding every wire type decodes to its expected
//! JSON with bounded allocation, and the JSON re-encodes to the message.

use mimic_core::grpc::wire::Message;
use serde_json::json;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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

fn var(number: u64, v: u64) -> Vec<u8> {
    [varint(number << 3), varint(v)].concat()
}

fn len(number: u64, payload: &[u8]) -> Vec<u8> {
    [
        varint(number << 3 | 2),
        varint(payload.len() as u64),
        payload.to_vec(),
    ]
    .concat()
}

/// The recorded message: 4096 bytes, fields 1 to 7.
fn recorded() -> (Vec<u8>, serde_json::Value) {
    let nested = [var(1, 1), len(2, b"nested"), {
        let mut f = vec![3 << 3 | 5];
        f.extend(7u32.to_le_bytes());
        f
    }]
    .concat();
    let head = [
        var(1, 150),
        len(2, b"hello grpc"),
        len(3, &nested),
        {
            let mut f = vec![4 << 3 | 1];
            f.extend(1u64.to_le_bytes());
            f
        },
        len(5, &[0xff, 0xfe, 0x00]),
        var(6, 1),
        var(6, 2),
        var(6, 3),
    ]
    .concat();
    // Field 7 fills the rest: one tag byte, a two-byte length.
    let pad = 4096 - head.len() - 3;
    let text = "lorem ipsum dolor sit amet ".repeat(pad / 27 + 1)[..pad].to_string();
    let bytes = [head, len(7, text.as_bytes())].concat();
    let expected = json!({
        "1": 150,
        "2": "hello grpc",
        "3": {"1": 1, "2": "nested", "3": {"$fixed32": 7}},
        "4": {"$fixed64": 1},
        "5": {"$bytes": "//4A"},
        "6": [1, 2, 3],
        "7": text,
    });
    (bytes, expected)
}

#[test]
fn grp_09_a_4_kib_message_decodes_to_its_json_with_bounded_allocation() {
    let (bytes, expected) = recorded();
    assert_eq!(bytes.len(), 4096);
    let _profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let message = Message::decode(&bytes).expect("decodes");
    let after = dhat::HeapStats::get();
    let allocated = after.total_bytes - before.total_bytes;
    assert!(
        allocated < 8 * 4096,
        "decoding allocated {allocated} bytes for 4096"
    );
    assert_eq!(message.to_json(), expected);
    let back = Message::from_json(&expected).expect("reads");
    assert_eq!(back.encode().expect("encodes"), bytes);
}

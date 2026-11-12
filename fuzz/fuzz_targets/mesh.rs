//! Fuzz target for the mesh view reader (spec MSH-13, §7.9).
//!
//! The input is read as a view document, as it is, and as the seed view with writes over its
//! bytes (every three bytes: two of position, one of value). `View::from_json` never panics
//! and its errors stay short; a document it accepts holds the bounds, writes canonical JSON
//! that reads back to the same view and the same bytes (a fixed point), with a stable ETag.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::mesh::view::{Limits, View, etag};

const SEED: &str = include_str!("../seeds/mesh/view");

fn check(text: &str, limits: &Limits) {
    match View::from_json(text, limits) {
        Err(e) => assert!(e.len() <= 200, "an error message is short"),
        Ok(view) => {
            assert!(view.services.len() <= limits.max_services);
            assert!(view.endpoint_count() <= limits.max_endpoints);
            let json = view.to_json();
            let back = View::from_json(&json, &Limits {
                max_view_bytes: json.len().max(limits.max_view_bytes),
                ..limits.clone()
            })
            .expect("a view reads back");
            assert_eq!(back, view);
            assert_eq!(back.to_json(), json);
            assert_eq!(etag(&json), etag(&back.to_json()));
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let limits = Limits::default();
    check(&String::from_utf8_lossy(data), &limits);
    let small = Limits {
        max_services: 2,
        max_endpoints: 3,
        max_view_bytes: 4096,
    };
    check(&String::from_utf8_lossy(data), &small);
    let mut bytes = SEED.as_bytes().to_vec();
    for write in data.chunks_exact(3) {
        if let Some(at) = usize::from(write[0])
            .checked_mul(256)
            .and_then(|n| n.checked_add(usize::from(write[1])))
            .and_then(|n| n.checked_rem(bytes.len()))
        {
            bytes[at] = write[2];
        }
    }
    check(&String::from_utf8_lossy(&bytes), &limits);
    check(&String::from_utf8_lossy(&bytes), &small);
    check(SEED, &limits);
});

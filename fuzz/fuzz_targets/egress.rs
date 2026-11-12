//! Fuzz target for the egress set (spec PUL-20, §7.7).
//!
//! The input is read three ways:
//!
//! 1. as text, by `EgressSet::from_json`: arbitrary bytes never panic, and a set it accepts
//!    serialises and reads back byte for byte;
//! 2. as text, by the controller's report path (`Report::parse_checked`, the CTL-22 check);
//! 3. as writes over the JSON of a valid report (every three bytes: two of position, one of
//!    value), which goes through the same path.
//!
//! A report that passes the §7.3 check is projected: the projection passes the egress set
//! check and reads back byte for byte, whatever the report holds.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::report::Report;

/// Endpoints per direction and authorities accepted from a push, as in the discovery target.
const PUSH: usize = 16;
const PUSH_AUTHORITIES: usize = 16;

/// A recorded report: an inbound and an outbound scope with samples, volatility and a tunnel.
const SEED: &str = include_str!("../seeds/egress/report");

fn read_back(set: &EgressSet) {
    let json = set.to_json().expect("a set serialises");
    match EgressSet::from_json(&json) {
        Ok(back) => assert_eq!(back.to_json().expect("a set serialises"), json),
        Err(e) => panic!("a set fails its check: {}", e.reason()),
    }
}

/// What the controller does with a stored report, then the projection of it.
fn project(text: &str) {
    let Ok(report) = Report::parse_checked(text, PUSH, PUSH_AUTHORITIES) else {
        return;
    };
    let set = EgressSet::project(&report);
    read_back(&set);
    assert_eq!(set, EgressSet::project(&report.clone()));
    // Format 1, what a sidecar up to F13 reads: it has no gRPC part, and passes its check.
    read_back(&EgressSet::project_as(&report, 1));
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(set) = EgressSet::from_json(&text) {
        read_back(&set);
    }
    project(&text);
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
    project(&String::from_utf8_lossy(&bytes));
    project(SEED);
});

//! Fuzz target for the discovery recorder (spec DSC-23, bounds of DSC-11, algebra of DSC-09).
//!
//! The input is split on NUL bytes into fields; every eight fields are one exchange:
//!
//! 1. a control field: status, status mode, flags (method, `grpc`, host present), port and
//!    latency, one byte each;
//! 2. the path;
//! 3. the query;
//! 4. the request headers, one `name: value` per line;
//! 5. the request body;
//! 6. the response headers, as above;
//! 7. the response body;
//! 8. the host.
//!
//! Exchanges are recorded alternately into two reports, which are then merged. The recorder
//! must not panic or hang, the endpoint limit and the sample budget hold, the serialised report
//! round-trips, and no custom-listed canary survives redaction. Content decoding is not part
//! of the pure recorder, bodies arrive decoded.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, merge};

const LIMITS: Limits = Limits {
    max_literals: 4,
    max_endpoints: 16,
    sample_budget: 256 * 1024,
};
/// Appears in every exchange under a listed name: it must never reach the report.
const CANARY: &str = "FUZZCANARYVALUE";

fn headers(field: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(field)
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn borrowed(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let redactor = Redactor::new(["fuzzsecret"]);
    let fields: Vec<&[u8]> = data.split(|b| *b == 0).collect();
    let mut reports = [Report::new(Some("a".into())), Report::new(Some("b".into()))];
    for (i, e) in fields.chunks(8).enumerate() {
        let control = e.first().copied().unwrap_or_default();
        let get = |n: usize| e.get(n).copied().unwrap_or_default();
        let byte = |n: usize, default: u8| control.get(n).copied().unwrap_or(default);
        // Common statuses, and any status in 100..=999 when the second byte asks for it.
        let status = if byte(1, 0) % 4 == 0 {
            100 + u16::from(byte(0, 0)) * 3
        } else {
            [200, 201, 204, 301, 400, 401, 404, 405, 500, 503][usize::from(byte(0, 0) % 10)]
        };
        let flags = byte(2, 0);
        let method = ["GET", "POST", "PUT", "DELETE"][usize::from(flags % 4)];
        let path = String::from_utf8_lossy(get(1)).into_owned();
        let query = format!("{}&fuzzsecret={CANARY}", String::from_utf8_lossy(get(2)));
        let mut req_headers = headers(get(3));
        req_headers.push(("x-fuzzsecret".into(), CANARY.into()));
        let resp_headers = headers(get(5));
        let host = String::from_utf8_lossy(get(7)).into_owned();
        let req = borrowed(&req_headers);
        let resp = borrowed(&resp_headers);
        let report = &mut reports[i % 2];
        report.record(
            &LIMITS,
            &redactor,
            &Exchange {
                direction: Direction::Inbound,
                port: u16::from(byte(3, 80)),
                method,
                path: &path,
                status,
                head_ms: u64::from(byte(4, 1)) << (flags % 24),
                host: (flags & 0x80 != 0).then_some(host.as_str()),
                grpc: flags & 0x40 != 0,
                observation: Observation {
                    query: &query,
                    req_headers: &req,
                    req_body: Some(get(4)),
                    resp_headers: &resp,
                    resp_body: Some(get(6)),
                },
            },
        );
        assert!(report.endpoints().len() <= LIMITS.max_endpoints);
        assert!(report.sample_bytes() <= LIMITS.sample_budget);
    }
    let [a, b] = reports;
    let merged = merge(a, b, &LIMITS);
    let json = merged.to_json();
    assert!(
        !json.contains(CANARY),
        "a canary under a listed name leaked"
    );
    match Report::from_json(&json) {
        Ok(back) => assert_eq!(back, merged),
        Err(e) => panic!("report does not round-trip: {e}"),
    }
});

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
//! Outbound exchanges (spec EGR-20): a flag of the control field makes the exchange outbound.
//! Its authority comes from the host field (the target), the SNI from the first line of the
//! query, the destination from the control bytes, and one more flag counts a tunnel whose kind
//! is a known one or the path (an arbitrary string). The request carries a canary under a name
//! outside the allowlist as well.
//!
//! Exchanges are recorded alternately into two reports, which are then merged. The recorder
//! must not panic or hang, the endpoint limit and the sample budget hold, the serialised report
//! round-trips, and no custom-listed canary survives redaction. Content decoding is not part
//! of the pure recorder, bodies arrive decoded.
//!
//! The controller side (spec CTL-22): the input as text, and the recorded report with bytes
//! of the input written over its JSON, go through `Report::from_json`, the structural check,
//! re-redaction and the bounded merge with a fixed valid report. Nothing panics, and every
//! merge result passes the check.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use mimic_core::discovery::report::{
    Direction, Exchange, Limits, Report, TUNNEL_KINDS, Via, merge,
};
use mimic_core::discovery::scope::{ScopeKey, authority_key};
use mimic_core::discovery::store::merge_bounded;

const LIMITS: Limits = Limits {
    max_literals: 4,
    max_endpoints: 16,
    max_authorities: 4,
    sample_budget: 256 * 1024,
    max_authorities: 8,
};
/// Endpoints per direction accepted from a push.
const PUSH: usize = 16;
/// Appears in every exchange under a listed name: it must never reach the report.
const CANARY: &str = "FUZZCANARYVALUE";

/// Everything the controller does with a pushed body, against a fixed valid report.
fn push(stored: &Report, text: &str) {
    let _ = Report::from_json(text);
    let Ok(mut pushed) = Report::parse_checked(text, PUSH) else {
        return;
    };
    pushed.re_redact(&Redactor::new(["fuzzsecret"]));
    pushed.stamp(20_000);
    let bounded = merge_bounded(stored.clone(), pushed, &LIMITS);
    assert!(
        bounded.report.check(LIMITS.max_endpoints).is_ok(),
        "a merge result fails the structural check"
    );
}

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
        let outbound = flags & 0x20 != 0;
        if outbound {
            // Outside the allowlist: redacted in an outbound request.
            req_headers.push(("x-fuzzcustom".into(), CANARY.into()));
        }
        let resp_headers = headers(get(5));
        let host = String::from_utf8_lossy(get(7)).into_owned();
        let req = borrowed(&req_headers);
        let resp = borrowed(&resp_headers);
        let report = &mut reports[i % 2];
        let target = host.as_str();
        let sni = String::from_utf8_lossy(get(2))
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let dst = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, byte(5, 0), byte(6, 0), byte(7, 1))),
            u16::from(byte(8, 80)) * 257,
        );
        let key = authority_key(
            Some(target),
            if flags & 0x08 != 0 { 443 } else { 80 },
            (flags & 0x04 != 0).then_some(sni.as_str()),
            dst,
        );
        assert!(
            ScopeKey::parse(&key).is_some(),
            "an authority key breaks its grammar"
        );
        if outbound && flags & 0x10 != 0 {
            let kind = if byte(9, 0) % 2 == 0 {
                TUNNEL_KINDS[usize::from(byte(9, 0)) % TUNNEL_KINDS.len()].to_string()
            } else {
                path.clone()
            };
            report.record_tunnel(&LIMITS, ScopeKey::Authority(key.clone()), &kind);
        }
        let exchange = Exchange {
            direction: if outbound {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
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
        };
        if outbound {
            let via = Via {
                tls: flags & 0x04 != 0,
                sni: Some(sni.as_str()),
                dst: Some(dst),
            };
            report.record_at(
                ScopeKey::Authority(key),
                &via,
                &LIMITS,
                &redactor,
                &exchange,
            );
        } else {
            report.record(&LIMITS, &redactor, &exchange);
        }
        for direction in [Direction::Inbound, Direction::Outbound] {
            assert!(
                report
                    .endpoints()
                    .iter()
                    .filter(|e| e.direction == direction)
                    .count()
                    <= LIMITS.max_endpoints
            );
        }
        assert!(report.sample_bytes() <= LIMITS.sample_budget);
    }
    let [a, b] = reports;
    let merged = merge(a, b, &LIMITS);
    let json = merged.to_json().expect("a report serialises");
    push(&merged, &String::from_utf8_lossy(data));
    let mut bytes = json.clone().into_bytes();
    for write in data.chunks_exact(3) {
        if let Some(at) = usize::from(write[0])
            .checked_mul(256)
            .and_then(|n| n.checked_add(usize::from(write[1])))
            .and_then(|n| n.checked_rem(bytes.len()))
        {
            bytes[at] = write[2];
        }
    }
    push(&merged, &String::from_utf8_lossy(&bytes));
    push(&merged, &json);
    assert!(
        !json.contains(CANARY),
        "a canary under a listed name leaked"
    );
    match Report::from_json(&json) {
        Ok(back) => assert_eq!(back, merged),
        Err(e) => panic!("report does not round-trip: {e}"),
    }
});

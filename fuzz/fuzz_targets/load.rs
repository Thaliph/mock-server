//! Fuzz target for the load plan and the request builder (spec LDT-18, §7.8).
//!
//! The input is read two ways: as a report (text, then the seed report with writes over its
//! bytes, every three bytes: two of position, one of value), and as the spec (its first bytes
//! pick the credentials, the limits and the seeds). A report that passes `Report::from_json`
//! is planned; the plan keeps its size bound and reads back, and every request built from it
//! is well formed (the LDT-02 property): no marker, no hop-by-hop header, one `host`, one
//! `mimic-load-run`, a `content-length` that matches the body.

#![no_main]

use std::collections::{BTreeMap, BTreeSet};

use libfuzzer_sys::fuzz_target;
use mimic_core::discovery::report::Report;
use mimic_core::load::plan::{CredentialField, MIN_PLAN, Plan, Spec, plan};
use mimic_core::load::request::{Context, Request, request};

/// A recorded report with an inbound scope, samples and volatility.
const SEED: &str = include_str!("../seeds/load/report");
const MARKER: &str = "<redacted>";
const FIELDS: [&str; 6] = [
    "header/authorization",
    "header/cookie",
    "query/api_key",
    "query/token",
    "body/card/token",
    "body",
];

fn well_formed(r: &Request) -> bool {
    let forbidden = |n: &str| {
        matches!(
            n,
            "content-encoding"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "expect"
        ) || n.starts_with("proxy-")
    };
    let count = |name: &str| r.headers.iter().filter(|(n, _)| n == name).count();
    !r.target.contains(MARKER)
        && !r
            .headers
            .iter()
            .any(|(n, v)| n.contains(MARKER) || v.contains(MARKER) || forbidden(n))
        && !String::from_utf8_lossy(&r.body).contains(MARKER)
        && count("mimic-load-run") == 1
        && count("host") == 1
        && r.headers
            .iter()
            .filter(|(n, _)| n == "content-length")
            .all(|(_, l)| l.parse::<usize>() == Ok(r.body.len()))
}

fn load(text: &str, knobs: &[u8]) {
    let Ok(report) = Report::from_json(text) else {
        return;
    };
    let byte = |i: usize| usize::from(knobs.get(i).copied().unwrap_or(0));
    let credentials: Vec<CredentialField> = FIELDS
        .iter()
        .enumerate()
        .filter(|(i, _)| byte(0) >> i & 1 == 1)
        .map(|(_, f)| CredentialField {
            field: (*f).to_string(),
            key: "k".to_string(),
        })
        .collect();
    let spec = Spec {
        include: None,
        exclude: BTreeSet::new(),
        credentials,
        max_endpoints: 1 + byte(1) % 70,
        max_plan: 64 + byte(2) * 256,
    };
    let planned = plan(&report, &spec);
    let json = planned.plan.to_json().expect("a plan serialises");
    // `plan` raises a limit below `MIN_PLAN` to it, so every plan reads back.
    let limit = spec.max_plan.max(MIN_PLAN);
    assert!(json.len() <= limit);
    assert!(planned.plan.endpoints.len() <= spec.max_endpoints);
    let back = Plan::from_json(&json, limit).expect("a plan reads back");
    assert_eq!(back, planned.plan);
    let secrets: BTreeMap<String, String> = [("k".to_string(), "secret value".to_string())].into();
    let ctx = Context {
        marker: "v1.payload.signature",
        ip: std::net::IpAddr::from([10, 0, 0, 1]),
        now_ms: u64::from(u32::from_le_bytes([
            knobs.get(3).copied().unwrap_or(0),
            knobs.get(4).copied().unwrap_or(0),
            knobs.get(5).copied().unwrap_or(0),
            knobs.get(6).copied().unwrap_or(0),
        ])) * 1000,
        secrets: &secrets,
    };
    for seed in 0..8 {
        if let Ok(r) = request(&planned.plan, &ctx, seed) {
            assert!(well_formed(&r), "a request breaks the LDT-02 property");
            assert_eq!(request(&planned.plan, &ctx, seed), Ok(r));
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let knobs = data.get(..8).unwrap_or(data);
    let rest = data.get(8..).unwrap_or_default();
    load(&String::from_utf8_lossy(rest), knobs);
    let mut bytes = SEED.as_bytes().to_vec();
    for write in rest.chunks_exact(3) {
        if let Some(at) = usize::from(write[0])
            .checked_mul(256)
            .and_then(|n| n.checked_add(usize::from(write[1])))
            .and_then(|n| n.checked_rem(bytes.len()))
        {
            bytes[at] = write[2];
        }
    }
    load(&String::from_utf8_lossy(&bytes), knobs);
    load(SEED, knobs);
});

//! SHR-21 (#339): the golden corpus. Reports built by the recorder and the bounded merge from
//! fixed seeds, inbound and outbound, below and at the store limits, are reproduced byte for
//! byte: `to_json`, both split parts, the §7.6 documents, the §7.5 index and endpoint list, and
//! the §7.7 egress projection and digest. The files were written before the first change of
//! #339; `MIMIC_BLESS=1` rewrites them and must not be used to make a change pass.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{build, limits, ops};
use mimic_core::discovery::egress::{EgressSet, digest};
use mimic_core::discovery::openapi;
use mimic_core::discovery::report::{Limits, Report};
use mimic_core::discovery::store::merge_bounded;
use serde_json::{Value, json};

struct Case {
    name: &'static str,
    /// The seeds of the pushes merged one after the other, each of `COUNT` exchanges.
    seeds: &'static [u64],
    limits: fn() -> Limits,
}

const COUNT: usize = 70;

const CASES: [Case; 5] = [
    Case {
        name: "single-below",
        seeds: &[1],
        limits: || limits(64, 16, 256 << 10),
    },
    Case {
        name: "merged-below",
        seeds: &[2, 3, 4],
        limits: || limits(64, 16, 256 << 10),
    },
    Case {
        name: "merged-endpoint-limit",
        seeds: &[5, 6, 7],
        limits: || limits(5, 16, 256 << 10),
    },
    Case {
        name: "merged-authority-limit",
        seeds: &[8, 9, 10],
        limits: || limits(64, 3, 256 << 10),
    },
    Case {
        name: "merged-sample-budget",
        seeds: &[11, 12, 13],
        limits: || limits(64, 16, 6 << 10),
    },
];

fn dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

/// The report of a case and the counts the bounded merge reported.
fn merged(case: &Case) -> (Report, Value) {
    let limits = (case.limits)();
    let mut stored = Report::new(None);
    let mut refused: BTreeMap<&str, u64> = BTreeMap::new();
    let mut evicted: BTreeMap<&str, u64> = BTreeMap::new();
    for (day, seed) in (19_000u64..).zip(case.seeds) {
        let push = build(&ops(*seed, COUNT), &limits, day);
        let out = merge_bounded(stored, push, &limits);
        for (k, n) in &out.refused {
            *refused.entry(k.as_str()).or_default() += n;
        }
        for (k, n) in &out.evicted {
            *evicted.entry(k.as_str()).or_default() += n;
        }
        stored = out.report;
    }
    (stored, json!({"refused": refused, "evicted": evicted}))
}

/// Every output of a report, by file name.
fn outputs(report: &Report, counts: Value) -> BTreeMap<&'static str, String> {
    let mut out = BTreeMap::new();
    out.insert("report.json", report.to_json().expect("serialises"));
    let (inbound, outbound) = report.split();
    out.insert("inbound.json", inbound.to_json().expect("serialises"));
    let outbound_json = outbound
        .as_ref()
        .map(|r| r.to_json().expect("serialises"))
        .unwrap_or_default();
    out.insert("outbound.json", outbound_json.clone());
    let index = openapi::index(report);
    let mut documents = BTreeMap::new();
    for entry in index["documents"].as_array().expect("documents") {
        let id = entry["id"].as_str().expect("an id");
        let document = openapi::document(report, "ns", "dep", 3, id).expect("a document");
        documents.insert(id.to_string(), document);
    }
    out.insert("documents.json", json!(documents).to_string());
    out.insert("index.json", index.to_string());
    out.insert("endpoints.json", openapi::endpoint_list(report).to_string());
    out.insert(
        "egress.json",
        EgressSet::project(report).to_json().expect("serialises"),
    );
    out.insert("digest.txt", digest(3, outbound_json.as_bytes()));
    out.insert("counts.json", counts.to_string());
    out
}

#[test]
fn shr_21_golden_corpus_is_reproduced_byte_for_byte() {
    let bless = std::env::var_os("MIMIC_BLESS").is_some();
    for case in &CASES {
        let (report, counts) = merged(case);
        let dir = dir(case.name);
        for (file, content) in outputs(&report, counts) {
            let path = dir.join(file);
            if bless {
                std::fs::create_dir_all(&dir).expect("a corpus directory");
                std::fs::write(&path, &content).expect("a corpus file");
                continue;
            }
            let golden = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                golden == content,
                "{}/{file} differs from the golden file",
                case.name
            );
        }
    }
}

#[test]
fn shr_21_golden_cases_reach_their_limits() {
    // The corpus must hold what it claims: a refusal or an eviction at each limit case, and
    // both directions in every case.
    for case in &CASES {
        let (report, counts) = merged(case);
        let json: Value = serde_json::from_str(&report.to_json().expect("serialises")).unwrap();
        assert!(json["scopes"]["inbound"].is_object(), "{}", case.name);
        assert!(json["scopes"]["outbound"].is_object(), "{}", case.name);
        if case.name.contains("limit") || case.name.contains("budget") {
            let touched = |k: &str| counts[k].as_object().is_some_and(|m| !m.is_empty());
            assert!(touched("refused") || touched("evicted"), "{}", case.name);
        }
    }
}

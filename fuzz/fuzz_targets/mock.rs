//! Fuzz target for the mock (spec MCK-20, §8).
//!
//! The input is split on NUL bytes into fields:
//!
//! 1. the control field: a flag byte (TLS, HTTP/2, a gRPC `content-type`) and the method
//!    (the rest, an arbitrary string);
//! 2. the target authority (`Host`), empty for none;
//! 3. the path;
//! 4. the query;
//! 5. the request headers, one `name: value` per line;
//! 6. the request body;
//! 7. an egress set, as `EgressSet::from_json` reads it. A set it refuses is replaced by the
//!    projection of the outbound part of a recorded report, so that the request parts always
//!    meet a real set.
//!
//! A canary (a 32-character string) is added to the request's query, headers and body. The
//! signature, the lookup, the tiers and the answer never panic, and every answer is a miss or
//! satisfies the MCK-04 property: its headers and body come from a sample of the request's
//! authority or from the fixed values, and the canary is not in it (unless the set holds it).
//! The latency draw stays within 300 s.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::Report;
use mimic_core::discovery::sample::Payload;
use mimic_core::grpc::frame::Parser;
use mimic_core::grpc::wire::Message;
use mimic_core::mock::{Index, MAX_DELAY_MS, Miss, Random, Request, draw, signature};

const PUSH: usize = 16;
const PUSH_AUTHORITIES: usize = 16;
const CANARY: &str = "Zq7XkP2mWv9LrT4nB8yD1cF6hJ3sG5aE";

/// A recorded report: an inbound and an outbound scope with samples, volatility and a tunnel.
const SEED: &str = include_str!("../seeds/egress/report");

/// A xorshift source seeded by the input.
struct Bits(u64);

impl Random for Bits {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound.max(1)
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn set_of(part: &[u8]) -> EgressSet {
    if let Ok(set) = EgressSet::from_json(&text(part)) {
        return set;
    }
    let report = Report::parse_checked(SEED, PUSH, PUSH_AUTHORITIES).expect("a valid report");
    EgressSet::project(&report)
}

/// One uncompressed frame whose payload is a message (the replay of a gRPC sample, §8).
fn is_message_frame(body: &[u8]) -> bool {
    let pushed = Parser::new().push(body);
    pushed.error.is_none()
        && matches!(pushed.frames.as_slice(), [frame]
            if !frame.compressed && Message::decode(&frame.payload).is_ok())
}

/// Every header and body value of an answer is a sample's of the authority, or fixed.
fn check(
    set: &EgressSet,
    authority: &str,
    (method, template): (&str, &str),
    response: &mimic_core::mock::Response,
    canary: bool,
) {
    // The samples of the matched endpoint only.
    let samples: Vec<_> = set
        .authorities
        .get(authority)
        .map(|a| {
            a.endpoints
                .iter()
                .filter(|e| e.method == method && e.template == template)
                .flat_map(|e| e.samples.iter())
                .collect()
        })
        .unwrap_or_default();
    for (name, value) in &response.headers {
        if canary {
            assert!(!value.contains(CANARY) && !name.contains(CANARY));
        }
        if name == "content-length" || name == "x-mimic-match" {
            continue;
        }
        // The fixed `content-type` of a gRPC answer whose sample has none (§8).
        if name == "content-type" && value == "application/grpc" {
            continue;
        }
        assert!(
            samples.iter().any(|s| s
                .response_headers()
                .iter()
                .any(|(n, v)| n == name && v == value)),
            "a header that no sample holds"
        );
    }
    // Trailers (gRPC, #463): a sample's, or the fixed `grpc-status: 0`.
    for (name, value) in &response.trailers {
        if canary {
            assert!(!value.contains(CANARY) && !name.contains(CANARY));
        }
        assert!(
            (name == "grpc-status" && value == "0")
                || samples.iter().any(|s| s
                    .trailers()
                    .iter()
                    .chain(s.response_headers())
                    .any(|(n, v)| n == name && v == value)),
            "a trailer that no sample holds"
        );
    }
    let body = response.body();
    if canary {
        assert!(!String::from_utf8_lossy(body).contains(CANARY));
    }
    let length = response
        .headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    assert_eq!(length, Some(body.len()));
    assert!(
        body.is_empty()
            || body == [0u8; 5]
            || is_message_frame(body)
            || samples.iter().any(|s| match s.response_payload() {
                Some((_, Payload::Json(t))) => t.as_bytes() == body,
                // The encoding is the index's: only that the bytes are a form's.
                Some((_, Payload::Form(_))) => true,
                None => false,
            }),
        "a body that no sample holds"
    );
}

fuzz_target!(|data: &[u8]| {
    let mut parts = data.split(|b| *b == 0);
    let mut next = || parts.next().unwrap_or_default();
    let control = next();
    let (flags, method) = control.split_first().map_or((0, &[][..]), |(f, m)| (*f, m));
    let method = text(method);
    let target = text(next());
    let path = text(next());
    let query = text(next());
    let header_lines = text(next());
    let body = next();
    let set = set_of(next());
    let seed = data.iter().fold(0x9e37_79b9_7f4a_7c15u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    });

    let query = format!("{query}&zz={CANARY}");
    let mut headers: Vec<(&str, &str)> = header_lines
        .split('\n')
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();
    headers.push(("x-canary", CANARY));
    if flags & 4 != 0 {
        headers.push(("content-type", "application/grpc"));
    }
    let request = Request {
        method: &method,
        target: (!target.is_empty()).then_some(target.as_str()),
        tls: flags & 1 != 0,
        sni: None,
        dst: std::net::SocketAddr::from(([10, 0, 0, 5], 8080)),
        h2: flags & 2 != 0,
        path: &path,
        query: &query,
        headers: &headers,
        body: (!body.is_empty()).then_some(body),
    };
    let redactor = Redactor::default();
    let sig = signature(&redactor, &request);
    let canary = !set.to_json().unwrap_or_default().contains(CANARY);

    let index = Index::build(set.clone());
    // A miss is a 599 with `x-mimic-miss: 1`, `content-length: 0` and nothing else.
    for miss in [Miss::Authority, Miss::Endpoint, Miss::Overload] {
        let response = miss.response();
        assert_eq!(response.status, 599);
        assert_eq!(response.headers.len(), 2);
        assert!(response.body().is_empty() && response.trailers.is_empty());
    }
    match index.lookup(&redactor, &request) {
        Err(miss) => {
            assert!(matches!(miss, Miss::Authority | Miss::Endpoint));
            assert_eq!(miss.response().status, 599);
        }
        Ok(hit) => {
            check(
                &set,
                sig.authority(),
                (hit.method, hit.template),
                &hit.response(&method),
                canary,
            );
            for factor in [0, 1, 100, 10_000] {
                assert!(draw(hit.latency, factor, &mut Bits(seed | 1)) <= MAX_DELAY_MS);
            }
            // The same request, the same answer.
            let again = index.lookup(&redactor, &request).expect("a second lookup");
            assert_eq!(again.tier, hit.tier);
            assert_eq!(again.answer, hit.answer);
        }
    }
});

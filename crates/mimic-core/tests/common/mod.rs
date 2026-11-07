//! Shared builders of the SHR-21 and SHR-22 tests (#339): reports made by the recorder, from a
//! fixed seed or from a proptest strategy.

#![allow(dead_code)]

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;

pub const METHODS: [&str; 8] = [
    "GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE",
];
pub const STATUSES: [u16; 8] = [200, 201, 204, 301, 400, 404, 409, 500];
pub const PATHS: [&str; 10] = [
    "/",
    "/users",
    "/users/42",
    "/users/7f9c2a10-1b2c-4d3e-8f40-0123456789ab",
    "/users/43/orders/1001",
    "/orders/2024-03-01",
    "/v1/payments/9876543210",
    "/health",
    "/files/a/b/c.txt",
    "/pkg.Svc/Call",
];
pub const AUTHORITIES: [&str; 6] = [
    "api.example.com:443",
    "10.0.0.5:8080",
    "[2001:db8::1]:8443",
    "users.disc-22.svc:8080",
    "pay.example:443",
    "<redacted>:443",
];
const QUERIES: [&str; 4] = [
    "",
    "page=2&sort=name",
    "key=abc123&q=x",
    "ids=1&ids=2&flag",
];

/// One exchange to record, by index into the tables above.
#[derive(Debug, Clone)]
pub struct Op {
    pub outbound: bool,
    pub target: usize,
    pub method: usize,
    pub path: usize,
    pub status: usize,
    pub query: usize,
    pub body: usize,
    pub tunnel: bool,
}

/// A small xorshift generator: the corpus must not depend on a crate's algorithm.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % bound as u64).expect("fits")
    }
}

pub fn ops(seed: u64, count: usize) -> Vec<Op> {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    (0..count)
        .map(|_| Op {
            outbound: rng.next(2) == 0,
            target: rng.next(AUTHORITIES.len()),
            method: rng.next(METHODS.len()),
            path: rng.next(PATHS.len()),
            status: rng.next(STATUSES.len()),
            query: rng.next(QUERIES.len()),
            body: rng.next(8),
            tunnel: rng.next(12) == 0,
        })
        .collect()
}

/// A JSON body, a form body or no body, by index; `n` varies the values.
fn body(kind: usize, n: usize) -> Option<Vec<u8>> {
    let text = match kind {
        0 => return None,
        1 => format!(r#"{{"id":{n},"name":"user-{n}","active":true}}"#),
        2 => format!(
            r#"{{"items":[{{"sku":"s{n}","qty":{}}},{{"sku":"t{n}","qty":2.5}}],"note":null}}"#,
            n % 7
        ),
        3 => format!(
            r#"{{"id":"7f9c2a10-1b2c-4d3e-8f40-{n:012}","at":"2024-03-0{}T10:00:00Z","token":"tok-{n}"}}"#,
            1 + n % 9
        ),
        4 => format!("name=user{n}&role=admin&n={n}"),
        5 => format!(r#"{{"nested":{{"a":{{"b":[{n},{n},"x"]}}}},"list":[]}}"#),
        6 => format!(r#"[{n},{{"k":"v{n}"}}]"#),
        _ => format!(r#"{{"authorization":"Bearer abc.def.{n}","email":"u{n}@example.com"}}"#),
    };
    Some(text.into_bytes())
}

/// Records `ops` into a fresh report, stamped at day `day`.
pub fn build(ops: &[Op], limits: &Limits, day: u64) -> Report {
    let mut report = Report::new(Some("pod-a".into()));
    let redactor = Redactor::default();
    for (n, op) in ops.iter().enumerate() {
        let (direction, scope, port) = if op.outbound {
            let authority = AUTHORITIES[op.target % AUTHORITIES.len()];
            (
                Direction::Outbound,
                ScopeKey::Authority(authority.into()),
                443,
            )
        } else {
            let port = 8000 + u16::try_from(op.target % 4).expect("small");
            (Direction::Inbound, ScopeKey::Port(port), port)
        };
        if op.tunnel && op.outbound {
            report.record_tunnel(limits, scope.clone(), "tcp");
        }
        let request = body(op.body, n);
        let response = body((op.body + 1) % 8, n + 1);
        let form = [("content-type", "application/x-www-form-urlencoded")];
        let json = [("content-type", "application/json")];
        let req_headers: &[(&str, &str)] = if op.body == 4 { &form } else { &json };
        let exchange = Exchange {
            direction,
            port,
            method: METHODS[op.method],
            path: PATHS[op.path],
            status: STATUSES[op.status],
            head_ms: u64::try_from(1 + n % 40).expect("small"),
            host: (!op.outbound).then_some("svc.example.com"),
            grpc: PATHS[op.path] == "/pkg.Svc/Call",
            observation: Observation {
                query: QUERIES[op.query],
                req_headers,
                req_body: request.as_deref(),
                resp_headers: &json,
                resp_body: response.as_deref(),
            },
        };
        let via = Via {
            tls: n % 2 == 0,
            sni: (n % 3 == 0).then_some("sni.example.com"),
            dst: None,
        };
        report.record_at(scope, &via, limits, &redactor, &exchange);
    }
    report.stamp(day);
    report
}

pub fn limits(max_endpoints: usize, max_authorities: usize, sample_budget: usize) -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints,
        max_authorities,
        sample_budget,
    }
}

/// The report of SHR-18 and SHR-22 at the store limits: 512 inbound endpoints (64 ports x 8
/// methods) and 512 outbound ones (128 authorities x 4 methods), six statuses each, with JSON
/// bodies of 40 keys, a full 4 MiB sample budget.
pub fn full_report(salt: usize) -> Report {
    const KEYS: usize = 40;
    let limits = limits(512, 128, 4 << 20);
    let mut report = Report::new(Some("pod-a".into()));
    let headers = [("content-type", "application/json")];
    for n in 0..1024usize {
        let (direction, scope, port, method) = if n < 512 {
            let port = 8000 + u16::try_from(n / 8).expect("small");
            (
                Direction::Inbound,
                ScopeKey::Port(port),
                port,
                METHODS[n % 8],
            )
        } else {
            let m = n - 512;
            let authority = format!("a{}.example.com:443", m / 4);
            (
                Direction::Outbound,
                ScopeKey::Authority(authority),
                443,
                METHODS[m % 4],
            )
        };
        for status in [200, 201, 202, 400, 404, 500] {
            let fields: Vec<String> = (0..KEYS)
                .map(|k| format!("\"field{k}\":\"value-{}-{status}-{k}\"", n + salt))
                .collect();
            let body = format!("{{{}}}", fields.join(","));
            let exchange = Exchange {
                direction,
                port,
                method,
                path: "/resource",
                status,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation {
                    query: "",
                    req_headers: &headers,
                    req_body: Some(body.as_bytes()),
                    resp_headers: &headers,
                    resp_body: Some(body.as_bytes()),
                },
            };
            let via = Via {
                tls: direction == Direction::Outbound,
                sni: None,
                dst: None,
            };
            report.record_at(scope.clone(), &via, &limits, &Redactor::default(), &exchange);
        }
    }
    report.stamp(20_000);
    report
}

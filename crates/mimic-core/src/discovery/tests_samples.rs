//! DSC-07, 08, 09 (with samples), 11: samples, bounds and the no-leak property.

use proptest::prelude::*;
use serde_json::json;
use xxhash_rust::xxh3::xxh3_64;

use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Outcome, Report, Sampled, merge};

fn limits() -> Limits {
    Limits {
        max_literals: 32,
        max_endpoints: 256,
        sample_budget: 16 << 20,
    }
}

/// An exchange with owned parts, so that generators can build it.
#[derive(Debug, Clone)]
struct Owned {
    method: String,
    path: String,
    status: u16,
    head_ms: u64,
    query: String,
    req_headers: Vec<(String, String)>,
    req_body: Option<Vec<u8>>,
    resp_headers: Vec<(String, String)>,
    resp_body: Option<Vec<u8>>,
}

impl Owned {
    fn new(path: &str, status: u16) -> Self {
        Owned {
            method: "GET".into(),
            path: path.into(),
            status,
            head_ms: 3,
            query: String::new(),
            req_headers: vec![],
            req_body: None,
            resp_headers: vec![],
            resp_body: None,
        }
    }

    fn query(mut self, q: &str) -> Self {
        self.query = q.into();
        self
    }

    fn json_response(mut self, body: &serde_json::Value) -> Self {
        self.resp_headers
            .push(("content-type".into(), "application/json".into()));
        self.resp_body = Some(body.to_string().into_bytes());
        self
    }

    fn json_request(mut self, body: &serde_json::Value) -> Self {
        self.req_headers
            .push(("content-type".into(), "application/json".into()));
        self.req_body = Some(body.to_string().into_bytes());
        self
    }

    fn record(&self, r: &mut Report, l: &Limits, redactor: &Redactor) -> Outcome {
        let req: Vec<(&str, &str)> = self
            .req_headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let resp: Vec<(&str, &str)> = self
            .resp_headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        r.record(
            l,
            redactor,
            &Exchange {
                direction: Direction::Inbound,
                port: 8080,
                method: &self.method,
                path: &self.path,
                status: self.status,
                head_ms: self.head_ms,
                host: None,
                grpc: false,
                observation: Observation {
                    query: &self.query,
                    req_headers: &req,
                    req_body: self.req_body.as_deref(),
                    resp_headers: &resp,
                    resp_body: self.resp_body.as_deref(),
                },
            },
        )
    }
}

fn report(l: &Limits, items: &[Owned]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for o in items {
        o.record(&mut r, l, &Redactor::default());
    }
    r
}

fn sample_hashes(r: &Report) -> Vec<u64> {
    r.endpoints()
        .into_iter()
        .flat_map(|e| e.stats.samples().iter().map(|(h, _)| h).collect::<Vec<_>>())
        .collect()
}

mod samples {
    use super::*;

    fn candidates() -> Vec<Owned> {
        let statuses = [200u16, 201, 400, 500];
        (0..25)
            .map(|i| {
                let status = statuses[i % 4];
                // Two response shapes per status, a different query for every candidate.
                let body = if i % 3 == 0 {
                    json!({"id": i})
                } else {
                    json!({"id": i, "name": "x"})
                };
                Owned::new("/items", status)
                    .query(&format!("page={i}"))
                    .json_response(&body)
            })
            .collect()
    }

    #[test]
    fn dsc_08_at_most_20_and_every_status_and_class() {
        let all = candidates();
        let r = report(&limits(), &all);
        let endpoints = r.endpoints();
        let samples = endpoints[0].stats.samples();
        assert_eq!(samples.len(), 20);
        let statuses: std::collections::BTreeSet<u16> =
            samples.iter().map(|(_, s)| s.status()).collect();
        assert_eq!(statuses.into_iter().collect::<Vec<_>>(), [200, 201, 400, 500]);
        // Every (status, request shape, response shape) class that a candidate has.
        let classes: std::collections::BTreeSet<_> = samples
            .iter()
            .map(|(_, s)| (s.status(), s.request_shape(), s.response_shape()))
            .collect();
        let mut expected = std::collections::BTreeSet::new();
        let mut one_by_one = Report::new(None);
        for o in &all {
            let mut single = Report::new(None);
            o.record(&mut single, &limits(), &Redactor::default());
            for (_, s) in single.endpoints()[0].stats.samples().iter() {
                expected.insert((s.status(), s.request_shape(), s.response_shape()));
            }
            o.record(&mut one_by_one, &limits(), &Redactor::default());
        }
        assert_eq!(classes, expected);
        assert_eq!(expected.len(), 8);
    }

    #[test]
    fn dsc_08_lowest_hashes_fill_the_rest() {
        let mut all = candidates();
        // Same status and shape everywhere: only the hash decides.
        for o in &mut all {
            o.status = 200;
            o.resp_body = Some(br#"{"id":1}"#.to_vec());
        }
        let r = report(&limits(), &all);
        let mut hashes: Vec<u64> = all
            .iter()
            .map(|o| {
                let single = report(&limits(), std::slice::from_ref(o));
                sample_hashes(&single)[0]
            })
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(sample_hashes(&r), hashes[..20]);
    }

    #[test]
    fn dsc_08_order_does_not_matter_and_duplicates_count_once() {
        let all = candidates();
        let forward = report(&limits(), &all);
        let mut reversed = all.clone();
        reversed.reverse();
        let backward = report(&limits(), &reversed);
        assert_eq!(sample_hashes(&forward), sample_hashes(&backward));
        assert_eq!(forward.to_json(), backward.to_json());

        let one = Owned::new("/a", 200).query("x=1");
        let twice = report(&limits(), &[one.clone(), one]);
        assert_eq!(twice.endpoints()[0].stats.samples().len(), 1);
        assert_eq!(twice.endpoints()[0].stats.statuses.get(&200), Some(&2));
    }

    #[test]
    fn dsc_08_big_and_opaque_bodies_are_not_kept() {
        let mut big = Owned::new("/a", 200);
        big.resp_headers
            .push(("content-type".into(), "application/json".into()));
        let mut body = br#"{"filler":""#.to_vec();
        body.extend(std::iter::repeat_n(b'z', 300 * 1024));
        body.extend(br#""}"#);
        big.resp_body = Some(body);
        let mut opaque = Owned::new("/b", 200);
        opaque
            .resp_headers
            .push(("content-type".into(), "text/plain".into()));
        opaque.resp_body = Some(b"OPAQUE-BODY-TEXT".to_vec());

        let r = report(&limits(), &[big, opaque]);
        let json = r.to_json();
        assert!(!json.contains("zzzzzzzz"), "big body kept");
        assert!(!json.contains("OPAQUE-BODY-TEXT"), "opaque body kept");
        // The size and the media type are.
        assert!(json.contains("\"bytes\":307212"), "{}", &json[..300]);
        assert!(json.contains("text/plain"));
        // The exchange is still a sample, without its body.
        assert_eq!(r.endpoints()[0].stats.samples().len(), 1);
    }

    #[test]
    fn dsc_08_budget_rejects_a_new_sample_but_counters_and_schemas_change() {
        let first = Owned::new("/a", 200).query("x=1");
        let second = Owned::new("/a", 201).query("x=2&extra=1");
        let mut probe = Report::new(None);
        first.record(&mut probe, &limits(), &Redactor::default());
        let size = probe.sample_bytes();
        assert!(size > 0);

        let l = Limits {
            sample_budget: size + 10,
            ..limits()
        };
        let mut r = Report::new(None);
        let a = first.record(&mut r, &l, &Redactor::default());
        assert!(matches!(
            a,
            Outcome::Recorded {
                sample: Sampled::Admitted,
                ..
            }
        ));
        let b = second.record(&mut r, &l, &Redactor::default());
        assert!(matches!(
            b,
            Outcome::Recorded {
                sample: Sampled::Budget,
                ..
            }
        ));
        let e = &r.endpoints()[0];
        assert_eq!(e.stats.samples().len(), 1);
        assert_eq!(e.stats.statuses.get(&201), Some(&1));
        assert!(r.to_json().contains("extra"), "schema did not change");
        assert!(r.sample_bytes() <= l.sample_budget);
    }

    #[test]
    fn dsc_08_samples_are_redacted() {
        let mut o = Owned::new("/a", 200).query("password=hunter2&page=1");
        o.req_headers
            .push(("authorization".into(), "Bearer abc.def".into()));
        let r = report(&limits(), &[o.json_request(&json!({"token": "t0k3n", "n": 1}))]);
        let json = r.to_json();
        assert!(!json.contains("hunter2") && !json.contains("t0k3n"));
        assert!(!json.contains("abc.def"));
        assert!(json.contains("<redacted>") && json.contains("page"));
    }
}

mod bounds {
    use super::*;

    fn recorded(o: &Outcome) -> bool {
        matches!(o, Outcome::Recorded { .. })
    }

    #[test]
    fn dsc_11_endpoint_limit() {
        let l = Limits {
            max_endpoints: 3,
            ..limits()
        };
        let mut r = Report::new(None);
        let red = Redactor::default();
        for p in ["/a", "/b", "/c"] {
            assert!(recorded(&Owned::new(p, 200).record(&mut r, &l, &red)));
        }
        assert_eq!(
            Owned::new("/d", 200).record(&mut r, &l, &red),
            Outcome::EndpointLimit
        );
        // A known endpoint still updates, and a new method is a new endpoint.
        assert!(recorded(&Owned::new("/a", 500).record(&mut r, &l, &red)));
        let mut post = Owned::new("/a", 200);
        post.method = "POST".into();
        assert_eq!(post.record(&mut r, &l, &red), Outcome::EndpointLimit);
        let e = r.endpoints();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].stats.statuses.get(&500), Some(&1));
        assert_eq!(r.exchanges(), 4);
    }

    #[test]
    fn dsc_11_not_found_and_not_allowed_create_nothing() {
        let l = limits();
        let red = Redactor::default();
        let mut r = Report::new(None);
        Owned::new("/users/42", 200).record(&mut r, &l, &red);
        // Matches the known template.
        let hit = Owned::new("/users/43", 404).record(&mut r, &l, &red);
        assert!(recorded(&hit));
        assert_eq!(
            Owned::new("/users/44", 405).record(&mut r, &l, &red),
            Outcome::Recorded {
                new_endpoint: false,
                sample: Sampled::None
            }
        );
        // Unknown path, unknown method.
        assert_eq!(
            Owned::new("/admin", 404).record(&mut r, &l, &red),
            Outcome::NotFound
        );
        assert_eq!(
            Owned::new("/users/abc", 404).record(&mut r, &l, &red),
            Outcome::NotFound
        );
        let mut post = Owned::new("/users/45", 405);
        post.method = "POST".into();
        assert_eq!(post.record(&mut r, &l, &red), Outcome::NotFound);
        let e = r.endpoints();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].template, "/users/{users_id}");
        assert_eq!(e[0].stats.statuses.get(&404), Some(&1));
        assert_eq!(e[0].stats.statuses.get(&405), Some(&1));
        assert_eq!(r.exchanges(), 3);
        // A 404 on a path nothing knows does not even create the scope.
        let mut empty = Report::new(None);
        Owned::new("/x", 404).record(&mut empty, &l, &red);
        assert!(empty.endpoints().is_empty());
        assert_eq!(empty.exchanges(), 0);
    }

    #[test]
    fn dsc_11_a_404_on_an_absorbed_position_matches() {
        let l = Limits {
            max_literals: 2,
            ..limits()
        };
        let red = Redactor::default();
        let mut r = Report::new(None);
        for p in ["/u/a", "/u/b", "/u/c"] {
            Owned::new(p, 200).record(&mut r, &l, &red);
        }
        assert!(recorded(&Owned::new("/u/zzz", 404).record(&mut r, &l, &red)));
        assert_eq!(r.endpoints().len(), 1);
    }

    #[test]
    fn dsc_11_long_and_deep_paths_are_invalid() {
        let l = limits();
        let red = Redactor::default();
        let mut r = Report::new(None);
        let long = format!("/{}", "a".repeat(2048));
        assert_eq!(long.len(), 2049);
        assert_eq!(Owned::new(&long, 200).record(&mut r, &l, &red), Outcome::Invalid);
        let ok = format!("/{}", "a".repeat(2047));
        assert!(recorded(&Owned::new(&ok, 200).record(&mut r, &l, &red)));

        let deep33 = "/a".repeat(33);
        assert_eq!(Owned::new(&deep33, 200).record(&mut r, &l, &red), Outcome::Invalid);
        let deep32 = "/a".repeat(32);
        assert!(recorded(&Owned::new(&deep32, 200).record(&mut r, &l, &red)));
        assert_eq!(r.endpoints().len(), 2);
        assert_eq!(r.exchanges(), 2);
        // A status outside 100..=599 is not an HTTP status.
        assert_eq!(Owned::new("/s", 99).record(&mut r, &l, &red), Outcome::Invalid);
        assert_eq!(Owned::new("/s", 600).record(&mut r, &l, &red), Outcome::Invalid);
    }

    fn arbitrary_exchange() -> impl Strategy<Value = Owned> {
        (
            prop_oneof![Just("GET"), Just("POST"), Just("DELETE")],
            proptest::collection::vec("[a-z0-9@%.-]{0,12}", 0..40)
                .prop_map(|s| format!("/{}", s.join("/"))),
            prop_oneof![Just(200u16), Just(404), Just(405), Just(500), 0u16..700],
            "[a-z0-9=&%+]{0,40}",
            proptest::collection::vec(any::<u8>(), 0..200),
            0u8..3,
        )
            .prop_map(|(method, path, status, query, body, kind)| {
                let mut o = Owned::new(&path, status).query(&query);
                o.method = method.into();
                match kind {
                    0 => {}
                    1 => {
                        o.resp_headers
                            .push(("content-type".into(), "application/json".into()));
                        o.resp_body = Some(body);
                    }
                    _ => {
                        o.req_headers.push((
                            "content-type".into(),
                            "application/x-www-form-urlencoded".into(),
                        ));
                        o.req_body = Some(body);
                    }
                }
                o
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        #[test]
        fn dsc_11_never_panics_and_state_stays_bounded(
            items in proptest::collection::vec(arbitrary_exchange(), 0..60),
        ) {
            let l = Limits { max_literals: 4, max_endpoints: 8, sample_budget: 1 << 20 };
            let red = Redactor::default();
            let mut r = Report::new(None);
            for o in &items {
                o.record(&mut r, &l, &red);
                prop_assert!(r.endpoints().len() <= l.max_endpoints);
            }
            prop_assert!(r.sample_bytes() <= l.sample_budget);
            let retained = r.to_json().len() - r.sample_bytes();
            prop_assert!(
                retained <= Report::state_bound(&l),
                "{retained} > {}",
                Report::state_bound(&l)
            );
            prop_assert_eq!(Report::from_json(&r.to_json()).unwrap(), r);
        }
    }
}

mod algebra {
    use super::*;

    fn exchange() -> impl Strategy<Value = Owned> {
        (
            prop_oneof![Just("/a"), Just("/a/1"), Just("/b"), Just("/b/x"), Just("/b/y")],
            prop_oneof![Just(200u16), Just(201), Just(400), Just(500)],
            0u8..4,
            0u8..3,
            0u64..3000,
        )
            .prop_map(|(path, status, q, body, ms)| {
                let mut o = Owned::new(path, status).query(&format!("q={q}"));
                o.head_ms = ms;
                match body {
                    0 => {}
                    1 => o = o.json_response(&json!({"id": q})),
                    _ => o = o.json_response(&json!({"id": q, "tags": ["a"]})),
                }
                o
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        #[test]
        fn dsc_09_samples_and_schemas_algebra(
            items in proptest::collection::vec(exchange(), 0..14),
            cut1 in 0usize..15,
            cut2 in 0usize..15,
        ) {
            let l = Limits { max_literals: 3, ..limits() };
            let whole = report(&l, &items);
            let a = cut1.min(items.len());
            let b = cut2.min(items.len()).max(a);
            let (x, y, z) = (
                report(&l, &items[..a]),
                report(&l, &items[a..b]),
                report(&l, &items[b..]),
            );
            let left = merge(merge(x.clone(), y.clone(), &l), z.clone(), &l);
            let right = merge(x.clone(), merge(y.clone(), z.clone(), &l), &l);
            prop_assert_eq!(left.to_json(), whole.to_json());
            prop_assert_eq!(right.to_json(), whole.to_json());
            prop_assert_eq!(merge(z.clone(), x.clone(), &l).to_json(), merge(x, z, &l).to_json());
        }
    }
}

mod no_leak {
    use super::*;

    /// How the canary is made recognisable: by the name that carries it, or by its shape.
    #[derive(Debug, Clone)]
    enum Form {
        Named(&'static str),
        Shaped(&'static str),
    }

    #[derive(Debug, Clone, Copy)]
    enum Place {
        Path,
        Query,
        Header,
        Form,
        Json(usize),
    }

    fn canary_body() -> impl Strategy<Value = String> {
        "[A-Za-z0-9]{16,32}"
    }

    fn form() -> impl Strategy<Value = Form> {
        prop_oneof![
            Just(Form::Named("password")),
            Just(Form::Named("X-Api-Key")),
            Just(Form::Named("access_token")),
            Just(Form::Named("session")),
            Just(Form::Named("internalref")),
            Just(Form::Shaped("ghp_")),
            Just(Form::Shaped("AKIA")),
            Just(Form::Shaped("sk_live_")),
            Just(Form::Shaped("glpat-")),
        ]
    }

    fn place() -> impl Strategy<Value = Place> {
        prop_oneof![
            Just(Place::Path),
            Just(Place::Query),
            Just(Place::Header),
            Just(Place::Form),
            (0usize..8).prop_map(Place::Json),
        ]
    }

    fn canary(form: &Form, body: &str) -> String {
        match form {
            Form::Named(_) => format!("CNRY{body}"),
            // Prefix tokens need their length; AWS ids are AKIA + 16 upper-case alphanumerics.
            Form::Shaped("AKIA") => format!("AKIA{}", body[..16].to_ascii_uppercase()),
            Form::Shaped(prefix) => format!("{prefix}{body}"),
        }
    }

    fn build(form: &Form, place: Place, response: bool, canary: &str) -> Owned {
        let name = match form {
            Form::Named(n) => *n,
            Form::Shaped(_) => "note",
        };
        let mut o = Owned::new("/api/items", 200);
        let (headers, body) = if response {
            (&mut o.resp_headers, &mut o.resp_body)
        } else {
            (&mut o.req_headers, &mut o.req_body)
        };
        match place {
            Place::Path => match form {
                Form::Shaped(_) => o.path = format!("/api/items/{canary}"),
                Form::Named(_) => o.path = "/api/items".into(),
            },
            Place::Query => o.query = format!("page=1&{name}={canary}"),
            Place::Header => {
                headers.push((name.to_ascii_lowercase(), canary.into()));
            }
            Place::Form => {
                headers.push((
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                ));
                *body = Some(format!("a=1&{name}={canary}").into_bytes());
            }
            Place::Json(depth) => {
                let mut value = json!({ name: canary, "other": 1 });
                for i in 0..depth {
                    value = if i % 2 == 0 {
                        json!({"level": value})
                    } else {
                        json!([{"item": value}])
                    };
                }
                headers.push(("content-type".into(), "application/json".into()));
                *body = Some(value.to_string().into_bytes());
            }
        }
        o
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        #[test]
        fn dsc_07_no_leak(
            form in form(),
            place in place(),
            response in any::<bool>(),
            body in canary_body(),
            repeat in 1usize..4,
        ) {
            let canary = canary(&form, &body);
            let o = build(&form, place, response, &canary);
            let red = Redactor::new(["internalref"]);
            let mut r = Report::new(Some("p".into()));
            let mut other = Report::new(Some("q".into()));
            for _ in 0..repeat {
                o.record(&mut r, &limits(), &red);
            }
            o.record(&mut other, &limits(), &red);
            let merged = merge(r, other, &limits());
            let hash = xxh3_64(canary.as_bytes());
            let json = merged.to_json();
            prop_assert!(!json.contains(&canary), "canary in {json}");
            for needle in [
                hash.to_string(),
                format!("{hash:x}"),
                format!("{hash:016x}"),
                format!("{hash:X}"),
            ] {
                prop_assert!(!json.contains(&needle), "hash {needle} in {json}");
            }
        }
    }
}

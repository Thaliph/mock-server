use proptest::prelude::*;

use super::histogram::{Histogram, OVERFLOW, bucket_index};
use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, merge};
use super::template::is_param;

fn ex<'a>(path: &'a str, status: u16, head_ms: u64) -> Exchange<'a> {
    Exchange {
        direction: Direction::Inbound,
        port: 8080,
        method: "GET",
        path,
        status,
        head_ms,
        host: None,
        grpc: false,
        observation: Observation::default(),
    }
}

fn limits(max_literals: usize) -> Limits {
    Limits {
        max_literals,
        max_endpoints: 256,
        sample_budget: 16 << 20,
    }
}

fn report_of(l: &Limits, exchanges: &[Exchange<'_>]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for e in exchanges {
        r.record(l, &Redactor::default(), e);
    }
    r
}

fn templates(r: &Report) -> Vec<String> {
    r.endpoints().into_iter().map(|e| e.template).collect()
}

fn template_of(path: &str) -> String {
    let r = report_of(&limits(32), &[ex(path, 200, 1)]);
    templates(&r).remove(0)
}

#[test]
fn dsc_01_params() {
    let params = [
        "42",
        "123e4567-e89b-12d3-a456-426614174000",
        "123E4567-E89B-12D3-A456-426614174000",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "5f2b9c1d",
        "2026-11-01",
        "2026-11-01T10:00:00Z",
        "2026-11-01T10:00:00.5+02:00",
        "a1b2c3d4e5f6g7h8i9",
        "alice@example.com",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
    ];
    for p in params {
        assert!(is_param(p), "{p} should be a parameter");
    }
    let literals = [
        "v1",
        "api",
        "users",
        "deadbeef",
        "health",
        "",
        "%41",
        "2026-13-01",
    ];
    for l in literals {
        assert!(!is_param(l), "{l:?} should be a literal");
    }
}

#[test]
fn dsc_01_names() {
    assert_eq!(
        template_of("/users/42/orders/7"),
        "/users/{users_id}/orders/{orders_id}"
    );
    assert_eq!(template_of("/42"), "/{param1}");
    assert_eq!(template_of("/a/1/a/2"), "/a/{a_id}/a/{param4}");
    assert_eq!(template_of("/Foo-Bar/1"), "/Foo-Bar/{foo_bar_id}");
    assert_eq!(template_of("/users/"), "/users/");
    assert_eq!(template_of("/"), "/");
    assert_eq!(template_of("/v1/%41"), "/v1/%41");
}

#[test]
fn dsc_01_grpc_never_templated() {
    let mut r = Report::new(None);
    let mut e = ex("/pkg.Svc/Get/42", 200, 1);
    e.grpc = true;
    r.record(&limits(1), &Redactor::default(), &e);
    e.path = "/pkg.Svc/Get/43";
    r.record(&limits(1), &Redactor::default(), &e);
    assert_eq!(templates(&r), ["/pkg.Svc/Get/42", "/pkg.Svc/Get/43"]);
}

#[test]
fn dsc_02_absorbing() {
    let l = limits(3);
    let r = report_of(
        &l,
        &[
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 200, 1),
            ex("/x/a", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/a", "/u/b", "/u/c", "/x/a"]);

    let r = report_of(
        &l,
        &[
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 500, 1),
            ex("/u/d", 200, 1),
            ex("/x/a", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/{u_id}", "/x/a"]);
    let eps = r.endpoints();
    assert_eq!(eps[0].stats.statuses.get(&200), Some(&3));
    assert_eq!(eps[0].stats.statuses.get(&500), Some(&1));
    assert_eq!(eps[0].stats.latency.count(), 4);

    // Later literals and a parameter share the absorbed node.
    let mut r = r;
    r.record(&l, &Redactor::default(), &ex("/u/zzz", 200, 1));
    r.record(&l, &Redactor::default(), &ex("/u/42", 200, 1));
    assert_eq!(templates(&r), ["/u/{u_id}", "/x/a"]);
    assert_eq!(r.endpoints()[0].stats.latency.count(), 6);

    // A parameter before the literals joins them on absorption.
    let r = report_of(
        &l,
        &[
            ex("/u/42", 200, 1),
            ex("/u/a", 200, 1),
            ex("/u/b", 200, 1),
            ex("/u/c", 200, 1),
            ex("/u/d", 200, 1),
        ],
    );
    assert_eq!(templates(&r), ["/u/{u_id}"]);
    assert_eq!(r.endpoints()[0].stats.latency.count(), 5);
}

#[test]
fn dsc_02_absorption_recurses() {
    let l = limits(2);
    let r = report_of(
        &l,
        &[
            ex("/a/x/1", 200, 1),
            ex("/a/y/2", 200, 1),
            ex("/a/x/p", 200, 1),
            ex("/a/y/q", 200, 1),
            ex("/a/x/r", 200, 1),
            ex("/a/z", 200, 1),
        ],
    );
    // `a` has three literals, so it absorbs; the merged children then hold p, q, r and absorb too.
    assert_eq!(templates(&r), ["/a/{a_id}", "/a/{a_id}/{param3}"]);
    assert_eq!(r.endpoints()[1].stats.latency.count(), 5);
}

fn path_strategy() -> impl Strategy<Value = String> {
    let seg = prop_oneof![
        Just("a".to_string()),
        Just("b".to_string()),
        Just("c".to_string()),
        Just("d".to_string()),
        Just("e".to_string()),
        Just("users".to_string()),
        Just("42".to_string()),
        Just("7".to_string()),
        Just("".to_string()),
    ];
    proptest::collection::vec(seg, 1..4).prop_map(|s| format!("/{}", s.join("/")))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn dsc_02_order_independent(
        paths in proptest::collection::vec(path_strategy(), 1..14),
        max in 1usize..4,
        seed in any::<u64>(),
    ) {
        let l = limits(max);
        let exs: Vec<_> = paths.iter().map(|p| ex(p, 200, 1)).collect();
        let a = report_of(&l, &exs);
        let mut shuffled = exs.clone();
        // Deterministic permutation from the seed.
        let mut s = seed;
        for i in (1..shuffled.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            shuffled.swap(i, (s >> 33) as usize % (i + 1));
        }
        let b = report_of(&l, &shuffled);
        prop_assert_eq!(templates(&a), templates(&b));
        prop_assert_eq!(a, b);
    }
}

#[test]
fn dsc_10_buckets() {
    assert_eq!(bucket_index(0), 0);
    assert_eq!(bucket_index(1), 0);
    assert_eq!(bucket_index(2), 4);
    assert_eq!(bucket_index(3), 7);
    assert_eq!(bucket_index(4), 8);
    assert_eq!(bucket_index(5), 10);
    assert_eq!(bucket_index(1024), 40);
    assert_eq!(bucket_index(131_072), 68);
    assert_eq!(bucket_index(131_073), OVERFLOW);
    assert_eq!(bucket_index(u64::MAX), OVERFLOW);
}

#[test]
fn dsc_10_merge_adds() {
    let mut a = Histogram::default();
    let mut b = Histogram::default();
    a.observe(0);
    a.observe(2);
    b.observe(1);
    b.observe(2);
    b.observe(200_000);
    a.merge(&b);
    assert_eq!(a.buckets().get(&0), Some(&2));
    assert_eq!(a.buckets().get(&4), Some(&2));
    assert_eq!(a.buckets().get(&OVERFLOW), Some(&1));
    assert_eq!(a.count(), 5);
}

fn exchange_strategy() -> impl Strategy<Value = (u16, String, u16, u64, u8)> {
    (
        prop_oneof![Just(8080u16), Just(9090u16)],
        path_strategy(),
        prop_oneof![Just(200u16), Just(201), Just(500)],
        prop_oneof![0u64..5, 0u64..3000, 100_000u64..300_000],
        0u8..3,
    )
}

fn build(l: &Limits, items: &[(u16, String, u16, u64, u8)]) -> Report {
    const HOSTS: [&str; 3] = ["h1", "h2", "h3"];
    let mut r = Report::new(Some("p".into()));
    for (port, path, status, ms, h) in items {
        r.record(
            l,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: *port,
                method: if path.len() % 2 == 0 { "GET" } else { "POST" },
                path,
                status: *status,
                head_ms: *ms,
                host: Some(HOSTS[*h as usize]),
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn dsc_09_counters_algebra(
        items in proptest::collection::vec(exchange_strategy(), 0..24),
        parts in proptest::collection::vec(0usize..3, 24),
        max in 1usize..4,
    ) {
        let l = limits(max);
        let whole = build(&l, &items);
        let mut groups: [Vec<_>; 3] = Default::default();
        for (i, it) in items.iter().enumerate() {
            groups[parts[i]].push(it.clone());
        }
        let [a, b, c] = groups.map(|g| build(&l, &g));
        // Commutative.
        prop_assert_eq!(merge(a.clone(), b.clone(), &l), merge(b.clone(), a.clone(), &l));
        // Associative.
        let left = merge(merge(a.clone(), b.clone(), &l), c.clone(), &l);
        let right = merge(a.clone(), merge(b.clone(), c.clone(), &l), &l);
        prop_assert_eq!(&left, &right);
        // A partition merges back to the sequential recording.
        prop_assert_eq!(&left, &whole);
        prop_assert_eq!(left.exchanges(), items.len() as u64);
        // Serialisation is canonical and round-trips.
        let json = whole.to_json().unwrap();
        prop_assert_eq!(&json, &left.to_json().unwrap());
        prop_assert_eq!(Report::from_json(&json).unwrap(), whole);
    }
}

#[test]
fn dsc_09_union_absorbs() {
    let l = limits(3);
    let a = report_of(&l, &[ex("/u/a", 200, 1), ex("/u/b", 200, 1)]);
    let b = report_of(&l, &[ex("/u/c", 200, 1), ex("/u/d", 500, 1)]);
    let m = merge(a, b, &l);
    assert_eq!(templates(&m), ["/u/{u_id}"]);
    assert_eq!(m.endpoints()[0].stats.statuses.get(&200), Some(&3));
    assert_eq!(m.endpoints()[0].stats.statuses.get(&500), Some(&1));
}

#[test]
fn dsc_09_json_shape() {
    let mut e = ex("/u/1", 200, 3);
    e.host = Some("svc");
    let r = report_of(&limits(3), &[e]);
    let json = r.to_json().unwrap();
    assert!(json.contains("\"version\":1"), "{json}");
    assert_eq!(json, r.clone().to_json().unwrap());
    assert_eq!(Report::from_json(&json).unwrap(), r);
    assert!(Report::from_json(&json.replace("\"version\":1", "\"version\":2")).is_err());
}

#[test]
fn dsc_09_hosts_capped() {
    let l = limits(3);
    let hosts: Vec<String> = (0..20).map(|i| format!("h{i:02}")).collect();
    let mut r = Report::new(None);
    for h in hosts.iter().rev() {
        let mut e = ex("/a", 200, 1);
        e.host = Some(h);
        r.record(&l, &Redactor::default(), &e);
    }
    let kept: Vec<_> = r.endpoints()[0].stats.hosts.iter().cloned().collect();
    assert_eq!(kept, hosts[..16]);
}

mod redaction {
    use super::super::redact::{MARKER, Redactor, path, value_rule};
    use super::super::report::{Exchange, Report};
    use super::super::template::is_param;
    use serde_json::json;

    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";

    fn r() -> Redactor {
        Redactor::new(["x-tenant-key", "PIN"])
    }

    #[test]
    fn dsc_05_built_in_names_and_variants() {
        let r = r();
        let names = [
            "authorization",
            "Proxy-Authorization",
            "cookie",
            "Set-Cookie",
            "password",
            "passwd",
            "secret",
            "token",
            "apikey",
            "auth",
            "session",
            "signature",
            "credential",
            "privatekey",
            "cvv",
            "cvc",
            "cardnumber",
            "pan",
            "iban",
            "ssn",
            "X-Api-Key",
            "client_secret",
            "refreshToken",
            "Session-Id",
            "x-tenant-key",
            "pin",
            "X_TENANT-KEY",
        ];
        for n in names {
            assert!(r.name_matches(n), "{n} should be redacted");
        }
        for n in ["id", "name", "amount", "email", "items"] {
            assert!(!r.name_matches(n), "{n} should be kept");
        }
        // A blank extra name must not match everything.
        assert!(!Redactor::new(["", "-_"]).name_matches("id"));
    }

    #[test]
    fn dsc_05_url_valued_headers_query() {
        let r = r();
        assert_eq!(
            r.header(
                "Location",
                "https://h/cb?code=SplxlOBeZQQ&access_token=abc123&state=ok"
            ),
            "https://h/cb?code=SplxlOBeZQQ&access_token=<redacted>&state=ok"
        );
        assert_eq!(
            r.header("Location", "https://h/cb?token=a&page=2#access_token=T&x=1"),
            "https://h/cb?token=<redacted>&page=2#access_token=<redacted>&x=1"
        );
        assert_eq!(
            r.header("Referer", "https://h/reset?token=abcdef123456"),
            "https://h/reset?token=<redacted>"
        );
        assert_eq!(
            r.header(
                "Link",
                "<https://api/x?api_key=zzz>; rel=next, </p?page=2>; rel=prev"
            ),
            "<https://api/x?api_key=<redacted>>; rel=next, </p?page=2>; rel=prev"
        );
        assert_eq!(
            r.header("Content-Location", "/a/b?secret=1"),
            "/a/b?secret=<redacted>"
        );
        assert_eq!(
            r.header("Refresh", "0; url=https://h/x?auth=1"),
            "0; url=https://h/x?auth=<redacted>"
        );
        assert_eq!(
            r.header("Content-Type", "application/json; charset=utf-8"),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn dsc_06_url_valued_headers_path_and_value() {
        let r = r();
        assert_eq!(
            r.header(
                "Location",
                &format!("https://h/reset/{JWT}/done?next=/home")
            ),
            "https://h/reset/<redacted>/done?next=/home"
        );
        assert_eq!(
            r.header("Referer", &format!("https://h/x?v={JWT}")),
            "https://h/x?v=<redacted>"
        );
        assert_eq!(r.header("Location", "https://u:p@h/x"), MARKER);
        assert_eq!(
            r.header("Location", "https://h/plain/path"),
            "https://h/plain/path"
        );
    }

    #[test]
    fn dsc_05_headers_queries_forms() {
        let r = r();
        assert_eq!(r.header("X-Api-Key", "abc"), MARKER);
        assert_eq!(r.header("Authorization", "Bearer x"), "Bearer <redacted>");
        assert_eq!(r.header("Authorization", "x"), MARKER);
        assert_eq!(
            r.header("Proxy-Authorization", "Basic eDp5"),
            "Basic <redacted>"
        );
        assert_eq!(r.header("Cookie", "a=1; b=2"), "a=<redacted>; b=<redacted>");
        assert_eq!(
            r.header("Set-Cookie", "s=1; Path=/; HttpOnly"),
            "s=<redacted>; Path=/; HttpOnly"
        );
        assert_eq!(r.header("x-custom", "plain"), "plain");
        assert_eq!(r.field("token", "abc"), MARKER);
        assert_eq!(r.field("refreshToken", "abc"), MARKER);
        assert_eq!(r.field("page", "2"), "2");
    }

    #[test]
    fn dsc_05_json_keys_nested_and_subtrees() {
        let r = r();
        let mut v = json!({
            "id": 1,
            "name": "n",
            "amount": 5,
            "password": "p",
            "user": {"client_secret": {"a": [1, 2]}, "ok": true},
            "list": [{"refreshToken": [1, 2, 3], "id": 2}],
        });
        r.json(&mut v);
        assert_eq!(
            v,
            json!({
                "id": 1,
                "name": "n",
                "amount": 5,
                "password": MARKER,
                "user": {"client_secret": MARKER, "ok": true},
                "list": [{"refreshToken": MARKER, "id": 2}],
            })
        );
    }

    #[test]
    fn dsc_06_values() {
        let secrets = [
            JWT,
            "-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----",
            "Bearer abc.def",
            "Basic dTpw",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "4111111111111111",
            "AKIAIOSFODNN7EXAMPLE",
            "ASIAIOSFODNN7EXAMPLE",
            "ghp_abc123",
            "gho_abc123",
            "ghu_abc123",
            "ghs_abc123",
            "ghr_abc123",
            "github_pat_abc123",
            "glpat-abc123",
            "xoxb-1-2",
            "xoxa-1-2",
            "xoxp-1-2",
            "xoxr-1-2",
            "xoxs-1-2",
            "sk_live_abc",
            "rk_live_abc",
            "sk-abc123",
            "https://u:p@h/",
        ];
        let r = r();
        for s in secrets {
            assert!(value_rule(s).is_some(), "{s:?} should be detected");
            assert_eq!(r.header("x-custom", s), MARKER, "header {s:?}");
            assert_eq!(r.field("q", s), MARKER, "query {s:?}");
            let mut v = json!({ "k": s });
            r.json(&mut v);
            assert_eq!(v, json!({ "k": MARKER }), "json {s:?}");
            if !s.contains(['/', '\n']) {
                assert_eq!(path(&format!("/a/{s}")), "/a/<redacted>", "path {s:?}");
            }
        }
        let near = [
            "4111111111111112",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0",
            "e30.e30.c2ln",
            "AKIAIOSFODNN7EXAMPL",
            "https://u@h/",
            "https://h/path",
            "hello",
            "12345",
        ];
        for s in near {
            assert!(value_rule(s).is_none(), "{s:?} should be kept");
            assert_eq!(r.header("x-custom", s), s);
        }
        // A card as a JSON number.
        let mut v = json!({"n": 4111111111111111u64, "m": 4111111111111112u64});
        r.json(&mut v);
        assert_eq!(v, json!({"n": MARKER, "m": 4111111111111112u64}));
    }

    #[test]
    fn dsc_06_redacted_segment_is_a_parameter_and_marker_is_constant() {
        for s in [
            JWT,
            "4111-1111-1111-1111",
            "ghp_abc",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(is_param(s), "{s}");
        }
        assert_eq!(path("/reset/ghp_a"), path("/reset/ghp_b"));
        assert_eq!(path("/reset/ghp_a"), "/reset/<redacted>");
    }

    #[test]
    fn dsc_06_a_grpc_path_is_redacted_but_not_templated() {
        let secret = format!("/pkg.Svc/{JWT}");
        let exchange = Exchange {
            grpc: true,
            ..super::ex(&secret, 200, 1)
        };
        let mut report = Report::new(None);
        report.record(&super::limits(50), &Redactor::default(), &exchange);
        let json = report.to_json().unwrap();
        assert!(!json.contains(JWT), "{json}");
        assert!(json.contains("/pkg.Svc/<redacted>"), "{json}");
        // Not templated: an id-like segment stays literal.
        report.record(
            &super::limits(50),
            &Redactor::default(),
            &Exchange {
                grpc: true,
                ..super::ex("/pkg.Svc/42", 200, 1)
            },
        );
        assert!(report.to_json().unwrap().contains("/pkg.Svc/42"));
    }

    #[test]
    fn dsc_21_path_segment_by_segment() {
        assert_eq!(path(&format!("/reset/{JWT}")), "/reset/<redacted>");
        assert_eq!(path("/a/b/c"), "/a/b/c");
        assert_eq!(path("/a//b/"), "/a//b/");
        assert_eq!(path("/"), "/");
        assert_eq!(path("/pay/4111-1111-1111-1111/ok"), "/pay/<redacted>/ok");
    }
}

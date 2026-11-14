//! MSH-12: `View::from_json` reads what `build` writes and refuses the rest.

use super::input::*;
use super::testkit::*;
use super::view::{Built, Limits, View, build};
use crate::Mode;

fn rich() -> Built {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.12.7"),
        svc(
            "shop",
            "db",
            "None",
            &[(5432, None, TargetPort::Number(5432))],
        ),
    ];
    s.slices = vec![slice(
        "shop",
        "cart-1",
        "cart",
        4,
        8080,
        vec![
            zoned("10.244.1.17", "a"),
            zoned("fd00::1", "b"),
            ep("10.244.1.1", true),
        ],
    )];
    s.configs = vec![cfg("shop", "c", 1, &[(TargetKind::Deployment, "cart")])];
    build(&s, "shop", "cart", Mode::Mock, &Limits::default())
}

fn read(text: &str) -> Result<View, String> {
    View::from_json(text, &Limits::default())
}

#[test]
fn msh_12_a_built_view_reads_back_equal_with_the_same_bytes() {
    let b = rich();
    let back = read(&b.json).expect("a built view reads back");
    assert_eq!(back, b.view);
    assert_eq!(back.to_json(), b.json);
    assert_eq!(super::view::etag(&back.to_json()), b.etag);
}

#[test]
fn msh_12_an_unknown_field_is_refused_at_every_level() {
    let b = rich();
    for (from, to) in [
        (r#"{"cluster""#, r#"{"extra":1,"cluster""#),
        (r#""shop/cart":{"#, r#""shop/cart":{"extra":1,"#),
        (r#""80":{"name""#, r#""80":{"extra":1,"name""#),
        (r#"[{"ip""#, r#"[{"extra":1,"ip""#),
        (r#""policy":{"#, r#""policy":{"extra":1,"#),
        (r#""loadBalancer":{"#, r#""loadBalancer":{"extra":1,"#),
    ] {
        assert!(b.json.contains(from), "{from}");
        assert!(read(&b.json.replacen(from, to, 1)).is_err(), "{from}");
    }
}

#[test]
fn msh_12_a_wrong_format_mode_address_port_or_key_is_refused() {
    let json = rich().json;
    let bad = |from: &str, to: &str| {
        assert!(json.contains(from), "{from}");
        assert!(read(&json.replacen(from, to, 1)).is_err(), "{to}");
    };
    bad(r#""format":1"#, r#""format":0"#);
    bad(r#""format":1"#, r#""format":2"#);
    bad(r#""mode":"mock""#, r#""mode":"record""#);
    bad(r#""ip":"10.244.1.1""#, r#""ip":"10.244.1""#);
    bad(r#""port":8080"#, r#""port":70000"#);
    bad(r#""shop/cart""#, r#""cart""#);
    bad(r#""shop/cart""#, r#""shop/cart/x""#);
    bad(r#""shop/cart""#, r#""/cart""#);
    bad(r#""ips":["10.96.12.7"]"#, r#""ips":["nope"]"#);
    bad(r#""slowStartSeconds":30"#, r#""slowStartSeconds":3601"#);
    bad(r#""mesh":true"#, r#""mesh":"yes""#);
}

#[test]
fn msh_12_a_missing_field_trailing_bytes_and_non_objects_are_refused() {
    let json = rich().json;
    assert!(read(&json.replacen(r#""mesh":true,"#, "", 1)).is_err());
    assert!(read(&json.replacen(r#""cluster":"local","#, "", 1)).is_err());
    assert!(read(&[json.as_str(), " x"].concat()).is_err());
    assert!(read(&[json.as_str(), json.as_str()].concat()).is_err());
    for (i, text) in ["", "null", "[]", "1", "\"x\"", "{", "{}"]
        .iter()
        .enumerate()
    {
        assert!(read(text).is_err(), "document {i}");
    }
}

#[test]
fn msh_12_a_document_over_the_bounds_is_refused() {
    let b = rich();
    let n = b.view.services.len();
    assert!(n >= 2 && b.view.endpoint_count() == 3);
    let limits = |max_services, max_endpoints, max_view_bytes| Limits {
        max_services,
        max_endpoints,
        max_view_bytes,
    };
    assert!(View::from_json(&b.json, &limits(n, 3, b.json.len())).is_ok());
    assert!(View::from_json(&b.json, &limits(n - 1, 3, b.json.len())).is_err());
    assert!(View::from_json(&b.json, &limits(n, 2, b.json.len())).is_err());
    assert!(View::from_json(&b.json, &limits(n, 3, b.json.len() - 1)).is_err());
}

#[test]
fn msh_12_an_error_message_is_short_whatever_the_input() {
    let json = rich().json;
    let long = "x".repeat(100_000);
    let inputs = [
        json.replacen(r#"{"cluster""#, &format!(r#"{{"{long}":1,"cluster""#), 1),
        json.replacen(r#""mode":"mock""#, &format!(r#""mode":"{long}""#), 1),
        json.replacen(r#""shop/cart""#, &format!(r#""{long}""#), 1),
        long.clone(),
    ];
    for input in inputs {
        let e = View::from_json(&input, &Limits::default()).expect_err("refused");
        assert!(e.len() <= 200, "{} bytes", e.len());
    }
}

#[test]
fn msh_12_a_generations_key_without_a_uid_is_refused() {
    let b = rich();
    let key = r#""mimicconfig/shop/c@uid-c""#;
    assert!(b.json.contains(key));
    for bad in [
        r#""mimicconfig/shop/c""#,
        r#""mimicconfig/shop/c@""#,
        r#""mimicconfig/c@uid-c""#,
        r#""mimicconfig/shop//c@uid-c""#,
    ] {
        assert!(read(&b.json.replacen(key, bad, 1)).is_err(), "{bad}");
    }
    let slice = r#""endpointslice/shop/cart-1@uid-cart-1""#;
    assert!(b.json.contains(slice));
    assert!(read(&b.json.replacen(slice, r#""endpointslice/shop/cart-1""#, 1)).is_err());
}

#[test]
fn msh_06_an_object_created_again_has_another_generations_key() {
    let first = rich();
    let mut s = snapshot();
    s.services = vec![http_svc("shop", "cart", "10.96.12.7")];
    let mut config = cfg("shop", "c", 1, &[(TargetKind::Deployment, "cart")]);
    config.uid = "uid-c-recreated".into();
    config.generation = 1;
    s.configs = vec![config];
    let again = build(&s, "shop", "cart", Mode::Mock, &Limits::default());
    assert_eq!(first.view.generations["mimicconfig/shop/c@uid-c"], 1);
    assert_eq!(
        again.view.generations["mimicconfig/shop/c@uid-c-recreated"],
        1
    );
    assert!(
        !again
            .view
            .generations
            .contains_key("mimicconfig/shop/c@uid-c")
    );
}

#[test]
fn rty_45_from_json_reads_back_a_policy_with_retries_and_refuses_the_rest() {
    let mut s = snapshot();
    s.services = vec![http_svc("shop", "cart", "10.96.12.7")];
    let mut c = cfg("shop", "c", 1, &[(TargetKind::Service, "cart")]);
    c.retries = Some(0);
    c.retry_budget = Some(RetryBudgetSpec {
        percent: Some(100),
        min_per_second: Some(1000),
        window_seconds: Some(60),
    });
    s.configs = vec![c];
    let b = build(&s, "shop", "cart", Mode::Mock, &Limits::default());
    let back = read(&b.json).expect("a built view reads back");
    assert_eq!(back, b.view);
    assert_eq!(back.to_json(), b.json);

    let json = rich().json;
    let bad = |from: &str, to: &str| {
        assert!(json.contains(from), "{from}");
        assert!(read(&json.replacen(from, to, 1)).is_err(), "{to}");
    };
    bad(r#""retryBudget":{"#, r#""retryBudget":{"extra":1,"#);
    bad(r#""retries":3,"#, "");
    bad(
        r#","retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}"#,
        "",
    );
    bad(r#""minPerSecond":10,"#, "");
    bad(r#""percent":20,"#, "");
    bad(r#""windowSeconds":10"#, r#""windowSeconds":10,"x":1"#);
    bad(r#""retries":3"#, r#""retries":11"#);
    bad(r#""percent":20"#, r#""percent":101"#);
    bad(r#""minPerSecond":10"#, r#""minPerSecond":1001"#);
    bad(r#""windowSeconds":10"#, r#""windowSeconds":0"#);
    bad(r#""windowSeconds":10"#, r#""windowSeconds":61"#);
    bad(r#""retries":3"#, r#""retries":-1"#);
    bad(r#""retries":3"#, r#""retries":"3""#);
    for ok in [
        (r#""retries":3"#, r#""retries":10"#),
        (r#""windowSeconds":10"#, r#""windowSeconds":1"#),
        (r#""windowSeconds":10"#, r#""windowSeconds":60"#),
    ] {
        assert!(read(&json.replacen(ok.0, ok.1, 1)).is_ok(), "{}", ok.1);
    }
}

// RTE-12: routes.

fn routed() -> Built {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("shop", "cart-v1", "10.96.0.2"),
        http_svc("shop", "cart-shadow", "10.96.0.3"),
    ];
    let pairs = |l: &[(&str, &str)]| l.iter().map(|(n, v)| ((*n).into(), (*v).into())).collect();
    let modifier = HeaderModifier {
        set: pairs(&[("x-env", "prod")]),
        add: pairs(&[("x-from", "mesh")]),
        remove: vec!["x-debug".into()],
    };
    let filter = |kind: &str| RouteFilter {
        kind: kind.into(),
        request_header_modifier: (kind == "RequestHeaderModifier").then(|| modifier.clone()),
        response_header_modifier: (kind == "ResponseHeaderModifier").then(|| modifier.clone()),
        request_mirror: None,
    };
    let rule = RouteRule {
        name: Some("canary".into()),
        matches: vec![RouteMatch {
            path: Some(PathMatch {
                kind: "PathPrefix".into(),
                value: "/cart".into(),
            }),
            headers: vec![NameMatch {
                kind: "Exact".into(),
                name: "x-canary".into(),
                value: "1".into(),
            }],
            query_params: vec![NameMatch {
                kind: "Exact".into(),
                name: "v".into(),
                value: "2".into(),
            }],
            method: Some("GET".into()),
        }],
        backend_refs: vec![
            backend(None, "cart-v1", 80, Some(90)),
            backend(None, "missing", 80, Some(10)),
        ],
        filters: vec![
            filter("RequestHeaderModifier"),
            filter("ResponseHeaderModifier"),
            RouteFilter {
                kind: "RequestMirror".into(),
                request_header_modifier: None,
                response_header_modifier: None,
                request_mirror: Some(Mirror {
                    group: String::new(),
                    kind: "Service".into(),
                    namespace: None,
                    name: "cart-shadow".into(),
                    port: Some(80),
                    percent: Some(10),
                    fraction: None,
                }),
            },
        ],
        request_timeout: Some("2s".into()),
        backend_request_timeout: Some("500ms".into()),
        has_retry: false,
    };
    s.routes = vec![route_at(
        "shop",
        "canary",
        1,
        vec![parent(None, "cart", Some(80))],
        vec![rule],
    )];
    build(&s, "shop", "cart", Mode::Mock, &Limits::default())
}

const RULE: &str = "/services/shop~1cart/routes/0/rules/0";

fn pointer(path: &str) -> String {
    format!("{RULE}{path}")
}

fn mutated(f: impl FnOnce(&mut serde_json::Value)) -> Result<View, String> {
    let mut v: serde_json::Value = serde_json::from_str(&routed().json).unwrap();
    f(&mut v);
    read(&v.to_string())
}

fn set(path: &str, value: serde_json::Value) -> Result<View, String> {
    mutated(|v| *v.pointer_mut(path).unwrap_or_else(|| panic!("{path}")) = value)
}

fn rte_12_refused(label: &str, result: Result<View, String>) {
    assert!(result.is_err(), "{label} was accepted");
}

fn grown(path: &str, n: usize) -> Result<View, String> {
    mutated(|v| {
        let list = v.pointer_mut(path).unwrap_or_else(|| panic!("{path}"));
        let first = list[0].clone();
        *list = serde_json::Value::Array(vec![first; n]);
    })
}

#[test]
fn rte_12_a_view_with_routes_reads_back_equal_with_the_same_bytes() {
    let b = routed();
    assert!(
        b.json
            .contains(r#""routes":[{"name":"shop/canary","ports":[80]"#)
    );
    let back = read(&b.json).expect("a built view reads back");
    assert_eq!(back, b.view);
    assert_eq!(back.to_json(), b.json);
}

#[test]
fn rte_12_a_backend_or_mirror_of_a_service_not_held_or_a_port_it_lacks_is_refused() {
    use serde_json::json;
    rte_12_refused(
        "backend service",
        set(&pointer("/backends/0/service"), json!("shop/nothere")),
    );
    rte_12_refused("backend port", set(&pointer("/backends/0/port"), json!(81)));
    rte_12_refused(
        "mirror service",
        set(
            &pointer("/filters/mirrors/0/service"),
            json!("shop/nothere"),
        ),
    );
    rte_12_refused(
        "mirror port",
        set(&pointer("/filters/mirrors/0/port"), json!(81)),
    );
    rte_12_refused(
        "ports entry",
        set("/services/shop~1cart/routes/0/ports", json!([81])),
    );
    rte_12_refused(
        "empty ports",
        set("/services/shop~1cart/routes/0/ports", json!([])),
    );
    // A backend that is invalid and names a Service, or valid and names none.
    rte_12_refused(
        "invalid with a name",
        mutated(|v| {
            v.pointer_mut(&pointer("/backends/1"))
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("service".into(), json!("shop/cart"));
        }),
    );
    rte_12_refused(
        "valid without a name",
        mutated(|v| {
            v.pointer_mut(&pointer("/backends/0"))
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("service");
        }),
    );
}

#[test]
fn rte_12_one_item_over_each_crd_maximum_is_refused() {
    for (path, max) in [
        ("/services/shop~1cart/routes/0/rules", 16),
        (&pointer("/matches"), 64),
        (&pointer("/matches/0/headers"), 16),
        (&pointer("/matches/0/queryParams"), 16),
        (&pointer("/backends"), 16),
        (&pointer("/filters/mirrors"), 16),
        (&pointer("/filters/requestHeaders/set"), 16),
        (&pointer("/filters/requestHeaders/add"), 16),
        (&pointer("/filters/requestHeaders/remove"), 16),
        (&pointer("/filters/responseHeaders/set"), 16),
    ] {
        let at_max = grown(path, max);
        // Backends, mirrors and rules at the maximum may name what the view holds.
        assert!(at_max.is_ok(), "{path} at {max}: {at_max:?}");
        rte_12_refused(path, grown(path, max + 1));
    }
}

#[test]
fn rte_12_header_names_must_be_lower_case_tokens() {
    use serde_json::json;
    for path in [
        pointer("/matches/0/headers/0/name"),
        pointer("/filters/requestHeaders/set/0/name"),
        pointer("/filters/requestHeaders/add/0/name"),
        pointer("/filters/responseHeaders/set/0/name"),
    ] {
        for bad in ["X-Canary", "a b", "a:b", "", "é"] {
            rte_12_refused(&format!("{path} {bad}"), set(&path, json!(bad)));
        }
        assert!(set(&path, json!("x-ok_1.2")).is_ok(), "{path}");
    }
    rte_12_refused(
        "remove",
        set(
            &pointer("/filters/requestHeaders/remove/0"),
            json!("X-Debug"),
        ),
    );
}

#[test]
fn rte_12_each_refused_modifier_name_is_refused_in_set_add_and_remove() {
    use serde_json::json;
    let refused = [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
        "x-mimic-error",
        "x-mimic-anything",
    ];
    for side in ["requestHeaders", "responseHeaders"] {
        for name in refused {
            for list in ["set", "add"] {
                let path = pointer(&format!("/filters/{side}/{list}/0/name"));
                rte_12_refused(&format!("{side} {list} {name}"), set(&path, json!(name)));
            }
            rte_12_refused(
                &format!("{side} remove {name}"),
                set(&pointer(&format!("/filters/{side}/remove/0")), json!(name)),
            );
        }
    }
    for ok in ["x-mimic", "hostname", "x-content-length"] {
        assert!(
            set(&pointer("/filters/requestHeaders/set/0/name"), json!(ok)).is_ok(),
            "{ok}"
        );
    }
}

#[test]
fn rte_12_values_paths_methods_weights_and_fractions_are_bounded() {
    use serde_json::json;
    for path in [
        pointer("/matches/0/headers/0/value"),
        pointer("/matches/0/queryParams/0/value"),
        pointer("/filters/requestHeaders/set/0/value"),
        pointer("/filters/requestHeaders/add/0/value"),
        pointer("/filters/responseHeaders/set/0/value"),
    ] {
        for bad in ["a\rb", "a\nb", "a\0b"] {
            rte_12_refused(&path, set(&path, json!(bad)));
        }
        rte_12_refused(&path, set(&path, json!("a".repeat(4097))));
        assert!(set(&path, json!("a".repeat(4096))).is_ok(), "{path}");
    }
    let path = pointer("/matches/0/path/value");
    for bad in ["cart".to_string(), format!("/{}", "a".repeat(1024))] {
        rte_12_refused(&bad[..bad.len().min(8)], set(&path, json!(bad)));
    }
    assert!(set(&path, json!(format!("/{}", "a".repeat(1023)))).is_ok());
    let method = pointer("/matches/0/method");
    for bad in ["get", "PROPFIND", ""] {
        rte_12_refused(bad, set(&method, json!(bad)));
    }
    for ok in [
        "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
    ] {
        assert!(set(&method, json!(ok)).is_ok(), "{ok}");
    }
    let weight = pointer("/backends/0/weight");
    rte_12_refused("weight", set(&weight, json!(1_000_001)));
    assert!(set(&weight, json!(1_000_000)).is_ok());
    let m = pointer("/filters/mirrors/0");
    for (n, d) in [(1, 0), (3, 2)] {
        rte_12_refused(
            "fraction",
            mutated(|v| {
                *v.pointer_mut(&format!("{m}/numerator")).unwrap() = json!(n);
                *v.pointer_mut(&format!("{m}/denominator")).unwrap() = json!(d);
            }),
        );
    }
}

#[test]
fn rte_12_an_unknown_field_is_refused_in_every_route_object() {
    use serde_json::json;
    for at in [
        "/services/shop~1cart/routes/0".to_string(),
        RULE.to_string(),
        pointer("/matches/0"),
        pointer("/matches/0/path"),
        pointer("/matches/0/headers/0"),
        pointer("/backends/0"),
        pointer("/filters"),
        pointer("/filters/requestHeaders"),
        pointer("/filters/requestHeaders/set/0"),
        pointer("/filters/mirrors/0"),
        pointer("/timeouts"),
    ] {
        rte_12_refused(
            &at,
            mutated(|v| {
                v.pointer_mut(&at)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert("extra".into(), json!(1));
            }),
        );
    }
}

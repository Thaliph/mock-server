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
    bad(r#","retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}"#, "");
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

//! ADM-09, ADM-10: the admission contract of a policy and of the view.

use super::admission::Contract;
use super::input::*;
use super::policy::{contract, statuses, validate};
use super::testkit::*;
use super::view::{Built, Limits, View, build};
use crate::Mode;

fn cluster() -> Snapshot {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("risk", "scoring", "10.50.0.1"),
    ];
    s.deployments = vec![("shop".into(), "cart".into())];
    s
}

fn refused(config: &Config, needle: &str) {
    let err = validate(config).expect_err(needle);
    assert!(err.contains(needle), "{err:?} lacks {needle:?}");
}

fn spec(concurrency: Option<u32>, rps: Option<u32>) -> Option<AdmissionSpec> {
    Some(AdmissionSpec { concurrency, rps })
}

/// Config `name` (created `created`) on Deployment `cart` with `admission`.
fn on_cart(name: &str, created: i64, admission: Option<AdmissionSpec>) -> Config {
    Config {
        admission,
        ..cfg("shop", name, created, &[(TargetKind::Deployment, "cart")])
    }
}

fn c(concurrency: u32, rps: u32) -> Config {
    on_cart("c", 1, spec(Some(concurrency), Some(rps)))
}

fn built_with(configs: Vec<Config>) -> Built {
    let mut s = cluster();
    s.configs = configs;
    view_of(&s, "shop", "cart")
}

fn read(text: &str) -> Result<View, String> {
    View::from_json(text, &Limits::default())
}

fn contract_of(concurrency: u32, rps: u32) -> Option<Contract> {
    Some(Contract { concurrency, rps })
}

#[test]
fn adm_09_no_config_or_a_config_without_admission_gives_no_contract() {
    assert_eq!(contract(None), None);
    let b = built_with(vec![]);
    assert_eq!(b.view.admission, None);
    assert!(!b.json.contains("admission"), "{}", b.json);
    let plain = on_cart("c", 1, None);
    assert_eq!(contract(Some(&plain)), None);
    let b = built_with(vec![plain]);
    assert_eq!(b.view.admission, None);
    assert!(!b.json.contains("admission"), "{}", b.json);
}

#[test]
fn adm_09_the_winning_contract_is_taken_whole_also_at_the_bounds() {
    let a = on_cart("a", 1, spec(Some(1), Some(1)));
    let b = on_cart("b", 2, spec(Some(100_000), Some(1_000_000)));
    let view = built_with(vec![a.clone(), b.clone()]).view;
    assert_eq!(view.admission, contract_of(1, 1));
    assert_eq!(validate(&b), Ok(()));
    assert_eq!(contract(Some(&b)), contract_of(100_000, 1_000_000));
    assert_eq!(
        built_with(vec![b]).view.admission,
        contract_of(100_000, 1_000_000)
    );
    // The older winner sets none: the newer one's contract is not merged in.
    let plain = on_cart("a", 1, None);
    let newer = on_cart("b", 2, spec(Some(7), Some(7)));
    assert_eq!(built_with(vec![plain, newer]).view.admission, None);
}

#[test]
fn adm_09_validate_refuses_each_bad_admission_naming_the_field() {
    for bad in [0, 100_001, u32::MAX] {
        refused(&c(bad, 200), "admission.concurrency is not 1 to 100000");
    }
    for bad in [0, 1_000_001] {
        refused(&c(64, bad), "admission.rps is not 1 to 1000000");
    }
    refused(
        &on_cart("c", 1, spec(None, Some(5))),
        "admission.concurrency is required",
    );
    refused(
        &on_cart("c", 1, spec(Some(5), None)),
        "admission.rps is required",
    );
    refused(
        &on_cart("c", 1, spec(None, None)),
        "admission.concurrency is required",
    );
    let service_only = Config {
        admission: spec(Some(64), Some(200)),
        ..cfg("shop", "c", 1, &[(TargetKind::Service, "cart")])
    };
    refused(&service_only, "admission fits a Deployment target only");
    let both = Config {
        admission: spec(Some(64), Some(200)),
        ..cfg(
            "shop",
            "c",
            1,
            &[
                (TargetKind::Deployment, "cart"),
                (TargetKind::Service, "cart"),
            ],
        )
    };
    assert_eq!(validate(&both), Ok(()));
    for (concurrency, rps) in [(1, 1), (100_000, 1_000_000), (64, 200)] {
        assert_eq!(validate(&c(concurrency, rps)), Ok(()));
    }
}

#[test]
fn adm_09_a_deployment_and_a_service_target_put_the_contract_in_the_deployment_view_only() {
    let mut s = cluster();
    s.deployments.push(("shop".into(), "front".into()));
    s.configs = vec![Config {
        admission: spec(Some(64), Some(200)),
        ..cfg(
            "shop",
            "c",
            1,
            &[
                (TargetKind::Deployment, "cart"),
                (TargetKind::Service, "cart"),
            ],
        )
    }];
    assert_eq!(
        view_of(&s, "shop", "cart").view.admission,
        contract_of(64, 200)
    );
    let front = view_of(&s, "shop", "front");
    assert_eq!(front.view.admission, None);
    assert!(front.view.services.contains_key("shop/cart"));
}

#[test]
fn adm_09_an_invalid_admission_makes_the_config_invalid_and_takes_no_part() {
    let mut s = cluster();
    s.configs = vec![
        on_cart("a", 1, spec(Some(0), Some(5))),
        on_cart("b", 2, spec(Some(5), Some(5))),
    ];
    let st = statuses(&s);
    let a = &st[&("shop".to_string(), "a".to_string())];
    assert_eq!(a.accepted.reason, "Invalid");
    assert!(
        a.accepted.message.contains("admission.concurrency"),
        "{}",
        a.accepted.message
    );
    let b = &st[&("shop".to_string(), "b".to_string())];
    assert!(b.accepted.ok, "{b:?}");
    assert_eq!(b.accepted.reason, "Accepted");
    assert_eq!(
        view_of(&s, "shop", "cart").view.admission,
        contract_of(5, 5)
    );
}

#[test]
fn adm_10_the_view_holds_the_contract_as_its_first_key_with_sorted_keys() {
    let b = built_with(vec![c(64, 200)]);
    assert!(
        b.json
            .starts_with(r#"{"admission":{"concurrency":64,"rps":200},"cluster":"local","#),
        "{}",
        b.json
    );
}

#[test]
fn adm_10_a_view_of_mesh_false_has_no_admission() {
    let off = Config {
        mesh_enabled: Some(false),
        ..c(64, 200)
    };
    assert_eq!(validate(&off), Ok(()));
    let b = built_with(vec![off]);
    assert!(!b.view.mesh);
    assert_eq!(b.view.admission, None);
    assert!(!b.json.contains("admission"), "{}", b.json);
}

#[test]
fn adm_10_a_change_of_either_value_or_of_its_presence_gives_a_new_etag() {
    let etags: Vec<String> = [on_cart("c", 1, None), c(64, 200), c(65, 200), c(64, 201)]
        .into_iter()
        .map(|config| built_with(vec![config]).etag)
        .collect();
    for (i, a) in etags.iter().enumerate() {
        for b in &etags[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert_eq!(built_with(vec![c(64, 200)]).etag, etags[1]);
}

#[test]
fn adm_10_from_json_reads_back_the_contract_and_no_field_is_no_contract() {
    for (concurrency, rps) in [(64, 200), (1, 1), (100_000, 1_000_000)] {
        let b = built_with(vec![c(concurrency, rps)]);
        let back = read(&b.json).expect("a built view reads back");
        assert_eq!(back, b.view);
        assert_eq!(back.admission, contract_of(concurrency, rps));
        assert_eq!(back.to_json(), b.json);
    }
    let b = built_with(vec![on_cart("c", 1, None)]);
    assert_eq!(read(&b.json).expect("reads back").admission, None);
}

#[test]
fn adm_10_from_json_refuses_what_validate_refuses_and_admission_with_mesh_false() {
    let json = built_with(vec![c(64, 200)]).json;
    let head = r#"{"admission":{"concurrency":64,"rps":200},"#;
    assert!(json.starts_with(head));
    let bad = |from: &str, to: &str| {
        assert!(json.contains(from), "{from}");
        let err = read(&json.replacen(from, to, 1)).expect_err(to);
        assert!(err.len() <= 200, "{err}");
    };
    let good = |from: &str, to: &str| {
        let text = json.replacen(from, to, 1);
        assert_ne!(text, json);
        assert!(read(&text).is_ok(), "{to}");
    };
    bad(r#"{"concurrency":64"#, r#"{"burst":1,"concurrency":64"#);
    bad(r#""concurrency":64,"#, "");
    bad(r#","rps":200"#, "");
    bad(
        r#""concurrency":64,"#,
        r#""concurrency":64,"concurrency":64,"#,
    );
    for value in ["0", "100001", "-1", r#""64""#, "1.5", "4294967296"] {
        bad(r#""concurrency":64"#, &format!(r#""concurrency":{value}"#));
    }
    for value in ["0", "1000001"] {
        bad(r#""rps":200"#, &format!(r#""rps":{value}"#));
    }
    bad(
        r#""admission":{"concurrency":64,"rps":200}"#,
        r#""admission":[]"#,
    );
    let off = built_with(vec![Config {
        mesh_enabled: Some(false),
        ..on_cart("c", 1, None)
    }])
    .json;
    assert!(off.contains(r#""mesh":false"#));
    let spliced = off.replacen('{', head, 1);
    assert!(read(&off).is_ok());
    let err = read(&spliced).expect_err("admission with mesh false");
    assert!(err.len() <= 200, "{err}");
    good(r#""concurrency":64"#, r#""concurrency":1"#);
    good(r#""concurrency":64"#, r#""concurrency":100000"#);
    good(r#""rps":200"#, r#""rps":1"#);
    good(r#""rps":200"#, r#""rps":1000000"#);
    let null = json.replacen(
        r#""admission":{"concurrency":64,"rps":200}"#,
        r#""admission":null"#,
        1,
    );
    assert_eq!(read(&null).expect("null is no contract").admission, None);
}

#[test]
fn adm_10_the_contract_counts_in_the_byte_bound_of_the_view() {
    let mut s = cluster();
    s.services.truncate(1);
    s.configs = vec![c(64, 200)];
    let full = build(&s, "shop", "cart", Mode::None, &Limits::default());
    assert_eq!(full.view.services.len(), 1);
    let l = full.json.len();
    let at = |max_view_bytes: usize| {
        build(
            &s,
            "shop",
            "cart",
            Mode::None,
            &Limits {
                max_view_bytes,
                ..Limits::default()
            },
        )
    };
    let exact = at(l);
    assert_eq!((exact.view.services.len(), exact.truncated), (1, 0));
    assert_eq!(exact.view.admission, contract_of(64, 200));
    let short = at(l - 1);
    assert_eq!(short.truncated, 1);
    assert!(
        short.json.len() <= l - 1,
        "{} over {}",
        short.json.len(),
        l - 1
    );
    assert_eq!(short.view.admission, contract_of(64, 200));
}

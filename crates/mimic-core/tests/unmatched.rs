//! Critères MCK-25 et RED-07 de la spec (§5.3, §7) : relevé des requêtes sans réponse du
//! mock et forme affichée du chemin, sans valeur.

use mimic_core::unmatched::{
    MAX_PATH, Reason, Unmatched, UnmatchedEntry, UnmatchedLog, UnmatchedReport, format_utc,
    path_template,
};
use proptest::prelude::*;

const SAAS: &str = "api.saas.io:443";

fn u(reason: Reason, method: &str, path: &str) -> Unmatched {
    Unmatched::new(reason, SAAS, method, path)
}

fn entry(
    method: &str,
    path: &str,
    reason: Reason,
    count: u64,
    first: u64,
    last: u64,
) -> UnmatchedEntry {
    UnmatchedEntry {
        authority: SAAS.into(),
        method: method.into(),
        path: path.into(),
        reason,
        count,
        first_ms: first,
        last_ms: last,
    }
}

#[test]
fn mck_25_requests_are_aggregated_by_key_with_bounded_keys() {
    let mut log = UnmatchedLog::new(3);
    log.record(&u(Reason::Unmatched, "GET", "/users/1"), 1_000);
    log.record(&u(Reason::Unmatched, "GET", "/users/2?email=a@b.io"), 2_000);
    log.record(&u(Reason::Unmatched, "POST", "/orders"), 3_000);
    log.record(&u(Reason::TooLarge, "POST", "/upload"), 4_000);
    assert_eq!(log.total(), 4);
    // `/users/1` et `/users/2` ont la même forme : trois clés, la capacité est atteinte.
    let r = log.report(10);
    assert_eq!(r.total, 4);
    assert_eq!(
        r.entries,
        vec![
            entry("POST", "/upload", Reason::TooLarge, 1, 4_000, 4_000),
            entry("POST", "/orders", Reason::Unmatched, 1, 3_000, 3_000),
            entry("GET", "/users/{int}", Reason::Unmatched, 2, 1_000, 2_000),
        ]
    );

    // Deux clés nouvelles : les deux vues le moins récemment sont évincées.
    log.record(&u(Reason::Unmatched, "GET", "/a"), 5_000);
    log.record(&u(Reason::Unmatched, "GET", "/b"), 5_000);
    let r = log.report(10);
    assert_eq!(r.total, 6, "le total compte les clés évincées");
    assert_eq!(log.len(), 3);
    let keys: Vec<(&str, &str)> = r
        .entries
        .iter()
        .map(|e| (e.method.as_str(), e.path.as_str()))
        .collect();
    // Même dernière occurrence : ordre de la clé.
    assert_eq!(keys, [("GET", "/a"), ("GET", "/b"), ("POST", "/upload")]);

    // `limit` respecté, en gardant les plus récentes.
    assert_eq!(log.report(2).entries.len(), 2);
    assert_eq!(log.report(2).total, 6);

    // Remise à zéro.
    log.clear();
    assert_eq!(log.total(), 0);
    assert!(log.is_empty());
    assert_eq!(log.report(10), UnmatchedReport::default());
}

#[test]
fn mck_25_report_does_not_depend_on_arrival_order() {
    let requests = [
        (Reason::Unmatched, "GET", "/users/1"),
        (Reason::Unmatched, "GET", "/users/2"),
        (Reason::Unmatched, "POST", "/orders"),
        (Reason::TooLarge, "PUT", "/files/9"),
        (Reason::Unmatched, "DELETE", "/carts/7"),
    ];
    let mut a = UnmatchedLog::default();
    let mut b = UnmatchedLog::default();
    for (r, m, p) in requests {
        a.record(&u(r, m, p), 7_000);
    }
    for (r, m, p) in requests.iter().rev() {
        b.record(&u(*r, m, p), 7_000);
    }
    assert_eq!(a.report(100), b.report(100));
    assert_eq!(
        serde_json::to_string(&a.report(100)).unwrap(),
        serde_json::to_string(&b.report(100)).unwrap()
    );
}

#[test]
fn mck_25_fields_are_bounded_and_serialized() {
    let long_host = format!("{}:443", "h".repeat(400));
    let x = Unmatched::new(
        Reason::Unmatched,
        &long_host,
        "PROPFINDPROPFINDPROPFIND",
        "/",
    );
    assert_eq!(x.authority.chars().count(), 255);
    assert_eq!(x.method.chars().count(), 16);
    assert_eq!(x.path, "/");
    let mut log = UnmatchedLog::default();
    log.record(&u(Reason::TooLarge, "POST", "/upload"), 1);
    let v = serde_json::to_value(log.report(10)).unwrap();
    assert_eq!(v["total"], 1);
    assert_eq!(v["entries"][0]["reason"], "too_large");
    assert_eq!(v["entries"][0]["path"], "/upload");
    assert_eq!(v["entries"][0]["first_ms"], 1);
    // Relevé d'un sidecar d'une version antérieure : champs absents.
    let empty: UnmatchedReport = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, UnmatchedReport::default());
    assert_eq!(Reason::Unmatched.status(), 599);
    assert_eq!(Reason::TooLarge.status(), 413);
}

#[test]
fn mck_25_reports_of_several_pods_are_merged() {
    let a = UnmatchedReport {
        total: 3,
        entries: vec![
            entry("POST", "/orders", Reason::Unmatched, 1, 2_000, 2_000),
            entry("GET", "/users/{int}", Reason::Unmatched, 2, 1_000, 1_500),
        ],
    };
    let b = UnmatchedReport {
        total: 5,
        entries: vec![
            entry("GET", "/users/{int}", Reason::Unmatched, 4, 500, 3_000),
            entry("POST", "/upload", Reason::TooLarge, 1, 2_500, 2_500),
        ],
    };
    let m = UnmatchedReport::merge([&a, &b], 50);
    assert_eq!(m.total, 8);
    assert_eq!(
        m.entries,
        vec![
            entry("GET", "/users/{int}", Reason::Unmatched, 6, 500, 3_000),
            entry("POST", "/upload", Reason::TooLarge, 1, 2_500, 2_500),
            entry("POST", "/orders", Reason::Unmatched, 1, 2_000, 2_000),
        ]
    );
    assert_eq!(m, UnmatchedReport::merge([&b, &a], 50));
    let bounded = UnmatchedReport::merge([&a, &b], 2);
    assert_eq!(bounded.entries.len(), 2);
    assert_eq!(bounded.total, 8);
}

#[test]
fn utc_dates_are_readable() {
    assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_utc(1_792_677_787_000), "2026-10-22T14:03:07Z");
}

#[test]
fn red_07_path_shape_hides_identifiers_and_sensitive_values() {
    let cases = [
        ("/", "/"),
        ("", "/"),
        ("/v1/customers/me", "/v1/customers/me"),
        ("/users/42", "/users/{int}"),
        ("/users/42/", "/users/{int}"),
        ("/users/42?email=a@b.io&token=s3cr3t", "/users/{int}"),
        (
            "/orders/7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "/orders/{uuid}",
        ),
        ("/events/01ARZ3NDEKTSV4RRFFQ69G5FAV", "/events/{id}"),
        ("/blobs/deadbeef1234", "/blobs/{id}"),
        ("/days/2026-10-22", "/days/{id}"),
        (
            "/users/jane.doe@example.com/orders",
            "/users/{redacted}/orders",
        ),
        ("/users/jane.doe%40example.com", "/users/{redacted}"),
        (
            "/auth/eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
            "/auth/{redacted}",
        ),
        ("/cards/4242%204242%204242%204242", "/cards/{redacted}"),
        ("/cards/4242424242424242", "/cards/{int}"),
        ("/k/<redacted:sha256:0123abcd>", "/k/{redacted}"),
        (
            "/products/the-very-long-product-name-of-winter",
            "/products/{opaque}",
        ),
        ("/products/blue-shirt", "/products/blue-shirt"),
        ("/keys/sk_test_AbcdefGhijklmnopQRSTuvwx", "/keys/{opaque}"),
        ("/v1/payment_intents", "/v1/payment_intents"),
    ];
    for (path, want) in cases {
        assert_eq!(path_template(path), want, "{path}");
    }
    let long = "/segment".repeat(75);
    let t = path_template(&long);
    assert!(t.chars().count() <= MAX_PATH, "{}", t.chars().count());
    assert!(t.ends_with("/…"), "{t}");
    assert!(t.starts_with("/segment/segment"), "{t}");
}

fn secret() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{3,10}@[a-z]{3,8}\\.(io|com|fr)",
        "[a-z]{3,10}%40[a-z]{3,8}\\.(io|com)",
        "[0-9]{6,18}",
        "eyJ[A-Za-z0-9_-]{8,20}\\.[A-Za-z0-9_-]{8,20}\\.[A-Za-z0-9_-]{8,20}",
        "sk_live_[A-Z][A-Za-z0-9]{23}",
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ]
}

proptest! {
    #[test]
    fn red_07_injected_value_never_appears_in_the_shape(
        prefix in prop::collection::vec("[a-z]{1,8}", 0..4),
        value in secret(),
        suffix in prop::collection::vec("[a-z]{1,8}", 0..3),
        query in "[a-z]{1,6}",
    ) {
        let mut path = String::new();
        for s in &prefix {
            path.push('/');
            path.push_str(s);
        }
        path.push('/');
        path.push_str(&value);
        for s in &suffix {
            path.push('/');
            path.push_str(s);
        }
        path.push_str(&format!("?{query}={value}"));
        let t = path_template(&path);
        prop_assert!(!t.contains(&value), "{path} → {t}");
        prop_assert!(!t.contains('?'), "{t}");
        // Même forme qu'une autre valeur de même nature : la valeur ne change pas la clé.
        let u = Unmatched::new(Reason::Unmatched, SAAS, "GET", &path);
        prop_assert_eq!(u.path, t);
    }
}

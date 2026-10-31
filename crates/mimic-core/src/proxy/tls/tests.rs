use super::*;
use proptest::prelude::*;
use std::net::{Ipv4Addr, Ipv6Addr};

fn ext(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut e = kind.to_be_bytes().to_vec();
    e.extend((data.len() as u16).to_be_bytes());
    e.extend(data);
    e
}

fn sni_ext(name: &str) -> Vec<u8> {
    let host = name.as_bytes();
    let mut d = ((host.len() + 3) as u16).to_be_bytes().to_vec();
    d.push(0);
    d.extend((host.len() as u16).to_be_bytes());
    d.extend(host);
    ext(0, &d)
}

fn alpn_ext(protos: &[&str]) -> Vec<u8> {
    let mut list = Vec::new();
    for p in protos {
        list.push(p.len() as u8);
        list.extend(p.as_bytes());
    }
    let mut d = (list.len() as u16).to_be_bytes().to_vec();
    d.extend(list);
    ext(16, &d)
}

fn versions_ext(versions: &[u16]) -> Vec<u8> {
    let mut d = vec![(versions.len() * 2) as u8];
    for v in versions {
        d.extend(v.to_be_bytes());
    }
    ext(43, &d)
}

fn hello_bytes(legacy: u16, extensions: &[u8]) -> Vec<u8> {
    let mut body = legacy.to_be_bytes().to_vec();
    body.extend([7u8; 32]);
    body.push(0);
    body.extend(2u16.to_be_bytes());
    body.extend([0x13, 0x01]);
    body.extend([1, 0]);
    body.extend((extensions.len() as u16).to_be_bytes());
    body.extend(extensions);
    let mut hs = vec![1];
    hs.extend(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend(body);
    hs
}

fn record(fragment: &[u8]) -> Vec<u8> {
    let mut r = vec![0x16, 3, 1];
    r.extend((fragment.len() as u16).to_be_bytes());
    r.extend(fragment);
    r
}

fn parsed(buf: &[u8]) -> ClientHello {
    match parse_client_hello(buf) {
        Parsed::Hello(h) => h,
        other => panic!("not a hello: {other:?}"),
    }
}

#[test]
fn tls_01_sni_alpn_and_versions() {
    let mut e = sni_ext("Example.COM");
    e.extend(alpn_ext(&["h2", "http/1.1", "imap"]));
    e.extend(versions_ext(&[0x0a0a, 0x0304, 0x0303]));
    let h = parsed(&record(&hello_bytes(0x0303, &e)));
    assert_eq!(h.sni.as_deref(), Some("Example.COM"));
    assert_eq!(h.alpn, ["h2", "http/1.1", "imap"]);
    assert!(h.tls12 && h.tls13);
}

#[test]
fn tls_01_versions_from_supported_versions_else_legacy() {
    let only13 = parsed(&record(&hello_bytes(0x0303, &versions_ext(&[0x0304]))));
    assert!(!only13.tls12 && only13.tls13);
    let old = parsed(&record(&hello_bytes(
        0x0303,
        &versions_ext(&[0x0302, 0x0301]),
    )));
    assert!(!old.tls12 && !old.tls13);
    let legacy12 = parsed(&record(&hello_bytes(0x0303, &[])));
    assert!(legacy12.tls12 && !legacy12.tls13);
    let legacy11 = parsed(&record(&hello_bytes(0x0302, &[])));
    assert!(!legacy11.tls12 && !legacy11.tls13);
}

#[test]
fn tls_01_no_extensions_no_facts() {
    let h = parsed(&record(&hello_bytes(0x0303, &[])));
    assert_eq!(h.sni, None);
    assert!(h.alpn.is_empty());
}

#[test]
fn tls_01_split_over_reads_and_records() {
    let mut e = sni_ext("split.example.org");
    e.extend(alpn_ext(&["h2"]));
    let hs = hello_bytes(0x0303, &e);
    let whole = record(&hs);
    for n in 0..whole.len() {
        assert_eq!(
            parse_client_hello(&whole[..n]),
            Parsed::NeedMore,
            "prefix {n}"
        );
    }
    let want = parsed(&whole);
    assert_eq!(want.sni.as_deref(), Some("split.example.org"));
    let (a, rest) = hs.split_at(10);
    let (b, c) = rest.split_at(20);
    let mut multi = record(a);
    multi.extend(record(b));
    assert_eq!(parse_client_hello(&multi), Parsed::NeedMore);
    multi.extend(record(c));
    assert_eq!(parsed(&multi), want);
}

#[test]
fn tls_01_not_tls_and_malformed() {
    assert_eq!(parse_client_hello(b"GET / HTTP/1.1\r\n"), Parsed::NotTls);
    assert_eq!(
        parse_client_hello(&record(&[2, 0, 0, 1, 0])),
        Parsed::NotTls
    );
    // A truncated hello in a complete record carries no facts.
    let h = parsed(&record(&[1, 0, 0, 4, 3, 3, 0, 0]));
    assert_eq!(h, ClientHello::default());
    // A malformed extension list carries no facts either.
    let mut e = sni_ext("a.test");
    e.extend([0, 16, 0, 9, 1]);
    assert_eq!(
        parsed(&record(&hello_bytes(0x0303, &e))),
        ClientHello::default()
    );
}

proptest! {
    #[test]
    fn tls_01_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..400)) {
        let _ = parse_client_hello(&buf);
    }

    #[test]
    fn tls_01_never_panics_tls_shaped(
        tail in proptest::collection::vec(any::<u8>(), 0..400),
        len in 0u16..500,
    ) {
        let mut buf = vec![0x16, 3, 1];
        buf.extend(len.to_be_bytes());
        buf.extend(tail);
        let _ = parse_client_hello(&buf);
    }

    #[test]
    fn tls_01_never_panics_mutated_hello(
        pos in 0usize..200,
        byte in any::<u8>(),
        cut in 0usize..200,
    ) {
        let mut e = sni_ext("prop.example.org");
        e.extend(alpn_ext(&["h2", "http/1.1"]));
        e.extend(versions_ext(&[0x0304, 0x0303]));
        let mut buf = record(&hello_bytes(0x0303, &e));
        let i = pos % buf.len();
        buf[i] = byte;
        buf.truncate(buf.len() - cut.min(buf.len() - 1));
        let _ = parse_client_hello(&buf);
    }
}

fn ip4(a: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, a))
}

fn hello(sni: Option<&str>, alpn: &[&str]) -> ClientHello {
    ClientHello {
        sni: sni.map(str::to_string),
        alpn: alpn.iter().map(|s| s.to_string()).collect(),
        tls12: true,
        tls13: true,
    }
}

fn no_exclusions() -> Exclusions {
    Exclusions::default()
}

fn memory() -> Bounded<DestKey, Reason> {
    Bounded::new(MEMORY_CAPACITY)
}

#[test]
fn tls_02_intercept_offers_h2_and_http1_in_client_order() {
    let h = hello(Some("a.test"), &["imap", "http/1.1", "h2"]);
    assert_eq!(
        decide(&h, ip4(1), 443, &no_exclusions(), &memory(), true),
        Decision::Intercept {
            alpn: vec!["http/1.1".into(), "h2".into()]
        }
    );
    // No ALPN at all: intercept, offering none.
    let h = hello(Some("a.test"), &[]);
    assert_eq!(
        decide(&h, ip4(1), 443, &no_exclusions(), &memory(), true),
        Decision::Intercept { alpn: vec![] }
    );
}

#[test]
fn tls_02_no_ca_and_unsupported() {
    let h = hello(Some("a.test"), &["h2"]);
    assert_eq!(
        decide(&h, ip4(1), 443, &no_exclusions(), &memory(), false),
        Decision::PassThrough(Reason::NoCa)
    );
    let alpn_only_imap = hello(Some("a.test"), &["imap"]);
    assert_eq!(
        decide(
            &alpn_only_imap,
            ip4(1),
            443,
            &no_exclusions(),
            &memory(),
            true
        ),
        Decision::PassThrough(Reason::Unsupported)
    );
    let mut old = hello(Some("a.test"), &[]);
    old.tls12 = false;
    old.tls13 = false;
    assert_eq!(
        decide(&old, ip4(1), 443, &no_exclusions(), &memory(), true),
        Decision::PassThrough(Reason::Unsupported)
    );
    // No CA wins over unsupported (table order).
    assert_eq!(
        decide(&old, ip4(1), 443, &no_exclusions(), &memory(), false),
        Decision::PassThrough(Reason::NoCa)
    );
}

#[test]
fn tls_02_order_excluded_then_remembered_then_no_ca() {
    let ex = Exclusions::parse("*.excluded.test").unwrap();
    let mut mem = memory();
    let h = hello(Some("a.excluded.test"), &[]);
    mem.insert(dest_key(&h, ip4(1), 443), Reason::UpstreamUntrusted);
    assert_eq!(
        decide(&h, ip4(1), 443, &ex, &mem, true),
        Decision::PassThrough(Reason::Excluded)
    );
    let h = hello(Some("Remembered.TEST."), &[]);
    mem.insert(dest_key(&h, ip4(1), 443), Reason::ClientCertRequested);
    // Remembered even without CA and even when the hello would be unsupported.
    assert_eq!(
        decide(&h, ip4(1), 443, &ex, &mem, false),
        Decision::PassThrough(Reason::ClientCertRequested)
    );
    // The port is part of the key.
    assert_eq!(
        decide(&h, ip4(1), 8443, &ex, &mem, false),
        Decision::PassThrough(Reason::NoCa)
    );
}

#[test]
fn tls_02_remembered_key_is_sni_else_ip() {
    let mut mem = memory();
    let no_sni = hello(None, &[]);
    mem.insert(dest_key(&no_sni, ip4(7), 443), Reason::ClientRejected);
    assert_eq!(
        decide(&no_sni, ip4(7), 443, &no_exclusions(), &mem, true),
        Decision::PassThrough(Reason::ClientRejected)
    );
    assert!(matches!(
        decide(&no_sni, ip4(8), 443, &no_exclusions(), &mem, true),
        Decision::Intercept { .. }
    ));
    // With an SNI the IP does not matter.
    let named = hello(Some("n.test"), &[]);
    mem.insert(dest_key(&named, ip4(1), 443), Reason::ClientRejected);
    assert_eq!(
        decide(&named, ip4(2), 443, &no_exclusions(), &mem, true),
        Decision::PassThrough(Reason::ClientRejected)
    );
}

#[test]
fn tls_02_reason_labels() {
    for (r, s) in [
        (Reason::Excluded, "excluded"),
        (Reason::NoCa, "no_ca"),
        (Reason::Unsupported, "unsupported"),
        (Reason::UpstreamUntrusted, "upstream_untrusted"),
        (Reason::ClientCertRequested, "client_cert_requested"),
        (Reason::ClientRejected, "client_rejected"),
    ] {
        assert_eq!(r.as_str(), s);
    }
}

#[test]
fn tls_03_exact_host() {
    let ex = Exclusions::parse("api.example.com").unwrap();
    for sni in ["api.example.com", "API.Example.COM", "api.example.com."] {
        assert!(ex.matches(Some(sni), ip4(1)), "{sni}");
    }
    for sni in ["x.api.example.com", "example.com", "api.example.org"] {
        assert!(!ex.matches(Some(sni), ip4(1)), "{sni}");
    }
    let ex = Exclusions::parse("Api.Example.com.").unwrap();
    assert!(ex.matches(Some("api.example.com"), ip4(1)));
}

#[test]
fn tls_03_suffix_pattern() {
    let ex = Exclusions::parse("*.example.com").unwrap();
    assert!(ex.matches(Some("a.example.com"), ip4(1)));
    assert!(ex.matches(Some("a.b.example.com"), ip4(1)));
    assert!(ex.matches(Some("A.B.Example.com."), ip4(1)));
    assert!(!ex.matches(Some("example.com"), ip4(1)));
    assert!(!ex.matches(Some("badexample.com"), ip4(1)));
    assert!(!ex.matches(None, ip4(1)));
}

#[test]
fn tls_03_ip_literal_only_without_sni() {
    let ex = Exclusions::parse("10.0.0.5,2001:db8::1,[2001:db8::2]").unwrap();
    assert!(ex.matches(None, ip4(5)));
    assert!(!ex.matches(None, ip4(6)));
    assert!(!ex.matches(Some("a.test"), ip4(5)));
    let v6 = |s: &str| IpAddr::V6(s.parse::<Ipv6Addr>().unwrap());
    assert!(ex.matches(None, v6("2001:db8::1")));
    assert!(ex.matches(None, v6("2001:db8::2")));
    assert!(!ex.matches(None, v6("2001:db8::3")));
}

#[test]
fn tls_03_parse_rejects() {
    assert!(Exclusions::parse("").unwrap().is_empty());
    for bad in [
        "a.test,,b.test",
        "a.test,",
        ",a.test",
        " ",
        "*",
        "*.",
        "a.*.com",
        "*.*.com",
        "a b.test",
        "-a.test",
        "a-.test",
        "a..test",
        ".test",
        "exa$mple.com",
        "http://a.test",
        "a.test:443",
        "*.a.*",
    ] {
        assert!(Exclusions::parse(bad).is_err(), "{bad:?}");
    }
    let long = format!("{}.test", "a".repeat(64));
    assert!(Exclusions::parse(&long).is_err());
}

#[test]
fn tls_03_parse_limit() {
    let many = |n: usize| {
        (0..n)
            .map(|i| format!("h{i}.test"))
            .collect::<Vec<_>>()
            .join(",")
    };
    assert_eq!(Exclusions::parse(&many(256)).unwrap().len(), 256);
    assert!(Exclusions::parse(&many(257)).is_err());
}

#[test]
fn tls_04_bounded_evicts_oldest_first() {
    let mut b = Bounded::new(3);
    for i in 0..5u32 {
        assert!(b.insert(i, i * 10));
    }
    assert_eq!(b.len(), 3);
    assert_eq!(b.get(&0), None);
    assert_eq!(b.get(&1), None);
    assert_eq!(b.get(&2), Some(&20));
    assert_eq!(b.get(&4), Some(&40));
}

#[test]
fn tls_04_bounded_keeps_first_value_and_order() {
    let mut b = Bounded::new(2);
    assert!(b.insert("a", Reason::Excluded));
    assert!(b.insert("b", Reason::NoCa));
    assert!(!b.insert("a", Reason::ClientRejected));
    assert_eq!(b.get(&"a"), Some(&Reason::Excluded));
    // Re-inserting did not refresh "a": it is still the oldest.
    assert!(b.insert("c", Reason::NoCa));
    assert_eq!(b.get(&"a"), None);
    assert_eq!(b.len(), 2);
}

#[test]
fn tls_04_default_capacity_is_4096() {
    assert_eq!(MEMORY_CAPACITY, 4096);
    let mut b = Bounded::new(MEMORY_CAPACITY);
    for i in 0..5000u32 {
        b.insert(i, ());
    }
    assert_eq!(b.len(), 4096);
    assert_eq!(b.get(&903), None);
    assert!(b.get(&904).is_some());
    assert!(b.get(&4999).is_some());
}

proptest! {
    #[test]
    fn tls_04_deterministic_and_bounded(
        keys in proptest::collection::vec(0u8..20, 0..100),
        cap in 1usize..8,
    ) {
        let run = || {
            let mut b = Bounded::new(cap);
            for (i, k) in keys.iter().enumerate() {
                b.insert(*k, i);
            }
            b
        };
        let (x, y) = (run(), run());
        prop_assert!(x.len() <= cap);
        for k in 0u8..20 {
            prop_assert_eq!(x.get(&k), y.get(&k));
        }
    }
}

const V: Validity = Validity {
    not_before: 1000,
    not_after: 1100,
};

#[test]
fn tls_05_usable_window() {
    assert!(!ca_usable(None, 1050));
    assert!(!ca_usable(Some(V), 999));
    assert!(ca_usable(Some(V), 1000));
    assert!(ca_usable(Some(V), 1099));
    assert!(!ca_usable(Some(V), 1100));
    assert!(!ca_usable(Some(V), 5000));
}

#[test]
fn tls_05_renewal_when_less_than_half_remains() {
    assert!(!V.renewal_due(1000));
    assert!(!V.renewal_due(1050)); // exactly half remains
    assert!(V.renewal_due(1051));
    assert!(V.renewal_due(1099));
    assert!(!fetch_due(Some(V), 1050));
    assert!(fetch_due(Some(V), 1051));
    assert!(fetch_due(None, 0));
    assert!(fetch_due(Some(V), 999));
    assert!(fetch_due(Some(V), 1100));
}

#[test]
fn tls_05_backoff_doubles_to_60_and_resets() {
    let mut b = Backoff::default();
    let secs: Vec<u64> = (0..10).map(|_| b.failure().as_secs()).collect();
    assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    b.success();
    assert_eq!(b.failure().as_secs(), 1);
    assert_eq!(b.failure().as_secs(), 2);
    let mut b = Backoff::default();
    for _ in 0..200 {
        assert!(b.failure().as_secs() <= 60);
    }
}

#[test]
fn tls_05_leaf_validity_equals_intermediate() {
    assert_eq!(V.leaf(), V);
}

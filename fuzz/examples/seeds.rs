//! Écrit les corpus de départ des cibles dans `seeds/<cible>/` : entrées valides tirées des
//! tests, et entrées hostiles connues (ticket #158). `cargo run --example seeds` depuis
//! `fuzz/` ; la sortie est déterministe, sauf le ClientHello de rustls (aléa, clé éphémère).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use mimic_core::model::{Body, Direction, Exchange, Header};
use mimic_core::snapshot::{MockSnapshot, Snapshot};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    let _ = std::fs::remove_dir_all(&root);
    let mut seeds = Seeds { root, count: 0 };
    net_classify(&mut seeds);
    tls_sni(&mut seeds);
    redact_json(&mut seeds);
    redact_form(&mut seeds);
    capture_decode(&mut seeds);
    inject_annotations(&mut seeds);
    cni_settings(&mut seeds);
    let snapshot = segments(&mut seeds);
    mock_form(&mut seeds, &snapshot);
    println!("{} entrées écrites", seeds.count);
}

struct Seeds {
    root: std::path::PathBuf,
    count: usize,
}

impl Seeds {
    fn put(&mut self, target: &str, name: &str, data: impl AsRef<[u8]>) {
        let dir = self.root.join(target);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), data).unwrap();
        self.count += 1;
    }
}

fn net_classify(s: &mut Seeds) {
    for (name, data) in [
        ("get", &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..]),
        ("h2-preface", b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
        ("tls", b"\x16\x03\x01\x02\x00\x01"),
        ("ambiguous", b"GE"),
        ("ambiguous-pri", b"PRI * HTTP/"),
        ("connect", b"CONNECT api.example.com:443 HTTP/1.1\r\n"),
        ("ssh", b"SSH-2.0-OpenSSH_9.6\r\n"),
        ("postgres", b"\x00\x00\x00\x08\x04\xd2\x16\x2f"),
    ] {
        s.put("net_classify", name, data);
    }
}

/// ClientHello réel produit par rustls pour `name`.
fn rustls_hello(name: &str) -> Vec<u8> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let server_name = rustls_pki_types::ServerName::try_from(name.to_string()).unwrap();
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name).unwrap();
    let mut out = vec![];
    conn.write_tls(&mut out).unwrap();
    out
}

/// ClientHello minimal dont l'extension SNI porte `name` ; `name_len` remplace la longueur
/// annoncée du nom.
fn hello(name: &[u8], name_len: Option<u16>) -> Vec<u8> {
    let len = name_len.unwrap_or(name.len() as u16);
    let mut list = vec![0u8];
    list.extend(len.to_be_bytes());
    list.extend(name);
    let mut ext = vec![];
    ext.extend((list.len() as u16).to_be_bytes());
    ext.extend(&list);
    let mut exts = vec![0, 0];
    exts.extend((ext.len() as u16).to_be_bytes());
    exts.extend(&ext);
    let mut body = vec![0x03, 0x03];
    body.extend([0x42; 32]); // aléa
    body.push(0); // pas d'identifiant de session
    body.extend([0x00, 0x02, 0x13, 0x01]); // une suite
    body.extend([0x01, 0x00]); // pas de compression
    body.extend((exts.len() as u16).to_be_bytes());
    body.extend(&exts);
    let mut hs = vec![0x01];
    hs.extend(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend((hs.len() as u16).to_be_bytes());
    record.extend(&hs);
    record
}

fn tls_sni(s: &mut Seeds) {
    let real = rustls_hello("API.Stripe.com");
    s.put("tls_sni", "rustls", &real);
    // Tronqué à chaque position.
    for n in 0..real.len() {
        s.put("tls_sni", &format!("truncated-{n:04}"), &real[..n]);
    }
    s.put("tls_sni", "minimal", hello(b"api.example.com", None));
    s.put(
        "tls_sni",
        "sni-len-over-record",
        hello(b"api.example.com", Some(0xFFFF)),
    );
    s.put("tls_sni", "sni-nul", hello(b"api.example.com\0.evil", None));
    s.put(
        "tls_sni",
        "sni-slash",
        hello(b"api.example.com/admin", None),
    );
    s.put("tls_sni", "sni-at", hello(b"user@api.example.com", None));
    s.put("tls_sni", "sni-256", hello(&[b'a'; 256], None));
    s.put("tls_sni", "sni-empty", hello(b"", None));
    s.put(
        "tls_sni",
        "sni-utf8",
        hello("bücher.example".as_bytes(), None),
    );
    // Enregistrement plus long que les octets reçus.
    let mut short = hello(b"api.example.com", None);
    short[3..5].copy_from_slice(&0xFFFFu16.to_be_bytes());
    s.put("tls_sni", "record-len-over-data", short);
}

fn redact_json(s: &mut Seeds) {
    for (name, data) in [
        ("object", r#"{"user":"ann","password":"hunter2","n":1}"#.to_string()),
        (
            "nested",
            r#"{"card":{"number":"4111111111111111","exp":"12/30"},"items":[{"session_id":"abc"}]}"#.into(),
        ),
        (
            "values",
            r#"["ann@example.com","4242-4242-4242-4242","FR7630006000011234567890189","eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln"]"#.into(),
        ),
        ("marker", r#"{"token":"<redacted:sha256:0123abcd>"}"#.into()),
        ("escaped-key", r#"{"password":"x","a":"foo"}"#.into()),
        ("scalar", "\"ann@example.com\"".into()),
        ("number-card", r#"{"n":4111111111111111}"#.into()),
        ("nested-10000-arrays", "[".repeat(10_000)),
        ("nested-10000-objects", r#"{"a":"#.repeat(10_000)),
        ("nested-127", format!("{}{}", "[".repeat(127), "]".repeat(127))),
        ("empty", String::new()),
    ] {
        s.put("redact_json", name, data);
    }
}

fn redact_form(s: &mut Seeds) {
    for (name, data) in [
        ("simple", "a=1&password=hunter2&b=2"),
        (
            "brackets",
            "card%5Bnumber%5D=4111111111111111&card%5Bcvc%5D=123",
        ),
        ("email", "email=ann%40example.com&name=Ann+Lee"),
        ("marker", "token=%3Credacted%3Asha256%3A0123abcd%3E"),
        ("percent", "a=%&b=%z&c=%4&d=%41"),
        ("empty-pairs", "&&=&a&=b"),
        ("utf8", "nom=%C3%A9t%C3%A9&secret=%FF%FE"),
    ] {
        s.put("redact_form", name, data);
    }
}

/// Entrée de `capture_decode` : encodage, borne, body.
fn decode_input(encoding: u8, limit: u32, body: &[u8]) -> Vec<u8> {
    let mut v = vec![encoding];
    v.extend(&limit.to_be_bytes()[1..]);
    v.extend(body);
    v
}

fn gzip(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `n` octets nuls compressés en brotli, sans les tenir en mémoire.
fn brotli_zeros(n: usize) -> Vec<u8> {
    let mut out = vec![];
    {
        let mut e = brotli::CompressorWriter::new(&mut out, 1 << 16, 9, 24);
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..n / chunk.len() {
            e.write_all(&chunk).unwrap();
        }
        e.flush().unwrap();
    }
    out
}

fn capture_decode(s: &mut Seeds) {
    const GZIP: u8 = 0;
    const DEFLATE: u8 = 2;
    const BR: u8 = 3;
    const ZSTD: u8 = 4;
    const LIMIT: u32 = 0xF_FFFF;
    let text =
        br#"{"id":42,"items":["a","b","c"],"note":"compressible compressible compressible"}"#;
    s.put(
        "capture_decode",
        "gzip",
        decode_input(GZIP, LIMIT, &gzip(text, 6)),
    );
    let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::fast());
    z.write_all(text).unwrap();
    s.put(
        "capture_decode",
        "deflate",
        decode_input(DEFLATE, LIMIT, &z.finish().unwrap()),
    );
    s.put(
        "capture_decode",
        "zstd",
        decode_input(ZSTD, LIMIT, &zstd::encode_all(&text[..], 3).unwrap()),
    );
    let mut br = vec![];
    {
        let mut e = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
        e.write_all(text).unwrap();
    }
    s.put("capture_decode", "br", decode_input(BR, LIMIT, &br));
    // Bombe gzip : DEFLATE plafonne vers 1 000:1, 1 Kio de gzip donne au plus 1 Mio.
    let bomb = gzip(&vec![0u8; 1 << 20], 9);
    assert!(bomb.len() <= 1100, "bombe gzip de {} octets", bomb.len());
    s.put(
        "capture_decode",
        "gzip-bomb-1k-to-1m",
        decode_input(GZIP, LIMIT, &bomb),
    );
    s.put(
        "capture_decode",
        "gzip-bomb-limit-small",
        decode_input(GZIP, 4096, &bomb),
    );
    // Bombe brotli : moins de 1 Kio pour 1 Gio.
    let br_bomb = brotli_zeros(1 << 30);
    assert!(
        br_bomb.len() <= 1024,
        "bombe brotli de {} octets",
        br_bomb.len()
    );
    s.put(
        "capture_decode",
        "br-bomb-1g",
        decode_input(BR, LIMIT, &br_bomb),
    );
    // Bombe zstd : blocs RLE, 1 Gio en quelques dizaines de Kio.
    let zstd_bomb = zstd_zeros(1 << 30);
    s.put(
        "capture_decode",
        "zstd-bomb-1g",
        decode_input(ZSTD, LIMIT, &zstd_bomb),
    );
    // Fenêtre brotli invalide (WBITS réservé), puis fenêtre large non négociée.
    s.put(
        "capture_decode",
        "br-bad-window",
        decode_input(BR, LIMIT, &[0x11, 0x01, 0x8c, 0xc1]),
    );
    s.put(
        "capture_decode",
        "br-large-window",
        decode_input(BR, LIMIT, &[0x11, 0x3e, 0x00, 0x03]),
    );
    // Trames zstd annonçant une fenêtre de 2^31 et de 2^27 octets, sans contenu.
    s.put(
        "capture_decode",
        "zstd-window-2g",
        decode_input(
            ZSTD,
            LIMIT,
            &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xa8, 0x01, 0x00, 0x00],
        ),
    );
    s.put(
        "capture_decode",
        "zstd-window-128m",
        decode_input(
            ZSTD,
            LIMIT,
            &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x88, 0x01, 0x00, 0x00],
        ),
    );
    s.put(
        "capture_decode",
        "gzip-truncated",
        decode_input(GZIP, LIMIT, &gzip(text, 6)[..20]),
    );
    s.put("capture_decode", "unknown", decode_input(6, LIMIT, text));
}

/// `n` octets nuls en zstd, en blocs RLE de 128 Kio écrits à la main.
fn zstd_zeros(n: usize) -> Vec<u8> {
    const BLOCK: usize = 128 * 1024;
    // Magic, descripteur : fenêtre seule (pas de taille de contenu), fenêtre 2^17.
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x38];
    let blocks = n / BLOCK;
    for i in 0..blocks {
        let last = u32::from(i + 1 == blocks);
        // En-tête de bloc : dernier, type RLE (1), taille régénérée.
        let header = last | (1 << 1) | ((BLOCK as u32) << 3);
        out.extend(&header.to_le_bytes()[..3]);
        out.push(0);
    }
    out
}

fn inject_annotations(s: &mut Seeds) {
    for (name, data) in [
        (
            "valid",
            "\n8080,9090\n10.0.0.0/8,fd00::/8\n5432\ninfo,mimic_proxy=debug",
        ),
        ("all", "i\n*\n\n\n"),
        ("none", "\n-\n"),
        ("cni", "c\n8080\n10.0.0.0/8\n53\nwarn"),
        ("cni-inbound", "ci\n\n\n\n"),
        ("double-comma", "\n8080,,9090\n10.0.0.0/8,,\n,,\n"),
        ("port-65536", "\n65536\n\n65536,0,-1\n"),
        (
            "cidr-33",
            "\n\n10.0.0.0/33,::/129,10.0.0.1/+8,10.0.0.0/8/8\n",
        ),
        (
            "iptables-injection",
            "\n8080 -j ACCEPT\n10.0.0.0/8 -j ACCEPT\n53 -j ACCEPT\n",
        ),
        (
            "iptables-injection-cni",
            "c\n8080 -j ACCEPT\n0.0.0.0/0 -j ACCEPT\n1 -j ACCEPT\n",
        ),
        ("log-invalid", "\n\n\n\n[{=debug"),
        ("spaces", "\n 80 , 443 \n 10.1.2.3 \n"),
    ] {
        s.put("inject_annotations", name, data);
    }
}

fn cni_settings(s: &mut Seeds) {
    for (name, data) in [
        (
            "valid",
            r#"{"MIMIC_EXCLUDE_OUTBOUND_CIDRS":"10.96.0.1/32","MIMIC_INBOUND_PORTS":"8080","MIMIC_MESH":"auto"}"#,
        ),
        (
            "all-keys",
            r#"{"MIMIC_INBOUND_PORTS":"*","MIMIC_EXCLUDE_OUTBOUND_CIDRS":"","MIMIC_EXCLUDE_OUTBOUND_PORTS":"53","MIMIC_MESH":"istio","MIMIC_IPV6":"true","MIMIC_IPTABLES":"iptables-legacy"}"#,
        ),
        ("iptables-nft", r#"{"MIMIC_IPTABLES":"iptables-nft"}"#),
        ("iptables-path", r#"{"MIMIC_IPTABLES":"/bin/sh"}"#),
        ("iptables-space", r#"{"MIMIC_IPTABLES":"iptables "}"#),
        ("iptables-upper", r#"{"MIMIC_IPTABLES":"IPTABLES"}"#),
        (
            "iptables-dup",
            r#"{"MIMIC_IPTABLES":"/bin/sh","MIMIC_IPTABLES":"iptables"}"#,
        ),
        ("unknown-key", r#"{"LD_PRELOAD":"/tmp/x.so"}"#),
        ("unknown-proxy-uid", r#"{"MIMIC_PROXY_UID":"0"}"#),
        ("non-string", r#"{"MIMIC_IPV6":true}"#),
        ("empty", "{}"),
        ("not-object", "[]"),
    ] {
        s.put("cni_settings", name, data);
    }
}

fn sample(id: &str, direction: Direction, status: u16, body: Body) -> Exchange {
    Exchange {
        id: id.into(),
        direction,
        started_at_ms: 1_700_000_000_000,
        scheme: "https".into(),
        authority: "api.stripe.com".into(),
        method: "POST".into(),
        path: "/v1/charges/ch_123".into(),
        query: vec![("expand".into(), "customer".into())],
        req_headers: vec![Header {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        req_body: Body::Captured {
            data: br#"{"amount":1000,"currency":"eur"}"#.to_vec(),
        },
        status,
        resp_headers: vec![Header {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        resp_body: body,
        resp_trailers: vec![],
        ttfb_us: 1200,
        total_us: 3400,
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
        redactions: vec![],
    }
}

fn segments(s: &mut Seeds) -> Snapshot {
    let exchanges = [
        sample(
            "01A",
            Direction::Outbound,
            200,
            Body::Captured {
                data: br#"{"id":"ch_123","paid":true}"#.to_vec(),
            },
        ),
        sample(
            "01B",
            Direction::Outbound,
            402,
            Body::Truncated {
                size: 9000,
                prefix: b"{\"err".to_vec(),
            },
        ),
        sample("01C", Direction::Inbound, 201, Body::Skipped { size: 12 }),
        sample("01D", Direction::Outbound, 101, Body::Empty),
    ];
    let mut jsonl = vec![];
    for e in &exchanges {
        jsonl.extend(serde_json::to_vec(e).unwrap());
        jsonl.push(b'\n');
    }
    let snapshot = Snapshot::parse(&jsonl).unwrap();
    s.put("segments", "valid", &jsonl);
    s.put(
        "segments",
        "one-line",
        serde_json::to_vec(&exchanges[0]).unwrap(),
    );
    let mut bad = jsonl.clone();
    bad.extend(b"{\"id\":\n");
    s.put("segments", "bad-line", bad);
    let mut b64 = serde_json::to_value(&exchanges[0]).unwrap();
    b64["req_body"]["data"] = "@@@=".into();
    s.put("segments", "bad-base64", serde_json::to_vec(&b64).unwrap());
    s.put("segments", "nested-10000", "[".repeat(10_000));
    s.put("segments", "empty-lines", "\n\n\n");
    snapshot
}

fn mock_form(s: &mut Seeds, snapshot: &Snapshot) {
    let json = MockSnapshot::from_snapshot(snapshot).to_json();
    let with = |e: u8, body: &[u8]| {
        let mut v = vec![e];
        v.extend(body);
        v
    };
    s.put("mock_form", "identity", with(0, &json));
    s.put("mock_form", "gzip", with(1, &gzip(&json, 6)));
    s.put(
        "mock_form",
        "zstd",
        with(2, &zstd::encode_all(&json[..], 3).unwrap()),
    );
    s.put("mock_form", "br-bomb", with(3, &brotli_zeros(1 << 30)));
    s.put(
        "mock_form",
        "wrong-format",
        with(
            0,
            br#"{"format":2,"id":"x","count":0,"operations":[],"records":[]}"#,
        ),
    );
    s.put(
        "mock_form",
        "nested-10000",
        with(0, "[".repeat(10_000).as_bytes()),
    );
}

//! TLS-13: `mimic-trust` against fake image roots, and on the real binary for exit codes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use mimic_core::trust::{encode_pem, parse_pem};
use mimic_runtime::config::Env;
use mimic_runtime::testing::run;
use mimic_trust::{Error, build};
use p12_keystore::{KeyStore, KeyStoreEntry, Pkcs12ImportPolicy};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-trust");

fn cert(cn: &str) -> Vec<u8> {
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

fn pem(certs: &[&Vec<u8>]) -> String {
    encode_pem(&certs.iter().map(|c| (*c).clone()).collect::<Vec<_>>())
}

fn put(root: &Path, path: &str, content: &str) {
    let full = root.join(path.trim_start_matches('/'));
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn read(out: &Path, name: &str) -> Vec<Vec<u8>> {
    let text = fs::read_to_string(out.join(name)).unwrap();
    let parsed = parse_pem(&text, |_| true);
    assert_eq!(parsed.skipped, 0, "{name}");
    parsed.certs
}

fn env(root: &Path, extra: &[(&str, &str)]) -> Env {
    let mut pairs = vec![
        (
            "MIMIC_TRUST_OUT".to_string(),
            root.join("out").display().to_string(),
        ),
        (
            "MIMIC_TRUST_FALLBACK".to_string(),
            "/mimic/bin/fallback-ca.pem".to_string(),
        ),
    ];
    pairs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    Env::from_pairs(pairs)
}

fn with_ca(root: &Path, ca: &Vec<u8>, extra: &[(&str, &str)]) -> Env {
    let ca_pem = pem(&[ca]);
    let mut all = vec![("MIMIC_TRUST_CA", ca_pem.as_str())];
    all.extend_from_slice(extra);
    env(root, &all)
}

fn p12_certs(out: &Path) -> Vec<Vec<u8>> {
    let data = fs::read(out.join("truststore.p12")).unwrap();
    let store = KeyStore::from_pkcs12(&data, "mimic", Pkcs12ImportPolicy::Strict).unwrap();
    store
        .entries()
        .map(|(_, entry)| match entry {
            KeyStoreEntry::Certificate(c) => c.as_der().to_vec(),
            other => panic!("not a trusted certificate: {other:?}"),
        })
        .collect()
}

#[test]
fn tls_13_debian_store_gets_the_root_appended_in_every_file() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, root) = (cert("a"), cert("b"), cert("root"));
    let store = format!(
        "# Debian\n{}-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n{}{}",
        pem(&[&a]),
        pem(&[&b]),
        pem(&[&a])
    );
    put(dir.path(), "/etc/ssl/certs/ca-certificates.crt", &store);
    let report = build(dir.path(), &with_ca(dir.path(), &root, &[])).unwrap();

    let out = dir.path().join("out");
    assert_eq!(read(&out, "image-ca.pem"), vec![a.clone(), b.clone()]);
    assert_eq!(read(&out, "mimic-ca.pem"), vec![root.clone()]);
    assert_eq!(
        read(&out, "ca-bundle.pem"),
        vec![a.clone(), b.clone(), root.clone()]
    );
    assert_eq!(read(&out, "node-extra-ca.pem"), vec![root.clone()]);
    let mut p12 = p12_certs(&out);
    p12.sort();
    let mut want = vec![a, b, root];
    want.sort();
    assert_eq!(p12, want);
    for name in [
        "image-ca.pem",
        "mimic-ca.pem",
        "ca-bundle.pem",
        "node-extra-ca.pem",
        "truststore.p12",
    ] {
        let mode = fs::metadata(out.join(name)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "{name}");
    }
    assert_eq!(report.image_certs, 2);
    assert_eq!(report.bundle_certs, 3);
    assert_eq!(report.skipped, 1);
    assert!(!report.no_image_store);
    assert_eq!(report.sources, vec!["/etc/ssl/certs/ca-certificates.crt"]);
}

#[test]
fn tls_13_rhel_store_is_found_and_only_the_first_existing_store_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, root) = (cert("a"), cert("b"), cert("root"));
    put(dir.path(), "/etc/pki/tls/certs/ca-bundle.crt", &pem(&[&a]));
    put(dir.path(), "/etc/ssl/cert.pem", &pem(&[&b]));
    build(dir.path(), &with_ca(dir.path(), &root, &[])).unwrap();
    assert_eq!(read(&dir.path().join("out"), "image-ca.pem"), vec![a]);
}

#[test]
fn tls_13_image_variables_add_their_files_and_deduplicate() {
    let dir = tempfile::tempdir().unwrap();
    let (a, c, d, e, root) = (cert("a"), cert("c"), cert("d"), cert("e"), cert("root"));
    put(
        dir.path(),
        "/etc/ssl/certs/ca-certificates.crt",
        &pem(&[&a]),
    );
    put(dir.path(), "/corp/ssl.pem", &pem(&[&c, &a]));
    put(dir.path(), "/corp/req.pem", &pem(&[&e]));
    put(dir.path(), "/corp/node.pem", &pem(&[&d]));
    let env = with_ca(
        dir.path(),
        &root,
        &[
            ("SSL_CERT_FILE", "/corp/ssl.pem"),
            ("REQUESTS_CA_BUNDLE", "/corp/req.pem"),
            ("CURL_CA_BUNDLE", "/corp/missing.pem"),
            ("NODE_EXTRA_CA_CERTS", "/corp/node.pem"),
        ],
    );
    build(dir.path(), &env).unwrap();
    let out = dir.path().join("out");
    assert_eq!(
        read(&out, "image-ca.pem"),
        vec![a.clone(), c.clone(), e.clone()]
    );
    assert_eq!(read(&out, "ca-bundle.pem"), vec![a, c, e, root.clone()]);
    assert_eq!(read(&out, "node-extra-ca.pem"), vec![d, root]);
}

#[test]
fn tls_13_image_without_store_uses_the_fallback_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let (f, root) = (cert("fallback"), cert("root"));
    put(dir.path(), "/mimic/bin/fallback-ca.pem", &pem(&[&f]));
    let report = build(dir.path(), &with_ca(dir.path(), &root, &[])).unwrap();
    assert!(report.no_image_store);
    assert_eq!(report.sources, vec!["/mimic/bin/fallback-ca.pem"]);
    let out = dir.path().join("out");
    assert_eq!(read(&out, "image-ca.pem"), vec![f.clone()]);
    assert_eq!(read(&out, "ca-bundle.pem"), vec![f, root]);
}

#[test]
fn tls_13_missing_fallback_still_writes_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = cert("root");
    let report = build(dir.path(), &with_ca(dir.path(), &root, &[])).unwrap();
    assert!(report.no_image_store);
    assert_eq!(report.image_certs, 0);
    let out = dir.path().join("out");
    assert!(read(&out, "image-ca.pem").is_empty());
    assert_eq!(read(&out, "ca-bundle.pem"), vec![root.clone()]);
    assert_eq!(p12_certs(&out), vec![root]);
}

#[test]
fn tls_13_a_missing_or_invalid_root_is_a_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    for ca in [
        None,
        Some(""),
        Some("not a certificate"),
        Some(&*pem(&[&b"x".to_vec()])),
    ] {
        let env = match ca {
            None => env(dir.path(), &[]),
            Some(v) => env(dir.path(), &[("MIMIC_TRUST_CA", v)]),
        };
        assert!(
            matches!(build(dir.path(), &env), Err(Error::Config(_))),
            "{ca:?}"
        );
    }
    let root = cert("root");
    let no_out = Env::from_pairs([("MIMIC_TRUST_CA".to_string(), pem(&[&root]))]);
    assert!(matches!(build(dir.path(), &no_out), Err(Error::Config(_))));
}

#[test]
fn tls_13_an_unwritable_output_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = cert("root");
    fs::write(dir.path().join("out"), "a file, not a directory").unwrap();
    let err = build(dir.path(), &with_ca(dir.path(), &root, &[])).unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
}

fn logs(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
        .collect()
}

#[test]
fn tls_13_binary_writes_the_bundle_logs_once_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let root = cert("root");
    let fallback = dir.path().join("fallback.pem");
    fs::write(&fallback, pem(&[&cert("f")])).unwrap();
    let out = dir.path().join("trust/0");
    let (status, stdout, stderr) = run(
        BIN,
        &[],
        &[
            ("MIMIC_TRUST_CA", &pem(&[&root])),
            ("MIMIC_TRUST_OUT", out.to_str().unwrap()),
            ("MIMIC_TRUST_FALLBACK", fallback.to_str().unwrap()),
        ],
    );
    assert_eq!(status.code(), Some(0), "{stdout}{stderr}");
    let lines = logs(&stdout);
    let written: Vec<_> = lines
        .iter()
        .filter(|l| l["message"] == "trust bundle written")
        .collect();
    assert_eq!(written.len(), 1, "{stdout}");
    assert_eq!(written[0]["level"], "INFO");
    assert!(read(&out, "ca-bundle.pem").contains(&root));
}

#[test]
fn tls_13_binary_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let root = cert("root");
    let file = dir.path().join("file");
    fs::write(&file, "x").unwrap();
    let blocked = file.join("out");
    let ca = pem(&[&root]);
    let cases: [(&[(&str, &str)], i32); 4] = [
        (&[("MIMIC_TRUST_OUT", "/tmp/unused")], 2),
        (
            &[
                ("MIMIC_TRUST_CA", "garbage"),
                ("MIMIC_TRUST_OUT", "/tmp/unused"),
            ],
            2,
        ),
        (&[("MIMIC_TRUST_CA", &ca)], 2),
        (
            &[
                ("MIMIC_TRUST_CA", &ca),
                ("MIMIC_TRUST_OUT", blocked.to_str().unwrap()),
            ],
            1,
        ),
    ];
    for (env, want) in cases {
        let (status, stdout, stderr) = run(BIN, &[], env);
        assert_eq!(status.code(), Some(want), "{env:?} {stdout}{stderr}");
    }
}

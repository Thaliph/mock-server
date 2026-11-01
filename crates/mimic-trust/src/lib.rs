//! `mimic-trust`: builds the trust bundle of one app image (spec §7.1, §10.5).

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use mimic_core::trust::{Parsed, encode_pem, merge, parse_pem};
use mimic_runtime::config::{ConfigError, Env};
use p12_keystore::{Certificate, KeyStore, KeyStoreEntry};

/// System stores, first existing wins.
const STORES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
];
/// Variables of the image that name extra trust files merged into `image-ca.pem`.
const IMAGE_VARS: [&str; 3] = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"];
/// Largest trust file read: system bundles are a few hundred KiB, anything above is not one.
const MAX_STORE: u64 = 8 * 1024 * 1024;
/// Public on purpose: the store holds no secret (spec §11.1).
const TRUSTSTORE_PASSWORD: &str = "mimic";

/// What a successful run wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Files the image's own certificates were read from, in order.
    pub sources: Vec<String>,
    /// The image had no trust store: the fallback store stands in for it.
    pub no_image_store: bool,
    /// Certificates in `image-ca.pem` / `ca-bundle.pem` (the latter includes the root).
    pub image_certs: usize,
    pub bundle_certs: usize,
    /// Unparsable certificate blocks skipped in the sources.
    pub skipped: usize,
}

#[derive(Debug)]
pub enum Error {
    /// Invalid variable or root certificate: exit 2.
    Config(ConfigError),
    /// Cannot write the output: exit 1.
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => e.fmt(f),
            Error::Io(e) => f.write_str(e),
        }
    }
}

/// Builds the bundle. Absolute store paths are looked up under `root` (`/` in production).
pub fn build(root: &Path, env: &Env) -> Result<Report, Error> {
    let out = Path::new(env.required("MIMIC_TRUST_OUT").map_err(Error::Config)?);
    let ca = env.required("MIMIC_TRUST_CA").map_err(Error::Config)?;
    let fallback = env
        .optional("MIMIC_TRUST_FALLBACK")
        .unwrap_or("/mimic/bin/fallback-ca.pem");

    let root_ca = parse_pem(ca, is_certificate);
    if root_ca.certs.is_empty() || root_ca.skipped > 0 {
        return Err(Error::Config(ConfigError::invalid(
            "MIMIC_TRUST_CA",
            "<redacted>",
            "expected PEM certificates",
        )));
    }

    let mut skipped = 0;
    let mut sources = Vec::new();
    let mut image = Vec::new();
    // The first existing system store, then the files the image's own variables name.
    let candidates = STORES
        .iter()
        .copied()
        .filter(|path| read_pem(root, path).is_some())
        .take(1)
        .chain(IMAGE_VARS.iter().filter_map(|var| env.optional(var)));
    for path in candidates {
        if let Some(parsed) = read_pem(root, path) {
            skipped += parsed.skipped;
            image.push(parsed.certs);
            sources.push(path.to_string());
        }
    }
    let no_image_store = sources.is_empty();
    if no_image_store && let Some(parsed) = read_pem(root, fallback) {
        skipped += parsed.skipped;
        image.push(parsed.certs);
        sources.push(fallback.to_string());
    }
    let image_certs = merge(&image.iter().map(Vec::as_slice).collect::<Vec<_>>());

    let mut node = Vec::new();
    if let Some(parsed) = env
        .optional("NODE_EXTRA_CA_CERTS")
        .and_then(|path| read_pem(root, path))
    {
        skipped += parsed.skipped;
        node = parsed.certs;
    }

    let bundle = merge(&[&image_certs, &root_ca.certs]);
    let node_extra = merge(&[&node, &root_ca.certs]);
    let root_ca = merge(&[&root_ca.certs]);

    let p12 = truststore(&bundle)?;
    fs::create_dir_all(out).map_err(|e| io(out, e))?;
    write(out, "image-ca.pem", encode_pem(&image_certs).as_bytes())?;
    write(out, "mimic-ca.pem", encode_pem(&root_ca).as_bytes())?;
    write(out, "ca-bundle.pem", encode_pem(&bundle).as_bytes())?;
    write(out, "node-extra-ca.pem", encode_pem(&node_extra).as_bytes())?;
    write(out, "truststore.p12", &p12)?;

    Ok(Report {
        sources,
        no_image_store,
        image_certs: image_certs.len(),
        bundle_certs: bundle.len(),
        skipped,
    })
}

/// Certificates of the file at `path` (absolute, looked up under `root`); `None` when it
/// cannot be read or is larger than [`MAX_STORE`].
fn read_pem(root: &Path, path: &str) -> Option<Parsed> {
    let file = fs::File::open(root.join(path.trim_start_matches('/'))).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_STORE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_STORE {
        return None;
    }
    Some(parse_pem(&String::from_utf8_lossy(&bytes), is_certificate))
}

fn is_certificate(der: &[u8]) -> bool {
    Certificate::from_der(der).is_ok()
}

/// A PKCS12 file holding every certificate as a trusted entry (the JVM's `trustStore`).
fn truststore(certs: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    let mut store = KeyStore::new();
    for (i, der) in certs.iter().enumerate() {
        let cert = Certificate::from_der(der).map_err(|e| Error::Io(format!("truststore: {e}")))?;
        store.add_entry(&format!("mimic-{i:04}"), KeyStoreEntry::Certificate(cert));
    }
    store
        .writer(TRUSTSTORE_PASSWORD)
        .write()
        .map_err(|e| Error::Io(format!("truststore: {e}")))
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}

fn write(dir: &Path, name: &str, content: &[u8]) -> Result<(), Error> {
    let path = dir.join(name);
    fs::write(&path, content).map_err(|e| io(&path, e))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).map_err(|e| io(&path, e))
}

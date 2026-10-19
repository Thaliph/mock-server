//! Serveurs du plan de contrôle (spec §9.2b) : certificat relu à chaud (INJ-14, CTL-29),
//! connexions bornées et `accept` sans boucle à vide (INJ-15, CTL-30), arrêt propre (INJ-16).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use mimic_net::serve::{ACCEPT_BACKOFF, LogLimiter, ServeOptions, accept_with_backoff, serve};
use mimic_net::tls::{CertReloader, Reload, Tls, TlsFiles};
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Certificat auto-signé pour `localhost` : (certificat PEM, clé PEM).
fn self_signed() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (cert.pem(), key.serialize_pem())
}

fn write(files: &TlsFiles, (cert, key): &(String, String)) {
    std::fs::write(&files.cert_file, cert).unwrap();
    std::fs::write(&files.key_file, key).unwrap();
}

fn files(dir: &tempfile::TempDir) -> TlsFiles {
    TlsFiles::new(dir.path().join("tls.crt"), dir.path().join("tls.key"))
}

/// Client qui ne fait confiance qu'à `cert`, sans connexion gardée d'un appel à l'autre.
async fn trusted_by(url: &str, cert: &str) -> bool {
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .tls_certs_only([reqwest::Certificate::from_pem(cert.as_bytes()).unwrap()])
        .build()
        .unwrap();
    client.get(url).send().await.is_ok()
}

fn app() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" }))
}

#[test]
fn inj_14_ctl_29_reload_follows_the_files_and_keeps_the_last_good_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let files = files(&dir);
    let (a, b) = (self_signed(), self_signed());
    write(&files, &a);
    let reloader = CertReloader::load(&files).unwrap();
    let served = |r: &CertReloader| r.current().cert[0].clone();
    let first = served(&reloader);
    assert_eq!(reloader.reload(), Reload::Unchanged);

    write(&files, &b);
    assert_eq!(reloader.reload(), Reload::Reloaded);
    let second = served(&reloader);
    assert_ne!(first, second);
    assert_eq!(reloader.reload(), Reload::Unchanged);

    // Contenu illisible, puis clé qui ne va pas avec le certificat : l'ancien reste servi.
    std::fs::write(&files.cert_file, "pas un certificat").unwrap();
    assert!(matches!(reloader.reload(), Reload::Refused(_)));
    assert_eq!(served(&reloader), second);
    std::fs::write(&files.cert_file, &a.0).unwrap();
    std::fs::write(&files.key_file, &b.1).unwrap();
    assert!(matches!(reloader.reload(), Reload::Refused(_)));
    assert_eq!(served(&reloader), second);
    // Fichier absent (échange du lien symbolique du Secret en cours) : idem.
    std::fs::remove_file(&files.key_file).unwrap();
    assert!(matches!(reloader.reload(), Reload::Refused(_)));
    assert_eq!(served(&reloader), second);

    write(&files, &a);
    assert_eq!(reloader.reload(), Reload::Reloaded);
    assert_eq!(served(&reloader), first);
}

#[test]
fn inj_14_ctl_29_unreadable_certificate_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let files = files(&dir);
    assert!(CertReloader::load(&files).is_err(), "fichiers absents");
    std::fs::write(&files.cert_file, "x").unwrap();
    std::fs::write(&files.key_file, "y").unwrap();
    let err = CertReloader::load(&files).unwrap_err();
    assert!(err.contains("tls.crt"), "{err}");
}

#[test]
fn reload_interval_defaults_to_ten_seconds() {
    let f: TlsFiles = serde_yaml::from_str("{cert_file: /c, key_file: /k}").unwrap();
    assert_eq!(f.reload_interval, Duration::from_secs(10));
    let f: TlsFiles =
        serde_yaml::from_str("{cert_file: /c, key_file: /k, reload_interval: 0.5}").unwrap();
    assert_eq!(f.reload_interval, Duration::from_millis(500));
    assert!(serde_yaml::from_str::<TlsFiles>("{cert_file: /c, key_file: /k, x: 1}").is_err());
}

#[tokio::test]
async fn inj_14_ctl_29_server_presents_the_renewed_certificate_without_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = files(&dir);
    files.reload_interval = Duration::from_millis(100);
    let (a, b) = (self_signed(), self_signed());
    write(&files, &a);
    let tls = Tls::new(&files, &[]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://localhost:{}/healthz",
        listener.local_addr().unwrap().port()
    );
    let task = tokio::spawn(serve(
        listener,
        app(),
        Some(tls),
        ServeOptions::default(),
        std::future::pending(),
    ));
    assert!(trusted_by(&url, &a.0).await);
    assert!(!trusted_by(&url, &b.0).await);

    write(&files, &b);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !trusted_by(&url, &b.0).await {
        assert!(Instant::now() < deadline, "nouveau certificat jamais servi");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!trusted_by(&url, &a.0).await);

    std::fs::write(&files.cert_file, "illisible").unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        trusted_by(&url, &b.0).await,
        "l'ancien certificat reste servi"
    );
    task.abort();
}

fn short() -> ServeOptions {
    ServeOptions {
        handshake_timeout: Duration::from_millis(300),
        header_read_timeout: Duration::from_millis(300),
        drain_timeout: Duration::from_secs(2),
    }
}

/// Lit jusqu'à la fermeture ; durée écoulée, ou `None` si rien ne ferme en 5 s.
async fn closed_after(mut s: TcpStream) -> Option<Duration> {
    let started = Instant::now();
    let mut buf = [0u8; 1024];
    let read = async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()
        .map(|()| started.elapsed())
}

#[tokio::test]
async fn inj_15_ctl_30_silent_connections_are_closed() {
    let dir = tempfile::tempdir().unwrap();
    let files = files(&dir);
    let cert = self_signed();
    write(&files, &cert);

    // HTTPS : client TCP muet, fermé au délai de poignée de main.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_addr = listener.local_addr().unwrap();
    let tls = Tls::new(&files, &[]).unwrap();
    let t1 = tokio::spawn(serve(
        listener,
        app(),
        Some(tls),
        short(),
        std::future::pending(),
    ));
    let silent = TcpStream::connect(tls_addr).await.unwrap();
    let after = closed_after(silent)
        .await
        .expect("poignée de main jamais coupée");
    assert!(after >= Duration::from_millis(250), "{after:?}");

    // HTTPS : poignée de main faite, aucun en-tête, fermé au délai des en-têtes.
    let tcp = TcpStream::connect(tls_addr).await.unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pki_types::CertificateDer::pem_slice_iter(cert.0.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let mut stream = connector
        .connect("localhost".try_into().unwrap(), tcp)
        .await
        .unwrap();
    let started = Instant::now();
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
    assert!(read.is_ok(), "en-têtes attendus sans fin");
    assert!(started.elapsed() >= Duration::from_millis(250));

    // HTTP en clair : en-têtes commencés, jamais finis.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let t2 = tokio::spawn(serve(
        listener,
        app(),
        None,
        short(),
        std::future::pending(),
    ));
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /healthz HTTP/1.1\r\nhost: x\r\n")
        .await
        .unwrap();
    assert!(
        closed_after(s).await.is_some(),
        "en-têtes attendus sans fin"
    );

    // Une requête complète est toujours servie.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /healthz HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    t1.abort();
    t2.abort();
}

#[tokio::test]
async fn inj_15_ctl_30_accept_errors_pause_before_retrying() {
    let calls = AtomicUsize::new(0);
    let log = LogLimiter::per_second();
    let started = Instant::now();
    let got = accept_with_backoff(
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 3 {
                    Err(std::io::Error::from_raw_os_error(24)) // EMFILE
                } else {
                    Ok(n)
                }
            }
        },
        &log,
    )
    .await;
    assert_eq!(got, 3);
    assert!(started.elapsed() >= ACCEPT_BACKOFF * 3);
    // Trois erreurs en 150 ms : une seule ligne écrite.
    assert_eq!(log.allow(Instant::now()), None);
}

#[tokio::test]
async fn inj_16_shutdown_stops_accepting_and_lets_requests_finish() {
    let slow = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "fini"
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, slow, None, short(), async {
        let _ = rx.await;
    }));
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /slow HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(()).unwrap();
    let mut out = vec![0u8; 1024];
    let n = s.read(&mut out).await.unwrap();
    let out = String::from_utf8_lossy(&out[..n]);
    assert!(
        out.starts_with("HTTP/1.1 200") && out.ends_with("fini"),
        "{out}"
    );
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("serve se termine après le drainage")
        .unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "écoute fermée");
}

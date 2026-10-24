//! Date d'expiration du certificat servi (spec §9.2b, MON-04).

use mimic_net::tls::{CertReloader, Reload, TlsFiles};

/// Certificat auto-signé pour `localhost` expirant à la date donnée : (certificat, clé).
fn expiring(y: i32, m: u8, d: u8) -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.not_after = rcgen::date_time_ymd(y, m, d);
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

fn write(files: &TlsFiles, (cert, key): &(String, String)) {
    std::fs::write(&files.cert_file, cert).unwrap();
    std::fs::write(&files.key_file, key).unwrap();
}

/// 2031-01-02T00:00:00Z et 2032-03-04T00:00:00Z.
const FIRST: i64 = 1_925_078_400;
const SECOND: i64 = 1_961_971_200;

#[test]
fn mon_04_expiry_follows_the_served_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let files = TlsFiles::new(dir.path().join("tls.crt"), dir.path().join("tls.key"));
    write(&files, &expiring(2031, 1, 2));
    let reloader = CertReloader::load(&files).unwrap();
    assert_eq!(reloader.not_after_unix(), FIRST);

    write(&files, &expiring(2032, 3, 4));
    assert_eq!(reloader.reload(), Reload::Reloaded);
    assert_eq!(reloader.not_after_unix(), SECOND);

    // Un certificat refusé ne change pas la date : c'est l'ancien qui est servi.
    std::fs::write(&files.cert_file, "illisible").unwrap();
    assert!(matches!(reloader.reload(), Reload::Refused(_)));
    assert_eq!(reloader.not_after_unix(), SECOND);
}

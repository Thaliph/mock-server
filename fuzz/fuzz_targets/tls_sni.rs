//! SNI d'un ClientHello (`tls::parse_sni`) : le nom devient l'authority `hôte:port` de la
//! requête déchiffrée.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_proxy::tls::parse_sni;

fuzz_target!(|data: &[u8]| {
    let Some(name) = parse_sni(data) else { return };
    assert!(
        !name.is_empty() && name.len() <= 255,
        "SNI de {} octets",
        name.len()
    );
    assert_eq!(name, name.to_ascii_lowercase());
    // Authority valide, dont l'hôte est exactement le nom : ni `/`, ni `@`, ni `\0`.
    let authority: http::uri::Authority = format!("{name}:443")
        .parse()
        .unwrap_or_else(|e| panic!("authority invalide pour {name:?} : {e}"));
    assert_eq!(authority.host(), name);
    assert_eq!(authority.port_u16(), Some(443));
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "nom d'hôte invalide : {name:?}"
    );
});

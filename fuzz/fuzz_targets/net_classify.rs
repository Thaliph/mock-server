//! Détection du protocole sur les premiers octets d'une connexion redirigée (`net::classify`,
//! cœur de `net::detect`).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_proxy::net::{Protocol, classify};

/// Plus long préfixe HTTP reconnu (`PRI * HTTP/2`).
const LONGEST_PREFIX: usize = 12;

fuzz_target!(|data: &[u8]| {
    let got = classify(data);
    match got {
        None => {
            // Indécis : rien reçu, ou début ambigu d'une méthode HTTP.
            assert!(
                data.len() < LONGEST_PREFIX,
                "indécis après {} octets",
                data.len()
            );
            assert!(data.first() != Some(&0x16));
        }
        Some(p) => {
            assert!(!data.is_empty());
            if data[0] == 0x16 {
                assert_eq!(p, Protocol::Tls);
            }
            // `detect` relit la connexion à mesure qu'elle se remplit : une conclusion ne
            // change plus avec les octets suivants.
            let mut longer = data.to_vec();
            longer.extend_from_slice(b"\x00 X");
            assert_eq!(classify(&longer), Some(p));
            for n in 1..data.len().min(LONGEST_PREFIX + 1) {
                let prefix = classify(&data[..n]);
                assert!(
                    prefix.is_none() || prefix == Some(p),
                    "préfixe {n} : {prefix:?} puis {p:?}"
                );
            }
            assert!(!p.as_str().is_empty());
        }
    }
});

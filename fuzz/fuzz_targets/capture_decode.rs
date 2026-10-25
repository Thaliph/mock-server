//! Décodage des bodies compressés (`capture::decode`, gzip, deflate, br, zstd) : capture et
//! forme de mock reçue du contrôleur. Une bombe de décompression s'arrête à `limit`.
//!
//! Entrée : un octet d'encodage, trois octets de borne (gros-boutiste, au plus 1 Mio), puis
//! le body.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_proxy::capture::encode;
use mimic_proxy::fuzzing::decode;

const ENCODINGS: [&str; 7] = [
    "gzip", "x-gzip", "deflate", "br", "zstd", "identity", "compress",
];

fuzz_target!(|data: &[u8]| {
    let [e, a, b, c, body @ ..] = data else {
        return;
    };
    let encoding = ENCODINGS[*e as usize % ENCODINGS.len()];
    let limit = u32::from_be_bytes([0, *a, *b, *c]) as usize & 0xF_FFFF;
    let Some(out) = decode(encoding, body, limit) else {
        return;
    };
    assert!(
        out.len() <= limit,
        "{} octets décodés pour une borne de {limit}",
        out.len()
    );
    // Ce qui est décodé se réencode et se relit à l'identique (rejeu).
    let again = encode(encoding, &out).expect("encodage géré au décodage");
    assert_eq!(decode(encoding, &again, limit).as_deref(), Some(&out[..]));
});

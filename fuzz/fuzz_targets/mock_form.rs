//! Forme de mock reçue du contrôleur par le sidecar (`control::mock_form`) : décompressée
//! sous une borne, relue, puis chargée dans le moteur qui sert les requêtes. Une erreur,
//! jamais de panique.
//!
//! Entrée : un octet d'encodage (`identity`, `gzip`, `zstd`, `br`), puis le body.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::mock::{MockConfig, MockEngine, MockRequest};
use mimic_core::snapshot::MockSnapshot;
use mimic_proxy::fuzzing::decode;

/// Borne de décompression (`share.max_snapshot_bytes`), réduite pour le fuzzing.
const MAX: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let [e, body @ ..] = data else { return };
    let json = match e % 4 {
        0 => body.to_vec(),
        n => {
            let encoding = ["gzip", "zstd", "br"][n as usize - 1];
            let Some(json) = decode(encoding, body, MAX) else {
                return;
            };
            assert!(json.len() <= MAX);
            json
        }
    };
    // L'identifiant attendu vient de la requête : on le prend dans le document pour aller
    // au-delà de sa vérification.
    let id = serde_json::from_slice::<serde_json::Value>(&json)
        .ok()
        .and_then(|v| v["id"].as_str().map(String::from))
        .unwrap_or_default();
    let Ok(form) = MockSnapshot::from_json(&json, &id) else {
        return;
    };
    assert_eq!(
        MockSnapshot::from_json(&form.to_json(), &id).as_ref(),
        Ok(&form)
    );

    let engine = MockEngine::from_snapshot(&form, MockConfig::default());
    let _ = engine.authorities();
    for r in form.records.iter().take(8) {
        let req = MockRequest {
            authority: r.authority.clone(),
            method: r.method.clone(),
            path: r.path.clone(),
            query: r.query.clone(),
            headers: r.req_headers.clone(),
            body: vec![],
        };
        let _ = engine.candidates(&req);
        let _ = engine.respond(&req);
    }
});

//! Segments JSONL reçus par le contrôleur de n'importe quel pod authentifié, et snapshots
//! importés (`Snapshot::parse`, comme `Ctl::ingest`) : une erreur, jamais de panique ; ce qui
//! est accepté se relit à l'identique sous le même identifiant et se redacte de façon
//! idempotente, comme à l'ingestion (spec §7).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::exemplar::Exemplars;
use mimic_core::mock::{MockConfig, MockEngine};
use mimic_core::snapshot::{MockSnapshot, Snapshot};
use mimic_fuzz::{redact_idempotent, redactor};

fuzz_target!(|data: &[u8]| {
    // Exemples relus du support : les lignes illisibles sont écartées.
    let _ = Exemplars::parse(data);
    let Ok(snapshot) = Snapshot::parse(data) else {
        return;
    };

    let canonical = snapshot.to_jsonl();
    let again = Snapshot::from_jsonl(&canonical, &snapshot.id)
        .unwrap_or_else(|e| panic!("forme canonique relue autrement : {e}"));
    assert_eq!(again.exchanges, snapshot.exchanges);
    assert_eq!(again.to_jsonl(), canonical, "forme canonique instable");

    let r = redactor();
    for e in snapshot.exchanges.iter().take(16) {
        redact_idempotent(&r, e.clone());
    }

    // Forme de mock servie aux sidecars, et moteur construit dessus.
    let form = MockSnapshot::from_snapshot(&snapshot);
    let json = form.to_json();
    assert_eq!(
        MockSnapshot::from_json(&json, &snapshot.id).as_ref(),
        Ok(&form)
    );
    let engine = MockEngine::from_snapshot(&form, MockConfig::default());
    assert_eq!(
        engine.len(),
        MockEngine::new(snapshot.exchanges.iter(), MockConfig::default()).len()
    );
});

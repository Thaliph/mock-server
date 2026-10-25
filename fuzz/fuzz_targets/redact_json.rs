//! Redaction des bodies JSON (spec §7) : idempotente, et un secret sous un nom sensible ne
//! ressort jamais (RED-04), quelle que soit la profondeur du document.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::mock::MockRequest;
use mimic_core::model::Header;
use mimic_fuzz::{SECRET, contains, exchange, redact_idempotent, redactor, req_bytes};
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let r = redactor();
    // Body tel quel : illisible (10 000 `[`), il est réduit à sa taille, sans débordement.
    let raw = redact_idempotent(&r, exchange("application/json", data.to_vec()));
    if serde_json::from_slice::<Value>(data).is_err() && !data.is_empty() {
        assert!(req_bytes(&raw).is_empty(), "body JSON illisible gardé");
    }

    // Secret injecté à côté du document.
    let Ok(value) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    if contains(
        serde_json::to_string(&value).unwrap().as_bytes(),
        SECRET.as_bytes(),
    ) {
        return;
    }
    let injected = match value {
        Value::Object(mut map) => {
            map.insert("password".into(), Value::String(SECRET.into()));
            Value::Object(map)
        }
        other => serde_json::json!({"data": other, "api_key": SECRET}),
    };
    let body = serde_json::to_vec(&injected).unwrap();
    let mut e = exchange("application/json; charset=utf-8", body.clone());
    e.query.push(("token".into(), SECRET.into()));
    e.req_headers.push(Header {
        name: "authorization".into(),
        value: SECRET.into(),
    });
    let e = redact_idempotent(&r, e);
    assert!(
        !contains(req_bytes(&e), SECRET.as_bytes()),
        "secret gardé dans le body"
    );
    let text = serde_json::to_string(&e).unwrap();
    assert!(!text.contains(SECRET), "secret gardé dans l'échange");

    // Même body reçu par le mock. Illisible (profondeur au-delà de 128), il est gardé tel
    // quel pour la comparaison et jamais écrit (spec §7) : rien à vérifier.
    if serde_json::from_slice::<Value>(&body).is_err() {
        return;
    }
    let mut req = MockRequest {
        method: "POST".into(),
        headers: vec![Header {
            name: "Content-Type".into(),
            value: "application/json".into(),
        }],
        body,
        ..MockRequest::default()
    };
    r.redact_mock_request(&mut req);
    assert!(
        !contains(&req.body, SECRET.as_bytes()),
        "secret gardé par le mock"
    );
    let once = req.body.clone();
    r.redact_mock_request(&mut req);
    assert_eq!(req.body, once, "redaction du mock non idempotente");
});

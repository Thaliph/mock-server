//! Redaction des bodies `application/x-www-form-urlencoded` (spec §7) : idempotente, et un
//! secret sous un nom sensible ne ressort jamais (RED-04).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::mock::MockRequest;
use mimic_core::model::Header;
use mimic_fuzz::{SECRET, contains, exchange, redact_idempotent, redactor, req_bytes};

const FORM: &str = "application/x-www-form-urlencoded";

fuzz_target!(|data: &[u8]| {
    let r = redactor();
    redact_idempotent(&r, exchange(FORM, data.to_vec()));

    if contains(data, SECRET.as_bytes()) {
        return;
    }
    // Secret en tête et en fin de formulaire, sous deux noms sensibles.
    let mut body = format!("client_secret={SECRET}&").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("&password={SECRET}").as_bytes());
    let e = redact_idempotent(&r, exchange(FORM, body.clone()));
    assert!(
        !contains(req_bytes(&e), SECRET.as_bytes()),
        "secret gardé dans le body"
    );

    let mut req = MockRequest {
        method: "POST".into(),
        headers: vec![Header {
            name: "content-type".into(),
            value: FORM.into(),
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

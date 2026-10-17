//! Conversion OpenAPI 3.1 → 3.0.3 (spec §9.5, OAS-*).

use mimic_core::openapi::to_3_0;
use serde_json::{Value, json};

fn walk(v: &Value, f: &mut impl FnMut(&str, &Value)) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                f(k, x);
                walk(x, f);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
        _ => {}
    }
}

#[test]
fn oas_01_nullable_types_and_examples_are_converted() {
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "t", "version": "1"},
        "paths": {"/users/{userId}": {"get": {
            "parameters": [{"in": "path", "name": "userId", "required": true, "schema": {"type": "integer"}}],
            "responses": {"200": {"content": {"application/json": {
                "schema": {
                    "type": "object",
                    "properties": {
                        "nick": {"type": ["string", "null"], "examples": ["bob", "al"]},
                        "tags": {"type": "array", "items": {"type": ["integer", "null"]}},
                        "any": {"type": ["string", "integer"]}
                    }
                },
                "examples": {"sample-1": {"value": {"nick": null}}}
            }}}}
        }}}
    });
    let out = to_3_0(&doc);
    assert_eq!(out["openapi"], "3.0.3");
    let props = &out["paths"]["/users/{userId}"]["get"]["responses"]["200"]["content"]["application/json"]
        ["schema"]["properties"];
    assert_eq!(
        props["nick"],
        json!({"type": "string", "nullable": true, "example": "bob"})
    );
    assert_eq!(
        props["tags"]["items"],
        json!({"type": "integer", "nullable": true})
    );
    assert_eq!(
        props["any"],
        json!({"oneOf": [{"type": "string"}, {"type": "integer"}]})
    );
    // Les exemples d'un media type existent en 3.0 : ils sont gardés.
    let media =
        &out["paths"]["/users/{userId}"]["get"]["responses"]["200"]["content"]["application/json"];
    assert!(media["examples"]["sample-1"].is_object());

    let mut types_ok = true;
    walk(&out, &mut |k, v| {
        if k == "type" && v.is_array() {
            types_ok = false;
        }
    });
    assert!(types_ok, "un type tableau subsiste : {out:#}");
}

#[test]
fn oas_01_null_only_type_and_components() {
    let doc = json!({
        "openapi": "3.1.0",
        "components": {"schemas": {"Gone": {"type": "null"}, "Pet": {"type": "object", "examples": [{"id": 1}]}}}
    });
    let out = to_3_0(&doc);
    assert_eq!(
        out["components"]["schemas"]["Gone"],
        json!({"nullable": true})
    );
    assert_eq!(
        out["components"]["schemas"]["Pet"],
        json!({"type": "object", "example": {"id": 1}})
    );
}

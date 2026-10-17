//! Différence entre deux documents OpenAPI (spec §9.5, DIF-*).

use mimic_core::diff::{ChangeKind, diff};
use serde_json::{Value, json};

fn doc(paths: Value) -> Value {
    json!({"openapi": "3.1.0", "info": {"title": "t", "version": "1"}, "paths": paths})
}

fn pets() -> Value {
    doc(json!({
        "/pets": {"get": {
            "parameters": [{"in": "query", "name": "limit", "required": false, "schema": {"type": "integer"}}],
            "responses": {"200": {"description": "ok"}}
        }},
        "/pets/{petId}": {
            "get": {"parameters": [{"in": "path", "name": "petId", "required": true}], "responses": {"200": {}}},
            "delete": {"parameters": [{"in": "path", "name": "petId", "required": true}], "responses": {"204": {}}}
        }
    }))
}

#[test]
fn dif_01_same_document_has_no_change() {
    let d = diff(&pets(), &pets());
    assert!(d.changes.is_empty(), "{d:?}");
    assert!(!d.is_breaking());
}

#[test]
fn dif_02_removed_operation_is_breaking_added_is_not() {
    let mut new = pets();
    new["paths"]["/pets/{petId}"]
        .as_object_mut()
        .unwrap()
        .remove("delete");
    new["paths"]["/stores"] = json!({"get": {"responses": {"200": {}}}});
    let d = diff(&pets(), &new);
    assert_eq!(d.changes.len(), 2, "{d:?}");
    let removed = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::OperationRemoved)
        .unwrap();
    assert_eq!(
        (removed.method.as_str(), removed.path.as_str()),
        ("DELETE", "/pets/{petId}")
    );
    assert!(removed.breaking);
    let added = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::OperationAdded)
        .unwrap();
    assert_eq!(added.path, "/stores");
    assert!(!added.breaking);
    assert!(d.is_breaking());
}

#[test]
fn dif_02_renamed_path_parameter_is_the_same_operation() {
    let mut new = pets();
    let ops = new["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/pets/{petId}")
        .unwrap();
    new["paths"]["/pets/{id}"] = ops;
    let text = serde_json::to_string(&new)
        .unwrap()
        .replace("\"petId\"", "\"id\"");
    let new: Value = serde_json::from_str(&text).unwrap();
    let d = diff(&pets(), &new);
    assert!(
        d.changes.iter().all(|c| !matches!(
            c.kind,
            ChangeKind::OperationAdded | ChangeKind::OperationRemoved
        )),
        "{d:?}"
    );
}

#[test]
fn dif_03_parameter_becoming_required_is_breaking() {
    let mut new = pets();
    let params = new["paths"]["/pets"]["get"]["parameters"]
        .as_array_mut()
        .unwrap();
    params[0]["required"] = json!(true);
    params.push(json!({"in": "query", "name": "sort", "required": false}));
    let d = diff(&pets(), &new);
    assert_eq!(d.changes.len(), 2, "{d:?}");
    let req = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::ParameterRequired)
        .unwrap();
    assert_eq!(req.detail, "query limit");
    assert!(req.breaking);
    let added = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::ParameterAdded)
        .unwrap();
    assert_eq!(added.detail, "query sort");
    assert!(!added.breaking);
}

#[test]
fn dif_03_required_parameter_added_is_breaking() {
    let mut new = pets();
    new["paths"]["/pets"]["get"]["parameters"]
        .as_array_mut()
        .unwrap()
        .push(json!({"in": "header", "name": "x-tenant", "required": true}));
    let d = diff(&pets(), &new);
    assert_eq!(d.changes.len(), 1);
    assert!(d.changes[0].breaking);
}

#[test]
fn dif_04_response_codes() {
    let mut new = pets();
    let responses = new["paths"]["/pets/{petId}"]["get"]["responses"]
        .as_object_mut()
        .unwrap();
    responses.remove("200");
    responses.insert("404".into(), json!({}));
    let d = diff(&pets(), &new);
    let added = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::ResponseAdded)
        .unwrap();
    assert_eq!(added.detail, "404");
    assert!(!added.breaking);
    let removed = d
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::ResponseRemoved)
        .unwrap();
    assert_eq!(removed.detail, "200");
    assert!(removed.breaking);
}

#[test]
fn dif_text_output_is_sorted_and_flags_breaking() {
    let mut new = pets();
    new["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/pets/{petId}");
    let text = diff(&pets(), &new).to_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("CASSANT"));
    assert!(lines[0].contains("DELETE /pets/{petId}"));
    assert!(lines[1].contains("GET /pets/{petId}"));
}

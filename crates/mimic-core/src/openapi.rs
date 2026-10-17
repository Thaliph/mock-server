//! Conversion d'un document OpenAPI 3.1 en 3.0.3, pour les outils qui ne lisent pas
//! encore la 3.1 (spec §6.1, §9.5).

use serde_json::{Map, Value, json};

/// Convertit un document 3.1 produit par l'inférence en document 3.0.3.
pub fn to_3_0(doc: &Value) -> Value {
    let mut out = doc.clone();
    walk(&mut out);
    if let Some(m) = out.as_object_mut() {
        m.insert("openapi".into(), json!("3.0.3"));
    }
    out
}

/// Parcourt le document et convertit chaque schéma rencontré : valeurs de `schema`
/// (paramètres, media types) et `components.schemas`.
fn walk(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                match k.as_str() {
                    "schema" => schema(x),
                    "schemas" => {
                        if let Value::Object(all) = x {
                            all.values_mut().for_each(schema);
                        }
                    }
                    _ => walk(x),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(walk),
        _ => {}
    }
}

fn schema(v: &mut Value) {
    let Value::Object(m) = v else { return };
    for key in ["items", "additionalProperties", "not"] {
        if let Some(x) = m.get_mut(key) {
            schema(x);
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(a)) = m.get_mut(key) {
            a.iter_mut().for_each(schema);
        }
    }
    if let Some(Value::Object(props)) = m.get_mut("properties") {
        props.values_mut().for_each(schema);
    }
    if let Some(Value::Array(examples)) = m.remove("examples")
        && let Some(first) = examples.into_iter().next()
    {
        m.insert("example".into(), first);
    }
    let ty = m.remove("type");
    match ty {
        Some(Value::Array(types)) => nullable_types(m, types),
        Some(Value::String(t)) if t == "null" => {
            m.insert("nullable".into(), json!(true));
        }
        Some(t) => {
            m.insert("type".into(), t);
        }
        None => {}
    }
}

/// `type: [T, "null"]` devient `type: T` + `nullable` ; plusieurs types non nuls, un `oneOf`.
fn nullable_types(m: &mut Map<String, Value>, types: Vec<Value>) {
    let nullable = types.iter().any(|t| t == "null");
    let rest: Vec<Value> = types.into_iter().filter(|t| t != "null").collect();
    match rest.len() {
        0 => {}
        1 => {
            m.insert("type".into(), rest.into_iter().next().unwrap_or_default());
        }
        _ => {
            let one_of = rest.into_iter().map(|t| json!({ "type": t })).collect();
            m.insert("oneOf".into(), Value::Array(one_of));
        }
    }
    if nullable {
        m.insert("nullable".into(), json!(true));
    }
}

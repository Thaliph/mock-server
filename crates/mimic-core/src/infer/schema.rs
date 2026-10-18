//! Inférence incrémentale de JSON Schema 2020-12 à partir d'exemples (spec §6.3).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::path::{is_iso_date, is_uuid};
use crate::redact::is_email;

/// Conditions pour déclarer une chaîne comme `enum` (spec §6.3). Un `enum` ferme le contrat :
/// il n'est produit que si l'échantillon est assez grand pour le justifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumRules {
    /// Nombre minimal d'observations de la chaîne.
    pub min_observations: u64,
    /// Nombre minimal d'observations de chaque valeur.
    pub min_value_count: u64,
    /// Nombre maximal de valeurs distinctes.
    pub max_values: usize,
}

impl Default for EnumRules {
    fn default() -> Self {
        Self {
            min_observations: 20,
            min_value_count: 2,
            max_values: 10,
        }
    }
}

const FORMATS: [&str; 6] = ["date-time", "date", "uuid", "email", "uri", "ipv4"];

/// Accumulateur de schéma : on y ajoute des valeurs, on en tire un schéma.
#[derive(Debug, Default, Clone)]
pub struct SchemaAcc {
    count: u64,
    nulls: u64,
    bools: u64,
    integers: u64,
    numbers: u64,
    strings: Option<StringAcc>,
    object: Option<ObjectAcc>,
    array: Option<Box<SchemaAcc>>,
    arrays: u64,
}

#[derive(Debug, Default, Clone)]
struct StringAcc {
    count: u64,
    /// Occurrences de chaque valeur distincte, `None` dès que [`EnumRules::max_values`] est
    /// dépassé : la mémoire reste bornée quel que soit le nombre de valeurs.
    values: Option<BTreeMap<String, u64>>,
    /// Formats encore compatibles avec toutes les valeurs vues.
    formats: BTreeSet<&'static str>,
}

#[derive(Debug, Default, Clone)]
struct ObjectAcc {
    count: u64,
    props: BTreeMap<String, SchemaAcc>,
}

impl SchemaAcc {
    pub fn add(&mut self, v: &Value, rules: &EnumRules) {
        self.count += 1;
        match v {
            Value::Null => self.nulls += 1,
            Value::Bool(_) => self.bools += 1,
            Value::Number(n) if n.is_i64() || n.is_u64() => self.integers += 1,
            Value::Number(_) => self.numbers += 1,
            Value::String(s) => self.add_string(s, rules),
            Value::Array(items) => {
                self.arrays += 1;
                let acc = self.array.get_or_insert_with(Default::default);
                for item in items {
                    acc.add(item, rules);
                }
            }
            Value::Object(map) => {
                let obj = self.object.get_or_insert_with(Default::default);
                obj.count += 1;
                for (k, v) in map {
                    obj.props.entry(k.clone()).or_default().add(v, rules);
                }
            }
        }
    }

    /// Ajoute une valeur textuelle typée au mieux (paramètres de query, headers, formulaires).
    pub fn add_scalar_text(&mut self, s: &str, rules: &EnumRules) {
        let v = if let Ok(i) = s.parse::<i64>() {
            Value::from(i)
        } else if s == "true" || s == "false" {
            Value::Bool(s == "true")
        } else {
            Value::String(s.to_string())
        };
        self.add(&v, rules);
    }

    fn add_string(&mut self, s: &str, rules: &EnumRules) {
        let acc = self.strings.get_or_insert_with(|| StringAcc {
            values: Some(BTreeMap::new()),
            formats: FORMATS.into_iter().collect(),
            ..Default::default()
        });
        acc.count += 1;
        if let Some(values) = &mut acc.values {
            match values.get_mut(s) {
                Some(n) => *n += 1,
                None => {
                    values.insert(s.to_string(), 1);
                }
            }
            if values.len() > rules.max_values {
                acc.values = None;
            }
        }
        acc.formats.retain(|f| matches_format(f, s));
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn to_schema(&self, rules: &EnumRules) -> Value {
        let mut variants: Vec<Value> = vec![];
        if self.bools > 0 {
            variants.push(json!({"type": "boolean"}));
        }
        if self.numbers > 0 {
            variants.push(json!({"type": "number"}));
        } else if self.integers > 0 {
            variants.push(json!({"type": "integer"}));
        }
        if let Some(s) = &self.strings {
            variants.push(s.to_schema(rules));
        }
        if let Some(o) = &self.object {
            variants.push(o.to_schema(rules));
        }
        if self.arrays > 0 {
            let items = self
                .array
                .as_ref()
                .filter(|a| a.count > 0)
                .map(|a| a.to_schema(rules))
                .unwrap_or_else(|| json!({}));
            variants.push(json!({"type": "array", "items": items}));
        }
        let nullable = self.nulls > 0;
        match variants.len() {
            0 if nullable => json!({"type": "null"}),
            0 => json!({}),
            1 => {
                let mut schema = variants.pop().unwrap();
                if nullable {
                    let t = schema["type"].clone();
                    schema["type"] = json!([t, "null"]);
                }
                schema
            }
            _ => {
                if nullable {
                    variants.push(json!({"type": "null"}));
                }
                json!({"oneOf": variants})
            }
        }
    }
}

impl StringAcc {
    fn to_schema(&self, rules: &EnumRules) -> Value {
        let mut schema = Map::new();
        schema.insert("type".into(), "string".into());
        // Le premier format compatible dans l'ordre de FORMATS (date-time avant date, etc.).
        if let Some(f) = FORMATS.iter().find(|f| self.formats.contains(*f)) {
            schema.insert("format".into(), (*f).into());
        } else if let Some(values) = self.enum_values(rules) {
            schema.insert("enum".into(), values);
        }
        Value::Object(schema)
    }

    /// Valeurs de l'`enum`, triées, si l'échantillon justifie de fermer le contrat (§6.3).
    fn enum_values(&self, rules: &EnumRules) -> Option<Value> {
        let values = self.values.as_ref()?;
        let enough = self.count >= rules.min_observations
            && (2..=rules.max_values).contains(&values.len())
            && values.values().all(|n| *n >= rules.min_value_count);
        if !enough {
            return None;
        }
        // Série numérotée (`user0`, `user6`) : deux valeurs de même squelette sans leurs chiffres.
        let skeletons: BTreeSet<String> = values.keys().map(|v| skeleton(v)).collect();
        if skeletons.len() < values.len() {
            return None;
        }
        Some(values.keys().cloned().collect())
    }
}

/// La valeur où chaque suite de chiffres est remplacée par `#` : `user12` → `user#`.
fn skeleton(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

impl ObjectAcc {
    fn to_schema(&self, rules: &EnumRules) -> Value {
        let properties: Map<String, Value> = self
            .props
            .iter()
            .map(|(k, acc)| (k.clone(), acc.to_schema(rules)))
            .collect();
        let required: Vec<Value> = self
            .props
            .iter()
            .filter(|(_, acc)| acc.count == self.count)
            .map(|(k, _)| Value::from(k.as_str()))
            .collect();
        let mut schema = Map::new();
        schema.insert("type".into(), "object".into());
        if !properties.is_empty() {
            schema.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        Value::Object(schema)
    }
}

fn matches_format(format: &str, s: &str) -> bool {
    match format {
        "date-time" => is_date_time(s),
        "date" => is_iso_date(s),
        "uuid" => is_uuid(s),
        "email" => is_email(s),
        "uri" => is_uri(s),
        "ipv4" => is_ipv4(s),
        _ => false,
    }
}

/// RFC 3339 : `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
fn is_date_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 || !s.get(..10).is_some_and(is_iso_date) || !matches!(b[10], b'T' | b't') {
        return false;
    }
    let time = &b[11..];
    let hms = time.len() >= 8
        && time[..8].iter().enumerate().all(|(i, c)| match i {
            2 | 5 => *c == b':',
            _ => c.is_ascii_digit(),
        });
    if !hms {
        return false;
    }
    let mut rest = &time[8..];
    if let Some(frac) = rest.strip_prefix(b".") {
        let digits = frac.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        rest = &frac[digits..];
    }
    match rest {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

fn is_uri(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        && !rest.is_empty()
        && !s.contains(char::is_whitespace)
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_times() {
        assert!(is_date_time("2026-10-17T08:33:09Z"));
        assert!(is_date_time("2026-10-17T08:33:09.123+02:00"));
        assert!(!is_date_time("2026-10-17 08:33:09"));
        assert!(!is_date_time("2026-10-17T08:33:09"));
    }

    #[test]
    fn ipv4() {
        assert!(is_ipv4("192.168.0.1"));
        assert!(!is_ipv4("256.0.0.1"));
        assert!(!is_ipv4("1.2.3"));
        assert!(!is_ipv4("01.2.3.4"));
    }

    #[test]
    fn empty_array_has_open_items() {
        let rules = EnumRules::default();
        let mut acc = SchemaAcc::default();
        acc.add(&json!([]), &rules);
        assert_eq!(acc.to_schema(&rules), json!({"type": "array", "items": {}}));
    }

    #[test]
    fn only_nulls() {
        let rules = EnumRules::default();
        let mut acc = SchemaAcc::default();
        acc.add(&Value::Null, &rules);
        assert_eq!(acc.to_schema(&rules), json!({"type": "null"}));
    }

    #[test]
    fn skeletons() {
        assert_eq!(skeleton("user12"), "user#");
        assert_eq!(skeleton("SKU-3-14"), "SKU-#-#");
        assert_eq!(skeleton("paid"), "paid");
    }
}

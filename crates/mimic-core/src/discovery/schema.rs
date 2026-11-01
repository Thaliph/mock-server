//! JSON Schema inference (spec §6): a mergeable description of JSON values, query strings
//! and headers. It holds types, formats and key names, never values. Merging is commutative
//! and associative, so recording one exchange is merging a one-exchange schema.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::redact::Redactor;
use super::template::{is_rfc3339, is_uuid};

/// Levels described; a value deeper than this is `{}` (the root is level 1).
const MAX_LEVEL: usize = 16;
/// Distinct keys of an object before it becomes a map.
const MAX_KEYS: usize = 128;
/// A map counts as this many copies of its value: it replaces up to twice `MAX_KEYS` keys.
const MAP_WEIGHT: u64 = 256;
/// Items of an array that are looked at.
const MAX_ITEMS: usize = 256;
/// Keys of a query, form or header set.
pub const MAX_PARAMS: usize = 64;

/// Header names that say nothing about the API (hop-by-hop and framing).
const SKIPPED_HEADERS: [&str; 7] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "upgrade",
    "content-length",
    "host",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Kind {
    Array,
    Boolean,
    Integer,
    Null,
    Number,
    Object,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Format {
    Date,
    DateTime,
    Email,
    Ipv4,
    Ipv6,
    Uri,
    Uuid,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::DateTime => "date-time",
            Self::Email => "email",
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
            Self::Uri => "uri",
            Self::Uuid => "uuid",
        }
    }

    fn of(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if is_uuid(b) {
            Some(Self::Uuid)
        } else if is_rfc3339(b) {
            Some(if b.len() == 10 {
                Self::Date
            } else {
                Self::DateTime
            })
        } else if s.parse::<Ipv4Addr>().is_ok() {
            Some(Self::Ipv4)
        } else if s.parse::<Ipv6Addr>().is_ok() {
            Some(Self::Ipv6)
        } else if is_email(s) {
            Some(Self::Email)
        } else if is_uri(s) {
            Some(Self::Uri)
        } else {
            None
        }
    }
}

fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains('@')
                && !s
                    .bytes()
                    .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        }
        None => false,
    }
}

fn is_uri(s: &str) -> bool {
    match s.split_once("://") {
        Some((scheme, rest)) => {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
                && !rest.is_empty()
                && !s
                    .bytes()
                    .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        }
        None => false,
    }
}

/// What the strings seen so far have in common.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Formats {
    #[default]
    Unseen,
    All(Format),
    Mixed,
}

impl Formats {
    fn of(s: &str) -> Self {
        Format::of(s).map_or(Self::Mixed, Self::All)
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unseen, x) | (x, Self::Unseen) => x,
            (Self::All(a), Self::All(b)) if a == b => Self::All(a),
            _ => Self::Mixed,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Object {
    props: BTreeMap<String, Schema>,
    required: BTreeSet<String>,
    /// Set once the object has too many keys: the merge of all its values, and no `props`.
    map: Option<Box<Schema>>,
}

impl Object {
    fn merge(self, other: Self) -> Self {
        if self.map.is_none() && other.map.is_none() {
            let required = self
                .required
                .intersection(&other.required)
                .cloned()
                .collect();
            let mut props = self.props;
            for (k, v) in other.props {
                let merged = match props.remove(&k) {
                    Some(mine) => mine.merge(v),
                    None => v,
                };
                props.insert(k, merged);
            }
            if props.len() <= MAX_KEYS {
                return Self {
                    props,
                    required,
                    map: None,
                };
            }
            return Self {
                map: Some(Box::new(merge_all(props.into_values()))),
                ..Self::default()
            };
        }
        let values = |o: Self| match o.map {
            Some(m) => *m,
            None => merge_all(o.props.into_values()),
        };
        let merged = values(self).merge(values(other));
        Self {
            map: Some(Box::new(merged)),
            ..Self::default()
        }
    }
}

fn merge_all(schemas: impl Iterator<Item = Schema>) -> Schema {
    schemas.fold(Schema::default(), Schema::merge)
}

/// A JSON Schema (2020-12 subset) inferred from the values seen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    kinds: BTreeSet<Kind>,
    format: Formats,
    items: Option<Box<Schema>>,
    object: Option<Object>,
}

impl Schema {
    /// Schema of one JSON value, already redacted. The root is level 1.
    pub fn infer(value: &Value) -> Self {
        Self::infer_at(value, 1)
    }

    fn infer_at(value: &Value, level: usize) -> Self {
        if level > MAX_LEVEL {
            return Self::default();
        }
        let mut s = Self::default();
        match value {
            Value::Null => {
                s.kinds.insert(Kind::Null);
            }
            Value::Bool(_) => {
                s.kinds.insert(Kind::Boolean);
            }
            Value::Number(n) => {
                let integral = n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.is_finite() && f.fract() == 0.0);
                s.kinds.insert(if integral {
                    Kind::Integer
                } else {
                    Kind::Number
                });
            }
            Value::String(text) => {
                s.kinds.insert(Kind::String);
                s.format = Formats::of(text);
            }
            Value::Array(items) => {
                s.kinds.insert(Kind::Array);
                if !items.is_empty() {
                    s.items = Some(Box::new(merge_all(
                        items
                            .iter()
                            .take(MAX_ITEMS)
                            .map(|v| Self::infer_at(v, level + 1)),
                    )));
                }
            }
            Value::Object(map) => {
                s.kinds.insert(Kind::Object);
                s.object = Some(if map.len() > MAX_KEYS {
                    Object {
                        map: Some(Box::new(merge_all(
                            map.values().map(|v| Self::infer_at(v, level + 1)),
                        ))),
                        ..Object::default()
                    }
                } else {
                    Object {
                        props: map
                            .iter()
                            .map(|(k, v)| (k.clone(), Self::infer_at(v, level + 1)))
                            .collect(),
                        required: map.keys().cloned().collect(),
                        map: None,
                    }
                });
            }
        }
        s
    }

    /// Nodes of the schema, counted as the 512-node cap counts them (spec §6).
    pub(super) fn nodes(&self) -> usize {
        usize::try_from(self.weight(1, usize::MAX)).unwrap_or(usize::MAX)
    }

    fn weight(&self, level: usize, cut: usize) -> u64 {
        if level > cut {
            return 1;
        }
        let mut n: u64 = 1;
        if let Some(items) = &self.items {
            n = n.saturating_add(items.weight(level + 1, cut));
        }
        if let Some(o) = &self.object {
            for v in o.props.values() {
                n = n.saturating_add(v.weight(level + 1, cut));
            }
            if let Some(m) = &o.map {
                n = n.saturating_add(m.weight(level + 1, cut).saturating_mul(MAP_WEIGHT));
            }
        }
        n
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.kinds.extend(other.kinds);
        self.format = self.format.merge(other.format);
        self.items = match (self.items, other.items) {
            (Some(a), Some(b)) => Some(Box::new(a.merge(*b))),
            (a, b) => a.or(b),
        };
        self.object = match (self.object, other.object) {
            (Some(a), Some(b)) => Some(a.merge(b)),
            (a, b) => a.or(b),
        };
        self
    }

    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        let mut names: Vec<&str> = Vec::new();
        for k in &self.kinds {
            names.push(match k {
                Kind::Array => "array",
                Kind::Boolean => "boolean",
                // Integers are numbers: `number` covers both.
                Kind::Integer if self.kinds.contains(&Kind::Number) => continue,
                Kind::Integer => "integer",
                Kind::Null => "null",
                Kind::Number => "number",
                Kind::Object => "object",
                Kind::String => "string",
            });
        }
        names.sort_unstable();
        match names.as_slice() {
            [] => {}
            [one] => {
                out.insert("type".into(), json!(one));
            }
            many => {
                out.insert("type".into(), json!(many));
            }
        }
        if let Formats::All(f) = self.format {
            out.insert("format".into(), json!(f.name()));
        }
        if let Some(items) = &self.items {
            out.insert("items".into(), items.to_json());
        }
        if let Some(o) = &self.object {
            if let Some(m) = &o.map {
                out.insert("additionalProperties".into(), m.to_json());
            } else if !o.props.is_empty() {
                let props: Map<String, Value> = o
                    .props
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect();
                out.insert("properties".into(), props.into());
                if !o.required.is_empty() {
                    out.insert("required".into(), json!(o.required));
                }
            }
        }
        out.into()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Param {
    /// Exchanges that had the key.
    count: u64,
    /// True when some exchange repeated the key.
    array: bool,
    schema: Schema,
}

/// Keyed values of one kind (query keys, form fields or headers) over many exchanges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    exchanges: u64,
    keys: BTreeMap<String, Param>,
}

/// A decoded, redacted key and value.
#[derive(Debug, Clone)]
pub(super) struct Pair {
    pub key: String,
    pub value: String,
}

fn decode(s: &str) -> String {
    percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// The pairs of a query string or form body: keys and values percent-decoded, then redacted.
pub(super) fn query_pairs(redactor: &Redactor, query: &str) -> Vec<Pair> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let key = decode(k);
            let value = redactor.field(&key, &decode(v));
            Pair { key, value }
        })
        .collect()
}

/// The pairs of a header list: names in lower case, skipped names absent, values redacted.
pub(super) fn header_pairs(redactor: &Redactor, headers: &[(&str, &str)]) -> Vec<Pair> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let key = name.to_ascii_lowercase();
            if key.starts_with(':') || SKIPPED_HEADERS.contains(&key.as_str()) {
                return None;
            }
            let value = redactor.header(&key, value);
            Some(Pair { key, value })
        })
        .collect()
}

impl Params {
    /// A query string or an `application/x-www-form-urlencoded` body.
    pub fn from_query(redactor: &Redactor, query: &str) -> Self {
        Self::from_pairs(&query_pairs(redactor, query))
    }

    pub fn from_headers(redactor: &Redactor, headers: &[(&str, &str)]) -> Self {
        Self::from_pairs(&header_pairs(redactor, headers))
    }

    pub(super) fn from_pairs(pairs: &[Pair]) -> Self {
        let mut keys: BTreeMap<String, Param> = BTreeMap::new();
        for p in pairs {
            let value = Schema::infer(&Value::String(p.value.clone()));
            match keys.remove(&p.key) {
                Some(mut seen) => {
                    seen.array = true;
                    seen.schema = seen.schema.merge(value);
                    keys.insert(p.key.clone(), seen);
                }
                None => {
                    keys.insert(
                        p.key.clone(),
                        Param {
                            count: 1,
                            array: false,
                            schema: value,
                        },
                    );
                }
            }
        }
        while keys.len() > MAX_PARAMS {
            keys.pop_last();
        }
        Self { exchanges: 1, keys }
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.exchanges += other.exchanges;
        for (k, p) in other.keys {
            let merged = match self.keys.remove(&k) {
                Some(mine) => Param {
                    count: mine.count + p.count,
                    array: mine.array || p.array,
                    schema: mine.schema.merge(p.schema),
                },
                None => p,
            };
            self.keys.insert(k, merged);
        }
        while self.keys.len() > MAX_PARAMS {
            self.keys.pop_last();
        }
        self
    }

    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("type".into(), json!("object"));
        if self.keys.is_empty() {
            return out.into();
        }
        let props: Map<String, Value> = self
            .keys
            .iter()
            .map(|(k, p)| {
                let v = p.schema.to_json();
                (
                    k.clone(),
                    if p.array {
                        json!({"type": "array", "items": v})
                    } else {
                        v
                    },
                )
            })
            .collect();
        out.insert("properties".into(), props.into());
        let required: Vec<&String> = self
            .keys
            .iter()
            .filter(|(_, p)| p.count == self.exchanges)
            .map(|(k, _)| k)
            .collect();
        if !required.is_empty() {
            out.insert("required".into(), json!(required));
        }
        out.into()
    }
}

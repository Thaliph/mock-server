//! JSON Schema inference (spec §6): a mergeable description of JSON values, query strings
//! and headers. It holds types, formats and key names, never values. Merging is commutative
//! and associative, so recording one exchange is merging a one-exchange schema.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::check::Violation;
use super::redact::{MARKER, Redactor};
use super::template::{is_rfc3339, is_uuid};

/// Levels described; a value deeper than this is `{}` (the root is level 1).
const MAX_LEVEL: usize = 16;
/// Nodes a body schema keeps (spec §6); deeper levels are cut away to fit.
pub(super) const MAX_NODES: usize = 512;
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
#[serde(deny_unknown_fields)]
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
                    Some(mine) => mine.merge_node(v),
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
        let merged = values(self).merge_node(values(other));
        Self {
            map: Some(Box::new(merged)),
            ..Self::default()
        }
    }
}

/// Longest name (query key, header name, JSON field name) kept, in bytes.
pub(super) const MAX_NAME: usize = 128;

/// A name as it is recorded: its first `MAX_NAME` bytes, cut on a character boundary. Redaction
/// is decided on the full name, before this.
pub(super) fn cut_name(name: &str) -> &str {
    if name.len() <= MAX_NAME {
        return name;
    }
    let mut end = MAX_NAME;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn merge_all(schemas: impl Iterator<Item = Schema>) -> Schema {
    schemas.fold(Schema::default(), Schema::merge_node)
}

/// A JSON Schema (2020-12 subset) inferred from the values seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    kinds: BTreeSet<Kind>,
    format: Formats,
    items: Option<Box<Schema>>,
    object: Option<Object>,
    /// Deepest level kept (the root is level 1). Meaningful on the root of a schema only;
    /// nested nodes keep the default.
    #[serde(default = "full_cut", skip_serializing_if = "is_full_cut")]
    cut: u8,
}

fn full_cut() -> u8 {
    MAX_LEVEL as u8
}

fn is_full_cut(cut: &u8) -> bool {
    *cut == full_cut()
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            kinds: BTreeSet::new(),
            format: Formats::default(),
            items: None,
            object: None,
            cut: full_cut(),
        }
    }
}

impl Schema {
    /// Schema of one JSON value, already redacted. The root is level 1.
    pub fn infer(value: &Value) -> Self {
        let mut s = Self::infer_at(value, 1);
        s.fit(MAX_LEVEL);
        s
    }

    /// The schema of the marker: what a value under a redacted name is recorded as.
    fn marker() -> Self {
        Self::infer(&Value::String(MARKER.to_string()))
    }

    /// Replaces what sits under a redacted property name by the schema of the marker, as the
    /// recorder does (spec §7.3, re-redaction).
    pub(super) fn redact_names(&mut self, redactor: &Redactor) {
        if let Some(items) = &mut self.items {
            items.redact_names(redactor);
        }
        let Some(object) = &mut self.object else {
            return;
        };
        for (name, schema) in &mut object.props {
            if redactor.name_matches(name) {
                *schema = Self::marker();
            } else {
                schema.redact_names(redactor);
            }
        }
        if let Some(map) = &mut object.map {
            map.redact_names(redactor);
        }
    }

    /// Cuts the schema at the deepest level, at most `max`, where it has at most
    /// `MAX_NODES` nodes, and records that level.
    fn fit(&mut self, max: usize) {
        let level = (0..=max)
            .rev()
            .find(|&l| self.weight(1, l) <= MAX_NODES as u64)
            .unwrap_or(0);
        self.truncate(1, level);
        self.cut = level as u8;
    }

    /// Replaces every node deeper than `cut` by `{}`.
    fn truncate(&mut self, level: usize, cut: usize) {
        if level > cut {
            *self = Self::default();
            return;
        }
        if let Some(items) = &mut self.items {
            items.truncate(level + 1, cut);
        }
        if let Some(o) = &mut self.object {
            for v in o.props.values_mut() {
                v.truncate(level + 1, cut);
            }
            if let Some(m) = &mut o.map {
                m.truncate(level + 1, cut);
            }
        }
    }

    /// Merges two schemas under the 512-node cap (spec §6): both are cut at the lower of
    /// their levels, united, then cut again where the union fits.
    pub fn merge(mut self, mut other: Self) -> Self {
        let cut = self.cut.min(other.cut);
        self.truncate(1, usize::from(cut));
        other.truncate(1, usize::from(cut));
        let mut merged = self.merge_node(other);
        merged.cut = cut;
        merged.fit(usize::from(cut));
        merged
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
                    let mut props: BTreeMap<String, Schema> = BTreeMap::new();
                    for (k, v) in map {
                        let child = Self::infer_at(v, level + 1);
                        let slot = props.entry(cut_name(k).to_string()).or_default();
                        *slot = std::mem::take(slot).merge_node(child);
                    }
                    Object {
                        required: props.keys().cloned().collect(),
                        props,
                        map: None,
                    }
                });
            }
        }
        s
    }

    /// Within the bounds of spec §6: at most `MAX_NODES` nodes and `MAX_LEVEL` levels, a cut
    /// level of at most `MAX_LEVEL`, names of at most `MAX_NAME` bytes. The `{}` that stands
    /// where a deeper value was cut away is not a level.
    pub(super) fn check(&self) -> Result<(), Violation> {
        self.check_node()?;
        if self.levels() > MAX_LEVEL {
            return Err(Violation::SchemaLevels);
        }
        if self.weight(1, usize::MAX) > MAX_NODES as u64 {
            return Err(Violation::SchemaNodes);
        }
        Ok(())
    }

    fn check_node(&self) -> Result<(), Violation> {
        if usize::from(self.cut) > MAX_LEVEL {
            return Err(Violation::SchemaCut);
        }
        if let Some(items) = &self.items {
            items.check_node()?;
        }
        if let Some(o) = &self.object {
            let names = o.props.keys().chain(&o.required);
            if names.into_iter().any(|n| n.len() > MAX_NAME) {
                return Err(Violation::NameLength);
            }
            for v in o.props.values().chain(o.map.as_deref()) {
                v.check_node()?;
            }
        }
        Ok(())
    }

    fn levels(&self) -> usize {
        if self.kinds.is_empty() && self.items.is_none() && self.object.is_none() {
            return 0;
        }
        let object = self.object.iter().flat_map(|o| {
            o.props
                .values()
                .map(Schema::levels)
                .chain(o.map.as_deref().map(Schema::levels))
        });
        let below = object
            .chain(self.items.as_deref().map(Schema::levels))
            .max()
            .unwrap_or(0);
        1 + below
    }

    /// Nodes of the schema, counted as the 512-node cap counts them (spec §6).
    #[cfg(test)]
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

    /// The union of two schemas, whatever their cut levels: the merge of the structures.
    fn merge_node(mut self, other: Self) -> Self {
        self.kinds.extend(other.kinds);
        self.format = self.format.merge(other.format);
        self.items = match (self.items, other.items) {
            (Some(a), Some(b)) => Some(Box::new(a.merge_node(*b))),
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
#[serde(deny_unknown_fields)]
struct Param {
    /// Exchanges that had the key.
    count: u64,
    /// True when some exchange repeated the key.
    array: bool,
    schema: Schema,
}

/// Keyed values of one kind (query keys, form fields or headers) over many exchanges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
            let key = cut_name(&key).to_string();
            Pair { key, value }
        })
        .collect()
}

/// The pairs of a header list: names in lower case, skipped names absent, values redacted.
pub(super) fn header_pairs(redactor: &Redactor, headers: &[(&str, &str)]) -> Vec<Pair> {
    kept_headers(headers)
        .into_iter()
        .map(|(key, value)| Pair {
            value: redactor.header(&key, value),
            key: cut_name(&key).to_string(),
        })
        .collect()
}

/// The headers a report keeps, in arrival order, with lower-case names: the skipped names and
/// pseudo-headers are absent, and only those of the first `MAX_PARAMS` names (cut to 128
/// bytes) in byte order are left. What is counted or redacted must be limited to these.
pub fn kept_headers<'a>(headers: &[(&'a str, &'a str)]) -> Vec<(String, &'a str)> {
    let mut kept: Vec<(String, &'a str)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), *value))
        .filter(|(key, _)| !key.starts_with(':') && !SKIPPED_HEADERS.contains(&key.as_str()))
        .collect();
    let names: BTreeSet<&str> = kept.iter().map(|(key, _)| cut_name(key)).collect();
    if names.len() > MAX_PARAMS {
        let first: BTreeSet<String> = names
            .into_iter()
            .take(MAX_PARAMS)
            .map(str::to_string)
            .collect();
        kept.retain(|(key, _)| first.contains(cut_name(key)));
    }
    kept
}

impl Params {
    /// Gives a key under a redacted name the schema of the marker.
    pub(super) fn redact_names(&mut self, redactor: &Redactor) {
        for (name, param) in &mut self.keys {
            if redactor.name_matches(name) {
                param.schema = Schema::marker();
            }
        }
    }

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
        self.exchanges = self.exchanges.saturating_add(other.exchanges);
        for (k, p) in other.keys {
            let merged = match self.keys.remove(&k) {
                Some(mine) => Param {
                    count: mine.count.saturating_add(p.count),
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

    /// At most `MAX_PARAMS` keys of at most `MAX_NAME` bytes, each with a valid schema.
    pub(super) fn check(&self) -> Result<(), Violation> {
        if self.keys.len() > MAX_PARAMS {
            return Err(Violation::Params);
        }
        for (key, param) in &self.keys {
            if key.len() > MAX_NAME {
                return Err(Violation::NameLength);
            }
            param.schema.check()?;
        }
        Ok(())
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

//! Schema-less protobuf wire-format reader and its inverse (spec §6.2, GRP-04 to GRP-09).

use std::collections::BTreeMap;
use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value as Json;

pub const MAX_DEPTH: usize = 32;
pub const MAX_FIELDS: usize = 4096;
pub const MAX_PAYLOAD: usize = 256 * 1024;
/// Largest field number, 2^29 - 1.
pub const MAX_FIELD_NUMBER: u32 = (1 << 29) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    TooLarge,
    Truncated,
    BadTag,
    Group,
    BadVarint,
    TooManyFields,
    /// More than `MAX_DEPTH` levels of messages.
    TooDeep,
    /// The JSON is not the decoded form of a message.
    Json(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Message(Message),
    Str(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub number: u32,
    pub value: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub fields: Vec<Field>,
}

/// One field as it lies on the wire, before a length-delimited value is classified.
enum Raw<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Len(&'a [u8]),
}

/// Reads the fields of a payload one after the other; every error is a malformed payload.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, WireError> {
        let mut value = 0u64;
        for i in 0..10 {
            let Some((&byte, rest)) = self.rest.split_first() else {
                return Err(WireError::Truncated);
            };
            self.rest = rest;
            // The tenth byte carries bit 63 only.
            if i == 9 && byte > 1 {
                return Err(WireError::BadVarint);
            }
            value |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(WireError::BadVarint)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if n > self.rest.len() {
            return Err(WireError::Truncated);
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn next(&mut self) -> Result<Option<(u32, Raw<'a>)>, WireError> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        let tag = self.varint()?;
        let number = u32::try_from(tag >> 3).map_err(|_| WireError::BadTag)?;
        if number == 0 || number > MAX_FIELD_NUMBER {
            return Err(WireError::BadTag);
        }
        let raw = match tag & 7 {
            0 => Raw::Varint(self.varint()?),
            1 => {
                let bytes = self.take(8)?.try_into().map_err(|_| WireError::Truncated)?;
                Raw::Fixed64(u64::from_le_bytes(bytes))
            }
            2 => {
                let n = usize::try_from(self.varint()?).map_err(|_| WireError::Truncated)?;
                Raw::Len(self.take(n)?)
            }
            5 => {
                let bytes = self.take(4)?.try_into().map_err(|_| WireError::Truncated)?;
                Raw::Fixed32(u32::from_le_bytes(bytes))
            }
            3 | 4 => return Err(WireError::Group),
            _ => return Err(WireError::BadTag),
        };
        Ok(Some((number, raw)))
    }
}

/// The number of fields when the whole payload is well formed, without building anything.
fn scan(bytes: &[u8]) -> Result<usize, WireError> {
    let mut reader = Reader { rest: bytes };
    let mut count = 0usize;
    while reader.next()?.is_some() {
        count += 1;
        if count > MAX_FIELDS {
            return Err(WireError::TooManyFields);
        }
    }
    Ok(count)
}

/// Builds the fields of a payload that `scan` accepted; `budget` is what the tree may still
/// spend on fields, the direct fields of this payload already taken out of it.
fn build(bytes: &[u8], depth: usize, budget: &mut usize) -> Result<Message, WireError> {
    let mut reader = Reader { rest: bytes };
    let mut fields = Vec::new();
    while let Some((number, raw)) = reader.next()? {
        let value = match raw {
            Raw::Varint(v) => Value::Varint(v),
            Raw::Fixed32(v) => Value::Fixed32(v),
            Raw::Fixed64(v) => Value::Fixed64(v),
            Raw::Len(payload) => classify(payload, depth, budget)?,
        };
        fields.push(Field { number, value });
    }
    Ok(Message { fields })
}

/// A length-delimited value: empty is `""`, then a message that parses fully (within the
/// depth and field bounds), then a UTF-8 string, then bytes.
fn classify(payload: &[u8], depth: usize, budget: &mut usize) -> Result<Value, WireError> {
    if payload.is_empty() {
        return Ok(Value::Str(String::new()));
    }
    if depth < MAX_DEPTH
        && let Ok(count) = scan(payload)
        && count <= *budget
    {
        *budget -= count;
        return build(payload, depth + 1, budget).map(Value::Message);
    }
    Ok(match std::str::from_utf8(payload) {
        Ok(text) => Value::Str(text.to_owned()),
        Err(_) => Value::Bytes(payload.to_vec()),
    })
}

fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn length_delimited(out: &mut Vec<u8>, tag: u64, payload: &[u8]) -> Result<(), WireError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(WireError::TooLarge);
    }
    push_varint(out, tag | 2);
    push_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
    Ok(())
}

impl Message {
    /// Reads a payload. The top level is an error when it is not a message, longer than
    /// `MAX_PAYLOAD` or holds more than `MAX_FIELDS` fields; below it a value that breaks a
    /// bound stays a string or bytes.
    pub fn decode(bytes: &[u8]) -> Result<Message, WireError> {
        if bytes.len() > MAX_PAYLOAD {
            return Err(WireError::TooLarge);
        }
        let count = scan(bytes)?;
        let mut budget = MAX_FIELDS - count;
        build(bytes, 1, &mut budget)
    }

    /// Canonical JSON text, keys in field-number order (spec §6.2).
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    pub fn to_json(&self) -> Json {
        serde_json::from_str(&self.to_json_string()).unwrap_or(Json::Null)
    }

    fn write_json(&self, out: &mut String) {
        let mut by_number: BTreeMap<u32, Vec<&Value>> = BTreeMap::new();
        for field in &self.fields {
            by_number
                .entry(field.number)
                .or_default()
                .push(&field.value);
        }
        out.push('{');
        for (i, (number, values)) in by_number.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{number}\":");
            if let [only] = values.as_slice() {
                only.write_json(out);
                continue;
            }
            out.push('[');
            for (j, value) in values.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                value.write_json(out);
            }
            out.push(']');
        }
        out.push('}');
    }

    /// The inverse of `to_json`, within the bounds of `decode`.
    pub fn from_json(json: &Json) -> Result<Message, WireError> {
        let Json::Object(map) = json else {
            return Err(WireError::Json("a message is an object"));
        };
        let mut budget = MAX_FIELDS;
        message_from_json(map, 1, &mut budget)
    }

    /// The wire form, fields in the order they hold; at most `MAX_PAYLOAD` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        self.encode_at(1)
    }

    fn encode_at(&self, depth: usize) -> Result<Vec<u8>, WireError> {
        if depth > MAX_DEPTH {
            return Err(WireError::TooDeep);
        }
        let mut out = Vec::new();
        for field in &self.fields {
            if field.number == 0 || field.number > MAX_FIELD_NUMBER {
                return Err(WireError::BadTag);
            }
            let tag = u64::from(field.number) << 3;
            match &field.value {
                Value::Varint(v) => {
                    push_varint(&mut out, tag);
                    push_varint(&mut out, *v);
                }
                Value::Fixed32(v) => {
                    push_varint(&mut out, tag | 5);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Value::Fixed64(v) => {
                    push_varint(&mut out, tag | 1);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Value::Str(text) => length_delimited(&mut out, tag, text.as_bytes())?,
                Value::Bytes(bytes) => length_delimited(&mut out, tag, bytes)?,
                Value::Message(inner) => {
                    let bytes = inner.encode_at(depth + 1)?;
                    length_delimited(&mut out, tag, &bytes)?;
                }
            }
            if out.len() > MAX_PAYLOAD {
                return Err(WireError::TooLarge);
            }
        }
        Ok(out)
    }
}

impl Value {
    fn write_json(&self, out: &mut String) {
        match self {
            Value::Varint(v) => {
                let _ = write!(out, "{v}");
            }
            Value::Fixed32(v) => {
                let _ = write!(out, "{{\"$fixed32\":{v}}}");
            }
            Value::Fixed64(v) => {
                let _ = write!(out, "{{\"$fixed64\":{v}}}");
            }
            Value::Str(text) => out.push_str(&Json::String(text.clone()).to_string()),
            Value::Bytes(bytes) => {
                let _ = write!(out, "{{\"$bytes\":\"{}\"}}", STANDARD.encode(bytes));
            }
            Value::Message(inner) => inner.write_json(out),
        }
    }
}

/// A key of the decoded form: a field number in its canonical decimal spelling.
fn field_number(key: &str) -> Result<u32, WireError> {
    match key.parse::<u32>() {
        Ok(n) if (1..=MAX_FIELD_NUMBER).contains(&n) && n.to_string() == key => Ok(n),
        _ => Err(WireError::Json("a key is not a field number")),
    }
}

fn message_from_json(
    map: &serde_json::Map<String, Json>,
    depth: usize,
    budget: &mut usize,
) -> Result<Message, WireError> {
    if depth > MAX_DEPTH {
        return Err(WireError::TooDeep);
    }
    let mut entries = Vec::with_capacity(map.len());
    for (key, value) in map {
        entries.push((field_number(key)?, value));
    }
    entries.sort_by_key(|(number, _)| *number);
    let mut fields = Vec::new();
    for (number, json) in entries {
        let items = match json {
            Json::Array(items) if items.is_empty() => {
                return Err(WireError::Json("an empty array"));
            }
            Json::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for item in items {
            *budget = budget.checked_sub(1).ok_or(WireError::TooManyFields)?;
            fields.push(Field {
                number,
                value: value_from_json(item, depth, budget)?,
            });
        }
    }
    Ok(Message { fields })
}

fn value_from_json(json: &Json, depth: usize, budget: &mut usize) -> Result<Value, WireError> {
    match json {
        Json::Number(n) => n
            .as_u64()
            .map(Value::Varint)
            .ok_or(WireError::Json("a number is an unsigned integer")),
        Json::String(text) => Ok(Value::Str(text.clone())),
        Json::Object(map) => {
            if !map.keys().any(|k| k.starts_with('$')) {
                return message_from_json(map, depth + 1, budget).map(Value::Message);
            }
            let mut entries = map.iter();
            let (Some((key, value)), None) = (entries.next(), entries.next()) else {
                return Err(WireError::Json("a $ key stands alone"));
            };
            match (key.as_str(), value) {
                ("$fixed32", Json::Number(n)) => n
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .map(Value::Fixed32)
                    .ok_or(WireError::Json("$fixed32 is a 32-bit integer")),
                ("$fixed64", Json::Number(n)) => n
                    .as_u64()
                    .map(Value::Fixed64)
                    .ok_or(WireError::Json("$fixed64 is a 64-bit integer")),
                ("$bytes", Json::String(text)) => STANDARD
                    .decode(text)
                    .map(Value::Bytes)
                    .map_err(|_| WireError::Json("$bytes is base64")),
                _ => Err(WireError::Json("an unknown $ key")),
            }
        }
        _ => Err(WireError::Json("a value is a number, string or object")),
    }
}

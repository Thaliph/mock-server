//! gRPC redaction (spec §6.3, GRP-12 to GRP-17): metadata and decoded messages, before anything
//! is recorded, logged or sent. Pure. Every walk keeps the bounds of the wire reader (depth 32,
//! 4096 fields) on a message that may have been built by hand, and an error is returned, never
//! turned into a partial result.

use std::collections::BTreeMap;

use super::wire::{Field, MAX_DEPTH, MAX_FIELD_NUMBER, MAX_FIELDS, Message, Value, WireError};
use crate::discovery::placeholder::Placeholder;
use crate::discovery::redact::{Redactor, Rule, value_placeholder, value_rule};

/// Entropy, in bits per byte, from which a string of at least `MIN_BLOB` bytes is a secret.
const STRING_ENTROPY: f64 = 4.5;
/// Entropy, in bits per byte, from which a bytes value of at least `MIN_BLOB` bytes is a secret.
const BYTES_ENTROPY: f64 = 7.0;
const MIN_BLOB: usize = 16;

/// Which side of a call a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Request,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Number(u32),
    Name(String),
}

/// A per-field rule (`spec.discovery.grpc.redact`): a method `<package.Service>/<Method>`, a
/// direction and a field path, each segment a field number or a field name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRule {
    method: String,
    direction: Direction,
    path: Vec<Segment>,
}

impl FieldRule {
    pub fn new(method: &str, direction: Direction, path: &str) -> Result<FieldRule, &'static str> {
        match method.split_once('/') {
            Some((service, name)) if !service.is_empty() && !name.is_empty() => {}
            _ => return Err("a method is <package.Service>/<Method>"),
        }
        let mut segments = Vec::new();
        for part in path.split('.') {
            segments.push(segment(part)?);
        }
        Ok(FieldRule {
            method: method.to_string(),
            direction,
            path: segments,
        })
    }
}

fn segment(part: &str) -> Result<Segment, &'static str> {
    let mut chars = part.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => match part.parse::<u32>() {
            Ok(n) if (1..=MAX_FIELD_NUMBER).contains(&n) && n.to_string() == part => {
                Ok(Segment::Number(n))
            }
            _ => Err("a field number is 1 to 2^29-1"),
        },
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            if chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
                Ok(Segment::Name(part.to_string()))
            } else {
                Err("a field name is letters, digits and _")
            }
        }
        _ => Err("a path segment is a field number or a field name"),
    }
}

/// Field names known for a number path (the numbers from the top level down), from reflection.
pub type FieldNames = BTreeMap<Vec<u32>, String>;

#[derive(Debug, Clone)]
pub struct GrpcRedactor {
    redactor: Redactor,
    rules: Vec<FieldRule>,
}

/// What a walk carries: the rules that apply to the call, the names and the field budget.
struct Walk<'a> {
    redactor: &'a Redactor,
    rules: Vec<&'a FieldRule>,
    names: &'a FieldNames,
    budget: usize,
}

impl GrpcRedactor {
    pub fn new(redactor: Redactor, rules: Vec<FieldRule>) -> Self {
        GrpcRedactor { redactor, rules }
    }

    /// A metadata value: the header rules, and every `-bin` value whatever its name.
    pub fn metadata(&self, name: &str, value: &str) -> String {
        let lower = name.to_ascii_lowercase();
        match lower.strip_suffix("-bin") {
            Some(base) => self
                .redactor
                .name_placeholder(base)
                .unwrap_or(Placeholder::Secret)
                .to_string(),
            None => self.redactor.header(name, value),
        }
    }

    /// A message of `method` in `direction`; `names` are the field names when known. The
    /// result replaces every sensitive value by the string of its placeholder.
    pub fn message(
        &self,
        method: &str,
        direction: Direction,
        message: &Message,
        names: &FieldNames,
    ) -> Result<Message, WireError> {
        let mut walk = Walk {
            redactor: &self.redactor,
            rules: self
                .rules
                .iter()
                .filter(|r| r.method == method && r.direction == direction)
                .collect(),
            names,
            budget: MAX_FIELDS,
        };
        walk.message(message, &mut Vec::new(), 1)
    }
}

impl Walk<'_> {
    fn message(
        &mut self,
        message: &Message,
        path: &mut Vec<u32>,
        depth: usize,
    ) -> Result<Message, WireError> {
        if depth > MAX_DEPTH {
            return Err(WireError::TooDeep);
        }
        let mut fields = Vec::with_capacity(message.fields.len());
        for field in &message.fields {
            self.budget = self.budget.checked_sub(1).ok_or(WireError::TooManyFields)?;
            path.push(field.number);
            // A nested message is walked even when it is replaced, so that a bound it breaks
            // is an error whatever the rules say.
            let value = match &field.value {
                Value::Message(inner) => Value::Message(self.message(inner, path, depth + 1)?),
                other => self.leaf(other),
            };
            let value = match self.field_placeholder(path) {
                Some(placeholder) => Value::Str(placeholder.to_string()),
                None => value,
            };
            path.pop();
            fields.push(Field {
                number: field.number,
                value,
            });
        }
        Ok(Message { fields })
    }

    /// The placeholder of the name rule or per-field rule that fires on the field at `path`.
    fn field_placeholder(&self, path: &[u32]) -> Option<Placeholder> {
        if let Some(name) = self.names.get(path)
            && let Some(placeholder) = self.redactor.name_placeholder(name)
        {
            return Some(placeholder);
        }
        self.rules
            .iter()
            .any(|rule| self.matches(rule, path))
            .then_some(Placeholder::Secret)
    }

    fn matches(&self, rule: &FieldRule, path: &[u32]) -> bool {
        rule.path.len() == path.len()
            && rule.path.iter().enumerate().all(|(i, seg)| match seg {
                Segment::Number(n) => *n == path[i],
                Segment::Name(name) => self.names.get(&path[..=i]) == Some(name),
            })
    }

    /// A value that is not a message: the value detectors, then the entropy rule.
    fn leaf(&self, value: &Value) -> Value {
        match value {
            Value::Str(text) => match value_placeholder(text) {
                Some(placeholder) => Value::Str(placeholder.to_string()),
                None if text.len() >= MIN_BLOB && entropy(text.as_bytes()) >= STRING_ENTROPY => {
                    Value::Str(Placeholder::Secret.to_string())
                }
                None => value.clone(),
            },
            Value::Bytes(bytes) if bytes.len() >= MIN_BLOB && entropy(bytes) >= BYTES_ENTROPY => {
                Value::Str(Placeholder::Secret.to_string())
            }
            Value::Varint(v) if value_rule(&v.to_string()) == Some(Rule::Card) => {
                Value::Str(Placeholder::Card.to_string())
            }
            other => other.clone(),
        }
    }
}

/// Shannon entropy of `bytes`, in bits per byte.
fn entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0usize; 256];
    for b in bytes {
        counts[usize::from(*b)] += 1;
    }
    let total = bytes.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = *c as f64 / total;
            -p * p.log2()
        })
        .sum()
}

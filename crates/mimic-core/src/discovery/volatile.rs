//! Volatile field detection (spec §6): per field, the number of observations `n` and a
//! bottom-32 sketch of the XXH3 hashes of the (redacted) values. Integer-only, so merging is
//! exact and order-independent.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use super::redact::MARKER;

/// Hashes kept per field.
const SKETCH: usize = 32;
/// Fields tracked per endpoint.
pub const MAX_FIELDS: usize = 128;

/// The 32 smallest distinct hashes seen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sketch {
    hashes: BTreeSet<u64>,
}

impl Sketch {
    pub fn insert(&mut self, value: &str) {
        self.hashes.insert(xxh3_64(value.as_bytes()));
        self.truncate();
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.hashes.extend(other.hashes);
        self.truncate();
        self
    }

    fn truncate(&mut self) {
        while self.hashes.len() > SKETCH {
            self.hashes.pop_last();
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Distinct values: exact below 32 hashes, else `31 / max normalised hash`.
    pub fn distinct(&self) -> u64 {
        if self.hashes.len() < SKETCH {
            return self.hashes.len() as u64;
        }
        let max = self.hashes.last().copied().unwrap_or(u64::MAX).max(1);
        let estimate = (31u128 << 64) / u128::from(max);
        u64::try_from(estimate).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Field {
    n: u64,
    redacted: bool,
    sketch: Sketch,
}

impl Field {
    fn volatile(&self) -> bool {
        self.n >= 3 && self.sketch.distinct().saturating_mul(2) >= self.n
    }
}

/// The tracked fields of one endpoint, at most 128 (the first in byte order).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volatility {
    fields: BTreeMap<String, Field>,
}

impl Volatility {
    /// One observation. A redacted field hashes the constant marker, never the value.
    pub fn observe(&mut self, field: &str, value: &str, redacted: bool) {
        if !self.fields.contains_key(field) {
            if self.fields.len() >= MAX_FIELDS {
                match self.fields.last_key_value() {
                    Some((last, _)) if field < last.as_str() => {
                        self.fields.pop_last();
                    }
                    _ => return,
                }
            }
            self.fields.insert(field.to_string(), Field::default());
        }
        if let Some(f) = self.fields.get_mut(field) {
            f.n += 1;
            f.redacted |= redacted;
            f.sketch.insert(if redacted { MARKER } else { value });
        }
    }

    pub fn merge(mut self, other: Self) -> Self {
        for (name, f) in other.fields {
            match self.fields.remove(&name) {
                Some(mine) => {
                    self.fields.insert(
                        name,
                        Field {
                            n: mine.n + f.n,
                            redacted: mine.redacted || f.redacted,
                            sketch: mine.sketch.merge(f.sketch),
                        },
                    );
                }
                None => {
                    self.fields.insert(name, f);
                }
            }
        }
        while self.fields.len() > MAX_FIELDS {
            self.fields.pop_last();
        }
        self
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Tracked field names, in byte order.
    pub fn fields(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }

    /// `n ≥ 3` and distinct values ≥ `n / 2`.
    pub fn is_volatile(&self, field: &str) -> bool {
        self.fields.get(field).is_some_and(Field::volatile)
    }

    pub fn is_redacted(&self, field: &str) -> bool {
        self.fields.get(field).is_some_and(|f| f.redacted)
    }

    pub(super) fn to_json(&self) -> serde_json::Value {
        self.fields
            .iter()
            .map(|(name, f)| {
                (
                    name.clone(),
                    serde_json::json!({"n": f.n, "redacted": f.redacted, "volatile": f.volatile()}),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

//! JSON Schema inference (spec §6): a mergeable description of JSON values, query strings
//! and headers. Stub for the red commit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::redact::Redactor;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {}

impl Schema {
    /// Schema of one JSON value, already redacted. The root is level 1.
    pub fn infer(_value: &Value) -> Self {
        todo!()
    }

    pub fn merge(self, _other: Self) -> Self {
        todo!()
    }

    pub fn to_json(&self) -> Value {
        todo!()
    }
}

/// Keyed values of one kind (query keys, form fields or headers) over many exchanges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {}

impl Params {
    /// A query string or an `application/x-www-form-urlencoded` body.
    pub fn from_query(_redactor: &Redactor, _query: &str) -> Self {
        todo!()
    }

    pub fn from_headers(_redactor: &Redactor, _headers: &[(&str, &str)]) -> Self {
        todo!()
    }

    pub fn merge(self, _other: Self) -> Self {
        todo!()
    }

    pub fn to_json(&self) -> Value {
        todo!()
    }
}

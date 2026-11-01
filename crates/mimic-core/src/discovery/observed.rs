//! What one endpoint has shown of its query, headers and bodies. Stub for the red commit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::redact::Redactor;

/// One exchange as the schemas see it. Bodies are already content-decoded.
#[derive(Debug, Clone, Default)]
pub struct Observation<'a> {
    pub query: &'a str,
    pub req_headers: &'a [(&'a str, &'a str)],
    pub req_body: Option<&'a [u8]>,
    pub resp_headers: &'a [(&'a str, &'a str)],
    pub resp_body: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {}

impl Observed {
    pub fn record(&mut self, _redactor: &Redactor, _o: &Observation<'_>) {
        todo!()
    }

    pub fn merge(self, _other: Self) -> Self {
        todo!()
    }

    pub fn to_json(&self) -> Value {
        todo!()
    }
}

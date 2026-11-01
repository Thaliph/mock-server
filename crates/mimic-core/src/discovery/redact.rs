//! Redaction (spec §6): by name and by value. Stub.

use std::collections::BTreeSet;

pub const MARKER: &str = "<redacted>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Name,
    Jwt,
    Pem,
    Credential,
    Card,
    Aws,
    TokenPrefix,
    Userinfo,
}

pub fn value_rule(_s: &str) -> Option<Rule> {
    None
}

pub fn is_jwt(_s: &str) -> bool {
    false
}

pub fn path(p: &str) -> String {
    p.to_string()
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    names: BTreeSet<String>,
}

impl Redactor {
    pub fn new<'a>(_extra: impl IntoIterator<Item = &'a str>) -> Self {
        Self::default()
    }

    pub fn name_matches(&self, _name: &str) -> bool {
        false
    }

    pub fn header(&self, _name: &str, value: &str) -> String {
        value.to_string()
    }

    pub fn field(&self, _name: &str, value: &str) -> String {
        value.to_string()
    }

    pub fn json(&self, _value: &mut serde_json::Value) {}
}

//! The request signature (spec §8).

use std::collections::BTreeMap;

use crate::discovery::redact::Redactor;

use super::Request;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    authority: String,
    path: String,
    fields: BTreeMap<String, Vec<String>>,
}

impl Signature {
    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn fields(&self) -> &BTreeMap<String, Vec<String>> {
        &self.fields
    }
}

pub fn signature(redactor: &Redactor, req: &Request<'_>) -> Signature {
    let _ = (redactor, req);
    Signature {
        authority: String::new(),
        path: String::new(),
        fields: BTreeMap::new(),
    }
}

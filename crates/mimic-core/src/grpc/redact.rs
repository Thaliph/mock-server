//! gRPC redaction (spec §6.3, GRP-12 to GRP-17). Skeleton: nothing is redacted yet.

use std::collections::BTreeMap;

use super::wire::{Message, WireError};
use crate::discovery::redact::Redactor;

/// Which side of a call a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Request,
    Response,
}

/// A per-field rule (`spec.discovery.grpc.redact`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRule;

impl FieldRule {
    pub fn new(
        _method: &str,
        _direction: Direction,
        _path: &str,
    ) -> Result<FieldRule, &'static str> {
        Ok(FieldRule)
    }
}

/// Field names known for a number path, from reflection.
pub type FieldNames = BTreeMap<Vec<u32>, String>;

#[derive(Debug, Clone)]
pub struct GrpcRedactor;

impl GrpcRedactor {
    pub fn new(_redactor: Redactor, _rules: Vec<FieldRule>) -> Self {
        GrpcRedactor
    }

    pub fn metadata(&self, _name: &str, value: &str) -> String {
        value.to_string()
    }

    pub fn message(
        &self,
        _method: &str,
        _direction: Direction,
        message: &Message,
        _names: &FieldNames,
    ) -> Result<Message, WireError> {
        Ok(message.clone())
    }
}

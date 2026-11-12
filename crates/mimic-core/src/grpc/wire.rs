//! Schema-less protobuf wire-format reader and its inverse (spec §6.2, GRP-04 to GRP-09).

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

impl Message {
    pub fn decode(_bytes: &[u8]) -> Result<Message, WireError> {
        todo!()
    }

    /// Canonical JSON text, keys in field-number order.
    pub fn to_json_string(&self) -> String {
        todo!()
    }

    pub fn to_json(&self) -> Json {
        todo!()
    }

    pub fn from_json(_json: &Json) -> Result<Message, WireError> {
        todo!()
    }

    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        todo!()
    }
}

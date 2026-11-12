//! gRPC messages (spec §6.2): length-prefixed framing and a schema-less protobuf wire-format
//! reader. Pure: no I/O, no clock. Every bound of the spec is enforced here, on untrusted
//! input.

pub mod frame;
pub mod redact;
pub mod wire;

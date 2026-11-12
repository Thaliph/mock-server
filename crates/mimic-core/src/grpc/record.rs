//! The first message of a captured gRPC body (spec §6.4, GRP-19). Pure.

use super::frame::{Encoding, FrameError, Parser, Payload};
use super::wire::{Message, WireError};

/// Why no message was read: the `result` of `mimic_discovery_bodies_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A bad flag, a first frame cut short, a payload that is not a message.
    Opaque,
    /// A frame or an inflated payload over 256 KiB, or a payload over the wire bounds.
    TooLarge,
    /// A compressed frame under an encoding that is not decoded.
    Encoding,
}

impl Failure {
    pub fn result(self) -> &'static str {
        match self {
            Failure::Opaque => "opaque",
            Failure::TooLarge => "too_large",
            Failure::Encoding => "encoding",
        }
    }
}

/// True for the media types whose body is a length-prefixed protobuf stream: `application/grpc`
/// and `application/grpc+proto` (`grpc-web` and `+json` stay opaque).
pub fn is_protobuf_media(media: &str) -> bool {
    media == "application/grpc" || media == "application/grpc+proto"
}

/// The first message of `body`, a whole captured body, under the `grpc-encoding` header value.
pub fn first_message(encoding: Option<&str>, body: &[u8]) -> Result<Message, Failure> {
    let mut parser = Parser::new();
    let pushed = parser.push(body);
    let Some(frame) = pushed.frames.into_iter().next() else {
        return Err(match pushed.error {
            Some(FrameError::Oversize(_)) => Failure::TooLarge,
            _ => Failure::Opaque,
        });
    };
    let payload = frame
        .decode(Encoding::from_header(encoding))
        .map_err(|e| match e {
            FrameError::Inflated => Failure::TooLarge,
            _ => Failure::Opaque,
        })?;
    let Payload::Bytes(bytes) = payload else {
        return Err(Failure::Encoding);
    };
    Message::decode(&bytes).map_err(|e| match e {
        WireError::TooLarge | WireError::TooManyFields => Failure::TooLarge,
        _ => Failure::Opaque,
    })
}

/// Most messages kept of a stream: the first (the sample's body) and 63 steps (spec §6.4).
pub const MAX_MESSAGES: usize = 64;

/// `end` bytes of a response body had arrived `ms` milliseconds after the response head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub end: usize,
    pub ms: u64,
}

/// A response message after the first, and the bucketed time since the one before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub delay_ms: u64,
    pub message: Message,
}

/// A delay rounded up to the upper bound of its latency bucket (spec §6.4).
pub fn bucketed(ms: u64) -> u64 {
    ms
}

/// Whether `body` is exactly one complete frame.
pub fn single_message(_body: &[u8]) -> bool {
    false
}

/// The messages after the first of a response body, at most 63, each with its bucketed delay.
pub fn steps(_encoding: Option<&str>, _body: &[u8], _marks: &[Mark]) -> Vec<Step> {
    Vec::new()
}

//! The first message of a captured gRPC body (spec §6.4, GRP-19). Pure.

use super::frame::{Encoding, Frame, FrameError, HEADER, Parser, Payload};
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
    message_of(&frame, Encoding::from_header(encoding))
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
pub use crate::discovery::histogram::bucketed;

/// Whether `body` is exactly one complete frame: a unary call or a server-streaming one, not
/// a client or bidirectional stream (spec §6.4).
pub fn single_message(body: &[u8]) -> bool {
    let mut parser = Parser::new();
    let pushed = parser.push(body);
    pushed.error.is_none() && pushed.frames.len() == 1 && parser.buffered() == 0
}

/// The message of a frame.
fn message_of(frame: &Frame, encoding: Encoding) -> Result<Message, Failure> {
    let payload = frame.decode(encoding).map_err(|e| match e {
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

/// The messages after the first of a response `body`, at most 63, each with the bucketed time
/// since the one before it (spec §6.4). A message arrives with the chunk that completes its
/// frame: the first mark whose offset reaches the end of the frame. The list ends at the first
/// frame that is not a message; none if the first is not.
pub fn steps(encoding: Option<&str>, body: &[u8], marks: &[Mark]) -> Vec<Step> {
    let encoding = Encoding::from_header(encoding);
    let pushed = Parser::new().push(body);
    let mut out = Vec::new();
    let (mut end, mut previous) = (0usize, 0u64);
    for (i, frame) in pushed.frames.iter().take(MAX_MESSAGES).enumerate() {
        end = end.saturating_add(HEADER + frame.payload.len());
        let Ok(message) = message_of(frame, encoding) else {
            break;
        };
        // Never before the message before it, whatever the order of the marks.
        let arrived = marks
            .iter()
            .filter(|m| m.end >= end)
            .map(|m| m.ms)
            .min()
            .map_or(previous, |ms| ms.max(previous));
        if i > 0 {
            out.push(Step {
                delay_ms: bucketed(arrived - previous),
                message,
            });
        }
        previous = arrived;
    }
    out
}

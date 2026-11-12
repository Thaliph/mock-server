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

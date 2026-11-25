//! Raw HTTP/2 frames (RFC 9113 §4, §6), for the tests that must send what the `h2` client crate
//! will not: more streams than the server allows, or a flood of resets.

use std::io::{self, Read};
use std::net::TcpStream;

pub use mimic_core::proxy::H2_PREFACE;

pub const DATA: u8 = 0x0;
pub const HEADERS: u8 = 0x1;
pub const RST_STREAM: u8 = 0x3;
pub const SETTINGS: u8 = 0x4;
pub const GOAWAY: u8 = 0x7;

pub const END_STREAM: u8 = 0x1;
pub const END_HEADERS: u8 = 0x4;
pub const ACK: u8 = 0x1;

pub const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 0x6;

pub const REFUSED_STREAM: u32 = 0x7;
pub const CANCEL: u32 = 0x8;
pub const ENHANCE_YOUR_CALM: u32 = 0xb;

/// HPACK block of `GET /` on `a`, every field from the static table (RFC 7541 appendix A).
pub const GET_ROOT: &[u8] = &[0x82, 0x86, 0x84, 0x41, 0x01, b'a'];

/// One frame as read from the server.
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: u8,
    pub flags: u8,
    pub stream: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    /// The `(identifier, value)` pairs of a SETTINGS frame.
    pub fn settings(&self) -> Vec<(u16, u32)> {
        self.payload
            .as_chunks::<6>()
            .0
            .iter()
            .map(|c| {
                (
                    u16::from_be_bytes([c[0], c[1]]),
                    u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
                )
            })
            .collect()
    }

    /// The error code of a RST_STREAM frame.
    pub fn rst_code(&self) -> u32 {
        u32::from_be_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ])
    }

    /// The error code of a GOAWAY frame.
    pub fn goaway_code(&self) -> u32 {
        u32::from_be_bytes([
            self.payload[4],
            self.payload[5],
            self.payload[6],
            self.payload[7],
        ])
    }
}

/// The bytes of one frame.
pub fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("small frame");
    let mut out = Vec::with_capacity(9 + payload.len());
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The client preface and an empty SETTINGS frame.
pub fn client_start() -> Vec<u8> {
    let mut out = H2_PREFACE.to_vec();
    out.extend(frame(SETTINGS, 0, 0, &[]));
    out
}

/// HEADERS of `GET /` on `stream`, without END_STREAM: the stream stays open.
pub fn open_stream(stream: u32) -> Vec<u8> {
    frame(HEADERS, END_HEADERS, stream, GET_ROOT)
}

pub fn rst_stream(stream: u32, code: u32) -> Vec<u8> {
    frame(RST_STREAM, 0, stream, &code.to_be_bytes())
}

/// Reads the next frame; an error at EOF, a reset or the read timeout of `stream`.
pub fn read_frame(stream: &mut TcpStream) -> io::Result<Frame> {
    let mut head = [0u8; 9];
    stream.read_exact(&mut head)?;
    let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Frame {
        kind: head[3],
        flags: head[4],
        stream: u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff,
        payload,
    })
}

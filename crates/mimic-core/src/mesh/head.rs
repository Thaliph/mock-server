//! The head reader of a mesh-routed HTTP/1 connection (spec §4.1 "Mesh-routed HTTP", limits 1
//! and 8): pure, no I/O. It sits between the app's socket and `hyper`: bytes go in, and the
//! bytes `hyper` may take come out, one whole request at a time. A request head it refuses, or
//! one that leaves `hyper` (`Upgrade`, `CONNECT`), is held back with everything after it.

use crate::proxy::http1::{Reason, RequestHead};

/// Why the reader stopped passing bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The head is refused: the sidecar answers `400` `invalid-request`.
    Refused {
        reason: Reason,
        /// The request line, when it parsed.
        request: Option<RequestHead>,
    },
    /// The head carries `Upgrade` or is a `CONNECT`: the connection leaves `hyper`.
    Upgrade,
}

/// The reader of one connection.
#[derive(Debug, Default)]
pub struct Reader {
    held: Vec<u8>,
}

impl Reader {
    pub fn new() -> Reader {
        Reader::default()
    }

    /// Reads `data` from the client. The bytes `hyper` may take are appended to `pass`; once a
    /// head is refused or leaves, the result is the same for every later call, and `pass` gets
    /// nothing more. With `upgrades` false an `Upgrade` or `CONNECT` head is passed on like any
    /// other and the reader stops judging (the caller answers it itself).
    pub fn feed(&mut self, data: &[u8], upgrades: bool, pass: &mut Vec<u8>) -> Option<Stop> {
        let _ = (data, upgrades, pass);
        None
    }

    /// Bytes read and neither passed nor held for a stop: the incomplete head being read.
    pub fn pending(&self) -> usize {
        0
    }

    /// After a stop: the bytes from the start of the head that stopped the reader to the end of
    /// what was fed.
    pub fn held(&self) -> &[u8] {
        &self.held
    }
}

/// Whether a request head carries an `Upgrade` field or is a `CONNECT`.
pub fn leaves(head: &[u8]) -> bool {
    let _ = head;
    false
}

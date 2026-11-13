//! The head reader of a mesh-routed HTTP/1 connection (spec §4.1 "Mesh-routed HTTP", limits 1
//! and 8): pure, no I/O. It sits between the app's socket and `hyper`: bytes go in, and the
//! bytes `hyper` may take come out, one whole request at a time. A request head it refuses, or
//! one that leaves `hyper` (`Upgrade`, `CONNECT`), is held back with everything after it.

use crate::proxy::http1::{Codec, Event, HEAD_LIMIT, Reason, RequestHead};

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

/// Where the reader is in the connection.
#[derive(Debug, Default)]
enum State {
    /// Reading a request head: its bytes are held until it is whole.
    #[default]
    Head,
    /// Passing the body of a request the codec follows.
    Body(Box<Codec>),
    /// The framing cannot be followed (or an upgrade is left to the caller): everything passes.
    Raw,
    /// Stopped.
    Stopped(Stop),
}

/// The reader of one connection.
#[derive(Debug, Default)]
pub struct Reader {
    state: State,
    /// Heads passed on so far.
    heads: usize,
    /// The bytes of the head being read, or after a stop everything from its start on.
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
        let mut input = data;
        // What follows a head, read again as the start of its body or of the next head.
        let mut tail: Vec<u8>;
        loop {
            match &mut self.state {
                State::Stopped(stop) => {
                    let stop = stop.clone();
                    self.held.extend_from_slice(input);
                    return Some(stop);
                }
                State::Raw => {
                    pass.extend_from_slice(input);
                    return None;
                }
                State::Body(codec) => {
                    let (used, events) = codec.client_bytes_limited(input);
                    pass.extend_from_slice(&input[..used]);
                    input = &input[used..];
                    let ended = events.iter().any(|e| matches!(e, Event::RequestEnd { .. }));
                    let unframed = events.iter().any(|e| matches!(e, Event::Unframed(_)));
                    if unframed {
                        // A body `hyper` judges itself (a bad chunk size): the head was good.
                        self.state = State::Raw;
                    } else if ended {
                        self.state = State::Head;
                    } else {
                        return None;
                    }
                }
                State::Head => {
                    let scanned_from = self.held.len().saturating_sub(3);
                    self.held.extend_from_slice(input);
                    let Some(end) = head_end(&self.held, scanned_from) else {
                        if self.held.len() > HEAD_LIMIT {
                            return Some(self.stop(Stop::Refused {
                                reason: Reason::HeadTooLarge,
                                request: None,
                            }));
                        }
                        return None;
                    };
                    tail = self.held.split_off(end);
                    let head = std::mem::take(&mut self.held);
                    if let Some(stop) = self.judge(&head, upgrades, pass) {
                        self.held = head;
                        self.held.extend_from_slice(&tail);
                        return Some(self.stop(stop));
                    }
                    input = &tail;
                }
            }
        }
    }

    fn stop(&mut self, stop: Stop) -> Stop {
        self.state = State::Stopped(stop.clone());
        stop
    }

    /// Judges a whole head: `Some` to stop, `None` after passing it (the state follows its
    /// body).
    fn judge(&mut self, head: &[u8], upgrades: bool, pass: &mut Vec<u8>) -> Option<Stop> {
        if let Some(reason) = malformed(head) {
            return Some(Stop::Refused {
                reason,
                request: None,
            });
        }
        let mut codec = Codec::new().one_request_at_a_time();
        let (used, events) = codec.client_bytes_limited(head);
        let mut request: Option<RequestHead> = None;
        let mut ended = false;
        for event in events {
            match event {
                Event::Request(h) => request = Some(h),
                Event::RequestEnd { .. } => ended = true,
                Event::Unframed(u) if u.reason == Reason::ConflictingLength => {
                    // Forwarded `chunked` by `hyper`: the framing cannot be followed here.
                    pass.extend_from_slice(head);
                    self.heads += 1;
                    self.state = State::Raw;
                    return None;
                }
                Event::Unframed(u) => {
                    return Some(Stop::Refused {
                        reason: u.reason,
                        request: u.request,
                    });
                }
                _ => {}
            }
        }
        if request.is_none() || used != head.len() {
            return Some(Stop::Refused {
                reason: Reason::BadStartLine,
                request: None,
            });
        }
        if leaves(head) {
            if upgrades {
                return Some(Stop::Upgrade);
            }
            pass.extend_from_slice(head);
            self.heads += 1;
            self.state = State::Raw;
            return None;
        }
        pass.extend_from_slice(head);
        self.heads += 1;
        self.state = if ended {
            State::Head
        } else {
            State::Body(Box::new(codec))
        };
        None
    }

    /// The request heads passed on so far: the requests `hyper` was given.
    pub fn heads(&self) -> usize {
        self.heads
    }

    /// Bytes read and neither passed nor held for a stop: the incomplete head being read.
    pub fn pending(&self) -> usize {
        match self.state {
            State::Head => self.held.len(),
            _ => 0,
        }
    }

    /// After a stop: the bytes from the start of the head that stopped the reader to the end of
    /// what was fed.
    pub fn held(&self) -> &[u8] {
        &self.held
    }
}

/// The end of the head in `buf`: after the empty line (CRLF or bare LF), looking from the line
/// that holds `from`.
fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut start = buf[..from.min(buf.len())]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    while let Some(lf) = buf[start..].iter().position(|&b| b == b'\n') {
        let at = start + lf;
        let next = &buf[at + 1..];
        if next.starts_with(b"\n") {
            return Some(at + 2);
        }
        if next.starts_with(b"\r\n") {
            return Some(at + 3);
        }
        start = at + 1;
    }
    None
}

/// What `httparse` (and so `hyper`) refuses and the codec lets by: a folded line, a control
/// character other than a tab in a header line.
fn malformed(head: &[u8]) -> Option<Reason> {
    for (n, line) in head.split(|&b| b == b'\n').enumerate().skip(1) {
        if matches!(line.first(), Some(b' ' | b'\t')) {
            return Some(Reason::ObsFold);
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if n > 0 && line.iter().any(|&b| b != b'\t' && (b < 0x20 || b == 0x7f)) {
            return Some(Reason::BadHeaderLine);
        }
    }
    None
}

/// Whether a request head carries an `Upgrade` field or is a `CONNECT`.
pub fn leaves(head: &[u8]) -> bool {
    let mut lines = head.split(|&b| b == b'\n');
    let request_line = lines.next().unwrap_or_default();
    if request_line
        .get(..8)
        .is_some_and(|m| m.eq_ignore_ascii_case(b"CONNECT "))
    {
        return true;
    }
    lines.any(|line| {
        line.iter()
            .position(|&b| b == b':')
            .is_some_and(|colon| line[..colon].trim_ascii().eq_ignore_ascii_case(b"upgrade"))
    })
}

//! Byte-exact HTTP/1 framer (spec §4.1, "HTTP/1.x" and "Unframed HTTP/1").
//!
//! The relay forwards every byte unchanged; the [`Codec`] only watches the two directions to
//! find message boundaries and to report what the access log and the metrics need. It never
//! answers and never rejects: input it cannot frame unambiguously yields
//! [`Event::Unframed`] and the codec goes silent (the rest of the connection is a tunnel).
//! State is bounded: a head or trailer section is buffered up to 64 KiB, a chunk-size line is
//! never buffered, at most 64 KiB of client bytes wait behind an upgrade request, at most
//! [`READ_AHEAD`] requests wait for their response. Bytes the codec holds or leaves unused
//! are never dropped by the relay (see [`Codec::client_bytes`], [`Codec::client_bytes_limited`]).

mod body;
#[cfg(test)]
mod proptests;
mod scan;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::mem;

use body::{Body, BodyScan, Cap};
use scan::{HeadScanner, Kind, Scan};

/// Largest head or trailer section, in bytes (the blank line included).
pub const HEAD_LIMIT: usize = 64 * 1024;
/// Most lines in a head (start line included) or in a trailer section.
pub const LINE_LIMIT: usize = 256;
/// Longest chunk-size line, line ending excluded.
pub const CHUNK_LINE_LIMIT: usize = 4 * 1024;
/// Longest `path` reported, in bytes.
pub const PATH_LIMIT: usize = 1024;
/// Longest `host` or `uri_authority` reported, in bytes (longer values are cut).
pub const AUTHORITY_LIMIT: usize = 1024;
/// Longest `method` reported, in bytes (longer values are cut).
pub const METHOD_LIMIT: usize = 64;
/// Payload bytes of a message body handed to the discovery tap (spec §6): the first
/// `CAPTURE_BODY_LIMIT` bytes; the `body_bytes` of the end event tells whether there were more.
pub const CAPTURE_BODY_LIMIT: usize = 256 * 1024;
/// Requests the relay reads ahead of their responses.
pub const READ_AHEAD: usize = 32;
/// Fixed part of the retained state of one direction, in bytes.
pub const FIXED_PER_DIRECTION: usize = 128 * 1024;
/// Bound of [`Codec::retained_bytes`]: 64 KiB of head or trailer section, plus a fixed size
/// per direction (the requests waiting for a response on the client side, one request head
/// on the server side).
pub const RETAINED_LIMIT: usize = HEAD_LIMIT + 2 * FIXED_PER_DIRECTION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// Path without query, at most [`PATH_LIMIT`] bytes; empty for `CONNECT`.
    pub path: String,
    /// Authority of an absolute-form target, or the target of a `CONNECT` (no userinfo).
    pub uri_authority: Option<String>,
    /// First `Host` header value.
    pub host: Option<String>,
    pub version: Version,
    /// `Connection: upgrade` with an `Upgrade` header.
    pub upgrade: bool,
    pub connect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: Version,
    pub status: u16,
    /// The request this response answers; `None` when no request was pending.
    pub request: Option<RequestHead>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

/// Why a message could not be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    HeadTooLarge,
    TooManyHeaderLines,
    TrailersTooLarge,
    TooManyTrailerLines,
    BareCr,
    BadStartLine,
    BadHeaderLine,
    ObsFold,
    /// Both `content-length` and `transfer-encoding`.
    ConflictingLength,
    BadContentLength,
    BadTransferEncoding,
    BadChunkSize,
    ChunkLineTooLong,
    BadChunkTerminator,
    /// Client bytes held back behind an upgrade request outgrew the head limit.
    HeldOverflow,
    /// A request head after [`READ_AHEAD`] unanswered ones, offered to [`Codec::client_bytes`]
    /// (the relay uses [`Codec::client_bytes_limited`], which never reports this).
    ReadAheadOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unframed {
    pub side: Side,
    pub reason: Reason,
    /// The request known so far (for a response: the pending one).
    pub request: Option<RequestHead>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Request(RequestHead),
    /// The request, body included, is complete. `body_bytes` counts payload bytes (chunk
    /// data for `chunked`).
    RequestEnd {
        body_bytes: u64,
    },
    /// A `1xx` other than `101`: not the final response, the request stays pending.
    Interim(ResponseHead),
    /// The final response head (also `101`).
    Response(ResponseHead),
    ResponseEnd {
        body_bytes: u64,
    },
    /// The HTTP part is over (`101`, or 2xx to `CONNECT`): the rest is opaque.
    Tunnel,
    /// Framing gave up: the rest is opaque, no further event.
    Unframed(Unframed),
    /// Bytes for the discovery tap, only from a [`Codec::with_capture`] codec.
    Captured(Captured),
}

/// What a capturing codec hands over: the raw head right after its `Request` or `Response`
/// event (a `1xx` other than `101` is not captured), then the body payload in order, without
/// chunk framing, at most [`CAPTURE_BODY_LIMIT`] bytes per message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    RequestHead(Vec<u8>),
    RequestBody(Vec<u8>),
    ResponseHead(Vec<u8>),
    ResponseBody(Vec<u8>),
}

/// What a head says about its body and its upgrade, before the direction-specific rules.
struct Fields {
    content_lengths: Vec<u64>,
    bad_content_length: bool,
    /// Transfer codings of every `transfer-encoding` header, lowercase.
    codings: Option<Vec<String>>,
    host: Option<String>,
    connection_upgrade: bool,
    has_upgrade: bool,
}

enum Framing {
    None,
    Length(u64),
    Chunked,
    UntilClose,
}

fn trim_ows(mut v: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = v {
        v = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = v {
        v = rest;
    }
    v
}

fn collect_fields<'a>(lines: impl Iterator<Item = &'a [u8]>) -> Fields {
    let mut f = Fields {
        content_lengths: Vec::new(),
        bad_content_length: false,
        codings: None,
        host: None,
        connection_upgrade: false,
        has_upgrade: false,
    };
    for line in lines {
        // A folded continuation belongs to a header that is not a framing header (the
        // scanner rejects the others); it carries nothing the codec reads.
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], trim_ows(&line[colon + 1..]));
        if name.eq_ignore_ascii_case(b"content-length") {
            for item in value.split(|&b| b == b',') {
                let item = trim_ows(item);
                let parsed = if item.is_empty() || !item.iter().all(u8::is_ascii_digit) {
                    None
                } else {
                    std::str::from_utf8(item)
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                };
                match parsed {
                    Some(n) => f.content_lengths.push(n),
                    None => f.bad_content_length = true,
                }
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            let codings = f.codings.get_or_insert_with(Vec::new);
            for item in value.split(|&b| b == b',') {
                let item = item.split(|&b| b == b';').next().unwrap_or_default();
                let item = trim_ows(item);
                if !item.is_empty() {
                    codings.push(String::from_utf8_lossy(item).to_ascii_lowercase());
                }
            }
        } else if name.eq_ignore_ascii_case(b"host") {
            if f.host.is_none() {
                f.host = Some(String::from_utf8_lossy(value).into_owned());
            }
        } else if name.eq_ignore_ascii_case(b"connection") {
            if value
                .split(|&b| b == b',')
                .any(|t| trim_ows(t).eq_ignore_ascii_case(b"upgrade"))
            {
                f.connection_upgrade = true;
            }
        } else if name.eq_ignore_ascii_case(b"upgrade") {
            f.has_upgrade = true;
        }
    }
    f
}

impl Fields {
    fn framing(&self, request: bool, version: Version) -> Result<Framing, Reason> {
        if self.bad_content_length || self.content_lengths.windows(2).any(|w| w[0] != w[1]) {
            return Err(Reason::BadContentLength);
        }
        match (&self.codings, self.content_lengths.first()) {
            (Some(_), Some(_)) => Err(Reason::ConflictingLength),
            (Some(codings), None) => {
                let chunked = codings.last().is_some_and(|c| c == "chunked");
                if request && (!chunked || version == Version::Http10) {
                    Err(Reason::BadTransferEncoding)
                } else if chunked {
                    Ok(Framing::Chunked)
                } else {
                    Ok(Framing::UntilClose)
                }
            }
            (None, Some(&n)) => Ok(Framing::Length(n)),
            (None, None) if request => Ok(Framing::None),
            (None, None) => Ok(Framing::UntilClose),
        }
    }
}

fn truncate(mut s: String, limit: usize) -> String {
    if s.len() > limit {
        let mut end = limit;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Bytes a request head keeps in memory.
fn head_size(head: &RequestHead) -> usize {
    mem::size_of::<RequestHead>()
        + head.method.len()
        + head.path.len()
        + head.uri_authority.as_ref().map_or(0, String::len)
        + head.host.as_ref().map_or(0, String::len)
}

/// The part of an authority after any userinfo.
fn host_port(authority: &str) -> String {
    let host = authority.rsplit('@').next().unwrap_or_default();
    truncate(host.to_string(), AUTHORITY_LIMIT)
}

/// What follows `<scheme>://` when `target` is in absolute form (RFC 9112 §3.2.2): the target
/// starts with a scheme (RFC 3986 §3.1). A `://` later in an origin-form target (in its path or
/// query) is not one, or a client could spoof the logged authority and path.
fn absolute_form(target: &str) -> Option<&str> {
    let (scheme, rest) = target.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(rest)
}

fn request_head(line: &[u8], fields: &Fields) -> Option<RequestHead> {
    let (method, target, minor) = scan::parse_request_line(line)?;
    let method = truncate(String::from_utf8_lossy(method).into_owned(), METHOD_LIMIT);
    let target = String::from_utf8_lossy(target);
    let connect = method == "CONNECT";
    let (path, uri_authority) = if connect {
        (String::new(), Some(host_port(&target)))
    } else if let Some(rest) = absolute_form(&target) {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let path = rest[end..].split(['?', '#']).next().unwrap_or_default();
        let path = if path.is_empty() { "/" } else { path };
        (path.to_string(), Some(host_port(&rest[..end])))
    } else {
        let path = target.split(['?', '#']).next().unwrap_or_default();
        (path.to_string(), None)
    };
    Some(RequestHead {
        method,
        path: truncate(path, PATH_LIMIT),
        uri_authority,
        host: fields.host.clone().map(|h| truncate(h, AUTHORITY_LIMIT)),
        version: if minor == 0 {
            Version::Http10
        } else {
            Version::Http11
        },
        upgrade: fields.connection_upgrade && fields.has_upgrade,
        connect,
    })
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    head: RequestHead,
}

#[derive(Debug)]
enum ReqState {
    Head(HeadScanner),
    Body {
        body: Body,
        head: RequestHead,
    },
    /// Behind an upgrade request: bytes wait for its final response.
    Held {
        seq: u64,
        buf: Vec<u8>,
    },
}

impl Default for ReqState {
    fn default() -> Self {
        ReqState::Head(HeadScanner::new(Kind::Request))
    }
}

#[derive(Debug)]
enum RespState {
    Head(HeadScanner),
    Body {
        body: Body,
        request: Option<RequestHead>,
    },
}

impl Default for RespState {
    fn default() -> Self {
        RespState::Head(HeadScanner::new(Kind::Response))
    }
}

/// Framer for one connection, both directions.
#[derive(Debug, Default)]
pub struct Codec {
    req: ReqState,
    resp: RespState,
    pending: VecDeque<Pending>,
    next_seq: u64,
    /// Unframed or tunnelled: no more events.
    opaque: bool,
    /// Emit [`Event::Captured`].
    capture: bool,
    req_cap: Cap,
    resp_cap: Cap,
}

impl Codec {
    pub fn new() -> Self {
        Self::default()
    }

    /// A codec that also emits [`Event::Captured`] for the discovery tap.
    pub fn with_capture() -> Self {
        Self {
            capture: true,
            ..Self::default()
        }
    }

    /// Bytes read from the client, all of them used. Without the relay's flow control: a
    /// request head after [`READ_AHEAD`] unanswered ones is [`Reason::ReadAheadOverflow`].
    ///
    /// Held bytes: after a request that asks for an upgrade or is a `CONNECT`, the codec keeps
    /// the client bytes that follow (up to [`HEAD_LIMIT`]) until the final response. They have
    /// been read and are never dropped: the relay forwards them unchanged whatever happens.
    /// On `101` or a 2xx to `CONNECT` ([`Event::Tunnel`]) the codec stops parsing them; on a
    /// declined upgrade it parses them as the next requests.
    pub fn client_bytes(&mut self, data: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        self.feed_requests(data, false, &mut events);
        events
    }

    /// Bytes read from the destination.
    pub fn server_bytes(&mut self, mut data: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        while !data.is_empty() && !self.opaque {
            match mem::take(&mut self.resp) {
                RespState::Head(mut scanner) => {
                    let (n, scan) = scanner.feed(data);
                    data = &data[n..];
                    match scan {
                        Scan::More => self.resp = RespState::Head(scanner),
                        Scan::Unframed(reason) => {
                            let request = self.pending.front().map(|p| p.head.clone());
                            self.unframed(Side::Server, reason, request, &mut events);
                        }
                        Scan::Done => self.response_head(&scanner, &mut events),
                    }
                }
                RespState::Body { mut body, request } => {
                    let (n, scan) = body.feed(data, &mut self.resp_cap);
                    data = &data[n..];
                    if !self.resp_cap.out.is_empty() {
                        let bytes = mem::take(&mut self.resp_cap.out);
                        events.push(Event::Captured(Captured::ResponseBody(bytes)));
                    }
                    match scan {
                        BodyScan::More => self.resp = RespState::Body { body, request },
                        BodyScan::Done(body_bytes) => {
                            events.push(Event::ResponseEnd { body_bytes });
                        }
                        BodyScan::Unframed(reason) => {
                            self.unframed(Side::Server, reason, request, &mut events);
                        }
                    }
                }
            }
        }
        events
    }

    /// The destination closed (FIN): ends a close-delimited response.
    pub fn server_eof(&mut self) -> Vec<Event> {
        if let RespState::Body { body, .. } = &self.resp
            && let Some(body_bytes) = body.at_eof()
        {
            self.resp = RespState::default();
            return vec![Event::ResponseEnd { body_bytes }];
        }
        Vec::new()
    }

    /// Like [`Codec::client_bytes`], but stops before the head of a request that would read
    /// more than [`READ_AHEAD`] requests ahead of their responses. Returns how many bytes of
    /// `data` were used; the relay forwards exactly those, keeps the rest unread (TCP
    /// back-pressure) and offers it again after a response completed.
    ///
    /// It also stops right after a request that asks for an upgrade or is a `CONNECT`
    /// (the client must wait for the final response). The bytes not used are never dropped:
    /// on [`Event::Tunnel`] or [`Event::Unframed`] the relay forwards them unchanged, and
    /// after any other final response it offers them again. Once the codec is opaque, every
    /// byte is used.
    pub fn client_bytes_limited(&mut self, data: &[u8]) -> (usize, Vec<Event>) {
        let mut events = Vec::new();
        let used = self.feed_requests(data, true, &mut events);
        (used, events)
    }

    /// Bytes the codec keeps for its parsing: partial head or trailer section, held client
    /// bytes, requests waiting for a response. Never above [`RETAINED_LIMIT`].
    pub fn retained_bytes(&self) -> usize {
        let req = match &self.req {
            ReqState::Head(scanner) => scanner.retained(),
            ReqState::Body { body, head } => body.retained() + head_size(head),
            ReqState::Held { buf, .. } => buf.len(),
        };
        let resp = match &self.resp {
            RespState::Head(scanner) => scanner.retained(),
            RespState::Body { body, request } => {
                body.retained() + request.as_ref().map_or(0, head_size)
            }
        };
        let pending: usize = self.pending.iter().map(|p| head_size(&p.head)).sum();
        req + resp + pending
    }

    /// Between two messages: no request waiting for its response, no message half-read, not
    /// opaque. Only then may the relay close the connection (shutdown) without cutting anything.
    pub fn idle(&self) -> bool {
        !self.opaque
            && self.pending.is_empty()
            && matches!(&self.req, ReqState::Head(scanner) if scanner.is_fresh())
            && matches!(&self.resp, RespState::Head(scanner) if scanner.is_fresh())
    }

    /// Payload bytes of the request body being read, 0 between two requests.
    pub fn request_body_bytes(&self) -> u64 {
        match &self.req {
            ReqState::Body { body, .. } => body.seen(),
            _ => 0,
        }
    }

    /// Payload bytes of the response body being read, 0 between two responses.
    pub fn response_body_bytes(&self) -> u64 {
        match &self.resp {
            RespState::Body { body, .. } => body.seen(),
            _ => 0,
        }
    }

    /// Requests read and not yet answered by a final response.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    fn unframed(
        &mut self,
        side: Side,
        reason: Reason,
        request: Option<RequestHead>,
        events: &mut Vec<Event>,
    ) {
        self.opaque = true;
        events.push(Event::Unframed(Unframed {
            side,
            reason,
            request,
        }));
    }

    /// Feed client bytes; returns how many were used (all of them unless `limited`).
    fn feed_requests(&mut self, mut data: &[u8], limited: bool, events: &mut Vec<Event>) -> usize {
        let total = data.len();
        while !data.is_empty() && !self.opaque {
            // Only a fresh head can wait: the count grows when a head completes.
            if matches!(self.req, ReqState::Head(_)) && self.pending.len() >= READ_AHEAD {
                if limited {
                    break;
                }
                self.unframed(Side::Client, Reason::ReadAheadOverflow, None, events);
                break;
            }
            if limited && matches!(self.req, ReqState::Held { .. }) {
                break;
            }
            match mem::take(&mut self.req) {
                ReqState::Head(mut scanner) => {
                    let (n, scan) = scanner.feed(data);
                    data = &data[n..];
                    match scan {
                        Scan::More => self.req = ReqState::Head(scanner),
                        Scan::Unframed(reason) => {
                            let no_fields = collect_fields(std::iter::empty());
                            let known = scanner
                                .lines()
                                .next()
                                .and_then(|line| request_head(line, &no_fields));
                            self.unframed(Side::Client, reason, known, events);
                        }
                        Scan::Done => self.request_head(&scanner, events),
                    }
                }
                ReqState::Body { mut body, head } => {
                    let (n, scan) = body.feed(data, &mut self.req_cap);
                    data = &data[n..];
                    if !self.req_cap.out.is_empty() {
                        let bytes = mem::take(&mut self.req_cap.out);
                        events.push(Event::Captured(Captured::RequestBody(bytes)));
                    }
                    match scan {
                        BodyScan::More => self.req = ReqState::Body { body, head },
                        BodyScan::Done(body_bytes) => self.request_done(&head, body_bytes, events),
                        BodyScan::Unframed(reason) => {
                            self.unframed(Side::Client, reason, Some(head), events);
                        }
                    }
                }
                ReqState::Held { seq, mut buf } => {
                    if buf.len() + data.len() > HEAD_LIMIT {
                        self.unframed(Side::Client, Reason::HeldOverflow, None, events);
                    } else {
                        buf.extend_from_slice(data);
                        self.req = ReqState::Held { seq, buf };
                    }
                    return total;
                }
            }
        }
        if self.opaque {
            total
        } else {
            total - data.len()
        }
    }

    fn request_head(&mut self, scanner: &HeadScanner, events: &mut Vec<Event>) {
        let mut lines = scanner.lines();
        let start = lines.next().unwrap_or_default();
        let fields = collect_fields(lines);
        let Some(head) = request_head(start, &fields) else {
            self.unframed(Side::Client, Reason::BadStartLine, None, events);
            return;
        };
        let framing = match fields.framing(true, head.version) {
            Ok(framing) => framing,
            Err(reason) => {
                self.unframed(Side::Client, reason, Some(head), events);
                return;
            }
        };
        self.pending.push_back(Pending {
            seq: self.next_seq,
            head: head.clone(),
        });
        events.push(Event::Request(head.clone()));
        if self.capture {
            events.push(Event::Captured(Captured::RequestHead(
                scanner.raw().to_vec(),
            )));
            self.req_cap.room = CAPTURE_BODY_LIMIT;
        }
        match framing {
            Framing::Length(n) if n > 0 => {
                self.req = ReqState::Body {
                    body: Body::length(n),
                    head,
                };
            }
            Framing::Chunked => {
                self.req = ReqState::Body {
                    body: Body::chunked(),
                    head,
                };
            }
            _ => self.request_done(&head, 0, events),
        }
    }

    /// The request is complete: next head, or hold behind an upgrade still unanswered.
    fn request_done(&mut self, head: &RequestHead, body_bytes: u64, events: &mut Vec<Event>) {
        events.push(Event::RequestEnd { body_bytes });
        let seq = self.next_seq;
        self.next_seq += 1;
        self.req = if (head.upgrade || head.connect) && self.pending.iter().any(|p| p.seq == seq) {
            ReqState::Held {
                seq,
                buf: Vec::new(),
            }
        } else {
            ReqState::default()
        };
    }

    fn response_head(&mut self, scanner: &HeadScanner, events: &mut Vec<Event>) {
        let mut lines = scanner.lines();
        let start = lines.next().unwrap_or_default();
        let front = self.pending.front().map(|p| p.head.clone());
        let Some((minor, status)) = scan::parse_status_line(start) else {
            self.unframed(Side::Server, Reason::BadStartLine, front, events);
            return;
        };
        let version = if minor == 0 {
            Version::Http10
        } else {
            Version::Http11
        };
        let head = ResponseHead {
            version,
            status,
            request: front.clone(),
        };
        if status < 200 && status != 101 {
            events.push(Event::Interim(head));
            return;
        }
        let answered = self.pending.pop_front();
        let connect_ok = (200..300).contains(&status) && front.as_ref().is_some_and(|r| r.connect);
        if status == 101 || connect_ok {
            events.push(Event::Response(head));
            if self.capture {
                events.push(Event::Captured(Captured::ResponseHead(
                    scanner.raw().to_vec(),
                )));
            }
            events.push(Event::Tunnel);
            self.opaque = true;
            return;
        }
        let bodyless =
            status == 204 || status == 304 || front.as_ref().is_some_and(|r| r.method == "HEAD");
        let framing = if bodyless {
            Ok(Framing::None)
        } else {
            collect_fields(lines).framing(false, version)
        };
        let body = match framing {
            Ok(Framing::Length(n)) if n > 0 => Some(Body::length(n)),
            Ok(Framing::Chunked) => Some(Body::chunked()),
            Ok(Framing::UntilClose) => Some(Body::until_close()),
            Ok(_) => None,
            Err(reason) => {
                self.unframed(Side::Server, reason, front, events);
                return;
            }
        };
        events.push(Event::Response(head));
        if self.capture {
            events.push(Event::Captured(Captured::ResponseHead(
                scanner.raw().to_vec(),
            )));
            self.resp_cap.room = CAPTURE_BODY_LIMIT;
        }
        match body {
            Some(body) => {
                self.resp = RespState::Body {
                    body,
                    request: front,
                }
            }
            None => events.push(Event::ResponseEnd { body_bytes: 0 }),
        }
        // A declined upgrade or CONNECT: the client bytes held behind it are HTTP again.
        let released = matches!(
            (&self.req, &answered),
            (ReqState::Held { seq, .. }, Some(a)) if a.seq == *seq
        );
        if released && let ReqState::Held { buf, .. } = mem::take(&mut self.req) {
            self.feed_requests(&buf, false, events);
        }
    }
}

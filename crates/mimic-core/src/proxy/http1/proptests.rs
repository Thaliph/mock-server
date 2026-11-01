//! PRX-27: codec properties.
use super::*;
use proptest::prelude::*;

const CASES: u32 = 10_000;
const CLOSE_BODY: &str = "until the end";

#[derive(Debug, Clone)]
enum BodyKind {
    /// No body bytes.
    None,
    Length(Vec<u8>),
    Chunked {
        chunks: Vec<usize>,
        ext: bool,
        trailers: bool,
    },
}

impl BodyKind {
    fn bytes(&self) -> u64 {
        match self {
            BodyKind::None => 0,
            BodyKind::Length(v) => v.len() as u64,
            BodyKind::Chunked { chunks, .. } => chunks.iter().sum::<usize>() as u64,
        }
    }
}

#[derive(Debug, Clone)]
struct Exchange {
    method: &'static str,
    path: String,
    upper: bool,
    lf: bool,
    extra: Vec<(String, String)>,
    req_body: BodyKind,
    interims: Vec<u16>,
    status: u16,
    resp_body: BodyKind,
    /// A `content-length` header on a body-less response.
    ghost_length: Option<u32>,
}

fn body() -> impl Strategy<Value = BodyKind> {
    prop_oneof![
        Just(BodyKind::None),
        proptest::collection::vec(any::<u8>(), 1..60).prop_map(BodyKind::Length),
        (
            proptest::collection::vec(1usize..40, 0..4),
            any::<bool>(),
            any::<bool>()
        )
            .prop_map(|(chunks, ext, trailers)| BodyKind::Chunked {
                chunks,
                ext,
                trailers
            }),
    ]
}

fn exchange() -> impl Strategy<Value = Exchange> {
    (
        prop_oneof![Just("GET"), Just("POST"), Just("HEAD"), Just("PUT")],
        "/[a-z0-9/]{0,30}",
        any::<bool>(),
        any::<bool>(),
        proptest::collection::vec(("[A-Za-z][A-Za-z0-9-]{0,10}", "[!-~]{0,20}"), 0..5),
        body(),
        proptest::collection::vec(prop_oneof![Just(100u16), Just(103u16)], 0..3),
        prop_oneof![
            Just(200u16),
            Just(201),
            Just(204),
            Just(304),
            Just(404),
            Just(500)
        ],
        body(),
        proptest::option::of(0u32..100_000),
    )
        .prop_map(
            |(method, path, upper, lf, extra, req_body, interims, status, resp_body, ghost)| {
                Exchange {
                    method,
                    path,
                    upper,
                    lf,
                    extra: extra
                        .into_iter()
                        .filter(|(n, _)| {
                            !matches!(
                                n.to_ascii_lowercase().as_str(),
                                "content-length"
                                    | "transfer-encoding"
                                    | "connection"
                                    | "upgrade"
                                    | "host"
                            )
                        })
                        .collect(),
                    req_body,
                    interims,
                    status,
                    resp_body,
                    ghost_length: ghost,
                }
            },
        )
}

fn eol(lf: bool) -> &'static str {
    if lf { "\n" } else { "\r\n" }
}

fn name(n: &str, upper: bool) -> String {
    if upper {
        n.to_ascii_uppercase()
    } else {
        n.to_ascii_lowercase()
    }
}

fn encode_body(out: &mut Vec<u8>, b: &BodyKind, lf: bool) {
    match b {
        BodyKind::None => {}
        BodyKind::Length(v) => out.extend_from_slice(v),
        BodyKind::Chunked {
            chunks,
            ext,
            trailers,
        } => {
            for n in chunks {
                let e = if *ext { ";name=value" } else { "" };
                out.extend(format!("{n:x}{e}{}", eol(lf)).bytes());
                out.extend(std::iter::repeat_n(b'z', *n));
                out.extend(eol(lf).bytes());
            }
            out.extend(format!("0{}", eol(lf)).bytes());
            if *trailers {
                out.extend(format!("X-Trailer: t{}", eol(lf)).bytes());
            }
            out.extend(eol(lf).bytes());
        }
    }
}

fn length_headers(b: &BodyKind, upper: bool, lf: bool) -> String {
    match b {
        BodyKind::None => String::new(),
        BodyKind::Length(v) => format!("{}: {}{}", name("Content-Length", upper), v.len(), eol(lf)),
        BodyKind::Chunked { .. } => {
            format!("{}: chunked{}", name("Transfer-Encoding", upper), eol(lf))
        }
    }
}

impl Exchange {
    fn request(&self) -> Vec<u8> {
        let mut out =
            format!("{} {} HTTP/1.1{}", self.method, self.path, eol(self.lf)).into_bytes();
        out.extend(format!("{}: example.test{}", name("Host", self.upper), eol(self.lf)).bytes());
        for (n, v) in &self.extra {
            out.extend(format!("{n}: {v}{}", eol(self.lf)).bytes());
        }
        out.extend(length_headers(&self.req_body, self.upper, self.lf).bytes());
        out.extend(eol(self.lf).bytes());
        encode_body(&mut out, &self.req_body, self.lf);
        out
    }

    fn bodyless_response(&self) -> bool {
        self.method == "HEAD" || self.status == 204 || self.status == 304
    }

    /// The body that is on the wire.
    fn wire_body(&self) -> BodyKind {
        if self.bodyless_response() {
            BodyKind::None
        } else {
            self.resp_body.clone()
        }
    }

    fn response(&self, close_delimited: bool) -> Vec<u8> {
        let lf = self.lf;
        let mut out = Vec::new();
        for i in &self.interims {
            out.extend(format!("HTTP/1.1 {i} X{}Link: <a>{}", eol(lf), eol(lf)).bytes());
            out.extend(eol(lf).bytes());
        }
        out.extend(format!("HTTP/1.1 {} Reason{}", self.status, eol(lf)).bytes());
        let body = self.wire_body();
        if self.bodyless_response() {
            if let Some(n) = self.ghost_length {
                out.extend(format!("Content-Length: {n}{}", eol(lf)).bytes());
            }
        } else if close_delimited {
            // No length at all.
        } else if matches!(body, BodyKind::None) {
            out.extend(format!("Content-Length: 0{}", eol(lf)).bytes());
        } else {
            out.extend(length_headers(&body, self.upper, lf).bytes());
        }
        out.extend(eol(lf).bytes());
        if close_delimited && !self.bodyless_response() {
            out.extend(CLOSE_BODY.bytes());
        } else {
            encode_body(&mut out, &body, lf);
        }
        out
    }
}

#[derive(Debug)]
struct Conversation {
    client: Vec<u8>,
    server: Vec<u8>,
    close_delimited: bool,
    exchanges: Vec<Exchange>,
}

fn conversation() -> impl Strategy<Value = Conversation> {
    (proptest::collection::vec(exchange(), 1..6), any::<bool>()).prop_map(|(exchanges, close)| {
        let last = exchanges.last().is_some_and(|e| !e.bodyless_response());
        let close_delimited = close && last;
        let mut client = Vec::new();
        let mut server = Vec::new();
        for (i, e) in exchanges.iter().enumerate() {
            client.extend(e.request());
            server.extend(e.response(close_delimited && i + 1 == exchanges.len()));
        }
        Conversation {
            client,
            server,
            close_delimited,
            exchanges,
        }
    })
}

/// Cut `data` at the given fractions of its length.
fn split<'a>(data: &'a [u8], cuts: &[u16]) -> Vec<&'a [u8]> {
    let mut points: Vec<usize> = cuts
        .iter()
        .map(|c| usize::from(*c) * data.len() / usize::from(u16::MAX))
        .collect();
    points.sort_unstable();
    let mut parts = Vec::new();
    let mut from = 0;
    for p in points {
        parts.push(&data[from..p]);
        from = p;
    }
    parts.push(&data[from..]);
    parts
}

/// Returns the events and the peak retained state.
fn drive(client: &[&[u8]], server: &[&[u8]], eof: bool) -> (Vec<Event>, usize) {
    let mut c = Codec::new();
    let mut ev = Vec::new();
    let mut peak = 0;
    for part in client {
        ev.extend(c.client_bytes(part));
        peak = peak.max(c.retained_bytes());
    }
    for part in server {
        ev.extend(c.server_bytes(part));
        peak = peak.max(c.retained_bytes());
    }
    if eof {
        ev.extend(c.server_eof());
    }
    (ev, peak)
}

fn check_framing(conv: &Conversation, ev: &[Event], peak: usize) -> Result<(), TestCaseError> {
    prop_assert!(peak <= RETAINED_LIMIT);
    let n = conv.exchanges.len();
    let requests: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Request(r) => Some((r.method.clone(), r.path.clone())),
            _ => None,
        })
        .collect();
    let expected: Vec<_> = conv
        .exchanges
        .iter()
        .map(|e| (e.method.to_string(), e.path.clone()))
        .collect();
    prop_assert_eq!(requests, expected);
    let req_ends: Vec<u64> = ev
        .iter()
        .filter_map(|e| match e {
            Event::RequestEnd { body_bytes } => Some(*body_bytes),
            _ => None,
        })
        .collect();
    prop_assert_eq!(
        req_ends,
        conv.exchanges
            .iter()
            .map(|e| e.req_body.bytes())
            .collect::<Vec<_>>()
    );
    let finals: Vec<(u16, String)> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Response(h) => Some((
                h.status,
                h.request
                    .as_ref()
                    .map(|r| r.path.clone())
                    .unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    prop_assert_eq!(
        finals,
        conv.exchanges
            .iter()
            .map(|e| (e.status, e.path.clone()))
            .collect::<Vec<_>>()
    );
    let interims = ev.iter().filter(|e| matches!(e, Event::Interim(_))).count();
    prop_assert_eq!(
        interims,
        conv.exchanges
            .iter()
            .map(|e| e.interims.len())
            .sum::<usize>()
    );
    let resp_ends: Vec<u64> = ev
        .iter()
        .filter_map(|e| match e {
            Event::ResponseEnd { body_bytes } => Some(*body_bytes),
            _ => None,
        })
        .collect();
    let mut want: Vec<u64> = conv
        .exchanges
        .iter()
        .map(|e| e.wire_body().bytes())
        .collect();
    if conv.close_delimited {
        want[n - 1] = CLOSE_BODY.len() as u64;
    }
    prop_assert_eq!(resp_ends, want);
    prop_assert!(
        !ev.iter()
            .any(|e| matches!(e, Event::Unframed(_) | Event::Tunnel))
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn prx_27_arbitrary_bytes_never_panic_and_retained_state_is_bounded(
        client in proptest::collection::vec(any::<u8>(), 0..2048),
        server in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let (_, peak) = drive(&[&client], &[&server], true);
        prop_assert!(peak <= RETAINED_LIMIT, "retained {peak}");
    }

    #[test]
    fn prx_27_textual_noise_never_panics_and_retained_state_is_bounded(
        client in "(GET |POST |HEAD |CONNECT )?[ -~\r\n\t]{0,300}(\r\n\r\n)?[0-9a-f;\r\n ]{0,100}",
        server in "(HTTP/1\\.1 [0-9]{3} )?[ -~\r\n\t]{0,300}(\r\n\r\n)?[0-9a-f;\r\n ]{0,100}",
    ) {
        let (_, peak) = drive(&[client.as_bytes()], &[server.as_bytes()], true);
        prop_assert!(peak <= RETAINED_LIMIT, "retained {peak}");
    }

    #[test]
    fn prx_27_long_streams_keep_retained_state_bounded(
        unit in "(GET /[a-z]{0,20} HTTP/1\\.1\r\n\r\n)|(POST / HTTP/1\\.1\r\nHost: [a-z]{0,2000}\r\nContent-Length: [0-9]\r\n\r\n)",
        times in 1usize..3000,
    ) {
        let mut c = Codec::new();
        let data = unit.repeat(times);
        let mut rest: &[u8] = data.as_bytes();
        while !rest.is_empty() {
            let (used, _) = c.client_bytes_limited(rest);
            prop_assert!(c.retained_bytes() <= RETAINED_LIMIT);
            if used == 0 {
                break;
            }
            rest = &rest[used..];
        }
        prop_assert!(c.pending() <= READ_AHEAD);
    }

    #[test]
    fn prx_27_plain_feed_never_has_more_than_the_read_ahead_pending(
        units in proptest::collection::vec(
            prop_oneof![
                Just(&b"GET / HTTP/1.1\r\n\r\n"[..]),
                Just(&b"GET /a\n\n"[..]),
                Just(&b"POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nx"[..]),
                Just(&b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"[..]),
                Just(&b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: x\r\n\r\n"[..]),
                Just(&b"CONNECT a:1 HTTP/1.1\r\n\r\n"[..]),
                Just(&b"\r\n"[..]),
                Just(&b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"[..]),
                Just(&b"HTTP/1.1 100 Continue\r\n\r\n"[..]),
            ],
            0..120,
        ),
        server_mask in proptest::collection::vec(any::<bool>(), 120),
    ) {
        let mut c = Codec::new();
        for (u, to_server) in units.iter().zip(server_mask) {
            if to_server && u.starts_with(b"HTTP") {
                c.server_bytes(u);
            } else {
                c.client_bytes(u);
            }
            prop_assert!(c.pending() <= READ_AHEAD, "{}", c.pending());
            prop_assert!(c.retained_bytes() <= RETAINED_LIMIT);
        }
    }

    #[test]
    fn prx_27_any_split_of_the_input_gives_the_same_events(
        client in proptest::collection::vec(any::<u8>(), 0..300),
        server in proptest::collection::vec(any::<u8>(), 0..300),
        ccuts in proptest::collection::vec(any::<u16>(), 0..8),
        scuts in proptest::collection::vec(any::<u16>(), 0..8),
    ) {
        let whole = drive(&[&client], &[&server], true).0;
        let cut = drive(&split(&client, &ccuts), &split(&server, &scuts), true).0;
        prop_assert_eq!(whole, cut);
    }

    #[test]
    fn prx_27_any_split_of_a_valid_conversation_gives_the_same_events(
        conv in conversation(),
        ccuts in proptest::collection::vec(any::<u16>(), 0..12),
        scuts in proptest::collection::vec(any::<u16>(), 0..12),
    ) {
        let whole = drive(&[&conv.client], &[&conv.server], conv.close_delimited).0;
        let cut = drive(
            &split(&conv.client, &ccuts),
            &split(&conv.server, &scuts),
            conv.close_delimited,
        ).0;
        prop_assert_eq!(whole, cut);
    }

    #[test]
    fn prx_27_generated_conversations_are_framed_exactly(
        conv in conversation(),
        ccuts in proptest::collection::vec(any::<u16>(), 0..12),
        scuts in proptest::collection::vec(any::<u16>(), 0..12),
    ) {
        let (ev, peak) = drive(
            &split(&conv.client, &ccuts),
            &split(&conv.server, &scuts),
            conv.close_delimited,
        );
        check_framing(&conv, &ev, peak)?;
    }
}

fn cuts_of(len: usize, cuts: &[u16]) -> Vec<usize> {
    let mut points: Vec<usize> = cuts
        .iter()
        .map(|c| usize::from(*c) * 7 % (len + 1))
        .collect();
    points.sort_unstable();
    points
}

fn pieces(data: &[u8], cuts: &[u16]) -> Vec<Vec<u8>> {
    let mut parts = Vec::new();
    let mut from = 0;
    for p in cuts_of(data.len(), cuts) {
        parts.push(data[from..p].to_vec());
        from = p;
    }
    parts.push(data[from..].to_vec());
    parts
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// Heads and trailer sections whose size straddles the 64 KiB limit, with the requests
    /// read ahead in front of them, cut anywhere.
    #[test]
    fn prx_27_sections_around_the_limit_keep_retained_state_bounded(
        client_value in HEAD_LIMIT - 2000..HEAD_LIMIT + 2000,
        server_value in HEAD_LIMIT - 2000..HEAD_LIMIT + 2000,
        trailer_value in HEAD_LIMIT - 2000..HEAD_LIMIT + 2000,
        ahead in 0usize..READ_AHEAD,
        host in 0usize..3000,
        open in any::<bool>(),
        ccuts in proptest::collection::vec(any::<u16>(), 0..6),
        scuts in proptest::collection::vec(any::<u16>(), 0..6),
    ) {
        let mut client = Vec::new();
        for _ in 0..ahead {
            client.extend(format!("GET http://{0}/{0} HTTP/1.1\r\nHost: {0}\r\n\r\n", "h".repeat(host)).bytes());
        }
        // An open section (no blank line yet) is what the scanner retains between two reads.
        let end = if open { "" } else { "\r\n\r\n" };
        client.extend(format!("GET / HTTP/1.1\r\nX: {}{end}", "a".repeat(client_value)).bytes());
        let mut server = Vec::new();
        for _ in 0..ahead {
            server.extend(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
        server.extend(format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nT: {}\r\n\r\n",
            "a".repeat(trailer_value)
        ).bytes());
        server.extend(format!("HTTP/1.1 200 OK\r\nX: {}{end}", "a".repeat(server_value)).bytes());
        let (client, server) = (pieces(&client, &ccuts), pieces(&server, &scuts));
        let (client, server): (Vec<&[u8]>, Vec<&[u8]>) = (
            client.iter().map(Vec::as_slice).collect(),
            server.iter().map(Vec::as_slice).collect(),
        );
        let (_, peak) = drive(&client, &server, true);
        prop_assert!(peak <= RETAINED_LIMIT, "retained {peak}");
        // The bound is approached, not merely respected by small inputs.
        if open && client_value < HEAD_LIMIT - 100 {
            prop_assert!(peak >= client_value, "retained {peak}");
        }
    }

    /// Client bytes held behind an upgrade, up to and past the limit, in any cut.
    #[test]
    fn prx_27_held_bytes_around_the_limit_keep_retained_state_bounded(
        total in HEAD_LIMIT - 2000..HEAD_LIMIT + 2000,
        cuts in proptest::collection::vec(any::<u16>(), 0..12),
    ) {
        let mut c = Codec::new();
        c.client_bytes(b"CONNECT a:1 HTTP/1.1\r\n\r\n");
        let mut peak = 0;
        for part in pieces(&vec![b'x'; total], &cuts) {
            c.client_bytes(&part);
            peak = peak.max(c.retained_bytes());
        }
        prop_assert!(peak <= RETAINED_LIMIT, "retained {peak}");
        if total <= HEAD_LIMIT {
            prop_assert!(peak >= total, "retained {peak}");
        }
    }
}

/// Request `i + 1` and response `i` are on the wire at the same time: their pieces alternate.
fn interleaved(conv: &Conversation, ccuts: &[u16], scuts: &[u16]) -> (Vec<Event>, usize) {
    let mut c = Codec::new();
    let (mut ev, mut peak) = (Vec::new(), 0);
    let n = conv.exchanges.len();
    let mut previous: Vec<Vec<u8>> = Vec::new();
    for i in 0..=n {
        let request = conv
            .exchanges
            .get(i)
            .map(Exchange::request)
            .unwrap_or_default();
        let mine = pieces(&request, ccuts);
        let mut theirs = previous.into_iter();
        let mut mine = mine.into_iter();
        loop {
            let (a, b) = (mine.next(), theirs.next());
            if a.is_none() && b.is_none() {
                break;
            }
            if let Some(a) = a {
                ev.extend(c.client_bytes(&a));
            }
            if let Some(b) = b {
                ev.extend(c.server_bytes(&b));
            }
            peak = peak.max(c.retained_bytes());
        }
        previous = conv.exchanges.get(i).map_or_else(Vec::new, |e| {
            pieces(&e.response(conv.close_delimited && i + 1 == n), scuts)
        });
    }
    if conv.close_delimited {
        ev.extend(c.server_eof());
    }
    (ev, peak)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn prx_27_interleaved_client_and_server_bytes_frame_exactly(
        conv in conversation(),
        ccuts in proptest::collection::vec(any::<u16>(), 0..12),
        scuts in proptest::collection::vec(any::<u16>(), 0..12),
    ) {
        let (ev, peak) = interleaved(&conv, &ccuts, &scuts);
        check_framing(&conv, &ev, peak)?;
    }
}

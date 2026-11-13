//! MSH-65 and MSH-66: the head reader of a mesh-routed HTTP/1 connection.

use super::head::{Reader, Stop, leaves};
use crate::proxy::http1::Reason;

const GET: &[u8] = b"GET /a HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n";
const POST: &[u8] = b"POST /b HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 5\r\n\r\nhello";
const CHUNKED: &[u8] = b"POST /c HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n0\r\n\r\n";
const UPGRADE: &[u8] =
    b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
const CONNECT: &[u8] = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";

/// Every way a refused head can look.
const REFUSED: [(&str, &[u8]); 9] = [
    (
        "a name that is not a token",
        b"GET / HTTP/1.1\r\nHost: a\r\nBad Header: 1\r\n\r\n",
    ),
    ("a bare CR", b"GET / HTTP/1.1\r\nHost: a\r\nX: a\rb\r\n\r\n"),
    (
        "obs-fold",
        b"GET / HTTP/1.1\r\nHost: a\r\nX: a\r\n  folded\r\n\r\n",
    ),
    (
        "two Content-Length",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
    ),
    (
        "a Content-Length that is not a number",
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: x\r\n\r\n",
    ),
    (
        "a Transfer-Encoding not ending in chunked",
        b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n",
    ),
    (
        "a NUL in a value",
        b"GET / HTTP/1.1\r\nHost: a\r\nX: a\0b\r\n\r\n",
    ),
    ("no request line", b"\r\n\r\n"),
    (
        "a line without a colon",
        b"GET / HTTP/1.1\r\nHost: a\r\nnonsense\r\n\r\n",
    ),
];

fn read(data: &[u8], upgrades: bool) -> (Option<Stop>, Vec<u8>, Vec<u8>) {
    let mut reader = Reader::new();
    let mut pass = Vec::new();
    let stop = reader.feed(data, upgrades, &mut pass);
    (stop, pass, reader.held().to_vec())
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn msh_65_good_requests_pass_unchanged() {
    for request in [GET, POST, CHUNKED] {
        let (stop, pass, _) = read(request, true);
        assert_eq!(stop, None);
        assert_eq!(pass, request);
    }
    let all = cat(&[GET, POST, CHUNKED, GET]);
    let (stop, pass, _) = read(&all, true);
    assert_eq!((stop, pass), (None, all));
}

#[test]
fn msh_65_a_refused_head_is_held_with_everything_after_it() {
    for (what, bad) in REFUSED {
        let (stop, pass, held) = read(bad, true);
        assert!(
            matches!(stop, Some(Stop::Refused { .. })),
            "{what}: {stop:?}"
        );
        assert!(pass.is_empty(), "{what}: nothing passed");
        assert_eq!(held, bad, "{what}");
        let input = cat(&[GET, POST, bad, GET]);
        let (stop, pass, held) = read(&input, true);
        assert!(
            matches!(stop, Some(Stop::Refused { .. })),
            "{what}: {stop:?}"
        );
        assert_eq!(pass, cat(&[GET, POST]), "{what}: the good ones passed");
        assert_eq!(held, cat(&[bad, GET]), "{what}");
    }
}

#[test]
fn msh_65_a_head_over_64_kib_is_refused() {
    let mut huge = b"GET / HTTP/1.1\r\nHost: a\r\nX: ".to_vec();
    huge.extend(std::iter::repeat_n(b'a', 70 * 1024));
    let (stop, pass, held) = read(&huge, true);
    assert!(
        matches!(
            stop,
            Some(Stop::Refused {
                reason: Reason::HeadTooLarge,
                ..
            })
        ),
        "{stop:?}"
    );
    assert!(pass.is_empty());
    assert_eq!(held, huge);
}

#[test]
fn msh_65_the_refusal_names_the_reason_and_the_request_line() {
    let (stop, _, _) = read(REFUSED[0].1, true);
    let Some(Stop::Refused { reason, request }) = stop else {
        panic!("refused");
    };
    assert_eq!(reason, Reason::BadHeaderLine);
    assert_eq!(request.map(|r| r.method), Some("GET".to_string()));
}

#[test]
fn msh_65_the_verdict_is_the_same_however_the_bytes_are_cut() {
    let mut inputs: Vec<Vec<u8>> = vec![
        cat(&[GET, POST, CHUNKED, UPGRADE, GET]),
        cat(&[GET, CONNECT]),
    ];
    inputs.extend(REFUSED.iter().map(|(_, bad)| cat(&[GET, POST, bad, GET])));
    for input in inputs {
        let whole = read(&input, true);
        for size in [1usize, 2, 3, 7, 16] {
            let mut reader = Reader::new();
            let mut pass = Vec::new();
            let mut stop = None;
            for chunk in input.chunks(size) {
                stop = stop.or(reader.feed(chunk, true, &mut pass));
            }
            assert_eq!(
                (stop, pass, reader.held().to_vec()),
                whole,
                "cut every {size} bytes"
            );
        }
    }
}

#[test]
fn msh_65_what_is_not_passed_is_held_or_pending_and_nothing_is_lost() {
    let input = cat(&[GET, POST, b"GET /partial HTTP/1.1\r\nHo"]);
    let mut reader = Reader::new();
    let mut pass = Vec::new();
    assert_eq!(reader.feed(&input, true, &mut pass), None);
    assert_eq!(pass, cat(&[GET, POST]));
    assert_eq!(reader.pending(), b"GET /partial HTTP/1.1\r\nHo".len());
}

#[test]
fn msh_65_an_upgrade_or_connect_head_leaves_with_the_bytes_after_it() {
    for head in [UPGRADE, CONNECT] {
        let input = cat(&[GET, head, b"frame-bytes"]);
        let (stop, pass, held) = read(&input, true);
        assert_eq!(stop, Some(Stop::Upgrade));
        assert_eq!(pass, GET);
        assert_eq!(held, cat(&[head, b"frame-bytes"]));
    }
}

#[test]
fn msh_65_without_upgrades_such_a_head_is_passed_and_the_reader_stops_judging() {
    let input = cat(&[GET, UPGRADE, REFUSED[0].1, GET]);
    let (stop, pass, _) = read(&input, false);
    assert_eq!(stop, None);
    assert_eq!(pass, input);
}

#[test]
fn msh_65_content_length_with_chunked_passes_and_later_heads_are_not_judged() {
    let cl_te = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n";
    let input = cat(&[cl_te, UPGRADE, GET]);
    let (stop, pass, _) = read(&input, true);
    assert_eq!(stop, None);
    assert_eq!(pass, input);
}

#[test]
fn msh_65_a_bad_chunk_size_is_left_to_hyper() {
    let input = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nabc";
    let (stop, pass, _) = read(input, true);
    assert_eq!(stop, None);
    assert_eq!(pass, input);
}

#[test]
fn msh_65_leaves_looks_for_upgrade_fields_and_connect() {
    assert!(leaves(UPGRADE));
    assert!(leaves(CONNECT));
    assert!(leaves(b"GET / HTTP/1.1\r\nHost: a\r\nupgrade: h2c\r\n\r\n"));
    assert!(!leaves(GET));
    assert!(!leaves(POST));
    assert!(!leaves(b"GET /upgrade HTTP/1.1\r\nHost: upgrade\r\n\r\n"));
}

use super::*;

/// Feed client bytes then server bytes, once whole and once byte by byte; both must give the
/// same events (the framer is incremental).
fn run(client: &[u8], server: &[u8]) -> Vec<Event> {
    let whole = {
        let mut c = Codec::new();
        let mut ev = c.client_bytes(client);
        ev.extend(c.server_bytes(server));
        ev
    };
    let split = {
        let mut c = Codec::new();
        let mut ev = Vec::new();
        for b in client {
            ev.extend(c.client_bytes(&[*b]));
        }
        for b in server {
            ev.extend(c.server_bytes(&[*b]));
        }
        ev
    };
    assert_eq!(whole, split, "byte-by-byte feeding changes the events");
    whole
}

fn req(events: &[Event]) -> RequestHead {
    events
        .iter()
        .find_map(|e| match e {
            Event::Request(h) => Some(h.clone()),
            _ => None,
        })
        .expect("a request event")
}

fn unframed(events: &[Event]) -> Unframed {
    let found: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Unframed(u) => Some(u.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(found.len(), 1, "exactly one unframed event in {events:?}");
    found[0].clone()
}

fn request_ends(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::RequestEnd { body_bytes } => Some(*body_bytes),
            _ => None,
        })
        .collect()
}

fn response_ends(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ResponseEnd { body_bytes } => Some(*body_bytes),
            _ => None,
        })
        .collect()
}

fn statuses(events: &[Event]) -> Vec<u16> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Response(h) | Event::Interim(h) => Some(h.status),
            _ => None,
        })
        .collect()
}

// --- PRX-26: framing ---------------------------------------------------------------------

#[test]
fn prx_26_request_without_length_ends_after_head() {
    let ev = run(
        b"GET /a/b?x=1#f HTTP/1.1\r\nHost: Example.com:8080\r\n\r\nGET /next HTTP/1.1\r\n\r\n",
        b"",
    );
    let h = req(&ev);
    assert_eq!(h.method, "GET");
    assert_eq!(h.path, "/a/b");
    assert_eq!(h.host.as_deref(), Some("Example.com:8080"));
    assert_eq!(h.uri_authority, None);
    assert_eq!(h.version, Version::Http11);
    assert_eq!(request_ends(&ev), vec![0, 0]);
    assert_eq!(ev.len(), 4);
}

#[test]
fn prx_26_bare_lf_line_endings() {
    let ev = run(
        b"GET / HTTP/1.1\nHost: a\n\n",
        b"HTTP/1.1 200 OK\nContent-Length: 2\n\nok",
    );
    assert_eq!(request_ends(&ev), vec![0]);
    assert_eq!(response_ends(&ev), vec![2]);
}

#[test]
fn prx_26_request_content_length_body() {
    let ev = run(
        b"POST /p HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloGET /q HTTP/1.1\r\n\r\n",
        b"",
    );
    assert_eq!(request_ends(&ev), vec![5, 0]);
    let paths: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Request(h) => Some(h.path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(paths, ["/p", "/q"]);
}

#[test]
fn prx_26_request_chunked_with_extensions_and_trailers() {
    let body = b"POST /c HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n\
        5;name=\"v;x\"\r\nhello\r\nA ; ext\r\n0123456789\r\n0;last\r\nX-Sum: 1\r\nX-Other: 2\r\n\r\n\
        GET /n HTTP/1.1\r\n\r\n";
    let ev = run(body, b"");
    assert_eq!(request_ends(&ev), vec![15, 0]);
}

#[test]
fn prx_26_chunked_without_trailers_and_lf_endings() {
    let ev = run(
        b"POST / HTTP/1.1\ntransfer-encoding: chunked\n\n3\nabc\n0\n\n",
        b"",
    );
    assert_eq!(request_ends(&ev), vec![3]);
}

#[test]
fn prx_26_response_content_length_matches_in_order() {
    let ev = run(
        b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcHTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(response_ends(&ev), vec![3, 0]);
    let answered: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Response(h) => Some((h.status, h.request.as_ref().unwrap().path.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(answered, [(200, "/1".to_string()), (404, "/2".to_string())]);
}

#[test]
fn prx_26_response_chunked_wins_as_last_coding() {
    let ev = run(
        b"GET / HTTP/1.1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nwxyz\r\n0\r\n\r\n",
    );
    assert_eq!(response_ends(&ev), vec![4]);
}

#[test]
fn prx_26_response_other_last_coding_is_close_delimited() {
    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\nabc");
    assert_eq!(statuses(&ev), vec![200]);
    assert!(response_ends(&ev).is_empty());
    assert!(c.server_bytes(b"def").is_empty());
    assert_eq!(c.server_eof(), vec![Event::ResponseEnd { body_bytes: 6 }]);
}

#[test]
fn prx_26_response_without_length_is_close_delimited() {
    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.0\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.0 200 OK\r\n\r\nbody");
    assert!(response_ends(&ev).is_empty());
    assert_eq!(c.pending(), 0);
    assert_eq!(c.server_eof(), vec![Event::ResponseEnd { body_bytes: 4 }]);
    assert!(c.server_eof().is_empty());
}

#[test]
fn prx_26_eof_outside_a_close_delimited_body_is_silent() {
    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.1\r\n\r\n");
    c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc");
    assert!(c.server_eof().is_empty());
}

#[test]
fn prx_26_head_204_304_have_no_body_whatever_the_length() {
    let ev = run(
        b"HEAD /h HTTP/1.1\r\n\r\nGET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\n\r\nGET /c HTTP/1.1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n\
          HTTP/1.1 204 No Content\r\nContent-Length: 7\r\n\r\n\
          HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n\
          HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx",
    );
    assert_eq!(statuses(&ev), vec![200, 204, 304, 200]);
    assert_eq!(response_ends(&ev), vec![0, 0, 0, 1]);
}

#[test]
fn prx_26_interim_response_is_not_final_and_keeps_the_request_pending() {
    let mut c = Codec::new();
    c.client_bytes(b"POST /u HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n");
    assert_eq!(c.pending(), 1);
    let ev = c.server_bytes(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: <x>\r\n\r\n",
    );
    assert_eq!(ev.len(), 2);
    assert!(ev.iter().all(|e| matches!(e, Event::Interim(_))));
    assert_eq!(c.pending(), 1);
    let ev = c.server_bytes(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(statuses(&ev), vec![201]);
    match &ev[0] {
        Event::Response(h) => assert_eq!(h.request.as_ref().unwrap().path, "/u"),
        other => panic!("{other:?}"),
    }
    assert_eq!(c.pending(), 0);
}

#[test]
fn prx_26_response_without_pending_request_has_no_request() {
    let mut c = Codec::new();
    let ev = c.server_bytes(
        b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    match &ev[0] {
        Event::Response(h) => {
            assert_eq!(h.status, 408);
            assert!(h.request.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn prx_26_101_ends_the_http_part() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /ws HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n");
    let ev = c.server_bytes(
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n\x81\x00garbage",
    );
    assert_eq!(statuses(&ev), vec![101]);
    assert_eq!(ev.last(), Some(&Event::Tunnel));
    assert!(c.server_bytes(b"HTTP/1.1 200 OK\r\n\r\n").is_empty());
    assert!(c.client_bytes(b"\x81\x80anything").is_empty());
}

#[test]
fn prx_26_connect_2xx_ends_the_http_part_other_status_does_not() {
    let mut c = Codec::new();
    let ev = c.client_bytes(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
    let h = req(&ev);
    assert!(h.connect);
    assert_eq!(h.uri_authority.as_deref(), Some("example.com:443"));
    assert_eq!(h.path, "");
    let ev = c.server_bytes(b"HTTP/1.1 200 Connection Established\r\n\r\n\x16\x03\x01");
    assert_eq!(statuses(&ev), vec![200]);
    assert_eq!(ev.last(), Some(&Event::Tunnel));

    let mut c = Codec::new();
    c.client_bytes(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n");
    let ev =
        c.server_bytes(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 1\r\n\r\nx");
    assert!(!ev.contains(&Event::Tunnel));
    assert_eq!(response_ends(&ev), vec![1]);
}

#[test]
fn prx_26_client_bytes_behind_an_upgrade_request_wait_for_the_final_response() {
    let mut c = Codec::new();
    let ev = c.client_bytes(
        b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\nGET /next HTTP/1.1\r\n\r\n",
    );
    assert!(req(&ev).upgrade);
    assert_eq!(request_ends(&ev), vec![0]);
    assert_eq!(ev.len(), 2, "the second request is held back: {ev:?}");

    // Declined: the held bytes are framed as HTTP again.
    let ev = c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(statuses(&ev), vec![200]);
    let held: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, Event::Request(_)))
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(c.pending(), 1);

    // Accepted: the held bytes are tunnel bytes, not requests.
    let mut c = Codec::new();
    c.client_bytes(b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\nGET /x HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
    assert!(!ev.iter().any(|e| matches!(e, Event::Request(_))));
    assert_eq!(ev.last(), Some(&Event::Tunnel));
}

#[test]
fn prx_26_path_is_cut_at_1024_bytes_and_absolute_form_gives_authority() {
    let long = "a".repeat(3000);
    let ev = run(format!("GET /{long}?q HTTP/1.1\r\n\r\n").as_bytes(), b"");
    assert_eq!(req(&ev).path.len(), PATH_LIMIT);

    let ev = run(
        b"GET http://user:pw@Example.com:81/p/q?x=1 HTTP/1.1\r\nHost: other\r\n\r\n",
        b"",
    );
    let h = req(&ev);
    assert_eq!(h.uri_authority.as_deref(), Some("Example.com:81"));
    assert_eq!(h.host.as_deref(), Some("other"));
    assert_eq!(h.path, "/p/q");
}

#[test]
fn prx_26_origin_form_with_a_url_inside_is_not_absolute_form() {
    for (target, path) in [
        ("/cb?u=http://evil/x", "/cb"),
        ("/a://b", "/a://b"),
        ("/r#http://evil/x", "/r"),
        ("*", "*"),
    ] {
        let ev = run(
            format!("GET {target} HTTP/1.1\r\nHost: app\r\n\r\n").as_bytes(),
            b"",
        );
        let h = req(&ev);
        assert_eq!(h.uri_authority, None, "{target}");
        assert_eq!(h.path, path, "{target}");
    }
    // A scheme must be letters, digits, `+`, `-`, `.` and start with a letter.
    let ev = run(b"GET 1http://evil/x HTTP/1.1\r\n\r\n", b"");
    assert_eq!(req(&ev).uri_authority, None);
    let ev = run(b"GET HTTPS://Example.com HTTP/1.1\r\n\r\n", b"");
    let h = req(&ev);
    assert_eq!(h.uri_authority.as_deref(), Some("Example.com"));
    assert_eq!(h.path, "/");
}

#[test]
fn prx_26_http10_request_and_leading_blank_lines() {
    let ev = run(
        b"\r\n\r\nPOST /x HTTP/1.0\r\nContent-Length: 2\r\n\r\nhi",
        b"",
    );
    assert_eq!(req(&ev).version, Version::Http10);
    assert_eq!(request_ends(&ev), vec![2]);
}

#[test]
fn prx_26_response_may_precede_the_end_of_its_request() {
    let mut c = Codec::new();
    c.client_bytes(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc");
    let ev = c.server_bytes(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(statuses(&ev), vec![413]);
    let ev = c.client_bytes(b"defghij");
    assert_eq!(request_ends(&ev), vec![10]);
}

#[test]
fn prx_26_header_name_case_and_spacing_are_irrelevant() {
    let ev = run(
        b"POST / HTTP/1.1\r\nCONTENT-length:\t 4 \t\r\n\r\nabcd",
        b"",
    );
    assert_eq!(request_ends(&ev), vec![4]);
    let ev = run(
        b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 4, 4\r\n\r\nabcd",
        b"",
    );
    assert_eq!(request_ends(&ev), vec![4]);
}

#[test]
fn prx_26_obs_fold_in_a_non_framing_header_is_accepted() {
    let ev = run(
        b"GET / HTTP/1.1\r\nX-A: one\r\n two\r\nHost: h\r\n\r\n",
        b"",
    );
    assert_eq!(req(&ev).host.as_deref(), Some("h"));
    assert_eq!(request_ends(&ev), vec![0]);
}

// --- PRX-29: unframed --------------------------------------------------------------------

fn assert_request_unframed(input: &[u8], reason: Reason) {
    let ev = run(input, b"");
    let u = unframed(&ev);
    assert_eq!(u.side, Side::Client, "{ev:?}");
    assert_eq!(u.reason, reason, "{ev:?}");
    assert_eq!(ev.last(), Some(&Event::Unframed(u)));
    // Silent afterwards.
    let mut c = Codec::new();
    c.client_bytes(input);
    assert!(c.client_bytes(b"GET / HTTP/1.1\r\n\r\n").is_empty());
    assert!(
        c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .is_empty()
    );
}

#[test]
fn prx_29_both_content_length_and_transfer_encoding() {
    assert_request_unframed(
        b"POST /s HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n",
        Reason::ConflictingLength,
    );
    let ev = run(
        b"POST /s HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"",
    );
    let known = unframed(&ev).request.expect("method and path are known");
    assert_eq!(known.method, "POST");
    assert_eq!(known.path, "/s");
}

#[test]
fn prx_29_differing_or_non_decimal_content_length() {
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\n",
        Reason::BadContentLength,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: 4, 5\r\n\r\n",
        Reason::BadContentLength,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: +4\r\n\r\n",
        Reason::BadContentLength,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: 0x4\r\n\r\n",
        Reason::BadContentLength,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length:\r\n\r\n",
        Reason::BadContentLength,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n",
        Reason::BadContentLength,
    );
}

#[test]
fn prx_29_request_transfer_encoding_not_ending_in_chunked_or_with_http10() {
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
        Reason::BadTransferEncoding,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nTransfer-Encoding: identity\r\n\r\n",
        Reason::BadTransferEncoding,
    );
    assert_request_unframed(
        b"POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
        Reason::BadTransferEncoding,
    );
}

#[test]
fn prx_29_obs_fold_in_a_framing_header() {
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nContent-Length: 4\r\n 5\r\n\r\n",
        Reason::ObsFold,
    );
    assert_request_unframed(
        b"POST / HTTP/1.1\r\nTransfer-Encoding:\r\n chunked\r\n\r\n",
        Reason::ObsFold,
    );
    assert_request_unframed(b"POST / HTTP/1.1\r\n X: y\r\n\r\n", Reason::ObsFold);
}

#[test]
fn prx_29_bare_cr_is_decided_at_the_byte_after_it() {
    let mut c = Codec::new();
    assert!(c.client_bytes(b"GET / HTTP/1.1\r\nX: a\r").is_empty());
    let ev = c.client_bytes(b"b");
    assert_eq!(unframed(&ev).reason, Reason::BareCr);
    assert_request_unframed(b"GET / HTTP/1.1\rHost: h\r\n\r\n", Reason::BareCr);
}

#[test]
fn prx_29_header_line_without_colon_or_with_a_bad_name() {
    assert_request_unframed(
        b"GET / HTTP/1.1\r\nNoColonHere\r\n\r\n",
        Reason::BadHeaderLine,
    );
    assert_request_unframed(
        b"GET / HTTP/1.1\r\nBad Name: v\r\n\r\n",
        Reason::BadHeaderLine,
    );
    assert_request_unframed(b"GET / HTTP/1.1\r\nName : v\r\n\r\n", Reason::BadHeaderLine);
    assert_request_unframed(b"GET / HTTP/1.1\r\n: v\r\n\r\n", Reason::BadHeaderLine);
}

#[test]
fn prx_29_bad_start_line() {
    assert_request_unframed(b"GET /\r\n\r\n", Reason::BadStartLine);
    assert_request_unframed(b"GET / HTTP/2.0\r\n\r\n", Reason::BadStartLine);
    assert_request_unframed(b"GET  / HTTP/1.1\r\n\r\n", Reason::BadStartLine);
    assert_request_unframed(b"G(T / HTTP/1.1\r\n\r\n", Reason::BadStartLine);

    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.1 20 OK\r\n\r\n");
    let u = unframed(&ev);
    assert_eq!((u.side, u.reason), (Side::Server, Reason::BadStartLine));
    assert_eq!(u.request.unwrap().path, "/");
}

#[test]
fn prx_29_invalid_chunk_sizes() {
    for (input, reason) in [
        (
            &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"[..],
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n-1\r\n",
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0x5\r\n",
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n 5\r\n",
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n",
            Reason::BadChunkSize,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nabX",
            Reason::BadChunkTerminator,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\rx",
            Reason::BadChunkSize,
        ),
    ] {
        let ev = run(input, b"");
        let u = unframed(&ev);
        assert_eq!(u.reason, reason, "{}", String::from_utf8_lossy(input));
        assert_eq!(u.request.as_ref().map(|r| r.method.as_str()), Some("POST"));
    }
}

#[test]
fn prx_29_response_chunk_error_is_a_server_side_unframed() {
    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nQ\r\n");
    let u = unframed(&ev);
    assert_eq!((u.side, u.reason), (Side::Server, Reason::BadChunkSize));
}

#[test]
fn prx_29_response_with_both_lengths_is_unframed() {
    let mut c = Codec::new();
    c.client_bytes(b"GET / HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    let u = unframed(&ev);
    assert_eq!(
        (u.side, u.reason),
        (Side::Server, Reason::ConflictingLength)
    );
}

#[test]
fn prx_29_head_over_64_kib_is_decided_at_the_first_byte_over() {
    let prefix = b"GET / HTTP/1.1\r\nX: ";
    let mut c = Codec::new();
    assert!(c.client_bytes(prefix).is_empty());
    let fill = vec![b'a'; HEAD_LIMIT - prefix.len()];
    assert!(
        c.client_bytes(&fill).is_empty(),
        "exactly the limit is still fine"
    );
    let ev = c.client_bytes(b"a");
    assert_eq!(unframed(&ev).reason, Reason::HeadTooLarge);
    assert_eq!(unframed(&ev).request.unwrap().path, "/");
}

#[test]
fn prx_29_a_head_of_exactly_64_kib_is_framed() {
    let prefix = b"GET / HTTP/1.1\r\nX: ";
    let suffix = b"\r\n\r\n";
    let mut head = prefix.to_vec();
    head.extend(vec![b'a'; HEAD_LIMIT - prefix.len() - suffix.len()]);
    head.extend(suffix);
    assert_eq!(head.len(), HEAD_LIMIT);
    let ev = run(&head, b"");
    assert_eq!(request_ends(&ev), vec![0]);
}

#[test]
fn prx_29_head_of_300_lines() {
    let mut head = b"GET / HTTP/1.1\r\n".to_vec();
    for i in 0..299 {
        head.extend(format!("X-{i}: v\r\n").bytes());
    }
    head.extend(b"\r\n");
    let ev = run(&head, b"");
    assert_eq!(unframed(&ev).reason, Reason::TooManyHeaderLines);

    // 256 lines (start line included) are fine; the 257th decides at its first byte.
    let mut ok = b"GET / HTTP/1.1\r\n".to_vec();
    for i in 0..255 {
        ok.extend(format!("X-{i}: v\r\n").bytes());
    }
    let mut c = Codec::new();
    c.client_bytes(&ok);
    let ev = c.client_bytes(b"X");
    assert_eq!(unframed(&ev).reason, Reason::TooManyHeaderLines);
    let mut ok_end = ok.clone();
    ok_end.extend(b"\r\n");
    assert_eq!(request_ends(&run(&ok_end, b"")), vec![0]);
}

#[test]
fn prx_29_chunk_size_line_over_4_kib() {
    let mut c = Codec::new();
    c.client_bytes(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5;");
    let ext = vec![b'e'; CHUNK_LINE_LIMIT - 2];
    assert!(c.client_bytes(&ext).is_empty(), "4096 bytes is the limit");
    let ev = c.client_bytes(b"e");
    assert_eq!(unframed(&ev).reason, Reason::ChunkLineTooLong);
}

#[test]
fn prx_29_trailer_section_over_its_limits() {
    let start = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n";
    let mut c = Codec::new();
    c.client_bytes(start);
    c.client_bytes(b"X: ");
    let ev = c.client_bytes(&vec![b'a'; HEAD_LIMIT]);
    assert_eq!(unframed(&ev).reason, Reason::TrailersTooLarge);

    let mut many = start.to_vec();
    for i in 0..300 {
        many.extend(format!("T{i}: v\r\n").bytes());
    }
    many.extend(b"\r\n");
    let ev = run(&many, b"");
    assert_eq!(unframed(&ev).reason, Reason::TooManyTrailerLines);

    let mut bad = start.to_vec();
    bad.extend(b"no colon\r\n\r\n");
    assert_eq!(unframed(&run(&bad, b"")).reason, Reason::BadHeaderLine);
}

#[test]
fn prx_29_held_client_bytes_are_bounded() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n");
    assert!(c.client_bytes(&vec![b'x'; HEAD_LIMIT]).is_empty());
    let ev = c.client_bytes(b"x");
    assert_eq!(unframed(&ev).reason, Reason::HeldOverflow);
}

#[test]
fn prx_29_unframed_response_to_an_earlier_request_still_reports_the_pending_one() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /one HTTP/1.1\r\n\r\nGET /two HTTP/1.1\r\n\r\n");
    let ev = c.server_bytes(b"HTTP/1.1 200 OK\r\nbroken\r\n\r\n");
    let u = unframed(&ev);
    assert_eq!(u.request.unwrap().path, "/one");
}

// --- PRX-27 / PRX-30: pairing, read-ahead ------------------------------------------------

#[test]
fn prx_27_pipelined_responses_are_paired_in_order() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /a HTTP/1.1\r\n\r\nHEAD /b HTTP/1.1\r\n\r\nGET /c HTTP/1.1\r\n\r\n");
    assert_eq!(c.pending(), 3);
    let ev = c.server_bytes(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx\
          HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n\
          HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
    );
    let paths: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Response(h) => Some(h.request.as_ref().unwrap().path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(paths, vec!["/a", "/b", "/c"]);
    assert_eq!(response_ends(&ev), vec![1, 0, 0]);
    assert_eq!(c.pending(), 0);
}

#[test]
fn prx_30_read_ahead_stops_at_the_request_boundary_after_32() {
    let one = b"GET /x HTTP/1.1\r\n\r\n";
    let all: Vec<u8> = one.iter().copied().cycle().take(one.len() * 100).collect();
    let mut c = Codec::new();
    let (used, ev) = c.client_bytes_limited(&all);
    assert_eq!(used, one.len() * READ_AHEAD);
    assert_eq!(c.pending(), READ_AHEAD);
    assert_eq!(request_ends(&ev).len(), READ_AHEAD);
    // Nothing more while the 32 are unanswered.
    let (used, ev) = c.client_bytes_limited(&all[used..]);
    assert_eq!((used, ev.len()), (0, 0));
    // A response lets exactly one more through.
    c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let (used, _) = c.client_bytes_limited(&all[one.len() * READ_AHEAD..]);
    assert_eq!(used, one.len());
    assert_eq!(c.pending(), READ_AHEAD);
}

#[test]
fn prx_30_read_ahead_still_lets_the_body_of_the_last_request_through() {
    let mut c = Codec::new();
    let mut data = Vec::new();
    for _ in 0..READ_AHEAD - 1 {
        data.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
    }
    data.extend_from_slice(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello");
    data.extend_from_slice(b"GET /next HTTP/1.1\r\n\r\n");
    let (used, ev) = c.client_bytes_limited(&data);
    assert_eq!(used, data.len() - b"GET /next HTTP/1.1\r\n\r\n".len());
    assert_eq!(request_ends(&ev).last(), Some(&5));
}

#[test]
fn prx_27_held_bytes_are_parsed_again_when_the_upgrade_is_declined() {
    let mut c = Codec::new();
    c.client_bytes(
        b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\nGET /after HTTP/1.1\r\n\r\n",
    );
    assert!(c.retained_bytes() > 0);
    let ev = c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(c.pending(), 1);
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Request(r) if r.path == "/after"))
    );
}

#[test]
fn prx_27_limited_feed_stops_after_an_upgrade_request_and_loses_nothing() {
    let head = b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n";
    let after = b"GET /after HTTP/1.1\r\n\r\n";
    let mut data = head.to_vec();
    data.extend_from_slice(after);
    let mut c = Codec::new();
    let (used, _) = c.client_bytes_limited(&data);
    assert_eq!(used, head.len());
    assert_eq!(c.client_bytes_limited(after).0, 0);
    // Declined: the relay offers the bytes again.
    c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let (used, ev) = c.client_bytes_limited(after);
    assert_eq!(used, after.len());
    assert_eq!(req(&ev).path, "/after");
}

#[test]
fn prx_27_limited_feed_uses_every_byte_once_the_codec_is_opaque() {
    let mut c = Codec::new();
    c.client_bytes_limited(
        b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n",
    );
    c.server_bytes(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
    assert_eq!(c.client_bytes_limited(b"\x81\x80frame").0, 7);
}

#[test]
fn prx_27_unlimited_feed_of_10000_requests_stays_at_the_read_ahead() {
    for unit in [
        &b"GET / HTTP/1.1\r\n\r\n"[..],
        b"GET /\n\n",
        b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nab",
        b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        b"CONNECT a:1 HTTP/1.1\r\n\r\n",
    ] {
        let mut c = Codec::new();
        let ev = c.client_bytes(&unit.repeat(10_000));
        assert!(c.pending() <= READ_AHEAD, "{unit:?}: {}", c.pending());
        assert!(c.retained_bytes() <= RETAINED_LIMIT);
        assert!(ev.len() < 4 * READ_AHEAD, "{unit:?}: {} events", ev.len());
    }
}

#[test]
fn prx_27_unlimited_feed_beyond_the_read_ahead_is_unframed() {
    let mut c = Codec::new();
    let data = b"GET / HTTP/1.1\r\n\r\n".repeat(READ_AHEAD + 1);
    let ev = c.client_bytes(&data);
    assert_eq!(unframed(&ev).reason, Reason::ReadAheadOverflow);
}

#[test]
fn prx_27_retained_state_counts_what_is_buffered() {
    let mut c = Codec::new();
    assert_eq!(c.retained_bytes(), 0);
    c.client_bytes(b"GET / HTTP/1.1\r\nX: 12345678901234567890");
    assert!(c.retained_bytes() >= 30);
    c.client_bytes(b"\r\n\r\n");
    assert!(c.retained_bytes() >= 1, "the pending request counts");
}

#[test]
fn prx_27_host_and_authority_are_cut_at_1024_bytes() {
    let long = "h".repeat(5000);
    let ev = run(
        format!("GET http://{long}/ HTTP/1.1\r\nHost: {long}\r\n\r\n").as_bytes(),
        b"",
    );
    let r = req(&ev);
    assert_eq!(r.host.unwrap().len(), AUTHORITY_LIMIT);
    assert_eq!(r.uri_authority.unwrap().len(), AUTHORITY_LIMIT);
}

#[test]
fn idle_only_between_two_messages() {
    let mut c = Codec::new();
    assert!(c.idle());
    c.client_bytes(b"GET / HT");
    assert!(!c.idle(), "half a request line");
    c.client_bytes(b"TP/1.1\r\nHost: a\r\n\r\n");
    assert!(!c.idle(), "a request waits for its response");
    c.server_bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no");
    assert!(!c.idle(), "half a response body");
    assert_eq!(c.response_body_bytes(), 1);
    c.server_bytes(b"k");
    assert!(c.idle());
    c.client_bytes(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab");
    assert_eq!(c.request_body_bytes(), 2);
    let mut u = Codec::new();
    u.client_bytes(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n");
    assert!(!u.idle(), "opaque after unframed");
}

/// DSC-12: a capturing codec hands over the raw heads and the body payload without chunk
/// framing, bounded per message; a plain codec hands over nothing.
#[test]
fn dsc_12_capturing_codec_emits_heads_and_payload() {
    let request = b"POST /orders?dry_run=true HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
    let response = b"HTTP/1.1 201 Created\r\nContent-Length: 4\r\n\r\nbody";
    let mut c = Codec::with_capture();
    let mut events = c.client_bytes(request);
    events.extend(c.server_bytes(response));
    let captured: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Captured(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    let head = |b: &[u8]| b[..b.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4].to_vec();
    assert_eq!(
        captured,
        vec![
            Captured::RequestHead(head(request)),
            Captured::RequestBody(b"abcde".to_vec()),
            Captured::ResponseHead(head(response)),
            Captured::ResponseBody(b"body".to_vec()),
        ]
    );
    let mut plain = Codec::new();
    let mut events = plain.client_bytes(request);
    events.extend(plain.server_bytes(response));
    assert!(!events.iter().any(|e| matches!(e, Event::Captured(_))));
}

#[test]
fn dsc_12_capturing_codec_caps_the_payload_and_reports_the_size() {
    let total = CAPTURE_BODY_LIMIT + 1000;
    let mut request = format!("POST /x HTTP/1.1\r\nContent-Length: {total}\r\n\r\n").into_bytes();
    request.extend(vec![b'x'; total]);
    let mut c = Codec::with_capture();
    let events = c.client_bytes(&request);
    let payload: usize = events
        .iter()
        .filter_map(|e| match e {
            Event::Captured(Captured::RequestBody(b)) => Some(b.len()),
            _ => None,
        })
        .sum();
    assert_eq!(payload, CAPTURE_BODY_LIMIT);
    assert_eq!(request_ends(&events), vec![total as u64]);
}

#[test]
fn prx_29_transfer_encoding_parameters_are_unframed() {
    for te in [
        "chunked;x=1",
        "chunked ; x=1",
        "gzip;q=1, chunked",
        "chunked, gzip;q=1",
        "\"chunked\"",
    ] {
        let ev = run(
            format!("POST / HTTP/1.1\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n").as_bytes(),
            b"",
        );
        let u = unframed(&ev);
        assert_eq!(u.reason, Reason::BadTransferEncoding, "{te}");
        assert_eq!(u.side, Side::Client, "{te}");
        let ev = run(
            b"GET / HTTP/1.1\r\n\r\n",
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n").as_bytes(),
        );
        let u = unframed(&ev);
        assert_eq!(u.reason, Reason::BadTransferEncoding, "{te}");
        assert_eq!(u.side, Side::Server, "{te}");
    }
    // Plain codings, in any case and spacing, stay framed.
    let ev = run(
        b"POST / HTTP/1.1\r\nTransfer-Encoding:  GZip ,\tChunked \r\n\r\n0\r\n\r\n",
        b"",
    );
    assert!(!ev.iter().any(|e| matches!(e, Event::Unframed(_))));
}

#[test]
fn prx_29_held_overflow_names_the_upgrade_and_uses_every_byte() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n");
    assert!(c.client_bytes(&vec![b'x'; HEAD_LIMIT]).is_empty());
    let retained = c.retained_bytes();
    assert!(retained >= HEAD_LIMIT);
    let ev = c.client_bytes(b"xyz");
    let u = unframed(&ev);
    assert_eq!(u.reason, Reason::HeldOverflow);
    assert_eq!(u.request.expect("the upgrade request").path, "/ws");
    assert!(c.retained_bytes() < retained, "the held copy is dropped");
    // Opaque from now on: every byte is used.
    assert_eq!(c.client_bytes_limited(b"more").0, 4);
}

#[test]
fn prx_27_held_bytes_are_counted_by_capacity() {
    let mut c = Codec::new();
    c.client_bytes(b"GET /ws HTTP/1.1\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n");
    let base = c.retained_bytes();
    // Odd sizes make a growing vector over-allocate; the buffer never holds more than the limit.
    for n in [1, 3, 5000, 7000, 20_000, 30_000] {
        c.client_bytes(&vec![b'x'; n]);
        assert!(c.retained_bytes() - base <= HEAD_LIMIT, "{n}");
    }
}

#[test]
fn prx_27_the_retained_limit_is_the_sum_of_the_real_bounds() {
    // Two 64 KiB sections (one per direction), the requests read ahead and the two copies of
    // a head a body keeps: nothing else.
    assert!(RETAINED_LIMIT < 2 * HEAD_LIMIT + 40 * 4096, "{RETAINED_LIMIT}");
}

#[test]
fn prx_27_cut_values_do_not_keep_their_capacity() {
    let long = "h".repeat(20_000);
    let mut c = Codec::new();
    for _ in 0..READ_AHEAD {
        c.client_bytes(format!("GET http://{long}/{long} HTTP/1.1\r\nHost: {long}\r\n\r\n").as_bytes());
    }
    assert_eq!(c.pending(), READ_AHEAD);
    assert!(c.retained_bytes() <= READ_AHEAD * 4096, "{}", c.retained_bytes());
}

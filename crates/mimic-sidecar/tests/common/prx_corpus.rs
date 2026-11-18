//! The PRX-15 corpus: HTTP/1 conversations the relay must pass on byte for byte (also run
//! through the inbound side of a meshed sidecar without a contract, ADM-23).
#![allow(dead_code)]

/// A conversation: steps of (bytes the client sends, bytes the destination answers once it
/// has read everything sent so far), and the final statuses the codec must report.
pub struct Conversation {
    pub name: &'static str,
    pub steps: Vec<(Vec<u8>, Vec<u8>)>,
    pub statuses: Vec<u16>,
}

pub fn step(req: &str, resp: &str) -> (Vec<u8>, Vec<u8>) {
    (req.as_bytes().to_vec(), resp.as_bytes().to_vec())
}

pub fn corpus() -> Vec<Conversation> {
    let many_headers: String = (0..200).map(|i| format!("X-H{i}: v{i}\r\n")).collect();
    let pipelined_req: String = (0..5)
        .map(|i| format!("GET /p{i} HTTP/1.1\r\nHost: a\r\n\r\n"))
        .collect();
    let pipelined_resp: String = (0..5)
        .map(|i| format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{i}!"))
        .collect();
    vec![
        Conversation {
            name: "headers: case, repeats, order, 200 response lines",
            steps: vec![step(
                "GET /a?q=1 HTTP/1.1\r\nHOST: a\r\nx-B: 1\r\nX-a: 2\r\nx-b: 3\r\nX-A:4\r\n\r\n",
                &format!("HTTP/1.1 200 OK\r\nContent-length: 2\r\n{many_headers}\r\nok"),
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "obs-fold in a non-framing header",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\nX-Fold: one\r\n  two\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Fold: a\r\n\tb\r\n\r\n",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "bare LF line ends",
            steps: vec![step(
                "GET / HTTP/1.1\nHost: a\n\n",
                "HTTP/1.1 200 OK\nContent-Length: 1\n\nx",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "chunked with extensions and trailers",
            steps: vec![step(
                "POST /c HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n0\r\nX-Trail: 1\r\n\r\n",
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n4;a=b\r\nwxyz\r\n0\r\nX-Sum: 9\r\nx-LOW: 1\r\n\r\n",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "close-delimited response",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                "HTTP/1.1 200 OK\r\n\r\nuntil the destination closes",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HTTP/1.0 client with keep-alive",
            steps: vec![step(
                "GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HTTP/1.0 client without keep-alive",
            steps: vec![step(
                "GET / HTTP/1.0\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "Expect: 100-continue",
            steps: vec![
                step(
                    "POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n",
                    "HTTP/1.1 100 Continue\r\n\r\n",
                ),
                step("body", "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n"),
            ],
            statuses: vec![201],
        },
        Conversation {
            name: "103 before 200",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                "HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n!",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HEAD, 204, 304",
            steps: vec![
                step(
                    "HEAD / HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
                ),
                step(
                    "GET /n HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 204 No Content\r\nContent-Length: 7\r\n\r\n",
                ),
                step(
                    "GET /m HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 304 Not Modified\r\nContent-Length: 9\r\n\r\n",
                ),
            ],
            statuses: vec![200, 204, 304],
        },
        Conversation {
            name: "5 pipelined requests in one write",
            steps: vec![step(&pipelined_req, &pipelined_resp)],
            statuses: vec![200; 5],
        },
        Conversation {
            name: "response while no request is pending",
            steps: vec![
                step(
                    "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                ),
                step(
                    "",
                    "HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n",
                ),
            ],
            statuses: vec![200, 408],
        },
    ]
}

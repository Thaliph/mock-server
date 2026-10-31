//! PRX-08, PRX-09: access log lines and RED metrics of the proxy, in process; the configuration
//! of the authority budget on the real binary.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{capture_logs, closed_by_peer, counted, echo_server, fixture, fixture_budget};
use mimic_runtime::testing::Process;
use mimic_sidecar::listener::Direction;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::ServerName;

const TIMEOUT: Duration = Duration::from_secs(5);
const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

/// A destination that, on every connection, reads one request (head plus `body_len` bytes) and
/// writes the next canned response, for each response in turn.
async fn dest(responses: &[&'static [u8]], body_len: usize) -> SocketAddr {
    let responses = responses.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let responses = responses.clone();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                for response in responses {
                    loop {
                        if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n")
                            && pending.len() >= p + 4 + body_len
                        {
                            pending.drain(..p + 4 + body_len);
                            break;
                        }
                        let mut buf = [0u8; 4096];
                        match s.read(&mut buf).await {
                            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    s.write_all(response).await.unwrap();
                }
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink).await;
            });
        }
    });
    addr
}

async fn read_exact(c: &mut TcpStream, total: usize) -> Vec<u8> {
    let mut out = vec![0u8; total];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut out))
        .await
        .expect("read in time")
        .expect("read");
    out
}

async fn read_to_end(c: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut out))
        .await
        .expect("closed in time")
        .expect("read");
    out
}

fn number(line: &Value, field: &str) -> u64 {
    line[field]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} is a number in {line}"))
}

#[tokio::test]
async fn prx_08_http_exchange_line_has_the_documented_fields() {
    let logs = capture_logs();
    let response: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nCANARY";
    let addr = dest(&[response], 11).await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(
        b"POST /a/b?secret=CANARY HTTP/1.1\r\nHost: Svc.Example:80\r\nCookie: c=CANARY\r\nX-H: CANARY\r\nContent-Length: 11\r\n\r\nCANARYhello",
    )
    .await
    .unwrap();
    assert_eq!(read_exact(&mut c, response.len()).await, response);

    let lines = logs.access_exactly(1).await;
    let line = &lines[0];
    assert_eq!(line["message"], "http exchange");
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["direction"], "outbound");
    assert_eq!(line["protocol"], "http1");
    assert_eq!(line["dst"], addr.to_string());
    assert_eq!(line["authority"], "svc.example");
    assert_eq!(line["method"], "POST");
    assert_eq!(line["path"], "/a/b");
    assert_eq!(line["status"], 200);
    assert_eq!(line["result"], "ok");
    assert_eq!(line["upgrade"], false);
    assert_eq!(number(line, "bytes_in"), 11);
    assert_eq!(number(line, "bytes_out"), 6);
    assert!(number(line, "head_ms") <= number(line, "duration_ms"));
    for absent in ["grpc_status", "sni"] {
        assert!(line.get(absent).is_none(), "{absent} in {line}");
    }
    assert!(!logs.text().contains("CANARY"), "{}", logs.text());
}

#[tokio::test]
async fn prx_08_authority_falls_back_to_the_destination() {
    let logs = capture_logs();
    let addr = dest(&[b"HTTP/1.1 204 No Content\r\n\r\n"], 0).await;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    read_exact(&mut c, 27).await;
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["authority"], addr.to_string());
    assert_eq!(lines[0]["direction"], "inbound");
    assert_eq!(lines[0]["status"], 204);
}

#[tokio::test]
async fn prx_08_grpc_status_comes_from_the_trailers() {
    let logs = capture_logs();
    let response: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: grpc-status\r\n\r\n3\r\nyes\r\n0\r\ngrpc-status: 7\r\n\r\n";
    let addr = dest(&[response], 0).await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /svc/Method HTTP/1.1\r\nHost: g\r\nTE: trailers\r\n\r\n")
        .await
        .unwrap();
    read_exact(&mut c, response.len()).await;
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["grpc_status"], 7);
    assert_eq!(lines[0]["status"], 200);
    assert_eq!(lines[0]["result"], "ok");
}

#[tokio::test]
async fn prx_08_destination_closing_without_answer_is_upstream_reset() {
    let logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
    });
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /gone HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    closed_by_peer(c).await;
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["result"], "upstream_reset");
    assert_eq!(lines[0]["path"], "/gone");
    assert!(lines[0].get("status").is_none(), "{}", lines[0]);
    assert!(lines[0].get("head_ms").is_none(), "{}", lines[0]);
}

#[tokio::test]
async fn prx_08_client_leaving_during_the_response_is_client_abort() {
    let logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        let _ = s
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
            .await;
        for _ in 0..100 {
            if s.write_all(&[b'x'; 10]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /big HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n";
    read_exact(&mut c, head.len() + 10).await;
    drop(c);
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["result"], "client_abort");
    assert_eq!(lines[0]["status"], 200);
    assert!(number(&lines[0], "bytes_out") < 1000);
}

/// PRX-15 (access lines): a pipelined conversation of five exchanges on one connection, each
/// with the method, path, final status and body byte counts of the conversation.
#[tokio::test]
async fn prx_15_every_exchange_of_a_pipelined_conversation_has_its_line() {
    let logs = capture_logs();
    let many = {
        let mut r = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..200 {
            r.extend_from_slice(format!("X-N{i}: v\r\n").as_bytes());
        }
        r.extend_from_slice(b"Content-Length: 1\r\n\r\nz");
        r
    };
    let responses: Vec<Vec<u8>> = vec![
        b"HTTP/1.1 103 Early Hints\r\nLink: </s>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_vec(),
        b"HTTP/1.1 204 No Content\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nX-T: 1\r\n\r\n".to_vec(),
        many,
    ];
    let total: usize = responses.iter().map(Vec::len).sum();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let canned = responses.clone();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        for response in canned {
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0u8; 4096];
                match s.read(&mut buf).await {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => return,
                }
            }
            let end = seen.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            seen.drain(..end);
            s.write_all(&response).await.unwrap();
        }
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink).await;
    });
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(
        b"GET /a?x=1 HTTP/1.1\r\nHost: Svc.Example\r\n\r\nHEAD /h HTTP/1.1\r\nHost: svc.example\r\n\r\nGET /n HTTP/1.1\r\nHost: svc.example\r\n\r\nGET /c HTTP/1.1\r\nHost: svc.example\r\n\r\nGET /many HTTP/1.1\r\nHost: svc.example\r\n\r\n",
    )
    .await
    .unwrap();
    assert_eq!(read_exact(&mut c, total).await, responses.concat());

    let lines = logs.access_exactly(5).await;
    let expected = [
        ("/a", "GET", 200, 3),
        ("/h", "HEAD", 200, 0),
        ("/n", "GET", 204, 0),
        ("/c", "GET", 200, 5),
        ("/many", "GET", 200, 1),
    ];
    for (line, (path, method, status, bytes_out)) in lines.iter().zip(expected) {
        assert_eq!(line["path"], path, "{line}");
        assert_eq!(line["method"], method, "{line}");
        assert_eq!(line["status"], status, "{line}");
        assert_eq!(line["authority"], "svc.example", "{line}");
        assert_eq!(line["result"], "ok", "{line}");
        assert_eq!(number(line, "bytes_in"), 0, "{line}");
        assert_eq!(number(line, "bytes_out"), bytes_out, "{line}");
    }
}

/// PRX-15: `head_ms` runs to the final (non-`1xx`) head; a response with no request pending is
/// an exchange without `method` and `path`.
#[tokio::test]
async fn prx_15_head_ms_ends_at_the_final_head_and_an_unsolicited_response_has_no_method() {
    let logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let interim: &[u8] = b"HTTP/1.1 103 Early Hints\r\nLink: </s>\r\n\r\n";
    let ok: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let late: &[u8] = b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n";
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        s.write_all(interim).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.write_all(ok).await.unwrap();
        s.write_all(late).await.unwrap();
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink).await;
    });
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /slow HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    read_exact(&mut c, interim.len() + ok.len() + late.len()).await;
    let lines = logs.access_exactly(2).await;
    assert_eq!(lines[0]["path"], "/slow");
    assert_eq!(lines[0]["status"], 200);
    assert!(number(&lines[0], "head_ms") >= 250, "{}", lines[0]);
    assert!(number(&lines[0], "head_ms") <= number(&lines[0], "duration_ms"));
    assert_eq!(lines[1]["status"], 408);
    assert_eq!(lines[1]["authority"], addr.to_string());
    for absent in ["method", "path"] {
        assert!(lines[1].get(absent).is_none(), "{absent}: {}", lines[1]);
    }
}

/// PRX-21: an unframed exchange is counted with `status_class="none"`, not observed in the
/// histogram, and its line has no duration; the destination decides what the bytes mean.
#[tokio::test]
async fn prx_21_unframed_http1_has_no_duration_and_is_not_observed() {
    let logs = capture_logs();
    let addr = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let bad: &[u8] = b"POST /x?q=1 HTTP/1.1\r\nHost: U.example\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\nabc";
    c.write_all(bad).await.unwrap();
    assert_eq!(read_exact(&mut c, bad.len()).await, bad);

    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["message"], "http exchange");
    assert_eq!(lines[0]["protocol"], "http1");
    assert_eq!(lines[0]["result"], "unframed");
    assert_eq!(lines[0]["method"], "POST");
    assert_eq!(lines[0]["path"], "/x");
    for absent in ["status", "duration_ms", "head_ms"] {
        assert!(lines[0].get(absent).is_none(), "{absent}: {}", lines[0]);
    }
    counted(
        &f.metrics,
        "mimic_http_requests_total{direction=\"outbound\",protocol=\"http1\",authority=\"u.example\",status_class=\"none\"} 1",
    )
    .await;
    let text = f.metrics.encode();
    assert!(
        !text.lines().any(
            |l| l.starts_with("mimic_http_request_duration_seconds_count") && !l.ends_with(" 0")
        ),
        "{text}"
    );

    // The rest of the connection is a tunnel: well-formed HTTP on it produces no line.
    let next: &[u8] = b"GET /next HTTP/1.1\r\nHost: u.example\r\n\r\n";
    c.write_all(next).await.unwrap();
    assert_eq!(read_exact(&mut c, next.len()).await, next);
    logs.access_exactly(1).await;
}

/// A destination that answers `101` to the first request head, then echoes raw bytes.
async fn upgrading() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            if s.read(&mut b).await.unwrap_or(0) == 0 {
                return;
            }
            head.push(b[0]);
        }
        s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 1024];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    addr
}

#[tokio::test]
async fn prx_08_upgraded_exchange_is_marked() {
    let logs = capture_logs();
    let addr = upgrading().await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /ws HTTP/1.1\r\nHost: ws.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await
        .unwrap();
    let head =
        b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n";
    read_exact(&mut c, head.len()).await;
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["upgrade"], true);
    assert_eq!(lines[0]["status"], 101);
    assert_eq!(lines[0]["result"], "ok");
    assert_eq!(lines[0]["path"], "/ws");
}

#[tokio::test]
async fn prx_08_hostile_and_long_paths_stay_in_one_json_line() {
    let logs = capture_logs();
    let no_content: &[u8] = b"HTTP/1.1 204 No Content\r\n\r\n";
    let addr = dest(&[no_content, no_content], 0).await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let hostile = "/q\"b\\s%0D%0Aforged%22%5C";
    let long = format!("/{}", "a".repeat(3000));
    for path in [hostile.to_string(), long.clone()] {
        c.write_all(format!("GET {path} HTTP/1.1\r\nHost: a\r\n\r\n").as_bytes())
            .await
            .unwrap();
        read_exact(&mut c, 27).await;
    }
    let lines = logs.access_exactly(2).await;
    assert_eq!(lines[0]["path"], hostile);
    assert_eq!(lines[1]["path"], long[..1024]);
    // One JSON object per line, whatever the path held.
    assert_eq!(
        logs.text()
            .lines()
            .filter(|l| l.contains("http exchange"))
            .count(),
        2
    );
}

#[tokio::test]
async fn prx_08_opaque_connection_line() {
    let logs = capture_logs();
    let addr = echo_server().await;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"\x00\x01ping").await.unwrap();
    assert_eq!(read_exact(&mut c, 6).await, b"\x00\x01ping");
    drop(c);
    let lines = logs.access_exactly(1).await;
    let line = &lines[0];
    assert_eq!(line["message"], "connection");
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["direction"], "inbound");
    assert_eq!(line["protocol"], "tcp");
    assert_eq!(line["dst"], addr.to_string());
    assert_eq!(line["result"], "ok");
    assert_eq!(number(line, "bytes_in"), 6);
    assert_eq!(number(line, "bytes_out"), 6);
    number(line, "duration_ms");
    for absent in ["sni", "method", "path", "status", "authority"] {
        assert!(line.get(absent).is_none(), "{absent} in {line}");
    }
}

/// Sends a TLS ClientHello for `name` to the proxy; the destination reads it and closes.
async fn client_hello(f_addr: SocketAddr, name: &'static str) {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(tokio_rustls::rustls::RootCertStore::empty())
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(f_addr).await.unwrap();
    let _ = connector
        .connect(ServerName::try_from(name).unwrap(), tcp)
        .await;
}

async fn hello_reader() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let _ = s.shutdown().await;
    });
    addr
}

#[tokio::test]
async fn prx_08_tls_connection_line_has_the_sni() {
    let logs = capture_logs();
    let addr = hello_reader().await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    client_hello(f.addr, "Secure.Example").await;
    let lines = logs.access_exactly(1).await;
    let line = &lines[0];
    assert_eq!(line["message"], "connection");
    assert_eq!(line["protocol"], "tls");
    assert_eq!(line["sni"], "secure.example");
    assert_eq!(line["dst"], addr.to_string());
    assert!(number(line, "bytes_in") > 0);
    assert!(line.get("method").is_none());
}

#[tokio::test]
async fn prx_09_metrics_show_the_exact_values_of_known_exchanges() {
    // Outbound: two requests on one connection to a.example.
    let d1 = dest(
        &[
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc",
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ],
        0,
    )
    .await;
    let out = fixture(Direction::Outbound, move |_| Some(d1)).await;
    let mut c = TcpStream::connect(out.addr).await.unwrap();
    c.write_all(b"GET /x HTTP/1.1\r\nHost: a.example\r\n\r\n")
        .await
        .unwrap();
    read_exact(&mut c, 38 + 3).await;
    c.write_all(b"GET /y HTTP/1.1\r\nHost: A.EXAMPLE:80\r\n\r\n")
        .await
        .unwrap();
    read_exact(&mut c, 45).await;

    let series = |direction: &str, authority: &str, class: &str| {
        format!(
            "direction=\"{direction}\",protocol=\"http1\",authority=\"{authority}\",status_class=\"{class}\"}} 1"
        )
    };
    counted(
        &out.metrics,
        &format!(
            "mimic_http_requests_total{{{}",
            series("outbound", "a.example", "2xx")
        ),
    )
    .await;
    counted(
        &out.metrics,
        &format!(
            "mimic_http_requests_total{{{}",
            series("outbound", "a.example", "4xx")
        ),
    )
    .await;
    counted(
        &out.metrics,
        "mimic_http_request_duration_seconds_count{direction=\"outbound\",authority=\"a.example\",status_class=\"2xx\"} 1",
    )
    .await;
    counted(
        &out.metrics,
        "mimic_http_request_duration_seconds_count{direction=\"outbound\",authority=\"a.example\",status_class=\"4xx\"} 1",
    )
    .await;
    counted(
        &out.metrics,
        &format!(
            "mimic_proxy_connections_total{{direction=\"outbound\",protocol=\"http1\",authority=\"{d1}\"}} 1"
        ),
    )
    .await;
    counted(
        &out.metrics,
        "mimic_proxy_bytes_total{direction=\"outbound\",protocol=\"http1\",flow=\"to_client\"} 3",
    )
    .await;

    // Inbound: a POST with a 5-byte body answered 500.
    let d2 = dest(&[b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n"], 5).await;
    let inbound = fixture(Direction::Inbound, move |_| Some(d2)).await;
    let mut c = TcpStream::connect(inbound.addr).await.unwrap();
    c.write_all(b"POST / HTTP/1.1\r\nHost: b.example\r\nContent-Length: 5\r\n\r\nhello")
        .await
        .unwrap();
    read_exact(&mut c, 40).await;
    counted(
        &inbound.metrics,
        &format!(
            "mimic_http_requests_total{{{}",
            series("inbound", "b.example", "5xx")
        ),
    )
    .await;
    counted(
        &inbound.metrics,
        "mimic_proxy_bytes_total{direction=\"inbound\",protocol=\"http1\",flow=\"to_upstream\"} 5",
    )
    .await;

    // Opaque TCP: 6 bytes each way.
    let echo = echo_server().await;
    let tcp = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(tcp.addr).await.unwrap();
    c.write_all(b"\x00\x01ping").await.unwrap();
    read_exact(&mut c, 6).await;
    drop(c);
    counted(
        &tcp.metrics,
        &format!(
            "mimic_proxy_connections_total{{direction=\"outbound\",protocol=\"tcp\",authority=\"{echo}\"}} 1"
        ),
    )
    .await;
    counted(
        &tcp.metrics,
        "mimic_proxy_bytes_total{direction=\"outbound\",protocol=\"tcp\",flow=\"to_upstream\"} 6",
    )
    .await;
    counted(
        &tcp.metrics,
        "mimic_proxy_bytes_total{direction=\"outbound\",protocol=\"tcp\",flow=\"to_client\"} 6",
    )
    .await;

    // TLS is counted under its SNI.
    let hello = hello_reader().await;
    let tls = fixture(Direction::Outbound, move |_| Some(hello)).await;
    client_hello(tls.addr, "secure.example").await;
    counted(
        &tls.metrics,
        "mimic_proxy_connections_total{direction=\"outbound\",protocol=\"tls\",authority=\"secure.example\"} 1",
    )
    .await;
}

#[tokio::test]
async fn prx_09_authorities_over_the_budget_are_counted_as_other() {
    let addr = dest(&[b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"], 0).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = fixture_budget(listener, Direction::Outbound, 2, move |_| Some(addr)).await;
    for host in ["a.example", "b.example", "c.example", "a.example"] {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        read_exact(&mut c, 38).await;
    }
    for (authority, count) in [("a.example", 2), ("b.example", 1), ("(other)", 1)] {
        counted(
            &f.metrics,
            &format!(
                "mimic_http_requests_total{{direction=\"outbound\",protocol=\"http1\",authority=\"{authority}\",status_class=\"2xx\"}} {count}"
            ),
        )
        .await;
    }
    let text = f.metrics.encode();
    assert!(!text.contains("c.example"), "{text}");
}

#[test]
fn prx_09_invalid_budget_exits_2() {
    for value in ["0", "abc", "10001"] {
        let mut sidecar = Process::spawn(
            BIN,
            &[
                ("MIMIC_METRICS_MAX_AUTHORITIES", value),
                ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
                ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
                ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
            ],
        );
        let status = sidecar.wait_exit(Duration::from_secs(5));
        assert_eq!(status.code(), Some(2), "{value}");
    }
}

#[tokio::test]
async fn prx_08_client_leaving_before_any_response_is_client_abort() {
    let logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // The destination reads the request and never answers.
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink).await;
    });
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    // The body is cut short. (A plain close after a complete request may be a half-close: the
    // proxy then keeps waiting for the answer, as the destination would.)
    c.write_all(b"POST /slow HTTP/1.1\r\nHost: a.example\r\nContent-Length: 10\r\n\r\nabc")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    socket2::SockRef::from(&c)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(c);
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["result"], "client_abort");
    assert_eq!(lines[0]["path"], "/slow");
    assert_eq!(number(&lines[0], "bytes_in"), 3);
    assert!(lines[0].get("status").is_none(), "{}", lines[0]);
    counted(
        &f.metrics,
        "mimic_http_requests_total{direction=\"outbound\",protocol=\"http1\",authority=\"a.example\",status_class=\"none\"} 1",
    )
    .await;
}

#[test]
fn prx_09_valid_budget_starts() {
    let mut sidecar = Process::spawn(
        BIN,
        &[
            ("MIMIC_METRICS_MAX_AUTHORITIES", "10000"),
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ],
    );
    sidecar.admin_addr();
}

#[tokio::test]
async fn prx_23_real_hosts_named_other_and_invalid_are_not_the_sentinels() {
    let addr = dest(&[b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"], 0).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = fixture_budget(listener, Direction::Outbound, 2, move |_| Some(addr)).await;
    // "(bad)" is invalid and "c.example" overflows: neither takes a slot, `other` does.
    for host in ["(bad)", "other", "a.example", "c.example", "other"] {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        read_exact(&mut c, 38).await;
    }
    for (authority, count) in [
        ("(invalid)", 1),
        ("other", 2),
        ("a.example", 1),
        ("(other)", 1),
    ] {
        counted(
            &f.metrics,
            &format!(
                "mimic_http_requests_total{{direction=\"outbound\",protocol=\"http1\",authority=\"{authority}\",status_class=\"2xx\"}} {count}"
            ),
        )
        .await;
    }
}

#[tokio::test]

async fn prx_21_h2_rejected_has_no_duration_and_is_not_observed() {
    let logs = capture_logs();
    let addr = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    // SETTINGS, then HEADERS on the even stream id 2.
    c.write_all(
        b"\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x03\x01\x05\x00\x00\x00\x02\x82\x86\x84",
    )
    .await
    .unwrap();
    read_to_end(&mut c).await;

    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["result"], "rejected");
    for absent in ["duration_ms", "head_ms"] {
        assert!(lines[0].get(absent).is_none(), "{absent}: {}", lines[0]);
    }
    counted(
        &f.metrics,
        &format!(
            "mimic_http_requests_total{{direction=\"outbound\",protocol=\"h2\",authority=\"{addr}\",status_class=\"none\"}} 1"
        ),
    )
    .await;
    let text = f.metrics.encode();
    assert!(
        !text.lines().any(
            |l| l.starts_with("mimic_http_request_duration_seconds_count") && !l.ends_with(" 0")
        ),
        "{text}"
    );
}

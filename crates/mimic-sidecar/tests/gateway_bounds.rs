//! GW-19 on the gateway role of `mimic-sidecar` (spec §7.10): the `no-route` answer over HTTP/1.1
//! and HTTP/2, and the bounds of listeners that face untrusted clients.

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::capture_logs;
use common::gateway_rig::{
    accept_queue_empty, closed, closed_after, connect, free_port, gateway, get, read_head,
};
use common::h2_frames::{
    self, ENHANCE_YOUR_CALM, GOAWAY, HEADERS, REFUSED_STREAM, RST_STREAM, SETTINGS,
    SETTINGS_MAX_CONCURRENT_STREAMS, SETTINGS_MAX_HEADER_LIST_SIZE,
};
use common::{h2_client, h2_send, peak_rss};
use mimic_runtime::testing::Process;

const TIMEOUTS: [(&str, &str); 2] = [
    ("MIMIC_GATEWAY_HEADER_TIMEOUT", "1s"),
    ("MIMIC_GATEWAY_IDLE_TIMEOUT", "2s"),
];

fn bounded(ports: &[u16]) -> Process {
    gateway(ports, &TIMEOUTS).0
}

/// A `POST` announcing a 1 MiB body, with its first 1 KiB only.
fn partial_post(stream: &mut TcpStream) {
    write!(
        stream,
        "POST /upload HTTP/1.1\r\nHost: a\r\ncontent-length: 1048576\r\n\r\n"
    )
    .unwrap();
    // The server may close once it answered: the write error is not the test's concern.
    let _ = stream.write_all(&[b'x'; 1024]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gw_19_each_port_answers_404_no_route_over_http1_and_h2() {
    let _logs = capture_logs();
    let ports = [free_port(), free_port()];
    let _gateway = bounded(&ports);
    for port in ports {
        let (first, second, post) = tokio::task::spawn_blocking(move || {
            let mut keep_alive = connect(port);
            let first = get(&mut keep_alive, "/");
            let second = get(&mut keep_alive, "/orders/1?x=y");
            let mut upload = connect(port);
            let sent = Instant::now();
            partial_post(&mut upload);
            let post = read_head(&mut upload).expect("the POST is answered");
            (first, second, (post, sent.elapsed()))
        })
        .await
        .unwrap();
        first.assert_no_route(&[]);
        second.assert_no_route(&[]);
        let (post, elapsed) = post;
        assert_eq!(post.status, 404, "{post:?}");
        assert_eq!(post.field("x-mimic-error"), Some("no-route"), "{post:?}");
        assert_eq!(post.field("content-length"), Some("0"), "{post:?}");
        assert!(post.field("date").is_none(), "{post:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");

        let mut h2 = h2_client(common::gateway_rig::local(port)).await;
        for (method, body) in [("GET", Vec::new()), ("POST", vec![b'x'; 1 << 20])] {
            let answer = h2_send(&mut h2, method, "/", &[], body).await.unwrap();
            assert_eq!(answer.status, 404, "{method}: {answer:?}");
            assert_eq!(
                answer.headers,
                vec![
                    ("content-length".to_string(), "0".to_string()),
                    ("x-mimic-error".to_string(), "no-route".to_string()),
                ],
                "{method}"
            );
            assert!(answer.body.is_empty(), "{method}: {answer:?}");
        }
    }
}

#[test]
fn gw_19_a_slow_or_silent_head_is_closed_at_the_header_timeout() {
    let _logs = capture_logs();
    let port = free_port();
    let _gateway = bounded(&[port]);
    let slow = std::thread::spawn(move || {
        // Before the connect: the server's timer starts at its accept, which can come first.
        let start = Instant::now();
        let mut a = connect(port);
        let mut writer = a.try_clone().unwrap();
        std::thread::spawn(move || {
            for byte in b"GET / HTTP/1.1\r\n" {
                if writer.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        closed_after(&mut a, start)
    });
    let start = Instant::now();
    let mut b = connect(port);
    let silent = closed_after(&mut b, start);
    let slow = slow.join().unwrap();
    for (name, t) in [("byte by byte", slow), ("silent", silent)] {
        assert!(
            t >= Duration::from_secs(1) && t < Duration::from_secs(2),
            "{name}: closed after {t:?}"
        );
    }
}

#[test]
fn gw_19_an_idle_keep_alive_connection_is_closed_at_the_idle_timeout() {
    let _logs = capture_logs();
    let port = free_port();
    let _gateway = bounded(&[port]);
    let mut c = connect(port);
    let start = Instant::now();
    get(&mut c, "/").assert_no_route(&[]);
    std::thread::sleep(Duration::from_millis(1500).saturating_sub(start.elapsed()));
    c.set_nonblocking(true).unwrap();
    let mut probe = [0u8; 1];
    let peeked = c.peek(&mut probe).map_err(|e| e.kind());
    assert_eq!(peeked, Err(std::io::ErrorKind::WouldBlock), "closed early");
    c.set_nonblocking(false).unwrap();
    let t = closed_after(&mut c, start);
    assert!(
        t >= Duration::from_secs(2) && t < Duration::from_secs(3),
        "closed after {t:?}"
    );
}

#[test]
fn gw_19_a_head_over_64_kib_gets_431_or_a_close() {
    let _logs = capture_logs();
    let port = free_port();
    let _gateway = bounded(&[port]);
    let mut c = connect(port);
    let mut writer = c.try_clone().unwrap();
    let sender = std::thread::spawn(move || {
        let mut head = b"GET / HTTP/1.1\r\nHost: a\r\nx-big: ".to_vec();
        head.extend(std::iter::repeat_n(b'a', 65 * 1024));
        head.extend_from_slice(b"\r\n\r\n");
        let _ = writer.write_all(&head);
    });
    match read_head(&mut c) {
        Ok(head) => assert_eq!(head.status, 431, "{head:?}"),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ),
            "{e}"
        ),
    }
    sender.join().unwrap();
}

#[test]
fn gw_19_the_connection_over_the_cap_is_closed_at_accept() {
    let _logs = capture_logs();
    let ports = [free_port(), free_port()];
    let (_gateway, _) = gateway(
        &ports,
        &[
            ("MIMIC_MAX_CONNECTIONS", "4"),
            ("MIMIC_GATEWAY_HEADER_TIMEOUT", "10s"),
            ("MIMIC_GATEWAY_IDLE_TIMEOUT", "10s"),
        ],
    );
    let mut open: Vec<TcpStream> = ports
        .iter()
        .flat_map(|&p| [connect(p), connect(p)])
        .collect();
    for port in ports {
        accept_queue_empty(port);
    }
    let mut fifth = connect(ports[0]);
    fifth
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert!(closed(&mut fifth), "the fifth connection is still open");
    for c in &mut open {
        get(c, "/").assert_no_route(&[]);
    }
    drop(open.pop());
    // The slot comes back once the server has seen the close.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut c = connect(ports[1]);
        write!(c, "GET / HTTP/1.1\r\nHost: a\r\n\r\n").unwrap();
        match read_head(&mut c) {
            Ok(head) => {
                head.assert_no_route(&[]);
                break;
            }
            Err(e) => assert!(Instant::now() < deadline, "no slot came back: {e}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The flood of `HEADERS` and `RST_STREAM` pairs on a new connection; its `GOAWAY` code, or
/// `None` without one within 2 s.
fn reset_flood(port: u16) -> Option<u32> {
    let mut c = connect(port);
    c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut flood = h2_frames::client_start();
    for i in 0..1000u32 {
        let id = 2 * i + 1;
        flood.extend(h2_frames::open_stream(id));
        flood.extend(h2_frames::rst_stream(id, h2_frames::CANCEL));
    }
    let _ = c.write_all(&flood);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        match h2_frames::read_frame(&mut c) {
            Ok(frame) if frame.kind == GOAWAY => return Some(frame.goaway_code()),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn gw_19_h2_stream_limit_and_reset_flood() {
    let _logs = capture_logs();
    let port = free_port();
    let _gateway = bounded(&[port]);

    let mut c = connect(port);
    let mut streams = h2_frames::client_start();
    for i in 0..101u32 {
        streams.extend(h2_frames::open_stream(2 * i + 1));
    }
    c.write_all(&streams).unwrap();
    let settings = h2_frames::read_frame(&mut c).unwrap();
    assert_eq!(settings.kind, SETTINGS, "{settings:?}");
    let values = settings.settings();
    assert!(
        values.contains(&(SETTINGS_MAX_CONCURRENT_STREAMS, 100)),
        "{values:?}"
    );
    assert!(
        values.contains(&(SETTINGS_MAX_HEADER_LIST_SIZE, 65_536)),
        "{values:?}"
    );
    let mut refused = Vec::new();
    let mut answered = 0;
    let start = Instant::now();
    while answered < 100 || refused.is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{refused:?}, {answered} answered"
        );
        let frame = h2_frames::read_frame(&mut c).expect("the frames in time");
        match frame.kind {
            RST_STREAM if frame.rst_code() == REFUSED_STREAM => refused.push(frame.stream),
            HEADERS => answered += 1,
            _ => {}
        }
    }
    assert_eq!(refused, vec![201]);

    assert_eq!(reset_flood(port), Some(ENHANCE_YOUR_CALM));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gw_19_rss_stays_under_64_mib() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _logs = capture_logs();
    let port = free_port();
    let gateway = bounded(&[port]);
    let addr = common::gateway_rig::local(port);

    let mut uploads = Vec::new();
    for _ in 0..10 {
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(b"POST / HTTP/1.1\r\nHost: a\r\ncontent-length: 1048576\r\n\r\n")
            .await
            .unwrap();
        let body = vec![b'x'; 256 * 1024];
        let _ = tokio::time::timeout(Duration::from_secs(2), c.write_all(&body)).await;
        uploads.push(c);
    }
    let mut clients = Vec::new();
    for _ in 0..200 {
        clients.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    let answers = futures::future::join_all(clients.into_iter().map(|mut c| async move {
        c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut buf = [0u8; 512];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut buf).await.unwrap();
            assert!(n > 0, "closed before the answer");
            head.extend_from_slice(&buf[..n]);
        }
        head.starts_with(b"HTTP/1.1 404")
    }))
    .await;
    assert!(answers.iter().all(|ok| *ok));
    let flood = tokio::task::spawn_blocking(move || reset_flood(port))
        .await
        .unwrap();
    assert_eq!(flood, Some(ENHANCE_YOUR_CALM));
    drop(uploads);

    let peak = peak_rss(gateway.pid());
    assert!(peak < 64 << 20, "peak RSS {} MiB", peak >> 20);
}

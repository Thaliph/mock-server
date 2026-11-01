//! DSC-13: content decoding of the bodies discovery infers from. One test only, in its own
//! process: the resident set size it checks must not move with other tests.

mod common;

use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use common::fixture_discovery;
use flate2::Compression;
use flate2::write::{GzEncoder, ZlibEncoder};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const JSON: &[u8] = br#"{"name":"brotli","n":1}"#;
/// `JSON` compressed with `brotli`.
const BROTLI: [u8; 27] = [
    0x0f, 0x0b, 0x80, 0x7b, 0x22, 0x6e, 0x61, 0x6d, 0x65, 0x22, 0x3a, 0x22, 0x62, 0x72, 0x6f, 0x74,
    0x6c, 0x69, 0x22, 0x2c, 0x22, 0x6e, 0x22, 0x3a, 0x31, 0x7d, 0x03,
];

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn reply(encoding: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// An app answering by path with canned encoded bodies.
async fn app(bomb: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut json = JSON.to_vec();
    json.extend_from_slice(b"\n");
    let replies: Vec<(&'static str, Vec<u8>)> = vec![
        ("/gzip", reply("gzip", &gzip(br#"{"name":"gzip","n":1}"#))),
        (
            "/deflate",
            reply("deflate", &zlib(br#"{"name":"deflate","n":1}"#)),
        ),
        ("/br", reply("br", &BROTLI)),
        ("/zstd", reply("zstd", b"\x28\xb5\x2f\xfd\x00\x00")),
        ("/bomb", reply("gzip", &bomb)),
        ("/invalid", reply("gzip", &gzip(b"{\"name\":"))),
        ("/corrupt", reply("gzip", b"\x1f\x8b\x08\x00garbage")),
    ];
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let replies = replies.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => return,
                    }
                }
                let line = String::from_utf8_lossy(&buf);
                let path = line.split(' ').nth(1).unwrap_or_default();
                if let Some((_, out)) = replies.iter().find(|(p, _)| *p == path) {
                    let _ = s.write_all(out).await;
                }
            });
        }
    });
    addr
}

async fn call(addr: SocketAddr, path: &str) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), c.read_to_end(&mut out))
        .await
        .expect("answered")
        .unwrap();
    out
}

fn sum(text: &str, labels: &[&str]) -> u64 {
    text.lines()
        .filter(|l| {
            l.starts_with("mimic_discovery_bodies_total") && labels.iter().all(|x| l.contains(x))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

/// Resident set size of this process, in bytes.
fn rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmRSS");
    kb * 1024
}

async fn wait_results(metrics: &mimic_runtime::metrics::Metrics, total: u64) {
    for _ in 0..250 {
        if sum(&metrics.encode(), &["part=\"response\""]) >= total {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("bodies not counted:\n{}", metrics.encode());
}

#[tokio::test]
async fn dsc_13_bodies_are_decoded_within_the_limit() {
    let bomb = gzip(&vec![b' '; 4 * 1024 * 1024]);
    assert!(bomb.len() <= 8 * 1024, "the bomb is small: {}", bomb.len());
    let app = app(bomb).await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), None).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;

    for path in ["/gzip", "/deflate", "/br", "/zstd", "/invalid", "/corrupt"] {
        let answer = call(f.addr, path).await;
        assert!(answer.starts_with(b"HTTP/1.1 200"), "{path}");
    }
    wait_results(&f.metrics, 6).await;
    let text = f.metrics.encode();
    for (result, n) in [("parsed", 3), ("encoding", 1), ("invalid", 2)] {
        assert_eq!(
            sum(
                &text,
                &["part=\"response\"", &format!("result=\"{result}\"")]
            ),
            n,
            "{result}:\n{text}"
        );
    }
    let json = discovery.report().to_json();
    for name in ["gzip", "deflate", "brotli"] {
        assert!(
            json.contains(&format!("\"{name}\"")) || json.contains(name),
            "{name}: {json}"
        );
    }
    // The three decoded bodies gave the same schema: an object with `name` and `n`.
    assert!(json.contains("\"n\""), "{json}");

    // The bomb: 4 MiB once decoded, counted `too_large`, and the memory does not move.
    for _ in 0..5 {
        call(f.addr, "/bomb").await;
    }
    wait_results(&f.metrics, 11).await;
    let before = rss();
    for _ in 0..50 {
        let answer = call(f.addr, "/bomb").await;
        assert!(answer.starts_with(b"HTTP/1.1 200"));
    }
    wait_results(&f.metrics, 61).await;
    let after = rss();
    assert!(
        after < before + 1024 * 1024,
        "RSS grew from {before} to {after}"
    );
    let text = f.metrics.encode();
    assert_eq!(
        sum(&text, &["part=\"response\"", "result=\"too_large\""]),
        55,
        "{text}"
    );
}

//! EGR-15: the memory discovery holds stays bounded with both directions full. One test only, in
//! its own process: the peak resident set size it checks must not move with other tests.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{capture_logs, fixture_discovery};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BODY: usize = 256 * 1024;
const REPLY: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";

/// An app that drains each request and answers 200.
async fn app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(s));
        }
    });
    addr
}

async fn serve(mut s: TcpStream) {
    let mut pending = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let (head_end, length) = loop {
            if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&pending[..p]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0usize);
                break (p + 4, length);
            }
            match s.read(&mut chunk).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        };
        while pending.len() < head_end + length {
            match s.read(&mut chunk).await {
                Ok(n) if n > 0 => pending.extend_from_slice(&chunk[..n]),
                _ => return,
            }
        }
        pending.drain(..head_end + length);
        if s.write_all(REPLY).await.is_err() {
            return;
        }
    }
}

/// A 256 KiB JSON body, different for every `seq`.
fn body(seq: usize) -> Vec<u8> {
    let head = format!("{{\"seq\":{seq},\"blob\":\"");
    let tail = "\"}";
    let pad = BODY - head.len() - tail.len();
    let fill = b'a' + (seq % 26) as u8;
    let mut out = head.into_bytes();
    out.resize(out.len() + pad, fill);
    out.extend_from_slice(tail.as_bytes());
    assert_eq!(out.len(), BODY);
    out
}

async fn post(c: &mut TcpStream, host: &str, path: &str, seq: usize) {
    let body = body(seq);
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    c.write_all(&request).await.unwrap();
    let mut got = vec![0u8; REPLY.len()];
    tokio::time::timeout(Duration::from_secs(30), c.read_exact(&mut got))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(got, REPLY);
}

fn sum(text: &str, metric: &str, label: &str) -> u64 {
    text.lines()
        .filter(|l| l.starts_with(metric) && l.contains(label))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

/// Peak resident set size of this process, in bytes.
fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb * 1024
}

async fn recorded(metrics: &mimic_runtime::metrics::Metrics, label: &str, n: u64) {
    for _ in 0..1500 {
        if sum(&metrics.encode(), "mimic_discovery_exchanges_total", label) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} x {label} not reached:\n{}", metrics.encode());
}

/// The WARN lines of the discovery target with this message.
fn warnings(logs: &common::Logs, message: &str) -> Vec<serde_json::Value> {
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["target"] == "discovery" && v["message"] == message)
        .collect()
}

#[tokio::test]
async fn egr_15_both_directions_full_stay_under_the_memory_limit() {
    let logs = capture_logs();
    let app = app().await;
    let config = DiscoveryConfig {
        max_literals: 1000,
        ..DiscoveryConfig::default()
    };
    let budget = config.sample_budget as u64;
    let discovery = Discovery::start(&config, None).unwrap();
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let inbound = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut c = TcpStream::connect(out.addr).await.unwrap();
    let mut seq = 0;
    // 256 outbound endpoints over 64 authorities: 4 paths each, 30 exchanges each.
    for path in 0..4 {
        for authority in 0..64 {
            for _ in 0..30 {
                post(
                    &mut c,
                    &format!("a{authority}.example"),
                    &format!("/p{path}"),
                    seq,
                )
                .await;
                seq += 1;
            }
            recorded(
                &out.metrics,
                "direction=\"outbound\",result=\"recorded\"",
                seq as u64,
            )
            .await;
        }
    }
    let report = discovery.report();
    let held = |d| {
        report
            .endpoints()
            .iter()
            .filter(|e| e.direction == d)
            .count()
    };
    assert_eq!(
        held(mimic_core::discovery::report::Direction::Outbound),
        256
    );
    assert_eq!(report.authorities(), 64);
    // Only outbound exchanges so far: its samples hold at most half of the budget.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bytes = sum(&out.metrics.encode(), "mimic_discovery_sample_bytes", "");
    assert!(
        bytes <= budget / 2,
        "{bytes} outbound sample bytes over {}",
        budget / 2
    );
    // A 65th authority and a 257th outbound endpoint are refused, one WARN each.
    post(&mut c, "a64.example", "/p0", seq).await;
    post(&mut c, "a0.example", "/p4", seq + 1).await;
    recorded(
        &out.metrics,
        "direction=\"outbound\",result=\"authority_limit\"",
        1,
    )
    .await;
    recorded(
        &out.metrics,
        "direction=\"outbound\",result=\"endpoint_limit\"",
        1,
    )
    .await;
    // A new inbound endpoint is recorded while outbound is full.
    let mut i = TcpStream::connect(inbound.addr).await.unwrap();
    let mut inbound_seq = 0;
    for endpoint in 0..256 {
        for _ in 0..30 {
            post(
                &mut i,
                "svc.ns:8080",
                &format!("/svc/e{endpoint}"),
                inbound_seq,
            )
            .await;
            inbound_seq += 1;
        }
        recorded(
            &inbound.metrics,
            "direction=\"inbound\",result=\"recorded\"",
            inbound_seq as u64,
        )
        .await;
    }
    post(&mut i, "svc.ns:8080", "/svc/e256", inbound_seq).await;
    recorded(
        &inbound.metrics,
        "direction=\"inbound\",result=\"endpoint_limit\"",
        1,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = discovery.report();
    let held = |d| {
        report
            .endpoints()
            .iter()
            .filter(|e| e.direction == d)
            .count()
    };
    assert_eq!(held(mimic_core::discovery::report::Direction::Inbound), 256);
    assert_eq!(
        held(mimic_core::discovery::report::Direction::Outbound),
        256
    );
    let bytes = sum(&out.metrics.encode(), "mimic_discovery_sample_bytes", "");
    assert!(bytes <= budget, "{bytes} sample bytes over {budget}");
    logs.settled().await;
    let authority = warnings(&logs, "authority limit reached");
    assert_eq!(authority.len(), 1, "{}", logs.text());
    let limits = warnings(&logs, "endpoint limit reached");
    let directions: Vec<_> = limits
        .iter()
        .map(|l| l["direction"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(directions.len(), 2, "{directions:?}");
    assert!(directions.contains(&"outbound") && directions.contains(&"inbound"));
    let peak = peak_rss();
    eprintln!("egr-15 peak RSS: {} MiB", peak / (1024 * 1024));
    assert!(
        peak < 128 * 1024 * 1024,
        "peak RSS {} MiB over 128 MiB",
        peak / (1024 * 1024)
    );
}

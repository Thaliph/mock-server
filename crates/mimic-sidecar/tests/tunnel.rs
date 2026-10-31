//! INJ-07: the opaque TCP tunnel, in process. The original destination is injected because a
//! test cannot install the iptables redirection.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::metrics::Metrics;
use mimic_sidecar::tunnel::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Fixture {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
}

async fn fixture(
    direction: Direction,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let original_dst: OriginalDst =
        Arc::new(move |_| dst(addr).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)));
    let shared = Arc::new(Shared::new(vec![addr], tunnel_metrics, original_dst));
    spawn_listener(listener, direction, shared);
    Fixture {
        addr,
        metrics: Arc::new(metrics),
    }
}

async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

async fn counted(metrics: &Metrics, line: &str) {
    for _ in 0..100 {
        if metrics.encode().contains(line) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("missing {line}:\n{}", metrics.encode());
}

async fn closed_by_peer(mut c: TcpStream) {
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn inj_07_tunnels_bytes_both_ways_and_counts_ok() {
    let echo = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"ping");
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="outbound",result="ok"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn inj_07_unreachable_destination_counts_connect_error() {
    let dead = closed_port().await;
    let f = fixture(Direction::Inbound, move |_| Some(dead)).await;
    closed_by_peer(TcpStream::connect(f.addr).await.unwrap()).await;
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="inbound",result="connect_error"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn inj_07_missing_original_destination_is_closed_and_counted_as_loop() {
    let f = fixture(Direction::Outbound, |_| None).await;
    closed_by_peer(TcpStream::connect(f.addr).await.unwrap()).await;
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="outbound",result="loop"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn inj_07_destination_that_is_a_sidecar_listener_is_closed_and_counted_as_loop() {
    let f = fixture(Direction::Inbound, Some).await;
    closed_by_peer(TcpStream::connect(f.addr).await.unwrap()).await;
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="inbound",result="loop"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn inj_07_unspecified_listener_matches_any_local_address_on_its_port() {
    let port = 15006;
    let shared = Shared::new(
        vec![SocketAddr::from(([0, 0, 0, 0], port))],
        TunnelMetrics::register(&mut Metrics::new("sidecar", "test")),
        Arc::new(|_| Err(io::ErrorKind::NotFound.into())),
    );
    assert!(shared.is_listener(SocketAddr::from(([10, 1, 2, 3], port))));
    assert!(!shared.is_listener(SocketAddr::from(([10, 1, 2, 3], port + 1))));
}

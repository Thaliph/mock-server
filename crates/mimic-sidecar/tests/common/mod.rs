//! Helpers shared by the in-process tests of the connection layer.
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Fixture {
    pub addr: SocketAddr,
    pub metrics: Arc<Metrics>,
}

pub async fn fixture(
    direction: Direction,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture_with(listener, direction, dst).await
}

pub async fn fixture_with(
    listener: TcpListener,
    direction: Direction,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Fixture {
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

pub async fn echo_server() -> SocketAddr {
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

pub async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

pub async fn counted(metrics: &Metrics, line: &str) {
    for _ in 0..100 {
        if metrics.encode().contains(line) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("missing {line}:\n{}", metrics.encode());
}

pub async fn closed_by_peer(mut c: TcpStream) {
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0);
}

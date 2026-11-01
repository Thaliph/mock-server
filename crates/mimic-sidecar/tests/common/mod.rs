//! Helpers shared by the in-process tests of the connection layer.
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::red::ProxyMetrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Fixture {
    pub addr: SocketAddr,
    pub metrics: Arc<Metrics>,
    pub shared: Arc<Shared>,
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
    fixture_budget(listener, direction, 50, dst).await
}

/// Like [`fixture`], with `max_authorities` distinct `authority` labels per metric.
pub async fn fixture_budget(
    listener: TcpListener,
    direction: Direction,
    max_authorities: usize,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Fixture {
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let original_dst: OriginalDst =
        Arc::new(move |_| dst(addr).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)));
    let proxy = ProxyMetrics::register(&mut metrics, max_authorities);
    let shared =
        Arc::new(Shared::new(vec![addr], tunnel_metrics, original_dst).with_proxy_metrics(proxy));
    spawn_listener(listener, direction, shared.clone());
    Fixture {
        addr,
        shared,
        metrics: Arc::new(metrics),
    }
}

/// Like [`fixture`], with `configure` applied to the shared state before it is used.
pub async fn fixture_configured(
    direction: Direction,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
    configure: impl FnOnce(Shared) -> Shared,
) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let original_dst: OriginalDst =
        Arc::new(move |_| dst(addr).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)));
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let shared = Arc::new(configure(
        Shared::new(vec![addr], tunnel_metrics, original_dst).with_proxy_metrics(proxy),
    ));
    spawn_listener(listener, direction, shared.clone());
    Fixture {
        addr,
        shared,
        metrics: Arc::new(metrics),
    }
}

/// Like [`fixture`] with the exchanges handed to `discovery` (spec §6).
pub async fn fixture_discovery(
    direction: Direction,
    discovery: &mimic_sidecar::discovery::Discovery,
    dst: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Fixture {
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let original_dst: OriginalDst =
        Arc::new(move |_| dst(addr).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)));
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    discovery.register(&mut metrics);
    let shared = Arc::new(
        Shared::new(vec![addr], tunnel_metrics, original_dst)
            .with_proxy_metrics(proxy)
            .with_discovery(discovery.tap()),
    );
    spawn_listener(listener, direction, shared.clone());
    Fixture {
        addr,
        shared,
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

/// An address that refuses connections. The port stays bound (without `listen`) for the rest of
/// the test process: a dropped listener would free it for a parallel test to take.
pub async fn closed_port() -> SocketAddr {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    std::mem::forget(socket);
    addr
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

/// Captures this thread's logs as the sidecar writes them (JSON lines). The test runtime is
/// single-threaded, so every task of the test logs here.
pub struct Logs {
    buf: Arc<std::sync::Mutex<Vec<u8>>>,
    _handle: mimic_runtime::logging::LogHandle,
    _guard: tracing::dispatcher::DefaultGuard,
}

struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

impl io::Write for Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn capture_logs() -> Logs {
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (dispatch, handle) = mimic_runtime::logging::build("info", Sink(buf.clone()));
    Logs {
        buf,
        _handle: handle,
        _guard: tracing::dispatcher::set_default(&dispatch),
    }
}

impl Logs {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }

    /// The access lines so far, parsed.
    pub fn access(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("every line is JSON"))
            .filter(|v| v["target"] == "access")
            .collect()
    }

    /// Waits for `n` access lines, lets stragglers arrive, and checks there are exactly `n`.
    pub async fn access_exactly(&self, n: usize) -> Vec<serde_json::Value> {
        for _ in 0..100 {
            if self.access().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let lines = self.access();
        assert_eq!(lines.len(), n, "access lines:\n{}", self.text());
        lines
    }
}

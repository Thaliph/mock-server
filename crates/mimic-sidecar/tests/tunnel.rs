//! INJ-07 and PRX-10: the TCP tunnel counters, in process. The original destination is injected
//! because a test cannot install the iptables redirection.

mod common;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use common::{closed_by_peer, closed_port, counted, echo_server, fixture};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, Shared, TunnelMetrics};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
async fn prx_10_missing_original_destination_is_closed_and_counted_as_no_original_dst() {
    let f = fixture(Direction::Outbound, |_| None).await;
    closed_by_peer(TcpStream::connect(f.addr).await.unwrap()).await;
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="outbound",result="no_original_dst"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn prx_10_destination_that_is_a_sidecar_listener_is_closed_and_counted_as_loop() {
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

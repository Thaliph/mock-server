//! PRX-31: the connection cap and the stall of an incomplete protocol prefix.

mod common;

use std::time::Duration;

use common::{echo_server, fixture_configured};
use mimic_runtime::testing::run;
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SIDECAR: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

async fn echoes(c: &mut TcpStream) -> bool {
    c.write_all(b"ping\n").await.unwrap();
    let mut buf = [0u8; 5];
    matches!(
        tokio::time::timeout(Duration::from_millis(600), c.read_exact(&mut buf)).await,
        Ok(Ok(_))
    )
}

#[tokio::test]
async fn prx_31_connections_over_the_cap_wait_until_one_ends() {
    let echo = echo_server().await;
    let f = fixture_configured(
        Direction::Inbound,
        move |_| Some(echo),
        |shared| shared.with_max_connections(2),
    )
    .await;
    let mut first = TcpStream::connect(f.addr).await.unwrap();
    let mut second = TcpStream::connect(f.addr).await.unwrap();
    assert!(echoes(&mut first).await && echoes(&mut second).await);
    // Accepted by the kernel, not served.
    let mut third = TcpStream::connect(f.addr).await.unwrap();
    third.write_all(b"ping\n").await.unwrap();
    let mut buf = [0u8; 5];
    let early = tokio::time::timeout(Duration::from_millis(600), third.read_exact(&mut buf)).await;
    assert!(early.is_err(), "served over the cap");
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), third.read_exact(&mut buf))
        .await
        .expect("served once a slot is free")
        .unwrap();
    assert_eq!(&buf, b"ping\n");
}

#[tokio::test]
async fn prx_31_a_stalled_prefix_is_reset_and_a_silent_connection_is_not() {
    let echo = echo_server().await;
    let f = fixture_configured(
        Direction::Inbound,
        move |_| Some(echo),
        |shared| shared.with_detect_stall(Duration::from_millis(300)),
    )
    .await;
    let mut silent = TcpStream::connect(f.addr).await.unwrap();
    let mut stalled = TcpStream::connect(f.addr).await.unwrap();
    stalled.write_all(b"GE").await.unwrap();
    let mut buf = [0u8; 8];
    let end = tokio::time::timeout(Duration::from_secs(3), stalled.read(&mut buf))
        .await
        .expect("a stalled prefix is closed");
    assert!(matches!(end, Ok(0) | Err(_)), "{end:?}");
    // No byte at all is a server-first protocol or a slow starter, not a prefix.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(echoes(&mut silent).await);
}

#[test]
fn prx_31_invalid_max_connections_exits_2() {
    for value in ["0", "x", "1000001"] {
        let (status, _, _) = run(SIDECAR, &[], &[("MIMIC_MAX_CONNECTIONS", value)]);
        assert_eq!(status.code(), Some(2), "{value}");
    }
}

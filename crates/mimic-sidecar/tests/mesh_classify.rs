//! MSH-38: outbound connections are classified as `in-scope` or `unscoped`, and nothing else
//! changes: the bytes reach the original destination as without the mesh (spec §7.9).

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{echo_server, fixture_configured};
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::listener::Direction;
use mimic_sidecar::mesh::MeshState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A view whose one Service has the ClusterIP `127.0.0.1` and this port.
fn view_with(port: u16, mesh: bool) -> View {
    let services = if mesh {
        format!(
            r#""shop/echo":{{"endpoints":{{}},"generations":{{}},"ips":["127.0.0.1"],"policy":{{"loadBalancer":{{"slowStartSeconds":30}}}},"ports":{{"{port}":{{"targetPort":{port}}}}}}}"#
        )
    } else {
        String::new()
    };
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":{mesh},"mode":"none","services":{{{services}}}}}"#
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

/// Sends `payload` through a listener whose original destination is `dst`, and returns what
/// came back from the destination (an echo server).
async fn through(state: &MeshState, direction: Direction, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mesh = state.clone();
    let fixture = fixture_configured(direction, move |_| Some(dst), move |s| s.with_mesh(mesh)).await;
    let mut client = TcpStream::connect(fixture.addr).await.unwrap();
    client.write_all(payload).await.unwrap();
    let mut back = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut back))
        .await
        .expect("echoed in time")
        .unwrap();
    back
}

fn payload() -> Vec<u8> {
    // Not HTTP, not TLS: an opaque stream with every byte value, 100 KiB.
    (0..100 * 1024).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn msh_38_in_scope_and_unscoped_connections_reach_the_destination_unchanged() {
    let (scoped, other) = (echo_server().await, echo_server().await);
    let state = MeshState::default();

    // Before the first view every call is unscoped.
    assert_eq!(through(&state, Direction::Outbound, scoped, &payload()).await, payload());
    assert_eq!(state.unscoped_connections(), 1);

    state.hold(view_with(scoped.port(), true), "\"1-a\"");
    assert_eq!(through(&state, Direction::Outbound, scoped, &payload()).await, payload());
    assert_eq!(state.unscoped_connections(), 1, "a Service of the view");

    // A destination that is no Service port of the view, a pod IP for instance.
    assert_eq!(through(&state, Direction::Outbound, other, &payload()).await, payload());
    assert_eq!(state.unscoped_connections(), 2);
    assert_eq!(through(&state, Direction::Outbound, other, b"hello").await, b"hello");
    assert_eq!(state.unscoped_connections(), 3);
}

#[tokio::test]
async fn msh_38_inbound_connections_are_not_classified() {
    let other = echo_server().await;
    let state = MeshState::default();
    state.hold(view_with(1, true), "\"1-a\"");
    assert_eq!(through(&state, Direction::Inbound, other, b"hello").await, b"hello");
    assert_eq!(state.unscoped_connections(), 0);
}

#[tokio::test]
async fn msh_38_a_view_with_the_mesh_off_classifies_nothing() {
    let other = echo_server().await;
    let state = MeshState::default();
    state.hold(view_with(1, false), "\"1-a\"");
    assert_eq!(through(&state, Direction::Outbound, other, &payload()).await, payload());
    assert_eq!(state.unscoped_connections(), 0);
}

#[tokio::test]
async fn msh_38_without_the_mesh_state_the_relay_is_as_before() {
    let other = echo_server().await;
    let fixture = fixture_configured(Direction::Outbound, move |_| Some(other), |s| s).await;
    let mut client = TcpStream::connect(fixture.addr).await.unwrap();
    client.write_all(&payload()).await.unwrap();
    let mut back = vec![0u8; payload().len()];
    client.read_exact(&mut back).await.unwrap();
    assert_eq!(back, payload());
}

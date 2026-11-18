//! MSH-62 (with MCK-16): the mode applies per request on a mesh-routed connection, HTTP/1
//! keep-alive and `h2c`: a switch to `mock` makes the next request mocked, a switch back makes
//! the next one balanced again. The mode is flipped the way a pull does.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use common::faults::{no_series, status};
use common::mesh_pods::{H1Client, Pod, Svc, view_on, view_services};
use common::metric;
use common::mock_rig::{AUTHORITY, Options, Rig, rig_with, users_set};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_core::mesh::view::View;
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::mode::Announced;
use tokio::net::TcpStream;

const REQUEST: &str = "GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";

async fn switch(rig: &Rig, mode: Mode) {
    rig.mode
        .observe(Announced {
            mode: Ok(mode),
            factor: None,
        })
        .await;
}

/// A mesh-routed listener in `discovery`: the ClusterIP is `cluster`, the only pod `pod`.
async fn mesh_rig(
    cluster: SocketAddr,
    pod: SocketAddr,
    passthrough: &'static str,
) -> (Rig, MeshState) {
    mesh_rig_with(
        cluster,
        view_on(cluster, &[pod], 0, true),
        passthrough,
        Mode::Discovery,
    )
    .await
}

/// A listener holding `view`, started in `mode`: every connection's destination is `cluster`.
async fn mesh_rig_with(
    cluster: SocketAddr,
    view: View,
    passthrough: &'static str,
    mode: Mode,
) -> (Rig, MeshState) {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(1),
        ..MeshConfig::default()
    });
    state.hold(view, "\"1-a\"");
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            mode: Some(mode),
            dst: Some(cluster),
            mesh: Some(state.clone()),
            passthrough,
            ..Options::default()
        },
    )
    .await;
    (rig, state)
}

fn matched(head: &str) -> bool {
    head.to_ascii_lowercase().contains("x-mimic-match")
}

fn mocked_requests(rig: &Rig) -> u64 {
    ["exact", "endpoint", "fallback", "miss"]
        .iter()
        .map(|tier| rig.counter(&format!("mimic_mock_requests_total{{result=\"{tier}\"}}")))
        .sum()
}

#[tokio::test]
async fn msh_62_an_http1_mesh_routed_connection_follows_the_mode_per_request() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let (rig, _state) = mesh_rig(cluster.addr, pod.addr, "").await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, body) = client.read().await.unwrap();
    assert!(
        !matched(&head) && body == b"pod",
        "balanced to the pod: {head}"
    );

    switch(&rig, Mode::Mock).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && matched(&head),
        "mocked: {head}"
    );
    assert_eq!(mocked_requests(&rig), 1, "the mock counted it");
    assert_eq!(pod.counts.requests(), 1, "the pod did not see it");
    assert_eq!(cluster.counts.requests(), 0, "nor the ClusterIP");

    switch(&rig, Mode::Discovery).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, body) = client.read().await.unwrap();
    assert!(!matched(&head) && body == b"pod", "balanced again: {head}");
    assert_eq!(pod.counts.requests(), 2);
    assert_eq!(pod.counts.accepted(), 1, "one connection to the pod");
    assert_eq!(mocked_requests(&rig), 1);
}

#[tokio::test]
async fn msh_62_an_h2c_mesh_routed_connection_follows_the_mode_per_request() {
    let (pod, cluster) = (
        Pod::h2c("pod", 0, None).await,
        Pod::h2c("cluster", 0, None).await,
    );
    let (rig, _state) = mesh_rig(cluster.addr, pod.addr, "").await;
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    let get = async |sender: &mut hyper::client::conn::http2::SendRequest<Full<Bytes>>| {
        let request = Request::builder()
            .uri(format!("http://{AUTHORITY}/users/3"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), sender.send_request(request))
            .await
            .expect("answered in time")
            .expect("an answer");
        let mocked = response.headers().contains_key("x-mimic-match");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (mocked, body)
    };
    assert_eq!(get(&mut sender).await, (false, Bytes::from("pod")));
    switch(&rig, Mode::Mock).await;
    assert!(get(&mut sender).await.0, "mocked");
    assert_eq!(mocked_requests(&rig), 1, "the mock counted it");
    assert_eq!(pod.counts.requests(), 1, "the pod did not see it");
    assert_eq!(cluster.counts.requests(), 0, "nor the ClusterIP");
    switch(&rig, Mode::Discovery).await;
    assert_eq!(get(&mut sender).await, (false, Bytes::from("pod")));
    assert_eq!(pod.counts.requests(), 2);
    assert_eq!(pod.counts.accepted(), 1, "one connection to the pod");
}

#[tokio::test]
async fn msh_62_a_passthrough_destination_is_never_mocked_on_a_mesh_routed_connection() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let (rig, _state) = mesh_rig(cluster.addr, pod.addr, "users.test").await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    client.read().await.unwrap();
    switch(&rig, Mode::Mock).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, body) = client.read().await.unwrap();
    assert!(!matched(&head) && body == b"pod", "still balanced: {head}");
    assert_eq!(pod.counts.requests(), 2);
}

#[tokio::test]
async fn msh_62_a_request_after_the_view_turned_the_mesh_off_goes_to_the_cluster_ip() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let (rig, state) = mesh_rig(cluster.addr, pod.addr, "").await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    assert_eq!(client.read().await.unwrap().1, b"pod");
    state.hold(
        common::mesh_pods::view_on(cluster.addr, &[pod.addr], 0, false),
        "\"1-b\"",
    );
    client.send(REQUEST.as_bytes()).await;
    assert_eq!(client.read().await.unwrap().1, b"cluster");
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn msh_65_in_mock_an_upgrade_request_on_a_mesh_routed_connection_is_a_mock_miss() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let (rig, _state) = mesh_rig(cluster.addr, pod.addr, "").await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, body) = client.read().await.unwrap();
    assert!(!matched(&head) && body == b"pod", "balanced: {head}");
    switch(&rig, Mode::Mock).await;
    client
        .send(b"GET /ws HTTP/1.1\r\nhost: users.test:8080\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.unwrap();
    assert!(head.starts_with("HTTP/1.1 599"), "a mock miss: {head}");
    assert_eq!(pod.counts.requests(), 1, "the pod did not see it");
    assert_eq!(cluster.counts.requests(), 0, "nor the ClusterIP");
    // Back in discovery, the reader no longer judges this connection: such a request is not
    // served (spec §4.1 limit 8).
    switch(&rig, Mode::Discovery).await;
    client
        .send(b"GET /ws HTTP/1.1\r\nhost: users.test:8080\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.unwrap();
    assert!(head.starts_with("HTTP/1.1 501"), "{head}");
    assert!(
        head.to_lowercase().contains("x-mimic-error: unsupported"),
        "{head}"
    );
}

/// `abort` 100 % on every request.
const ALL: &str = r#"[{"abort":{"percent":100,"status":503}}]"#;

fn aborted(head: &str) -> bool {
    status(head) == 503
        && head
            .to_ascii_lowercase()
            .contains("x-mimic-error: fault-injected")
}

fn aborts(rig: &Rig) -> f64 {
    metric(
        &rig.metrics.encode(),
        "mimic_mesh_faults_total{",
        &["kind=\"abort\""],
    )
}

/// `shop/cart` on the ClusterIP `cluster`, its pod `pod`, aborting every request.
fn faulted(cluster: SocketAddr, pod: SocketAddr) -> View {
    view_services(&[Svc::new("shop/cart", "127.0.0.1", cluster.port(), &[pod]).faults(ALL)])
}

#[tokio::test]
async fn flt_23_in_mode_mock_the_mock_answers_and_no_fault_is_counted() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let view = || faulted(cluster.addr, pod.addr);
    // Accepted in `mock`.
    let (rig, _state) = mesh_rig_with(cluster.addr, view(), "", Mode::Mock).await;
    let mut client = H1Client::connect(rig.addr).await;
    for _ in 0..3 {
        client.send(REQUEST.as_bytes()).await;
        let (head, _) = client.read().await.unwrap();
        assert!(
            head.starts_with("HTTP/1.1 200") && matched(&head),
            "mocked: {head}"
        );
    }
    let scrape = rig.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_faults_total{"), "{scrape}");
    assert_eq!(pod.counts.requests(), 0);
    assert_eq!(cluster.counts.requests(), 0);

    // A mesh-routed connection switched to `mock`.
    let (rig, _state) = mesh_rig_with(cluster.addr, view(), "", Mode::Discovery).await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(aborted(&head), "{head}");
    switch(&rig, Mode::Mock).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && matched(&head),
        "mocked: {head}"
    );
    assert_eq!(aborts(&rig), 1.0);
    assert_eq!(pod.counts.requests() + cluster.counts.requests(), 0);
}

#[tokio::test]
async fn flt_23_a_passthrough_destination_on_a_mesh_routed_connection_is_aborted_in_mode_mock() {
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let view = faulted(cluster.addr, pod.addr);
    let (rig, _state) = mesh_rig_with(cluster.addr, view, "users.test", Mode::Discovery).await;
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(aborted(&head), "{head}");
    switch(&rig, Mode::Mock).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, _) = client.read().await.unwrap();
    assert!(aborted(&head) && !matched(&head), "not mocked: {head}");
    assert_eq!(mocked_requests(&rig), 0);
    assert_eq!(aborts(&rig), 2.0);
    // A connection accepted in `mock` takes the byte relay (spec §11.1).
    let mut client = H1Client::connect(rig.addr).await;
    client.send(REQUEST.as_bytes()).await;
    let (head, body) = client.read().await.unwrap();
    assert!(
        head.starts_with("HTTP/1.1 200") && body == b"cluster",
        "the ClusterIP: {head}"
    );
    assert_eq!(aborts(&rig), 2.0);
    assert_eq!(pod.counts.requests(), 0);
}

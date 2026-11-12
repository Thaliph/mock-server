//! MSH-62 (with MCK-16): the mode applies per request on a mesh-routed connection, HTTP/1
//! keep-alive and `h2c`: a switch to `mock` makes the next request mocked, a switch back makes
//! the next one balanced again. The mode is flipped the way a pull does.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use common::mesh_pods::{H1Client, Pod, view_on};
use common::mock_rig::{AUTHORITY, Options, Rig, rig_with, users_set};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
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
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(1),
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster, &[pod], 0, true), "\"1-a\"");
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            mode: Some(Mode::Discovery),
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

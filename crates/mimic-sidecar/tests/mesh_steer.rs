//! MSH-68 and MSH-69: a short connect timeout, steering away from a pod that failed to
//! connect, a failed reconnect falling back to the original destination, and the HTTP/2
//! `CONNECT` answer being counted and logged (spec §7.9 "Load balancing").

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::mesh_pods::{Blackhole, H1Client, Pod, cluster_ip, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::net::TcpStream;

fn state_for(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(21),
        connect_timeout: Duration::from_millis(300),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

#[tokio::test]
async fn msh_68_a_blackholed_pod_costs_one_connect_timeout_and_is_then_avoided() {
    let _logs = capture_logs();
    let hole = Blackhole::new().await;
    let live = Pod::http1("live", 0).await;
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, &[hole.addr, live.addr], |c| {
        c.avoid_failed_for = Duration::from_secs(30);
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let began = Instant::now();
    for _ in 0..30 {
        assert_eq!(client.get("/").await.1, "live");
    }
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "30 requests took {:?}",
        began.elapsed()
    );
    assert_eq!(cluster.counts.requests(), 0, "no fallback was needed");
}

#[tokio::test]
async fn msh_68_a_service_with_only_a_blackholed_pod_is_answered_by_the_original_destination() {
    let _logs = capture_logs();
    let hole = Blackhole::new().await;
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, &[hole.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let began = Instant::now();
    let (head, body) = H1Client::connect(f.addr).await.get("/").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && body == "cluster",
        "{head}"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(250) && began.elapsed() < Duration::from_secs(3),
        "took {:?}",
        began.elapsed()
    );
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        1.0
    );
}

#[tokio::test]
async fn msh_68_a_pod_is_avoided_for_the_window_and_chosen_again_after_it() {
    let _logs = capture_logs();
    let hole = Blackhole::new().await;
    let live = Pod::http1("live", 0).await;
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, &[hole.addr, live.addr], |c| {
        c.avoid_failed_for = Duration::from_millis(600);
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        assert_eq!(client.get("/").await.1, "live");
    }
    let revived = hole.revive_http("revived");
    tokio::time::sleep(Duration::from_millis(700)).await;
    for _ in 0..20 {
        client.get("/").await;
    }
    assert!(
        revived.requests() > 0,
        "the pod is chosen again once the window is over"
    );
}

#[tokio::test]
async fn msh_68_a_successful_connect_ends_the_avoidance_at_once() {
    let _logs = capture_logs();
    let (first, second) = (Blackhole::new().await, Blackhole::new().await);
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, &[first.addr, second.addr], |c| {
        c.avoid_failed_for = Duration::from_secs(60);
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // Both pods time out, the original destination answers; both are now avoided.
    assert_eq!(client.get("/").await.1, "cluster");
    let revived = first.revive_http("first");
    for _ in 0..10 {
        client.get("/").await;
    }
    // The first success cleared its avoidance: the other pod, still avoided, gets none.
    assert!(revived.requests() >= 9, "got {}", revived.requests());
}

#[tokio::test]
async fn msh_69_an_unwritten_request_whose_new_connection_fails_goes_to_the_original_destination() {
    let _logs = capture_logs();
    let (pod, cluster) = (Pod::http1("pod", 0).await, Pod::http1("cluster", 0).await);
    let state = state_for(cluster.addr, &[pod.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    assert_eq!(client.get("/one").await.1, "pod");
    state.poison_next_checkout();
    state.refuse_next_reconnect();
    let (head, body) = client.get("/two").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && body == "cluster",
        "{head}"
    );
    let metrics = f.metrics.encode();
    assert_eq!(
        counter(&metrics, "mimic_mesh_reconnects_total"),
        1.0,
        "{metrics}"
    );
    assert_eq!(
        counter(&metrics, "mimic_mesh_fallbacks_total"),
        1.0,
        "{metrics}"
    );
    assert_eq!(cluster.counts.requests(), 1);
}

#[tokio::test]
async fn msh_69_with_the_original_destination_refusing_too_the_answer_is_connect_failed() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let cluster = closed_port().await;
    let state = state_for(cluster, &[pod.addr], |_| {});
    let f = fixture_mesh(cluster, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    state.poison_next_checkout();
    state.refuse_next_reconnect();
    let (head, _) = client.get("/two").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        head.to_lowercase()
            .contains("x-mimic-error: connect-failed")
    );
}

#[tokio::test]
async fn msh_69_an_h2_stream_whose_new_connection_fails_goes_to_the_original_destination() {
    let _logs = capture_logs();
    let (pod, cluster) = (
        Pod::h2c("pod", 0, None).await,
        Pod::h2c("cluster", 0, None).await,
    );
    let state = state_for(cluster.addr, &[pod.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    assert_eq!(
        common::mesh_pods::h2c_calls(f.addr, 1, 1).await,
        vec!["pod"]
    );
    state.poison_next_checkout();
    state.refuse_next_reconnect();
    assert_eq!(
        common::mesh_pods::h2c_calls(f.addr, 1, 1).await,
        vec!["cluster"]
    );
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        1.0
    );
}

#[tokio::test]
async fn msh_69_an_h2_connect_is_counted_and_logged() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let logs = capture_logs();
    let pod = Pod::h2c("a", 0, None).await;
    let state = state_for(cluster_ip(), &[pod.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = hyper::Request::connect("example.com:443")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = sender.send_request(request).await.expect("an answer");
    assert_eq!(response.status(), 501);
    let metrics = f.metrics.encode();
    let line = "mimic_mesh_requests_total{service=\"shop/cart\",route=\"none\",backend=\"shop/cart:80\",status_class=\"5xx\",reason=\"unsupported\"}";
    assert_eq!(counter(&metrics, line), 1.0, "{metrics}");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["status"], 501, "{}", lines[0]);
}

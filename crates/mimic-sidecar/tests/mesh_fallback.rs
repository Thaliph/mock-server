//! MSH-61: a pod that cannot be connected, and a stale view, fall back to the original
//! destination (the ClusterIP) instead of an error (spec §7.9).

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::mesh_pods::{H1Client, Pod, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};

fn state_for(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(9),
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
async fn msh_61_the_only_endpoint_refusing_falls_back_to_the_cluster_ip() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, &[closed_port().await], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..3 {
        let (head, body) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "cluster");
    }
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        3.0,
        "fallbacks"
    );
    let lines = logs.access_exactly(3).await;
    assert_eq!(
        lines[0]["upstream"],
        cluster.addr.to_string(),
        "the access line"
    );
}

#[tokio::test]
async fn msh_61_one_refusing_endpoint_of_two_is_skipped_without_a_fallback() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let live = Pod::http1("live", 0).await;
    let state = state_for(cluster.addr, &[closed_port().await, live.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..30 {
        let (_, body) = client.get("/").await;
        assert_eq!(body, "live");
    }
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        0.0,
        "fallbacks"
    );
    assert_eq!(cluster.counts.requests(), 0, "the ClusterIP was not used");
}

#[tokio::test]
async fn msh_61_the_cluster_ip_refusing_too_gives_connect_failed() {
    let _logs = capture_logs();
    let (cluster, pod) = (closed_port().await, closed_port().await);
    let state = state_for(cluster, &[pod], |_| {});
    let f = fixture_mesh(cluster, &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        head.to_lowercase()
            .contains("x-mimic-error: connect-failed"),
        "{head}"
    );
}

#[tokio::test]
async fn msh_61_a_stale_view_is_not_used_for_pod_addresses() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let pod = Pod::http1("pod", 0).await;
    let state = state_for(cluster.addr, &[pod.addr], |c| {
        c.stale_after = Duration::from_millis(300)
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut open = H1Client::connect(f.addr).await;
    assert_eq!(open.get("/").await.1, "pod");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        open.get("/").await.1,
        "cluster",
        "an open mesh-routed connection"
    );
    assert_eq!(
        H1Client::connect(f.addr).await.get("/").await.1,
        "cluster",
        "a new connection"
    );
    assert_eq!(
        pod.counts.requests(),
        1,
        "the pod of the view was used before the view was stale"
    );
    assert!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total") >= 1.0,
        "fallbacks"
    );
}

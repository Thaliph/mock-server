//! MSH-85 and MSH-86: the answers the mesh generates are counted and never recorded, and the
//! stale fallback never uses a pod of an old view and ends with a fresh one (spec §12.21,
//! "M1 failure modes").

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::mesh_pods::{H1Client, Pod, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};

fn state_for(
    cluster: SocketAddr,
    view: View,
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let _ = cluster;
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(9),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view, "\"1-a\"");
    state
}

/// A view whose endpoints are all serving and terminating (`ready: false`).
fn terminating(cluster: SocketAddr, pods: &[SocketAddr]) -> View {
    let (ip, port) = (cluster.ip(), cluster.port());
    let endpoints: Vec<String> = pods
        .iter()
        .map(|p| format!(r#"{{"ip":"{}","port":{},"ready":false}}"#, p.ip(), p.port()))
        .collect();
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":true,"mode":"none","services":{{"shop/cart":{{"endpoints":{{"{port}":[{}]}},"generations":{{}},"ips":["{ip}"],"policy":{{"loadBalancer":{{"slowStartSeconds":0}},"retries":3,"retryBudget":{{"minPerSecond":10,"percent":20,"windowSeconds":10}}}},"ports":{{"{port}":{{"targetPort":8080}}}}}}}}}}"#,
        endpoints.join(",")
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn lower(head: &str) -> String {
    head.to_lowercase()
}

#[tokio::test]
async fn msh_85_no_endpoint_is_synthetic_not_recorded_and_not_sent_to_the_cluster_ip() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let state = state_for(cluster.addr, view_on(cluster.addr, &[], 0, true), |_| {});
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster.addr, &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..2 {
        let (head, body) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
        assert!(
            lower(&head).contains("x-mimic-error: no-endpoint"),
            "{head}"
        );
        assert!(lower(&head).contains("content-length: 0"), "{head}");
        assert!(body.is_empty());
    }
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 2",
    )
    .await;
    assert_eq!(cluster.counts.requests(), 0, "the ClusterIP got a request");
    let text = f.metrics.encode();
    assert_eq!(counter(&text, "mimic_mesh_fallbacks_total"), 0.0);
    assert!(
        !text.contains("result=\"recorded\""),
        "a generated answer was recorded"
    );
    assert!(discovery.report().endpoints().is_empty());
    logs.access_exactly(2).await;

    // A connect-failed answer is synthetic too (the third, on the same discovery).
    let dead = closed_port().await;
    let state = state_for(dead, view_on(dead, &[closed_port().await], 0, true), |_| {});
    let f = fixture_mesh(dead, &state, Some(&discovery)).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert!(
        lower(&head).contains("x-mimic-error: connect-failed"),
        "{head}"
    );
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 3",
    )
    .await;
    assert!(discovery.report().endpoints().is_empty());
}

#[tokio::test]
async fn msh_85_serving_terminating_endpoints_are_balanced_when_none_is_ready() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (a, b) = (Pod::http1("a", 0).await, Pod::http1("b", 0).await);
    let state = state_for(
        cluster.addr,
        terminating(cluster.addr, &[a.addr, b.addr]),
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..20 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    assert!(a.counts.requests() > 0 && b.counts.requests() > 0);
    assert_eq!(a.counts.requests() + b.counts.requests(), 20);
    assert_eq!(cluster.counts.requests(), 0);
}

#[tokio::test]
async fn msh_86_a_stale_view_never_reaches_a_pod_warns_once_and_recovers_with_a_fresh_view() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let pod = Pod::http1("pod", 0).await;
    let state = state_for(
        cluster.addr,
        view_on(cluster.addr, &[pod.addr], 0, true),
        |c| c.stale_after = Duration::from_millis(200),
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    assert_eq!(client.get("/").await.1, "pod");
    tokio::time::sleep(Duration::from_millis(400)).await;
    for _ in 0..3 {
        state.tick();
        assert_eq!(client.get("/").await.1, "cluster");
    }
    // Exactly one attempt each: the ClusterIP got 3 requests, the pod stayed at 1.
    assert_eq!(cluster.counts.requests(), 3, "no retry on the ClusterIP");
    assert_eq!(pod.counts.requests(), 1, "a pod of the stale view was used");
    logs.settled().await;
    let warns = logs.text().matches("mesh view stale").count();
    assert_eq!(warns, 1, "one WARN per streak");

    // A fresh view: the pods again, the age back to 0, and a later staleness warns again.
    state.hold(view_on(cluster.addr, &[pod.addr], 0, true), "\"1-b\"");
    state.tick();
    assert_eq!(client.get("/").await.1, "pod");
    assert_eq!(pod.counts.requests(), 2);
    assert!(f.metrics.encode().contains("mimic_mesh_view_age_seconds 0"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    state.tick();
    logs.settled().await;
    assert_eq!(logs.text().matches("mesh view stale").count(), 2);
}

#[tokio::test]
async fn msh_86_a_stale_view_with_a_refusing_cluster_ip_is_connect_failed_never_a_pod() {
    let _logs = capture_logs();
    let cluster = closed_port().await;
    let pod = Pod::http1("pod", 0).await;
    let state = state_for(cluster, view_on(cluster, &[pod.addr], 0, true), |c| {
        c.stale_after = Duration::from_millis(100)
    });
    let f = fixture_mesh(cluster, &state, None).await;
    // A connection mesh-routed while the view was fresh (a new one would be a byte relay).
    let mut client = H1Client::connect(f.addr).await;
    assert_eq!(client.get("/").await.1, "pod");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (head, _) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        lower(&head).contains("x-mimic-error: connect-failed"),
        "{head}"
    );
    assert_eq!(pod.counts.requests(), 1, "a pod of the stale view was used");
}

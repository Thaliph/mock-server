//! GW-15: the gateway reflectors follow the presence of the `gateways` CRD and start nothing
//! while the gateway is off, against a scripted API server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::fake::{FakeApi, Replica, settings};
use super::metrics::GatewayMetrics;
use super::reconcile::Reconciler;
use super::watch::start;
use crate::mesh::MeshConfig;
use crate::mesh::tests::{Lines, hub};
use crate::mesh::tests_watch::{json as reply, not_found, scripted, watch_until_gone};

const GATEWAYS: &str = "/apis/gateway.networking.k8s.io/v1/gateways";
const CLASSES: &str = "/apis/gateway.networking.k8s.io/v1/gatewayclasses";
const DEPLOYMENTS: &str = "/apis/apps/v1/deployments";
const SERVICES: &str = "/api/v1/services";

fn list(api_version: &str, kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": api_version, "kind": format!("{kind}List"),
           "metadata": {"resourceVersion": "5"}, "items": items})
}

/// An API server with a mimic GatewayClass, no labelled Deployment or Service, and the
/// `gateways` resource present or not (with a Gateway in `other`): `present` flipping to false
/// ends the open watch, as a deleted CRD does. The URIs asked are recorded.
fn client(present: &watch::Sender<bool>, seen: &Arc<Mutex<Vec<String>>>) -> kube::Client {
    let present = present.clone();
    scripted(seen, move |uri| {
        let path = uri.path();
        let watching = uri.query().unwrap_or("").contains("watch=true");
        match path {
            GATEWAYS if !*present.borrow() => not_found(),
            GATEWAYS if watching => watch_until_gone(&present, false),
            CLASSES | DEPLOYMENTS | SERVICES if watching => watch_until_gone(&present, true),
            GATEWAYS => reply(
                200,
                &list(
                    "gateway.networking.k8s.io/v1",
                    "Gateway",
                    vec![
                        json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
                           "metadata": {"namespace": "other", "name": "web", "uid": "u",
                                        "generation": 1, "resourceVersion": "5"},
                           "spec": {"gatewayClassName": "mimic",
                                    "listeners": [{"name": "http", "port": 80,
                                                   "protocol": "HTTP"}]}}),
                    ],
                ),
            ),
            CLASSES => reply(
                200,
                &list(
                    "gateway.networking.k8s.io/v1",
                    "GatewayClass",
                    vec![
                        json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
                           "metadata": {"name": "mimic", "generation": 1, "resourceVersion": "5"},
                           "spec": {"controllerName": "mimic.platform.io/gateway-controller"}}),
                    ],
                ),
            ),
            DEPLOYMENTS => reply(200, &list("apps/v1", "Deployment", vec![])),
            SERVICES => reply(200, &list("v1", "Service", vec![])),
            _ => not_found(),
        }
    })
}

#[tokio::test(start_paused = true)]
async fn gw_15_no_request_when_the_namespaces_are_empty() {
    let present = watch::channel(true).0;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    start(
        client(&present, &seen),
        hub(MeshConfig::default()),
        None::<Arc<Reconciler>>,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|p| !p.contains("/gateways")
            && !p.contains("/gatewayclasses")
            && !p.contains("labelSelector=mimic.platform.io")),
        "{seen:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn gw_15_nothing_is_written_while_gateways_answers_404() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    );
    let present = watch::channel(false).0;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let hub = hub(MeshConfig::default());
    let cluster = Arc::new(FakeApi::default());
    let metrics = GatewayMetrics::register(&mut Registry::default());
    let replica = Arc::new(Replica {
        api: cluster.clone(),
    });
    let reconciler = Reconciler::new(settings(&["shop"]), replica, metrics);
    start(client(&present, &seen), hub.clone(), Some(reconciler));
    let gauge = || hub.metrics().gateway_api_present.get();

    // Absent: nothing written, one WARN however long it stays absent, the gauge at 0.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(cluster.whats(), Vec::<String>::new());
    assert_eq!(lines.count("gateway api crds missing"), 1);
    assert_eq!(gauge(), 0);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(cluster.whats(), Vec::<String>::new());
    assert_eq!(lines.count("gateway api crds missing"), 1);
    let looks = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.starts_with(GATEWAYS) && p.contains("limit=1"))
        .count();
    assert_eq!(looks, 2, "one look every 30 s");

    // Served: one INFO at the next look, then the class and the Gateway status.
    present.send_replace(true);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(lines.count("gateway api crds found"), 1);
    assert_eq!(gauge(), 1);
    assert_eq!(
        cluster.whats(),
        ["status GatewayClass mimic", "status Gateway other/web"]
    );

    // Deleted while watched: one more WARN, and nothing more is written.
    present.send_replace(false);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines.count("gateway api crds missing"), 2);
    assert_eq!(gauge(), 0);
    assert_eq!(cluster.whats().len(), 2);
}

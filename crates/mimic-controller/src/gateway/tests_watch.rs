//! GW-15: the gateway reflectors follow the presence of the `gateways` CRD, start nothing while
//! the gateway is off, and list the `gateways` again for a new mimic class, against a scripted
//! API server.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};

use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::fake::{FakeApi, Replica, settings};
use super::metrics::GatewayMetrics;
use super::reconcile::Reconciler;
use super::watch::start;
use crate::mesh::MeshConfig;
use crate::mesh::tests::{Lines, hub};
use crate::mesh::tests_watch::{Body, json as reply, not_found, scripted, watch_until_gone};

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

    // Served: one INFO at the next look, then the class status (the Gateway in `other` gets
    // none).
    present.send_replace(true);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(lines.count("gateway api crds found"), 1);
    assert_eq!(gauge(), 1);
    assert_eq!(cluster.whats(), ["status GatewayClass mimic"]);

    // Deleted while watched: one more WARN, and nothing more is written.
    present.send_replace(false);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines.count("gateway api crds missing"), 2);
    assert_eq!(gauge(), 0);
    assert_eq!(cluster.whats().len(), 1);
}

/// A watch answer that sends `event` once `fire` is true, then stays open.
fn watch_then(fire: &watch::Sender<bool>, event: Value) -> http::Response<Body> {
    let mut fire = fire.subscribe();
    let line = Bytes::from(format!("{event}\n"));
    let stream = futures::stream::once(async move {
        let _ = fire.wait_for(|f| *f).await;
        Ok::<_, Infallible>(Frame::data(line))
    })
    .chain(futures::stream::pending());
    http::Response::builder()
        .status(200)
        .body(StreamBody::new(stream).boxed_unsync())
        .unwrap()
}

fn mimic_class() -> Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
           "metadata": {"name": "mimic", "generation": 1, "resourceVersion": "6"},
           "spec": {"controllerName": "mimic.platform.io/gateway-controller"}})
}

#[tokio::test(start_paused = true)]
async fn gw_15_a_new_mimic_class_lists_the_gateways_again() {
    let present = watch::channel(true).0;
    let fire = watch::channel(false).0;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let client = {
        let (present, fire) = (present.clone(), fire.clone());
        scripted(&seen, move |uri| {
            let watching = uri.query().unwrap_or("").contains("watch=true");
            match uri.path() {
                CLASSES if watching => {
                    watch_then(&fire, json!({"type": "ADDED", "object": mimic_class()}))
                }
                GATEWAYS | DEPLOYMENTS | SERVICES if watching => watch_until_gone(&present, true),
                CLASSES => reply(
                    200,
                    &list("gateway.networking.k8s.io/v1", "GatewayClass", vec![]),
                ),
                GATEWAYS => reply(
                    200,
                    &list(
                        "gateway.networking.k8s.io/v1",
                        "Gateway",
                        vec![json!({"apiVersion": "gateway.networking.k8s.io/v1",
                            "kind": "Gateway",
                            "metadata": {"namespace": "shop", "name": "web", "uid": "u",
                                         "generation": 1, "resourceVersion": "5"},
                            "spec": {"gatewayClassName": "mimic",
                                     "listeners": [{"name": "http", "port": 80,
                                                    "protocol": "HTTP"}]}})],
                    ),
                ),
                DEPLOYMENTS => reply(200, &list("apps/v1", "Deployment", vec![])),
                SERVICES => reply(200, &list("v1", "Service", vec![])),
                _ => not_found(),
            }
        })
    };
    let cluster = Arc::new(FakeApi::default());
    let metrics = GatewayMetrics::register(&mut Registry::default());
    let replica = Arc::new(Replica {
        api: cluster.clone(),
    });
    let reconciler = Reconciler::new(settings(&["shop"]), replica, metrics);
    start(client, hub(MeshConfig::default()), Some(reconciler));
    let lists = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|p| {
                p.starts_with(GATEWAYS) && !p.contains("watch=true") && !p.contains("limit=1")
            })
            .count()
    };

    // No mimic class: the Gateway is a stub, nothing is written.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(lists(), 1);
    assert_eq!(cluster.whats(), Vec::<String>::new());

    // The class appears: one more list of the Gateways, which now get their objects.
    fire.send_replace(true);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(lists(), 2);
    let whats = cluster.whats();
    assert!(
        whats.iter().any(|w| w == "apply Deployment shop/web-mimic"),
        "{whats:?}"
    );
}

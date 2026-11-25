//! GW-15: the gateway reflectors follow the presence of the `gateways` CRD and start nothing
//! while the gateway is off, against a scripted API server.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::metrics::GatewayMetrics;
use super::reconcile::Reconciler;
use super::tests_reconcile::{Recorder, settings};
use super::watch::start;
use crate::mesh::MeshConfig;
use crate::mesh::tests::{Lines, hub};

const GATEWAYS: &str = "/apis/gateway.networking.k8s.io/v1/gateways";
const CLASSES: &str = "/apis/gateway.networking.k8s.io/v1/gatewayclasses";

type Body = UnsyncBoxBody<Bytes, Infallible>;

fn reply(body: &Value) -> http::Response<Body> {
    http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())).boxed_unsync())
        .unwrap()
}

fn list(kind: &str, item: Value) -> Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": format!("{kind}List"),
           "metadata": {"resourceVersion": "5"}, "items": [item]})
}

/// An API server with a mimic GatewayClass, and the `gateways` resource present or not (with a
/// Gateway in `other`): `present` flipping to false ends the open watch, as a deleted CRD does.
/// The URIs asked are recorded.
fn client(present: &watch::Sender<bool>, seen: &Arc<Mutex<Vec<String>>>) -> kube::Client {
    let (present, seen) = (present.clone(), seen.clone());
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let (present, seen) = (present.clone(), seen.clone());
        async move {
            let uri = request.uri().clone();
            seen.lock().unwrap().push(uri.to_string());
            let path = uri.path();
            let served = path == CLASSES || (path == GATEWAYS && *present.borrow());
            if !served {
                return Ok::<_, Infallible>(
                    http::Response::builder()
                        .status(404)
                        .header("content-type", "text/plain; charset=utf-8")
                        .body(Full::new(Bytes::from("404 page not found\n")).boxed_unsync())
                        .unwrap(),
                );
            }
            if uri.query().unwrap_or("").contains("watch=true") {
                let mut gone = present.subscribe();
                let classes = path == CLASSES;
                let stream = futures::stream::once(async move {
                    if classes {
                        std::future::pending::<()>().await;
                    }
                    let _ = gone.wait_for(|p| !*p).await;
                })
                .filter_map(|()| async { None::<Result<Frame<Bytes>, Infallible>> });
                return Ok(http::Response::builder()
                    .status(200)
                    .body(StreamBody::new(stream).boxed_unsync())
                    .unwrap());
            }
            Ok(reply(&if path == CLASSES {
                list(
                    "GatewayClass",
                    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
                           "metadata": {"name": "mimic", "generation": 1, "resourceVersion": "5"},
                           "spec": {"controllerName": "mimic.platform.io/gateway-controller"}}),
                )
            } else {
                list(
                    "Gateway",
                    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
                           "metadata": {"namespace": "other", "name": "web", "uid": "u",
                                        "generation": 1, "resourceVersion": "5"},
                           "spec": {"gatewayClassName": "mimic",
                                    "listeners": [{"name": "http", "port": 80,
                                                   "protocol": "HTTP"}]}}),
                )
            }))
        }
    });
    kube::Client::new(service, "default")
}

fn lines_with(lines: &Lines, text: &str) -> usize {
    String::from_utf8(lines.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter(|l| l.contains(text))
        .count()
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
    let cluster = Arc::new(Recorder::default());
    let metrics = GatewayMetrics::register(&mut Registry::default());
    let reconciler = Reconciler::new(settings(&["shop"]), cluster.clone(), metrics);
    start(client(&present, &seen), hub.clone(), Some(reconciler));
    let gauge = || hub.metrics().gateway_api_present.get();

    // Absent: nothing written, one WARN however long it stays absent, the gauge at 0.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(cluster.whats(), Vec::<String>::new());
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 1);
    assert_eq!(gauge(), 0);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(cluster.whats(), Vec::<String>::new());
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 1);
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
    assert_eq!(lines_with(&lines, "gateway api crds found"), 1);
    assert_eq!(gauge(), 1);
    assert_eq!(
        cluster.whats(),
        ["status class mimic", "status gateway other/web"]
    );

    // Deleted while watched: one more WARN, and nothing more is written.
    present.send_replace(false);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 2);
    assert_eq!(gauge(), 0);
    assert_eq!(cluster.whats().len(), 2);
}

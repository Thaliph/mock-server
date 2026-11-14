//! RTE-18: the `httproutes` watch follows the presence of the Gateway API CRD, against a
//! scripted API server.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use serde_json::json;
use tokio::sync::watch;

use super::tests::{Lines, hub};
use super::{MeshConfig, watch as reflectors};

const PATH: &str = "/apis/gateway.networking.k8s.io/v1/httproutes";

type Body = UnsyncBoxBody<Bytes, Infallible>;

fn route() -> serde_json::Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
        "metadata": {"namespace": "shop", "name": "split", "resourceVersion": "5",
                     "creationTimestamp": "2026-11-12T10:00:00Z"},
        "spec": {"parentRefs": [{"group": "", "kind": "Service", "name": "cart"}]}})
}

fn reply(status: u16, body: &serde_json::Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())).boxed_unsync())
        .unwrap()
}

/// An API server with the `httproutes` resource, present or not: `present` flipping to false
/// ends the open watch, as a deleted CRD does. The paths asked are recorded.
fn client(present: &watch::Sender<bool>, seen: &Arc<Mutex<Vec<String>>>) -> kube::Client {
    let (present, seen) = (present.clone(), seen.clone());
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let (present, seen) = (present.clone(), seen.clone());
        async move {
            let uri = request.uri().clone();
            seen.lock().unwrap().push(uri.to_string());
            if uri.path() != PATH || !*present.borrow() {
                return Ok::<_, Infallible>(reply(
                    404,
                    &json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                            "reason": "NotFound", "code": 404, "message": "no"}),
                ));
            }
            if uri.query().unwrap_or("").contains("watch=true") {
                let mut gone = present.subscribe();
                let stream = futures::stream::once(async move {
                    let _ = gone.wait_for(|p| !*p).await;
                })
                .filter_map(|()| async { None::<Result<Frame<Bytes>, Infallible>> });
                return Ok(http::Response::builder()
                    .status(200)
                    .body(StreamBody::new(stream).boxed_unsync())
                    .unwrap());
            }
            Ok(reply(
                200,
                &json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRouteList",
                        "metadata": {"resourceVersion": "5"}, "items": [route()]}),
            ))
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
async fn rte_18_the_watch_follows_the_gateway_api_crd_with_one_log_line_per_change() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    );
    let present = watch::channel(false).0;
    let seen = Arc::default();
    let hub = hub(MeshConfig::default());
    tokio::spawn(reflectors::routes(client(&present, &seen), hub.clone()));
    let gauge = || hub.metrics().gateway_api_present.get();
    let held = || {
        hub.publish_now();
        hub.published().snapshot.routes.len()
    };

    // Absent: one WARN however long it stays absent, the gauge at 0, no routes.
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 1);
    assert_eq!((gauge(), held()), (0, 0));
    assert!(
        seen.lock().unwrap().len() >= 3,
        "it is looked for again: {:?}",
        seen.lock().unwrap()
    );

    // Installed: one INFO within the poll interval, the gauge at 1, the route held.
    present.send_replace(true);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(lines_with(&lines, "gateway api crds found"), 1);
    assert_eq!((gauge(), held()), (1, 1));
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 1);

    // Deleted again: the routes go and one more WARN.
    present.send_replace(false);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines_with(&lines, "gateway api crds missing"), 2);
    assert_eq!(lines_with(&lines, "gateway api crds found"), 1);
    assert_eq!((gauge(), held()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn rte_18_a_present_crd_logs_nothing_and_a_missing_one_leaves_the_other_watches_alone() {
    let lines = Lines::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(lines.clone())
            .finish(),
    );
    let present = watch::channel(true).0;
    let hub = hub(MeshConfig::default());
    tokio::spawn(reflectors::routes(
        client(&present, &Arc::default()),
        hub.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(70)).await;
    assert_eq!(lines_with(&lines, "gateway api crds"), 0);
    assert_eq!(hub.metrics().gateway_api_present.get(), 1);
}

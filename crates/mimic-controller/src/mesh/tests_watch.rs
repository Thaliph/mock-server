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

pub(crate) type Body = UnsyncBoxBody<Bytes, Infallible>;

/// What a real API server answers for a resource of a group it does not serve: a plain-text
/// 404, which a watch request reads as a broken event stream.
pub(crate) fn not_found() -> http::Response<Body> {
    http::Response::builder()
        .status(404)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from("404 page not found\n")).boxed_unsync())
        .unwrap()
}

pub(crate) fn json(status: u16, body: &serde_json::Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())).boxed_unsync())
        .unwrap()
}

/// A watch answer with no event: it ends when `present` turns false, as a deleted CRD ends it,
/// or never when `hold`.
pub(crate) fn watch_until_gone(present: &watch::Sender<bool>, hold: bool) -> http::Response<Body> {
    let mut gone = present.subscribe();
    let stream = futures::stream::once(async move {
        if hold {
            std::future::pending::<()>().await;
        }
        let _ = gone.wait_for(|p| !*p).await;
    })
    .filter_map(|()| async { None::<Result<Frame<Bytes>, Infallible>> });
    http::Response::builder()
        .status(200)
        .body(StreamBody::new(stream).boxed_unsync())
        .unwrap()
}

/// A scripted API server: each request URI is recorded in `seen`, then answered by `route`.
pub(crate) fn scripted(
    seen: &Arc<Mutex<Vec<String>>>,
    route: impl Fn(&http::Uri) -> http::Response<Body> + Clone + Send + 'static,
) -> kube::Client {
    let seen = seen.clone();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let (seen, route) = (seen.clone(), route.clone());
        async move {
            let uri = request.uri().clone();
            seen.lock().unwrap().push(uri.to_string());
            Ok::<_, Infallible>(route(&uri))
        }
    });
    kube::Client::new(service, "default")
}

fn route() -> serde_json::Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
        "metadata": {"namespace": "shop", "name": "split", "resourceVersion": "5",
                     "creationTimestamp": "2026-11-12T10:00:00Z"},
        "spec": {"parentRefs": [{"group": "", "kind": "Service", "name": "cart"}]}})
}

/// An API server with the `httproutes` resource, present or not: `present` flipping to false
/// ends the open watch, as a deleted CRD does. The paths asked are recorded.
fn client(present: &watch::Sender<bool>, seen: &Arc<Mutex<Vec<String>>>) -> kube::Client {
    let present = present.clone();
    scripted(seen, move |uri| {
        if uri.path() != PATH || !*present.borrow() {
            not_found()
        } else if uri.query().unwrap_or("").contains("watch=true") {
            watch_until_gone(&present, false)
        } else {
            json(
                200,
                &json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRouteList",
                        "metadata": {"resourceVersion": "5"}, "items": [route()]}),
            )
        }
    })
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
    assert_eq!(lines.count("gateway api crds missing"), 1);
    assert_eq!((gauge(), held()), (0, 0));
    assert!(
        seen.lock().unwrap().len() >= 3,
        "it is looked for again: {:?}",
        seen.lock().unwrap()
    );

    // Installed: one INFO within the poll interval, the gauge at 1, the route held.
    present.send_replace(true);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(lines.count("gateway api crds found"), 1);
    assert_eq!((gauge(), held()), (1, 1));
    assert_eq!(lines.count("gateway api crds missing"), 1);

    // Deleted again: the routes go and one more WARN.
    present.send_replace(false);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(lines.count("gateway api crds missing"), 2);
    assert_eq!(lines.count("gateway api crds found"), 1);
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
    assert_eq!(lines.count("gateway api crds"), 0);
    assert_eq!(hub.metrics().gateway_api_present.get(), 1);
}

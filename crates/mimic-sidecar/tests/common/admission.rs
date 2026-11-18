//! The admission rig (ADM-16 to ADM-28): an inbound meshed sidecar in process in front of an
//! `h2c` app that records what it receives and the most it had in flight at once.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::header::HeaderMap;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::admission::Contract;
use mimic_core::mesh::view::View;
use mimic_sidecar::listener::{Direction, Shared};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use super::mesh_pods::{Counts, view_services};
use super::{Fixture, H2Sender, fixture_mesh_on};

/// The time any one answer may take before the test fails.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// A view of no Service with the contract `c/r`.
pub fn contract(c: u32, r: u32) -> View {
    let mut view = view_services(&[]);
    view.admission = Some(Contract {
        concurrency: c,
        rps: r,
    });
    view
}

/// A view of no Service and no contract.
pub fn no_contract() -> View {
    view_services(&[])
}

/// A meshed state (`MIMIC_MESH=on`, no view held) with `tune` applied to its config.
pub fn meshed(pod: Option<&str>, tune: impl FnOnce(&mut MeshConfig)) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        pod: pod.map(str::to_string),
        ..MeshConfig::default()
    };
    tune(&mut config);
    MeshState::new(&config)
}

/// An inbound listener of a meshed sidecar whose app is at `app`.
pub async fn inbound(
    app: SocketAddr,
    state: &MeshState,
    configure: impl FnOnce(Shared) -> Shared,
) -> Fixture {
    fixture_mesh_on(
        Direction::Inbound,
        Arc::new(move |_| Ok(app)),
        state,
        None,
        configure,
    )
    .await
}

type AppBody = BoxBody<Bytes, Infallible>;

/// The app: `/` answers `200` after the delay; `/trailers` sends `a`, then the trailers
/// `x-t: 1` 600 ms later; `/sse` sends one event and stays open; `/close` drops the app
/// connection without an answer (the connections opened later answer).
pub struct App {
    pub addr: SocketAddr,
    pub counts: Counts,
}

impl App {
    pub async fn h2c(delay_ms: u64) -> App {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counts = Counts::default();
        counts.delay_ms.store(delay_ms, Ordering::SeqCst);
        let shared = counts.clone();
        let close = Arc::new(Notify::new());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (counts, close) = (shared.clone(), close.clone());
                tokio::spawn(async move {
                    let closing = close.clone();
                    let service =
                        service_fn(move |req| route(counts.clone(), closing.clone(), req));
                    let served = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service);
                    tokio::select! {
                        _ = served => {}
                        () = close.notified() => {}
                    }
                });
            }
        });
        App { addr, counts }
    }

    pub fn in_flight(&self) -> usize {
        self.counts.in_flight.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.counts.max_in_flight.load(Ordering::SeqCst)
    }
}

async fn route(
    counts: Counts,
    close: Arc<Notify>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<AppBody>, Infallible> {
    counts.arrivals.lock().unwrap().push(Instant::now());
    counts.requests.fetch_add(1, Ordering::SeqCst);
    match req.uri().path() {
        "/trailers" => {
            let mut trailers = HeaderMap::new();
            trailers.insert("x-t", "1".parse().unwrap());
            let frames = futures::stream::once(async { Ok(Frame::data(Bytes::from("a"))) }).chain(
                futures::stream::once(async move {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    Ok(Frame::trailers(trailers))
                }),
            );
            Ok(Response::new(BodyExt::boxed(StreamBody::new(frames))))
        }
        "/sse" => {
            let frames =
                futures::stream::once(async { Ok(Frame::data(Bytes::from("data: 1\n\n"))) })
                    .chain(futures::stream::pending());
            Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(BodyExt::boxed(StreamBody::new(frames)))
                .unwrap())
        }
        "/close" => {
            close.notify_waiters();
            std::future::pending().await
        }
        _ => {
            let now = counts.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            counts.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let delay = counts.delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            counts.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Response::new(Full::new(Bytes::from("ok")).boxed()))
        }
    }
}

/// One answer as the client saw it.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    /// Every field, sorted.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// From just before the request was sent to the end of the body.
    pub elapsed: Duration,
}

impl Answer {
    pub fn error(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == "x-mimic-error")
            .map(|(_, v)| v.as_str())
    }
}

/// Sends `GET path` with `headers` on `sender` and reads the whole answer.
pub async fn call(sender: &mut H2Sender, path: &str, headers: &[(&str, &str)]) -> Answer {
    let mut request = Request::get(format!("http://cart.shop.svc{path}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(http_body_util::Empty::<Bytes>::new()).unwrap();
    let start = Instant::now();
    let answered = tokio::time::timeout(ANSWER_TIMEOUT, async {
        let response = sender.send_request(request).await.expect("an answer");
        let (head, body) = response.into_parts();
        let body = body
            .collect()
            .await
            .expect("a whole body")
            .to_bytes()
            .to_vec();
        (head, body)
    })
    .await
    .expect("answered in time");
    let (head, body) = answered;
    let mut headers: Vec<(String, String)> = head
        .headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    headers.sort();
    Answer {
        status: head.status.as_u16(),
        headers,
        body,
        elapsed: start.elapsed(),
    }
}

/// The four fields of a refusal of the pod at `app`, sorted.
pub fn refusal_fields(error: &str, app: SocketAddr, origin: &str) -> Vec<(String, String)> {
    let mut fields = vec![
        ("content-length".to_string(), "0".to_string()),
        ("x-mimic-endpoint".to_string(), app.to_string()),
        ("x-mimic-error".to_string(), error.to_string()),
        ("x-mimic-origin".to_string(), origin.to_string()),
    ];
    fields.sort();
    fields
}

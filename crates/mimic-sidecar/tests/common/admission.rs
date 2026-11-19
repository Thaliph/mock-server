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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// An HTTP/1 app over raw sockets that records every byte. `/` and any path not below answer
/// `200` (`content-length: 2`) after the delay; `/chunked` sends 3 chunks 400 ms apart;
/// `/sse` sends one event and stays open; `/close` closes without an answer; `/cd` answers a
/// close-delimited body after 300 ms, then closes; `/ws` with `Upgrade` answers `101`, then
/// echoes; `/ws-no` answers `200` after 300 ms; `CONNECT` answers `200`, then echoes. A
/// connection that never carried a byte stays open 500 ms after the caller's FIN.
pub struct Http1App {
    pub addr: SocketAddr,
    pub counts: Counts,
    /// Every byte read, from every connection, in the order it came.
    pub raw: Arc<std::sync::Mutex<Vec<u8>>>,
    /// The bytes read on each accepted connection, in accept order.
    pub per_conn: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl App {
    pub async fn http1(delay_ms: u64) -> Http1App {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Http1App::serve(listener, delay_ms)
    }
}

impl Http1App {
    pub fn serve(listener: TcpListener, delay_ms: u64) -> Http1App {
        let addr = listener.local_addr().unwrap();
        let app = Http1App {
            addr,
            counts: Counts::default(),
            raw: Arc::default(),
            per_conn: Arc::default(),
        };
        app.counts.delay_ms.store(delay_ms, Ordering::SeqCst);
        let (counts, raw, per_conn) = (app.counts.clone(), app.raw.clone(), app.per_conn.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                counts.accepted.fetch_add(1, Ordering::SeqCst);
                let index = {
                    let mut per_conn = per_conn.lock().unwrap();
                    per_conn.push(0);
                    per_conn.len() - 1
                };
                let conn = Http1Conn {
                    stream,
                    pending: Vec::new(),
                    counts: counts.clone(),
                    raw: raw.clone(),
                    per_conn: per_conn.clone(),
                    index,
                };
                tokio::spawn(conn.run());
            }
        });
        app
    }

    pub fn in_flight(&self) -> usize {
        self.counts.in_flight.load(Ordering::SeqCst)
    }

    pub fn max_in_flight(&self) -> usize {
        self.counts.max_in_flight.load(Ordering::SeqCst)
    }

    /// Whether the app read `needle` anywhere.
    pub fn got(&self, needle: &str) -> bool {
        let raw = self.raw.lock().unwrap();
        raw.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    pub fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().unwrap()).into_owned()
    }

    pub fn per_conn(&self) -> Vec<usize> {
        self.per_conn.lock().unwrap().clone()
    }
}

struct Http1Conn {
    stream: tokio::net::TcpStream,
    pending: Vec<u8>,
    counts: Counts,
    raw: Arc<std::sync::Mutex<Vec<u8>>>,
    per_conn: Arc<std::sync::Mutex<Vec<usize>>>,
    index: usize,
}

impl Http1Conn {
    /// Reads more bytes; `false` at the end of the connection.
    async fn fill(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        match self.stream.read(&mut buf).await {
            Ok(n) if n > 0 => {
                self.raw.lock().unwrap().extend_from_slice(&buf[..n]);
                self.per_conn.lock().unwrap()[self.index] += n;
                self.pending.extend_from_slice(&buf[..n]);
                true
            }
            _ => false,
        }
    }

    /// The next request head (and its `content-length` body, dropped).
    async fn request(&mut self) -> Option<String> {
        loop {
            if let Some(p) = self.pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.pending[..p]).into_owned();
                let length = head
                    .lines()
                    .find_map(|l| {
                        let (n, v) = l.split_once(':')?;
                        n.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while self.pending.len() < p + 4 + length {
                    if !self.fill().await {
                        return None;
                    }
                }
                self.pending.drain(..p + 4 + length);
                return Some(head);
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> bool {
        self.stream.write_all(bytes).await.is_ok()
    }

    /// Echoes what comes until the end of the connection.
    async fn echo(&mut self) {
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() && !self.write(&rest).await {
            return;
        }
        loop {
            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    self.raw.lock().unwrap().extend_from_slice(&buf[..n]);
                    if !self.write(&buf[..n]).await {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    async fn run(mut self) {
        self.serve().await;
        // A connection that never carried a byte is kept open a while after the caller's FIN,
        // as an app waiting for its first request may do.
        if self.per_conn.lock().unwrap()[self.index] == 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn serve(&mut self) {
        while let Some(head) = self.request().await {
            self.counts.arrivals.lock().unwrap().push(Instant::now());
            self.counts.requests.fetch_add(1, Ordering::SeqCst);
            let line = head.lines().next().unwrap_or_default().to_string();
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let upgrade = head.to_ascii_lowercase().contains("upgrade: websocket");
            if method == "CONNECT" {
                if !self
                    .write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                {
                    return;
                }
                return self.echo().await;
            }
            match path.as_str() {
                "/ws" if upgrade => {
                    let reply = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
                    if !self.write(reply).await {
                        return;
                    }
                    return self.echo().await;
                }
                "/ws-no" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if !self
                        .write(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nno")
                        .await
                    {
                        return;
                    }
                }
                "/chunked" => {
                    let head = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\na\r\n";
                    if !self.write(head).await {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    if !self.write(b"1\r\nb\r\n").await {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    if !self.write(b"1\r\nc\r\n0\r\n\r\n").await {
                        return;
                    }
                }
                "/sse" => {
                    let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n9\r\ndata: 1\n\n\r\n";
                    if !self.write(head).await {
                        return;
                    }
                    while self.fill().await {}
                    return;
                }
                "/close" => return,
                "/cd" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = self
                        .write(b"HTTP/1.1 200 OK\r\n\r\nuntil the app closes")
                        .await;
                    let _ = self.stream.shutdown().await;
                    while self.fill().await {}
                    return;
                }
                _ => {
                    let now = self.counts.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    self.counts.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    let delay = self.counts.delay_ms.load(Ordering::SeqCst);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    self.counts.in_flight.fetch_sub(1, Ordering::SeqCst);
                    if !self
                        .write(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await
                    {
                        return;
                    }
                }
            }
        }
    }
}

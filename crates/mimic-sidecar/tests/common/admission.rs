//! The admission rig (ADM-16 to ADM-28): an inbound meshed sidecar in process in front of an
//! `h2c` app that records what it receives and the most it had in flight at once.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use mimic_runtime::config::Env;
use mimic_sidecar::listener::{Direction, Shared};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub use super::Answer;
use super::mesh_pods::{Counts, H1Client, meshed, view_services};
use super::{Fixture, H2Sender, fixture_mesh_on, h2_send, until};

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
/// connection without an answer (the connections opened later answer); `/forge` answers `503`
/// with `x-mimic-error: overloaded` and `x-mimic-endpoint: <its own address>` after the delay.
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
                        service_fn(move |req| route(counts.clone(), closing.clone(), addr, req));
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
}

async fn route(
    counts: Counts,
    close: Arc<Notify>,
    addr: SocketAddr,
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
        "/forge" => {
            tokio::time::sleep(Duration::from_millis(
                counts.delay_ms.load(Ordering::SeqCst),
            ))
            .await;
            Ok(Response::builder()
                .status(503)
                .header("x-mimic-error", "overloaded")
                .header("x-mimic-endpoint", addr.to_string())
                .body(Full::new(Bytes::new()).boxed())
                .unwrap())
        }
        _ => {
            let in_flight = counts.enter();
            let delay = counts.delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            drop(in_flight);
            Ok(Response::new(Full::new(Bytes::from("ok")).boxed()))
        }
    }
}

/// Sends `GET path` with `headers` on `sender` and reads the whole answer.
pub async fn call(sender: &mut H2Sender, path: &str, headers: &[(&str, &str)]) -> Answer {
    h2_send(sender, "GET", path, headers, Vec::new())
        .await
        .expect("an answer in time")
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
/// echoes; `/ws-no` answers `200` after 300 ms; `/forge` answers `503` with
/// `x-mimic-error: overloaded` and `x-mimic-endpoint: <its own address>` after the delay;
/// `CONNECT` answers `200`, then echoes. A connection that never carried a byte stays open
/// 500 ms after the caller's FIN.
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
                    addr,
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
    /// The app's own address (the original destination of the sidecar).
    addr: SocketAddr,
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
                "/forge" => {
                    let delay = self.counts.delay_ms.load(Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let reply = format!(
                        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nx-mimic-endpoint: {}\r\ncontent-length: 0\r\n\r\n",
                        self.addr
                    );
                    if !self.write(reply.as_bytes()).await {
                        return;
                    }
                }
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
                    let in_flight = self.counts.enter();
                    let delay = self.counts.delay_ms.load(Ordering::SeqCst);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    drop(in_flight);
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

/// A full queue under `contract(1, …)`: a held slot (`GET /slow`, held by the app for its
/// delay), then one queued filler. The next head is then refused `overloaded` at once, for the
/// 150 ms the filler waits: send it right away.
pub async fn full_queue(f: &Fixture, app: &Http1App, state: &MeshState) -> (H1Client, H1Client) {
    let mut held = H1Client::connect(f.addr).await;
    held.send(b"GET /slow HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
        .await;
    until("the app holds /slow", || app.counts.in_flight() == 1).await;
    let mut filler = H1Client::connect(f.addr).await;
    filler
        .send(b"GET /filler HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
        .await;
    until("the filler is queued", || {
        state.admission_counts_for_test().depth == 1
    })
    .await;
    (held, filler)
}

/// An app answering after `delay_ms`, and a meshed sidecar in front of it holding `view`.
pub async fn rig(view: View, delay_ms: u64) -> (App, MeshState, Fixture) {
    let app = App::h2c(delay_ms).await;
    let state = meshed(Some("cart-7d9f-x2k"), |_| {});
    state.hold(view, "\"1-a\"");
    let f = inbound(app.addr, &state, |s| s).await;
    (app, state, f)
}

/// Sends `/` on a clone of `sender` in a task.
pub fn spawn_call(sender: &H2Sender, path: &'static str) -> JoinHandle<Answer> {
    let mut sender = sender.clone();
    tokio::spawn(async move { call(&mut sender, path, &[]).await })
}

/// A held slot: a `/` the app holds (its delay is set long before), sent first.
pub async fn hold_slot(app: &App, sender: &H2Sender) -> JoinHandle<Answer> {
    let before = app.counts.requests();
    let held = spawn_call(sender, "/");
    until("the app holds the first request", || {
        app.counts.requests() > before && app.counts.in_flight() >= 1
    })
    .await;
    held
}

/// `<dir>/state/mesh.json` holding `view`, fetched now.
pub fn persist(dir: &std::path::Path, view: &View) {
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(
        dir.join("state").join("mesh.json"),
        format!(
            "{{\"fetchedAt\":{},\"view\":{}}}",
            unix_now(),
            view.to_json()
        ),
    )
    .unwrap();
}

/// A meshed state of `MIMIC_MESH=on` with no controller, its state file in `dir`.
pub fn from_env(dir: &std::path::Path, vars: &[(&str, &str)]) -> MeshState {
    let env = Env::from_pairs(
        [("MIMIC_MESH", "on"), ("MIMIC_POD_NAME", "cart-7d9f-x2k")]
            .iter()
            .chain(vars)
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    MeshState::new(&MeshConfig::from_env(&env, dir).unwrap())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

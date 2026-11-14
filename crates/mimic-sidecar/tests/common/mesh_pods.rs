//! Fake pods and a view for the load balancing tests (MSH-48 and after): local listeners that
//! stand for the pods of a Service, and a view that lists them.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::view::{Limits, View};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What a fake pod counts.
#[derive(Clone, Default)]
pub struct Counts {
    pub requests: Arc<AtomicUsize>,
    pub accepted: Arc<AtomicUsize>,
    /// Connections open now.
    pub open: Arc<AtomicUsize>,
    /// Requests being served now.
    pub in_flight: Arc<AtomicUsize>,
    /// How long each request is held before it is answered, in milliseconds.
    pub delay_ms: Arc<AtomicU64>,
}

impl Counts {
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

pub struct Pod {
    pub addr: SocketAddr,
    pub counts: Counts,
}

struct OpenGuard(Arc<AtomicUsize>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn answer(
    name: String,
    counts: Counts,
    _req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    counts.requests.fetch_add(1, Ordering::SeqCst);
    counts.in_flight.fetch_add(1, Ordering::SeqCst);
    let delay = counts.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    counts.in_flight.fetch_sub(1, Ordering::SeqCst);
    Ok(Response::builder()
        .header("x-pod", name.as_str())
        .body(Full::new(Bytes::from(name)))
        .unwrap())
}

impl Pod {
    /// An HTTP/1 server answering with its own name.
    pub async fn http1(name: &str, delay_ms: u64) -> Pod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counts = Counts::default();
        counts.delay_ms.store(delay_ms, Ordering::SeqCst);
        let (name, shared) = (name.to_string(), counts.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (name, counts) = (name.clone(), shared.clone());
                counts.accepted.fetch_add(1, Ordering::SeqCst);
                counts.open.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _open = OpenGuard(counts.open.clone());
                    let service = service_fn(move |req| answer(name.clone(), counts.clone(), req));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Pod { addr, counts }
    }

    /// An `h2c` server answering with its own name; `max_streams` is its
    /// `MAX_CONCURRENT_STREAMS`.
    pub async fn h2c(name: &str, delay_ms: u64, max_streams: Option<u32>) -> Pod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counts = Counts::default();
        counts.delay_ms.store(delay_ms, Ordering::SeqCst);
        let (name, shared) = (name.to_string(), counts.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (name, counts) = (name.clone(), shared.clone());
                counts.accepted.fetch_add(1, Ordering::SeqCst);
                counts.open.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _open = OpenGuard(counts.open.clone());
                    let service = service_fn(move |req| answer(name.clone(), counts.clone(), req));
                    let mut builder =
                        hyper::server::conn::http2::Builder::new(TokioExecutor::new());
                    builder.max_concurrent_streams(max_streams);
                    let _ = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Pod { addr, counts }
    }
}

/// A server that reads request heads off the wire and answers each with `reply(head)`; the
/// bytes it received are kept as they came.
pub struct RawPod {
    pub addr: SocketAddr,
    pub received: Arc<std::sync::Mutex<Vec<u8>>>,
    pub counts: Counts,
}

impl RawPod {
    pub async fn start(reply: impl Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static) -> RawPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let counts = Counts::default();
        let reply = Arc::new(reply);
        let (log, shared) = (received.clone(), counts.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, counts, reply) = (log.clone(), shared.clone(), reply.clone());
                counts.accepted.fetch_add(1, Ordering::SeqCst);
                counts.open.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _open = OpenGuard(counts.open.clone());
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        let Ok(n) = stream.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head: Vec<u8> = pending.drain(..p + 4).collect();
                            counts.requests.fetch_add(1, Ordering::SeqCst);
                            let answer = reply(&head);
                            if answer.is_empty() {
                                return;
                            }
                            if stream.write_all(&answer).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        RawPod {
            addr,
            received,
            counts,
        }
    }

    pub fn received(&self) -> Vec<u8> {
        self.received.lock().unwrap().clone()
    }
}

/// The ClusterIP and port the app dials in these tests: no one listens there, the
/// original destination is faked.
pub fn cluster_ip() -> SocketAddr {
    "127.0.0.1:80".parse().unwrap()
}

/// A view with the Service `shop/cart` on `cluster_ip()`, whose pods are `pods`.
pub fn view(pods: &[SocketAddr], slow_start_seconds: u32) -> View {
    view_text(pods, slow_start_seconds, true)
}

pub fn view_text(pods: &[SocketAddr], slow_start_seconds: u32, mesh: bool) -> View {
    view_on(cluster_ip(), pods, slow_start_seconds, mesh)
}

/// Like [`view`], with the Service on `cluster`.
pub fn view_on(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    slow_start_seconds: u32,
    mesh: bool,
) -> View {
    let (ip, port) = (cluster.ip(), cluster.port());
    let endpoints: Vec<String> = pods
        .iter()
        .map(|p| format!(r#"{{"ip":"{}","port":{},"ready":true}}"#, p.ip(), p.port()))
        .collect();
    let services = if mesh {
        format!(
            r#""shop/cart":{{"endpoints":{{"{port}":[{}]}},"generations":{{}},"ips":["{ip}"],"policy":{{"loadBalancer":{{"slowStartSeconds":{slow_start_seconds}}},"retries":3,"retryBudget":{{"minPerSecond":10,"percent":20,"windowSeconds":10}}}},"ports":{{"{port}":{{"targetPort":8080}}}}}}"#,
            endpoints.join(",")
        )
    } else {
        String::new()
    };
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":{mesh},"mode":"none","services":{{{services}}}}}"#
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

/// A keep-alive HTTP/1 client over one connection.
pub struct H1Client {
    stream: tokio::net::TcpStream,
    pending: Vec<u8>,
}

impl H1Client {
    pub async fn connect(addr: SocketAddr) -> H1Client {
        H1Client {
            stream: tokio::net::TcpStream::connect(addr).await.unwrap(),
            pending: Vec::new(),
        }
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }

    /// Reads one response: its head and its `content-length` body; `None` when the
    /// connection ends first.
    pub async fn read(&mut self) -> Option<(String, Vec<u8>)> {
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
                if self.pending.len() >= p + 4 + length {
                    let body = self.pending[p + 4..p + 4 + length].to_vec();
                    self.pending.drain(..p + 4 + length);
                    return Some((head, body));
                }
            }
            let mut buf = vec![0u8; 16 * 1024];
            let read = tokio::time::timeout(Duration::from_secs(10), self.stream.read(&mut buf))
                .await
                .expect("a response in time");
            match read {
                Ok(n) if n > 0 => self.pending.extend_from_slice(&buf[..n]),
                _ => return None,
            }
        }
    }

    /// `GET path` and the body of the answer.
    pub async fn get(&mut self, path: &str) -> (String, String) {
        self.send(format!("GET {path} HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n").as_bytes())
            .await;
        let (head, body) = self.read().await.expect("an answer");
        (head, String::from_utf8_lossy(&body).into_owned())
    }
}

/// Sends `requests` `h2c` requests over one connection, `parallel` at a time, and returns the
/// body of each answer.
pub async fn h2c_calls(addr: SocketAddr, requests: usize, parallel: usize) -> Vec<String> {
    use http_body_util::BodyExt;
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let gate = Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut tasks = Vec::new();
    for _ in 0..requests {
        let permit = gate.clone().acquire_owned().await.unwrap();
        let mut sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            let request = Request::get("http://cart.shop.svc/")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = sender.send_request(request).await.expect("an answer");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            drop(permit);
            String::from_utf8_lossy(&body).into_owned()
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

impl H1Client {
    /// Reads `n` raw bytes (a tunnel's), after what the head reader already holds.
    pub async fn read_raw(&mut self, n: usize) -> Vec<u8> {
        while self.pending.len() < n {
            let mut buf = vec![0u8; 4096];
            let read = tokio::time::timeout(Duration::from_secs(10), self.stream.read(&mut buf))
                .await
                .expect("bytes in time")
                .unwrap();
            assert!(read > 0, "the connection ended");
            self.pending.extend_from_slice(&buf[..read]);
        }
        self.pending.drain(..n).collect()
    }
}

/// A TCP server that echoes what it reads and counts the connections it accepts.
pub struct EchoPod {
    pub addr: SocketAddr,
    pub accepted: Arc<AtomicUsize>,
    /// Everything read, in the order it came.
    pub received: Arc<std::sync::Mutex<Vec<u8>>>,
    /// Connections open now.
    pub open: Arc<AtomicUsize>,
}

impl EchoPod {
    pub async fn start() -> EchoPod {
        EchoPod::on(TcpListener::bind("127.0.0.1:0").await.unwrap(), false)
    }

    /// Reads what comes, then closes without answering.
    pub async fn closer() -> EchoPod {
        EchoPod::on(TcpListener::bind("127.0.0.1:0").await.unwrap(), true)
    }

    /// Serves on a listener that already exists (a revived pod).
    pub fn on(listener: TcpListener, close_after_read: bool) -> EchoPod {
        let addr = listener.local_addr().unwrap();
        let pod = EchoPod {
            addr,
            accepted: Arc::default(),
            received: Arc::default(),
            open: Arc::default(),
        };
        let (accepted, received, open) =
            (pod.accepted.clone(), pod.received.clone(), pod.open.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                open.fetch_add(1, Ordering::SeqCst);
                let (received, open) = (received.clone(), open.clone());
                tokio::spawn(async move {
                    let _open = OpenGuard(open);
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        received.lock().unwrap().extend_from_slice(&buf[..n]);
                        if close_after_read || stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        pod
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    pub fn received(&self) -> Vec<u8> {
        self.received.lock().unwrap().clone()
    }
}

/// An address whose accept queue is full, so that connecting to it hangs (neither accepted nor
/// refused). `revive` serves on it.
pub struct Blackhole {
    pub addr: SocketAddr,
    listener: TcpListener,
    _fill: Vec<tokio::net::TcpStream>,
}

impl Blackhole {
    pub async fn new() -> Blackhole {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut fill = Vec::new();
        for _ in 0..8 {
            match tokio::time::timeout(
                Duration::from_millis(150),
                tokio::net::TcpStream::connect(addr),
            )
            .await
            {
                Ok(Ok(stream)) => fill.push(stream),
                _ => break,
            }
        }
        Blackhole {
            addr,
            listener,
            _fill: fill,
        }
    }

    /// Starts accepting (what queued is served too).
    pub fn revive(self) -> EchoPod {
        EchoPod::on(self.listener, false)
    }
}

impl Blackhole {
    /// Starts answering HTTP requests with `name` (what queued is served too).
    pub fn revive_http(self, name: &'static str) -> Counts {
        let counts = Counts::default();
        let shared = counts.clone();
        let listener = self.listener;
        tokio::spawn(async move {
            let _keep = self._fill;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let counts = shared.clone();
                counts.accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..p + 4);
                            counts.requests.fetch_add(1, Ordering::SeqCst);
                            let body = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{name}",
                                name.len()
                            );
                            if stream.write_all(body.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        counts
    }
}

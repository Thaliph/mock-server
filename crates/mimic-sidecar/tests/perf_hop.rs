//! #490: latency the proxy hop adds, over loopback. Not a criterion test: run by hand,
//! `cargo test --release -p mimic-sidecar --test perf_hop -- --ignored --nocapture`
//! (`MIMIC_PERF_SECS`, default 10, at 200 rps over 8 connections, 5 ms service, 1 KiB).

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::fixture;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn service() -> Duration {
    Duration::from_micros(
        std::env::var("MIMIC_PERF_SERVICE_US")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000),
    )
}
fn conns() -> usize {
    std::env::var("MIMIC_PERF_CONNS").ok().and_then(|s| s.parse().ok()).unwrap_or(8)
}
const RPS: u64 = 200;

fn secs() -> u64 {
    std::env::var("MIMIC_PERF_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

async fn http1_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            tokio::spawn(async move {
                let body = vec![b'x'; 1024];
                let head = b"HTTP/1.1 200 OK\r\ncontent-length: 1024\r\n\r\n";
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if !service().is_zero() {
                        tokio::time::sleep(service()).await;
                    }
                    let mut out = head.to_vec();
                    out.extend_from_slice(&body);
                    if s.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn h2_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<hyper::body::Incoming>| async {
                    if !service().is_zero() {
                        tokio::time::sleep(service()).await;
                    }
                    Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                        vec![b'x'; 1024],
                    ))))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    addr
}

/// The sidecars run on a runtime of their own, `MIMIC_PERF_PROXY_THREADS` workers (default: one per core, as the binary).
async fn proxy(to: SocketAddr, dir: Direction) -> SocketAddr {
    // `MIMIC_PERF_LOG=1`: the real JSON logger (access lines included), written to a sink.
    if std::env::var("MIMIC_PERF_LOG").is_ok_and(|v| v == "1") {
        static LOG: std::sync::Once = std::sync::Once::new();
        LOG.call_once(|| {
            let (dispatch, handle) = mimic_runtime::logging::build("info", std::io::sink());
            std::mem::forget(handle);
            let _ = tracing::dispatcher::set_global_default(dispatch);
        });
    }
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let rt = RT.get_or_init(|| {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.enable_all();
        if let Some(n) = std::env::var("MIMIC_PERF_PROXY_THREADS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            b.worker_threads(n);
        }
        b.build().unwrap()
    });
    let f = rt
        .spawn(async move { fixture(dir, move |_| Some(to)).await })
        .await
        .unwrap();
    METRICS.lock().unwrap().push(f.metrics.clone());
    f.addr
}

static METRICS: std::sync::Mutex<Vec<std::sync::Arc<mimic_runtime::metrics::Metrics>>> =
    std::sync::Mutex::new(Vec::new());

/// `mimic_proxy_overhead_seconds`: the sidecar's own time per exchange, as it measures it.
fn overhead() {
    for (i, m) in METRICS.lock().unwrap().iter().enumerate() {
        let text = m.encode();
        let get = |suffix: &str| -> f64 {
            text.lines()
                .filter(|l| l.starts_with(&format!("mimic_proxy_overhead_seconds_{suffix}")))
                .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
                .sum()
        };
        let (sum, count) = (get("sum"), get("count"));
        if count > 0.0 {
            println!("sidecar {i}: overhead {:.1} us/exchange over {count}", sum / count * 1e6);
        }
    }
}

/// The floor: a plain `copy_bidirectional`, nothing parsed, on the sidecars' runtime settings.
async fn l4(to: SocketAddr) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let mut u = TcpStream::connect(to).await.unwrap();
            c.set_nodelay(true).unwrap();
            u.set_nodelay(true).unwrap();
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut c, &mut u).await;
            });
        }
    });
    addr
}

/// CPU time of this process so far (`utime` + `stime`, 10 ms ticks).
fn cpu() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    let ticks: u64 = f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap();
    Duration::from_millis(ticks * 10)
}

async fn measured(label: &str, run: impl std::future::Future<Output = Vec<Duration>>) {
    let before = cpu();
    let v = run.await;
    let per = (cpu() - before).as_secs_f64() * 1e6 / v.len() as f64;
    report(&format!("{label} cpu={per:.0}us/req"), v);
}

fn report(label: &str, mut v: Vec<Duration>) {
    v.sort();
    let q = |p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)].as_secs_f64() * 1e3;
    println!(
        "{label:40} n={:5} p50={:.3} p90={:.3} p99={:.3} ms",
        v.len(),
        q(0.5),
        q(0.9),
        q(0.99)
    );
}

async fn load_http1(target: SocketAddr) -> Vec<Duration> {
    let total = if service().is_zero() { 5000 * conns() as u64 } else { secs() * RPS };
    let per = total / conns() as u64;
    let gap = Duration::from_secs_f64(conns() as f64 / RPS as f64);
    let mut tasks = vec![];
    for i in 0..conns() {
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(target).await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut out = vec![];
            let start = tokio::time::Instant::now() + gap / conns() as u32 * i as u32;
            let mut buf = vec![0u8; 4096];
            for k in 0..per {
                if !service().is_zero() {
                    tokio::time::sleep_until(start + gap * k as u32).await;
                }
                let t = Instant::now();
                s.write_all(b"GET /x HTTP/1.1\r\nhost: a\r\n\r\n").await.unwrap();
                let mut got = 0;
                while got < 41 + 1024 {
                    got += s.read(&mut buf).await.unwrap();
                }
                out.push(t.elapsed());
            }
            out
        }));
    }
    let mut all = vec![];
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all
}

async fn load_h2(target: SocketAddr) -> Vec<Duration> {
    let total = if service().is_zero() { 5000 * conns() as u64 } else { secs() * RPS };
    let per = total / conns() as u64;
    let gap = Duration::from_secs_f64(conns() as f64 / RPS as f64);
    let mut tasks = vec![];
    for i in 0..conns() {
        tasks.push(tokio::spawn(async move {
            let tcp = TcpStream::connect(target).await.unwrap();
            tcp.set_nodelay(true).unwrap();
            let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .handshake::<_, Empty<Bytes>>(TokioIo::new(tcp))
                .await
                .unwrap();
            tokio::spawn(conn);
            let mut out = vec![];
            let start = tokio::time::Instant::now() + gap / conns() as u32 * i as u32;
            for k in 0..per {
                if !service().is_zero() {
                    tokio::time::sleep_until(start + gap * k as u32).await;
                }
                let t = Instant::now();
                let req = Request::get("http://a/x").body(Empty::new()).unwrap();
                let resp = sender.send_request(req).await.unwrap();
                let _ = resp.into_body().collect().await.unwrap();
                out.push(t.elapsed());
            }
            out
        }));
    }
    let mut all = vec![];
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark"]
async fn perf_hop_http1() {
    let server = http1_server().await;
    let one = proxy(server, Direction::Inbound).await;
    let callee = proxy(server, Direction::Inbound).await;
    let caller = proxy(callee, Direction::Outbound).await;
    load_http1(server).await;
    measured("http1 plain", load_http1(server)).await;
    measured("http1 one proxy", load_http1(one)).await;
    let floor = l4(server).await;
    measured("http1 one l4 floor", load_http1(floor)).await;
    measured("http1 pair", load_http1(caller)).await;
    overhead();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark"]
async fn perf_hop_h2c() {
    let server = h2_server().await;
    let one = proxy(server, Direction::Inbound).await;
    let callee = proxy(server, Direction::Inbound).await;
    let caller = proxy(callee, Direction::Outbound).await;
    load_h2(server).await;
    measured("h2c plain", load_h2(server)).await;
    measured("h2c one proxy", load_h2(one)).await;
    let floor = l4(server).await;
    measured("h2c one l4 floor", load_h2(floor)).await;
    measured("h2c pair", load_h2(caller)).await;
    overhead();
}

enum Client {
    Http1(TcpStream),
    H2(hyper::client::conn::http2::SendRequest<Empty<Bytes>>),
}

impl Client {
    async fn open(h2: bool, target: SocketAddr) -> Client {
        let tcp = TcpStream::connect(target).await.unwrap();
        tcp.set_nodelay(true).unwrap();
        if !h2 {
            return Client::Http1(tcp);
        }
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, Empty<Bytes>>(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        Client::H2(sender)
    }

    async fn call(&mut self) {
        match self {
            Client::Http1(s) => {
                s.write_all(b"GET /x HTTP/1.1\r\nhost: a\r\n\r\n").await.unwrap();
                let mut buf = vec![0u8; 4096];
                let mut got = 0;
                while got < 41 + 1024 {
                    got += s.read(&mut buf).await.unwrap();
                }
            }
            Client::H2(sender) => {
                let req = Request::get("http://a/x").body(Empty::new()).unwrap();
                let resp = sender.send_request(req).await.unwrap();
                let _ = resp.into_body().collect().await.unwrap();
            }
        }
    }
}

/// One request at a time, taking turns between the paths, so that every path sees the same machine.
async fn interleaved(h2: bool) {
    let server = if h2 { h2_server().await } else { http1_server().await };
    let one = proxy(server, Direction::Inbound).await;
    let callee = proxy(server, Direction::Inbound).await;
    let caller = proxy(callee, Direction::Outbound).await;
    let mut paths = vec![];
    for t in [server, one, caller] {
        paths.push(Client::open(h2, t).await);
    }
    let rounds = secs() * RPS / 3;
    let mut times = vec![vec![], vec![], vec![]];
    for _ in 0..rounds {
        for (i, c) in paths.iter_mut().enumerate() {
            let t = Instant::now();
            c.call().await;
            times[i].push(t.elapsed());
            tokio::time::sleep(Duration::from_micros(1_000_000 / RPS - 200)).await;
        }
    }
    let kind = if h2 { "h2c" } else { "http1" };
    for (label, v) in ["plain", "one proxy", "pair"].iter().zip(times) {
        report(&format!("{kind} interleaved {label}"), v);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark"]
async fn perf_hop_interleaved_http1() {
    interleaved(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark"]
async fn perf_hop_interleaved_h2c() {
    interleaved(true).await;
}

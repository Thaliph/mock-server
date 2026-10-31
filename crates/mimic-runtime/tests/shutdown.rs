//! FND-05: graceful shutdown drains in-flight requests, refuses new connections, and gives up
//! after the grace period.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use mimic_runtime::admin::serve_until;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

fn app() -> Router {
    Router::new()
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                "done"
            }),
        )
        .route(
            "/hang",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "never"
            }),
        )
}

async fn start(
    grace: Duration,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_until(listener, app(), grace, async {
        let _ = stopped.await;
    }));
    (addr, stop, server)
}

async fn send(addr: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    stream
}

#[tokio::test]
async fn fnd_05_in_flight_requests_finish_and_new_connections_are_refused() {
    let (addr, stop, server) = start(Duration::from_secs(5)).await;
    let mut in_flight = send(addr, "/slow").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    stop.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "new connection accepted during shutdown"
    );

    let mut response = String::new();
    in_flight.read_to_string(&mut response).await.unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("done"),
        "{response}"
    );

    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn fnd_05_the_grace_period_bounds_the_drain() {
    let grace = Duration::from_millis(300);
    let (addr, stop, server) = start(grace).await;
    let _hung = send(addr, "/hang").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let begin = Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve returned")
        .unwrap()
        .unwrap();
    assert!(
        begin.elapsed() >= grace && begin.elapsed() < grace + Duration::from_secs(2),
        "{:?}",
        begin.elapsed()
    );
}

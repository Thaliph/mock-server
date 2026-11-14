//! The inbound answer to a refused app connection (spec §4.1 "Refused app connections"): when
//! the connect to the app fails on a meshed pod, a caller that speaks cleartext HTTP/1 or
//! `h2c` gets `503` `x-mimic-error: connect-failed`, so that its sidecar can retry on another
//! pod (§7.9 "Retries"). Written by the sidecar itself, never relayed from the app.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use mimic_core::proxy::{DETECT_LIMIT, Detection, detect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use crate::mesh::MeshState;
use crate::transport::Stream;

const READ_CHUNK: usize = 4096;
/// What the sidecar still reads from an HTTP/1 caller after the answer, and for how long, so
/// that a request body in flight does not turn the close into a reset.
const DRAIN_BYTES: usize = 64 * 1024;
const DRAIN_TIME: Duration = Duration::from_secs(1);
/// The most an `h2c` connection is served.
const H2_TIME: Duration = Duration::from_secs(5);

/// Answers `client` if it speaks cleartext HTTP/1 (once its head is whole) or `h2c`; anything
/// else is closed without a byte, as before. `stall` bounds the wait for the caller's bytes.
pub(crate) async fn answer(
    mut client: TcpStream,
    dst: SocketAddr,
    error: &str,
    mesh: &MeshState,
    stall: Duration,
    abort: impl Future<Output = ()>,
) {
    let mut abort = pin!(abort);
    let deadline = tokio::time::sleep(stall);
    let mut deadline = pin!(deadline);
    let mut buf = Vec::new();
    let h2 = loop {
        let mut chunk = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                _ => return,
            },
            () = &mut deadline => return,
            () = &mut abort => return,
        }
        match detect(&buf) {
            Detection::H2 => break true,
            Detection::Http1 if buf.windows(4).any(|w| w == b"\r\n\r\n") => break false,
            Detection::NeedMore | Detection::Http1 if buf.len() < DETECT_LIMIT => {}
            _ => return,
        }
    };
    let origin = mesh.inbound_refusal();
    tracing::warn!(%dst, %error, "inbound connect failed, answered 503 connect-failed");
    if h2 {
        serve_h2(client, buf, dst, &origin, abort.as_mut()).await;
        return;
    }
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: connect-failed\r\nx-mimic-endpoint: {dst}\r\nx-mimic-origin: {origin}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    if client.write_all(head.as_bytes()).await.is_err() || client.shutdown().await.is_err() {
        return;
    }
    let mut seen = 0;
    let drain = async {
        let mut chunk = [0u8; READ_CHUNK];
        while seen < DRAIN_BYTES {
            match client.read(&mut chunk).await {
                Ok(n) if n > 0 => seen += n,
                _ => return,
            }
        }
    };
    tokio::select! {
        () = drain => {}
        () = tokio::time::sleep(DRAIN_TIME) => {}
        () = &mut abort => {}
    }
}

/// Every stream gets the answer; `GOAWAY` follows the first one so that the caller does not keep
/// a connection that answers `503` once the app is back.
async fn serve_h2(
    client: TcpStream,
    buf: Vec<u8>,
    dst: SocketAddr,
    origin: &str,
    abort: impl Future<Output = ()>,
) {
    let first = Arc::new(Notify::new());
    let (signal, endpoint, origin) = (first.clone(), dst.to_string(), origin.to_string());
    let service = service_fn(move |_| {
        signal.notify_one();
        let mut response = Response::new(Empty::<Bytes>::new());
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        let headers = response.headers_mut();
        for (name, value) in [
            ("x-mimic-error", "connect-failed"),
            ("x-mimic-endpoint", endpoint.as_str()),
            ("x-mimic-origin", origin.as_str()),
            ("content-length", "0"),
        ] {
            if let Ok(value) = value.parse() {
                headers.insert(name, value);
            }
        }
        std::future::ready(Ok::<_, Infallible>(response))
    });
    let stream = Stream::plain_with_prefix(client, buf);
    let served = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .serve_connection(TokioIo::new(stream), service);
    let mut served = pin!(served);
    let mut abort = pin!(abort);
    let end = tokio::time::sleep(H2_TIME);
    let mut end = pin!(end);
    tokio::select! {
        _ = served.as_mut() => return,
        () = first.notified() => served.as_mut().graceful_shutdown(),
        () = &mut end => return,
        () = &mut abort => return,
    }
    tokio::select! {
        _ = served.as_mut() => {}
        () = &mut end => {}
        () = &mut abort => {}
    }
}

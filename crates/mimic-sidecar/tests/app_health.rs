//! INJ-18: the `/app-health` probe forwarder, in process against a real local HTTP(S) app.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::{any, get};
use mimic_runtime::config::Env;
use mimic_sidecar::probes::{self, Probes};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tower::ServiceExt;

async fn app() -> SocketAddr {
    let router = Router::new()
        .route("/ok", get(|| async { "fine" }))
        .route(
            "/redirect",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ok")]) }),
        )
        .route("/teapot", get(|| async { StatusCode::IM_A_TEAPOT }))
        .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }),
        )
        .route(
            "/headers",
            any(|method: Method, h: HeaderMap| async move {
                let ok = method == Method::GET
                    && h.get("x-probe").is_some_and(|v| v == "yes")
                    && h.get(header::HOST).is_some_and(|v| v == "app.local");
                if ok {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// A TLS server with a self-signed certificate that answers 204 to anything.
async fn https_app() -> SocketAddr {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let der = cert.cert.der().clone();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![der], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = tls.read(&mut buf).await;
                let _ = tls
                    .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

fn env(probes: &str) -> Env {
    Env::from_pairs([
        ("MIMIC_APP_PROBES".to_string(), probes.to_string()),
        ("MIMIC_POD_IP".to_string(), "127.0.0.1".to_string()),
    ])
}

async fn get_status(router: &Router, uri: &str) -> u16 {
    let req = Request::get(uri).body(Body::empty()).unwrap();
    router.clone().oneshot(req).await.unwrap().status().as_u16()
}

async fn router_for(json: &str) -> Router {
    probes::router(Arc::new(Probes::from_env(&env(json)).unwrap()))
}

fn probe(addr: SocketAddr, path: &str) -> String {
    format!(
        r#"{{"path":"{path}","port":{},"scheme":"HTTP","timeoutSeconds":1}}"#,
        addr.port()
    )
}

#[tokio::test]
async fn inj_18_answers_the_app_status_code_unchanged_and_does_not_follow_redirects() {
    let addr = app().await;
    let json = format!(
        r#"{{"web/readyz":{},"web/livez":{},"web/startupz":{},"api/readyz":{}}}"#,
        probe(addr, "/ok"),
        probe(addr, "/redirect"),
        probe(addr, "/teapot"),
        probe(addr, "/boom"),
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 200);
    assert_eq!(get_status(&r, "/app-health/web/livez").await, 302);
    assert_eq!(get_status(&r, "/app-health/web/startupz").await, 418);
    assert_eq!(get_status(&r, "/app-health/api/readyz").await, 500);
}

#[tokio::test]
async fn inj_18_unconfigured_paths_under_app_health_are_404() {
    let addr = app().await;
    let r = router_for(&format!(r#"{{"web/readyz":{}}}"#, probe(addr, "/ok"))).await;
    for uri in [
        "/app-health/web/livez",
        "/app-health/other/readyz",
        "/app-health/web/readyz/extra",
        "/app-health/web",
        "/app-health/",
        "/app-health/web/bogus",
    ] {
        assert_eq!(get_status(&r, uri).await, 404, "{uri}");
    }
    let empty = router_for("{}").await;
    assert_eq!(get_status(&empty, "/app-health/web/readyz").await, 404);
}

#[tokio::test]
async fn inj_18_forwards_the_probe_method_headers_and_host() {
    let addr = app().await;
    let json = format!(
        r#"{{"web/readyz":{{"path":"/headers","port":{},"scheme":"HTTP","timeoutSeconds":1,
            "httpHeaders":[{{"name":"X-Probe","value":"yes"}},{{"name":"Host","value":"app.local"}}]}}}}"#,
        addr.port()
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 200);
}

#[tokio::test]
async fn inj_18_honours_the_probe_host_instead_of_the_pod_ip() {
    let addr = app().await;
    let json = format!(
        r#"{{"web/readyz":{{"host":"127.0.0.1","path":"/ok","port":{},"scheme":"HTTP"}}}}"#,
        addr.port()
    );
    let env = Env::from_pairs([
        ("MIMIC_APP_PROBES".to_string(), json),
        ("MIMIC_POD_IP".to_string(), "192.0.2.1".to_string()),
    ]);
    let r = probes::router(Arc::new(Probes::from_env(&env).unwrap()));
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 200);
}

#[tokio::test]
async fn inj_18_answers_503_when_the_app_refuses_the_connection() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = l.local_addr().unwrap();
    drop(l);
    let r = router_for(&format!(r#"{{"web/readyz":{}}}"#, probe(dead, "/ok"))).await;
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 503);
}

#[tokio::test]
async fn inj_18_answers_503_after_the_probe_timeout() {
    let addr = app().await;
    let r = router_for(&format!(r#"{{"web/readyz":{}}}"#, probe(addr, "/slow"))).await;
    let started = std::time::Instant::now();
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 503);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn inj_18_https_probe_does_not_verify_the_certificate() {
    let addr = https_app().await;
    let json = format!(
        r#"{{"web/readyz":{{"path":"/","port":{},"scheme":"HTTPS","timeoutSeconds":2}}}}"#,
        addr.port()
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 204);
}

#[test]
fn inj_18_invalid_probes_are_rejected() {
    for bad in [
        "not json",
        r#"[]"#,
        r#"{"web":{"port":80}}"#,
        r#"{"web/health":{"port":80}}"#,
        r#"{"web/readyz":{"path":"/","port":0}}"#,
        r#"{"web/readyz":{"path":"/"}}"#,
        r#"{"web/readyz":{"path":"/","port":80,"scheme":"FTP"}}"#,
        r#"{"web/readyz":{"path":"/","port":80,"httpHeaders":[{"name":"bad name","value":"x"}]}}"#,
    ] {
        assert!(Probes::from_env(&env(bad)).is_err(), "{bad}");
    }
    assert!(Probes::from_env(&Env::default()).is_ok());
}

#[tokio::test]
async fn inj_18_tcp_probe_answers_200_when_it_connects_and_503_otherwise() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let json = format!(
        r#"{{"web/readyz":{{"tcpSocket":{{"port":{port}}},"timeoutSeconds":1}},
            "web/livez":{{"tcpSocket":{{"port":{port},"host":"127.0.0.1"}}}},
            "api/readyz":{{"tcpSocket":{{"port":{closed_port}}},"timeoutSeconds":1}}}}"#
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/web/readyz").await, 200);
    assert_eq!(get_status(&r, "/app-health/web/livez").await, 200);
    assert_eq!(get_status(&r, "/app-health/api/readyz").await, 503);
}

/// An h2c server answering `Health/Check`. The service name asked for picks the answer: "up"
/// and "" are SERVING, "down" NOT_SERVING, "err" a gRPC error (UNIMPLEMENTED).
async fn grpc_app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let Ok(mut conn) = h2::server::handshake(tcp).await else {
                    return;
                };
                while let Some(Ok((request, mut respond))) = conn.accept().await {
                    tokio::spawn(async move {
                        let mut body = request.into_body();
                        let mut data = Vec::new();
                        while let Some(Ok(chunk)) = body.data().await {
                            let _ = body.flow_control().release_capacity(chunk.len());
                            data.extend_from_slice(&chunk);
                        }
                        // 5-byte frame header, then field 1 (tag 0x0a, one-byte length) if any.
                        let service = String::from_utf8_lossy(data.get(7..).unwrap_or_default());
                        let response = axum::http::Response::builder()
                            .header("content-type", "application/grpc")
                            .body(())
                            .unwrap();
                        let mut stream = respond.send_response(response, false).unwrap();
                        let mut trailers = axum::http::HeaderMap::new();
                        if service == "err" {
                            trailers.insert("grpc-status", "12".parse().unwrap());
                        } else {
                            let serving = u8::from(service == "up" || service.is_empty());
                            let msg = [0u8, 0, 0, 0, 2, 0x08, serving];
                            stream.send_data(msg.to_vec().into(), false).unwrap();
                            trailers.insert("grpc-status", "0".parse().unwrap());
                        }
                        stream.send_trailers(trailers).unwrap();
                    });
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn inj_18_grpc_probe_answers_200_only_for_serving() {
    let addr = grpc_app().await;
    let port = addr.port();
    let g = |service: &str| {
        format!(r#"{{"grpc":{{"port":{port},"service":"{service}"}},"timeoutSeconds":2}}"#)
    };
    let json = format!(
        r#"{{"a/readyz":{},"a/livez":{},"a/startupz":{},"c/readyz":{{"grpc":{{"port":{port}}},"timeoutSeconds":2}}}}"#,
        g("up"),
        g("down"),
        g("err"),
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/a/readyz").await, 200);
    assert_eq!(get_status(&r, "/app-health/a/livez").await, 503);
    assert_eq!(get_status(&r, "/app-health/a/startupz").await, 503);
    // No service: the empty name, which this server reports as SERVING.
    assert_eq!(get_status(&r, "/app-health/c/readyz").await, 200);
}

#[tokio::test]
async fn inj_18_grpc_probe_answers_503_for_a_refused_connection_and_a_non_grpc_server() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let addr = app().await;
    let json = format!(
        r#"{{"a/readyz":{{"grpc":{{"port":{closed_port}}}}},"b/readyz":{{"grpc":{{"port":{}}},"timeoutSeconds":1}}}}"#,
        addr.port()
    );
    let r = router_for(&json).await;
    assert_eq!(get_status(&r, "/app-health/a/readyz").await, 503);
    assert_eq!(get_status(&r, "/app-health/b/readyz").await, 503);
}

#[test]
fn inj_18_invalid_tcp_and_grpc_probes_are_rejected() {
    for bad in [
        r#"{"web/readyz":{"tcpSocket":{"port":0}}}"#,
        r#"{"web/readyz":{"tcpSocket":{}}}"#,
        r#"{"web/readyz":{"grpc":{"port":0}}}"#,
        r#"{"web/readyz":{"grpc":{"port":"x"}}}"#,
    ] {
        assert!(Probes::from_env(&env(bad)).is_err(), "{bad}");
    }
}

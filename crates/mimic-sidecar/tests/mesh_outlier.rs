//! CBR-15 to CBR-21, CBR-23 and CBR-24, and CBR-22 with the part of CBR-20 that concerns L4:
//! passive outlier detection of the sidecar (spec §7.9 "Outlier detection", §12.27).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{EchoPod, H1Client, Pod, Svc, mesh_state, view_services};
use common::{Logs, capture_logs, closed_port, fixture_mesh};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What a fake pod does with a request head.
#[derive(Clone)]
enum Mode {
    /// `200` with this name.
    Ok(&'static str),
    /// The connection is closed after the request was read.
    Reset,
    /// This status and no `x-mimic-*` field.
    Status(u16),
    /// `503` with this `x-mimic-error`, `x-mimic-endpoint` naming `names` (the pod itself when
    /// `None`) and this `x-mimic-origin`.
    Refuse {
        error: &'static str,
        names: Option<SocketAddr>,
        origin: Option<&'static str>,
    },
}

struct ModePod {
    addr: SocketAddr,
    heads: Arc<AtomicUsize>,
}

impl ModePod {
    async fn start(mode: Mode) -> ModePod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let heads = Arc::new(AtomicUsize::new(0));
        let counted = heads.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (mode, heads) = (mode.clone(), counted.clone());
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        let Ok(n) = stream.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..p + 4);
                            heads.fetch_add(1, Ordering::SeqCst);
                            let answer = match &mode {
                                Mode::Reset => return,
                                Mode::Ok(name) => format!(
                                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{name}",
                                    name.len()
                                ),
                                Mode::Status(code) => {
                                    format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\n\r\n")
                                }
                                Mode::Refuse {
                                    error,
                                    names,
                                    origin,
                                } => {
                                    let names = names.unwrap_or(addr);
                                    let origin = origin
                                        .map(|o| format!("x-mimic-origin: {o}\r\n"))
                                        .unwrap_or_default();
                                    format!(
                                        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: {error}\r\nx-mimic-endpoint: {names}\r\n{origin}content-length: 0\r\n\r\n"
                                    )
                                }
                            };
                            if stream.write_all(answer.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        ModePod { addr, heads }
    }

    fn heads(&self) -> usize {
        self.heads.load(Ordering::SeqCst)
    }
}

/// The Service `shop/cart` the app dials on `cluster`.
fn cart_on(cluster: SocketAddr, pods: &[SocketAddr]) -> Svc {
    let ip = Box::leak(cluster.ip().to_string().into_boxed_str());
    Svc::new("shop/cart", ip, cluster.port(), pods)
}

/// The Service the app dials, `shop/cart` on `127.0.0.1:80` (no one listens there).
fn cart(pods: &[SocketAddr]) -> Svc {
    Svc::new("shop/cart", "127.0.0.1", 80, pods)
}

fn dst() -> SocketAddr {
    "127.0.0.1:80".parse().unwrap()
}

fn metric(scrape: &str, name: &str, labels: &[&str]) -> f64 {
    scrape
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn ejections(scrape: &str, reason: &str) -> f64 {
    metric(
        scrape,
        "mimic_mesh_ejections_total{",
        &[&format!("reason=\"{reason}\"")],
    )
}

fn ejected(scrape: &str, backend: &str) -> f64 {
    metric(
        scrape,
        "mimic_mesh_ejected_endpoints{",
        &[&format!("backend=\"{backend}\"")],
    )
}

/// The lines of the log with this message.
async fn lines(logs: &Logs, message: &str) -> Vec<serde_json::Value> {
    logs.settled().await;
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == message)
        .collect()
}

async fn status(client: &mut H1Client, method: &str, path: &str) -> u16 {
    let body = if method == "POST" { "abc" } else { "" };
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    client.send(head.as_bytes()).await;
    let (head, _) = client.read().await.expect("an answer");
    head[9..12].parse().unwrap()
}

#[tokio::test]
async fn cbr_15_a_resetting_pod_is_ejected_after_five_and_restored_after_its_time() {
    let logs = capture_logs();
    let (a, b) = (
        ModePod::start(Mode::Ok("a")).await,
        ModePod::start(Mode::Ok("b")).await,
    );
    let bad = ModePod::start(Mode::Reset).await;
    let state = mesh_state(
        &[cart(&[a.addr, b.addr, bad.addr]).outlier(1, 0, 5, 50)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..100 {
        assert_eq!(status(&mut client, "GET", "/").await, 200, "request {n}");
    }
    assert_eq!(bad.heads(), 5, "five heads, then none while ejected");
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_failures"), 1.0, "{scrape}");
    assert_eq!(ejected(&scrape, "shop/cart:80"), 1.0, "{scrape}");
    let ejected_lines = lines(&logs, "endpoint ejected").await;
    assert_eq!(ejected_lines.len(), 1, "{}", logs.text());
    let line = &ejected_lines[0];
    assert_eq!(line["service"], "shop/cart");
    assert_eq!(line["backend"], "shop/cart:80");
    assert_eq!(line["endpoint"], bad.addr.to_string());
    assert_eq!(line["reason"], "consecutive_failures");
    assert_eq!(line["ejection_seconds"], 1);
    assert_eq!(line["ejections"], 1);
    // Back after its second, and ejected again for two.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    state.tick();
    let restored = lines(&logs, "endpoint restored").await;
    assert_eq!(restored.len(), 1, "{}", logs.text());
    assert_eq!(restored[0]["cause"], "elapsed");
    assert_eq!(restored[0]["endpoint"], bad.addr.to_string());
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 0.0);
    for n in 0..200 {
        assert_eq!(status(&mut client, "GET", "/").await, 200, "request {n}");
        if bad.heads() >= 10 {
            break;
        }
    }
    assert_eq!(bad.heads(), 10, "five more heads once restored");
    let again = lines(&logs, "endpoint ejected").await;
    assert_eq!(again.len(), 2, "{}", logs.text());
    assert_eq!(again[1]["ejection_seconds"], 2);
    assert_eq!(again[1]["ejections"], 2);
}

#[tokio::test]
async fn cbr_15_a_post_the_pod_resets_counts_as_a_failure_too() {
    let _logs = capture_logs();
    let ok = ModePod::start(Mode::Ok("ok")).await;
    let bad = ModePod::start(Mode::Reset).await;
    let state = mesh_state(&[cart(&[ok.addr, bad.addr])], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    for _ in 0..80 {
        // A POST is not retried: the app sees the close, on a connection of its own.
        let mut client = H1Client::connect(f.addr).await;
        let _ = status_or_closed(&mut client).await;
    }
    assert_eq!(bad.heads(), 5, "five heads, then none while ejected");
    assert_eq!(ejections(&f.metrics.encode(), "consecutive_failures"), 1.0);
}

async fn status_or_closed(client: &mut H1Client) -> Option<u16> {
    client
        .send(b"POST / HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 3\r\n\r\nabc")
        .await;
    client
        .read()
        .await
        .map(|(head, _)| head[9..12].parse().unwrap())
}

#[tokio::test]
async fn cbr_16_a_refusing_pod_is_avoided_then_ejected_after_its_fifth_connect() {
    let _logs = capture_logs();
    let ok = ModePod::start(Mode::Ok("ok")).await;
    let state = mesh_state(&[cart(&[ok.addr, closed_port().await])], |c| {
        c.avoid_failed_for = Duration::from_millis(100)
    });
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let failed = |f: &common::Fixture| {
        metric(
            &f.metrics.encode(),
            "mimic_mesh_attempts_total{",
            &["result=\"connect_failed\""],
        )
    };
    for _ in 0..60 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        if failed(&f) >= 5.0 {
            break;
        }
        // Past the avoidance, so that the pod is picked again.
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    assert_eq!(failed(&f), 5.0, "five failed connects");
    assert_eq!(ejections(&f.metrics.encode(), "consecutive_failures"), 1.0);
    for _ in 0..8 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    assert_eq!(failed(&f), 5.0, "no connect to it while it is ejected");
}

#[tokio::test]
async fn cbr_17_busy_pods_and_the_original_destination_are_never_ejected() {
    let logs = capture_logs();
    for error in ["overloaded", "queue-timeout"] {
        let ok = ModePod::start(Mode::Ok("ok")).await;
        let busy = ModePod::start(Mode::Refuse {
            error,
            names: None,
            origin: None,
        })
        .await;
        let state = mesh_state(
            &[cart(&[ok.addr, busy.addr]).outlier(30, 3, 2, 100)],
            |_| {},
        );
        let f = fixture_mesh(dst(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        for n in 0..60 {
            assert_eq!(status(&mut client, "GET", "/").await, 200, "request {n}");
        }
        assert!(busy.heads() > 5, "the busy pod was tried: {}", busy.heads());
        let scrape = f.metrics.encode();
        assert_eq!(ejections(&scrape, "consecutive_failures"), 0.0, "{scrape}");
        assert_eq!(ejections(&scrape, "consecutive_5xx"), 0.0, "{scrape}");
        assert!(
            !scrape.contains("mimic_mesh_ejections_total{"),
            "no series: {scrape}"
        );
    }
    // A stale view sends every request to the original destination: no pod, no outcome.
    let cluster = ModePod::start(Mode::Status(500)).await;
    let state = mesh_state(&[cart_on(cluster.addr, &[]).outlier(30, 1, 1, 100)], |c| {
        c.stale_after = Duration::from_millis(1)
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        assert_eq!(status(&mut client, "GET", "/").await, 500);
    }
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
}

#[tokio::test]
async fn cbr_18_consecutive_5xx_is_off_by_default_and_ejects_when_set() {
    let _logs = capture_logs();
    let ok = ModePod::start(Mode::Ok("ok")).await;
    let five = ModePod::start(Mode::Status(500)).await;
    let state = mesh_state(&[cart(&[ok.addr, five.addr])], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut fives = 0;
    for _ in 0..60 {
        if status(&mut client, "GET", "/").await == 500 {
            fives += 1;
        }
    }
    assert_eq!(fives, five.heads(), "every 500 reaches the app");
    assert!(five.heads() > 5, "{}", five.heads());
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));

    let ok = ModePod::start(Mode::Ok("ok")).await;
    let five = ModePod::start(Mode::Status(500)).await;
    let state = mesh_state(&[cart(&[ok.addr, five.addr]).outlier(30, 3, 5, 50)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..60 {
        status(&mut client, "GET", "/").await;
    }
    assert_eq!(five.heads(), 3, "three 5xx, then none");
    assert_eq!(ejections(&f.metrics.encode(), "consecutive_5xx"), 1.0);
}

#[tokio::test]
async fn cbr_18_a_refusal_naming_another_pod_is_the_5xx_of_the_pod_that_sent_it() {
    let logs = capture_logs();
    let (b, c) = (
        ModePod::start(Mode::Ok("b")).await,
        ModePod::start(Mode::Ok("c")).await,
    );
    let a = ModePod::start(Mode::Refuse {
        error: "overloaded",
        names: Some(b.addr),
        origin: None,
    })
    .await;
    let state = mesh_state(
        &[cart(&[a.addr, b.addr, c.addr]).outlier(30, 3, 5, 50)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..60 {
        status(&mut client, "GET", "/").await;
    }
    assert_eq!(a.heads(), 3, "three 5xx, then none");
    let ejected_lines = lines(&logs, "endpoint ejected").await;
    assert_eq!(ejected_lines.len(), 1, "{}", logs.text());
    assert_eq!(ejected_lines[0]["endpoint"], a.addr.to_string());
    assert_eq!(ejected_lines[0]["reason"], "consecutive_5xx");
}

#[tokio::test]
async fn cbr_19_a_trusted_app_refusal_ejects_its_pod_and_a_forged_one_nothing() {
    let logs = capture_logs();
    let ok = ModePod::start(Mode::Ok("ok")).await;
    let refusing = ModePod::start(Mode::Refuse {
        error: "connect-failed",
        names: None,
        origin: Some("inbound"),
    })
    .await;
    let state = mesh_state(&[cart(&[ok.addr, refusing.addr])], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..60 {
        assert_eq!(status(&mut client, "GET", "/").await, 200, "request {n}");
    }
    assert_eq!(refusing.heads(), 5, "five refusals, then none");
    assert_eq!(ejections(&f.metrics.encode(), "consecutive_failures"), 1.0);
    // A peer that forges the three fields bound to another pod's address.
    let (a, b) = (
        ModePod::start(Mode::Ok("a")).await,
        ModePod::start(Mode::Ok("b")).await,
    );
    let forger = ModePod::start(Mode::Refuse {
        error: "connect-failed",
        names: Some(a.addr),
        origin: Some("inbound"),
    })
    .await;
    let state = mesh_state(
        &[cart(&[a.addr, b.addr, forger.addr]).outlier(30, 0, 2, 100)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..60 {
        status(&mut client, "GET", "/").await;
    }
    assert!(forger.heads() > 5, "{}", forger.heads());
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
    let ejected_lines = lines(&logs, "endpoint ejected").await;
    assert_eq!(ejected_lines.len(), 1, "only the first Service ejected");
}

#[tokio::test]
async fn cbr_20_at_most_the_allowed_share_is_ejected_and_a_single_pod_never() {
    let _logs = capture_logs();
    // Two resetting pods of 2 at 50 %: one is ejected.
    let (x, y) = (
        ModePod::start(Mode::Reset).await,
        ModePod::start(Mode::Reset).await,
    );
    let state = mesh_state(&[cart(&[x.addr, y.addr])], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..40 {
        let _ = status_or_closed(&mut client).await;
        client = H1Client::connect(f.addr).await;
    }
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_failures"), 1.0, "{scrape}");
    assert_eq!(ejected(&scrape, "shop/cart:80"), 1.0, "{scrape}");

    // A single resetting pod is never ejected: M2 path, then the ClusterIP.
    let cluster = Pod::http1("cluster", 0).await;
    let only = ModePod::start(Mode::Reset).await;
    let state = mesh_state(&[cart_on(cluster.addr, &[only.addr])], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    for _ in 0..8 {
        let mut client = H1Client::connect(f.addr).await;
        assert_ne!(status_or_closed(&mut client).await, Some(200));
    }
    assert!(only.heads() >= 8, "it keeps its tries: {}", only.heads());
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
}

#[tokio::test]
async fn cbr_20_with_every_pod_ejected_nothing_is_tried_and_an_l4_connection_is_closed() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (x, y) = (
        ModePod::start(Mode::Reset).await,
        ModePod::start(Mode::Reset).await,
    );
    let state = mesh_state(
        &[cart_on(cluster.addr, &[x.addr, y.addr]).outlier(30, 0, 5, 100)],
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let label = format!("shop/cart:{}", cluster.addr.port());
    for _ in 0..30 {
        let mut client = H1Client::connect(f.addr).await;
        let _ = status_or_closed(&mut client).await;
    }
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_failures"), 2.0, "{scrape}");
    assert_eq!(ejected(&scrape, &label), 2.0, "{scrape}");
    let heads = (x.heads(), y.heads());
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = client.get("/").await;
    let head = head.to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 503"), "{head}");
    assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
    assert!(head.contains("x-mimic-error: breaker-open"), "{head}");
    assert!(
        !head.contains("x-mimic-endpoint") && body.is_empty(),
        "{head}"
    );
    assert_eq!(
        (x.heads(), y.heads()),
        heads,
        "no try while all are ejected"
    );
    assert_eq!(cluster.counts.requests(), 0, "the ClusterIP sees nothing");
    let warned = lines(&logs, "breaker open").await;
    assert_eq!(warned.len(), 1, "{}", logs.text());
    assert_eq!(warned[0]["cause"], "all_ejected");
    assert_eq!(warned[0]["service"], "shop/cart");
}

#[tokio::test]
async fn cbr_21_views_restore_and_forget_ejected_pods() {
    let logs = capture_logs();
    let ok = [
        ModePod::start(Mode::Ok("a")).await,
        ModePod::start(Mode::Ok("b")).await,
    ];
    let bad = [
        ModePod::start(Mode::Reset).await,
        ModePod::start(Mode::Reset).await,
    ];
    let pods = [ok[0].addr, ok[1].addr, bad[0].addr, bad[1].addr];
    let state = mesh_state(&[cart(&pods)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..400 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        if ejected(&f.metrics.encode(), "shop/cart:80") == 2.0 {
            break;
        }
    }
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 2.0);
    // The view takes the port down to the two ejected pods: one is restored at once.
    state.hold(
        view_services(&[cart(&[bad[0].addr, bad[1].addr])]),
        "\"2-a\"",
    );
    let restored = lines(&logs, "endpoint restored").await;
    assert_eq!(restored.len(), 1, "{}", logs.text());
    assert_eq!(restored[0]["cause"], "max_percent");
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 1.0);
    // A view that sets the percent to 0 restores every ejected pod.
    state.hold(
        view_services(&[cart(&[bad[0].addr, bad[1].addr]).outlier(30, 0, 5, 0)]),
        "\"3-a\"",
    );
    assert_eq!(lines(&logs, "endpoint restored").await.len(), 2);
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 0.0);
    // An ejected pod that leaves the view drops from the gauge, without a line.
    state.hold(
        view_services(&[cart(&[bad[0].addr, bad[1].addr])]),
        "\"3b-a\"",
    );
    for _ in 0..40 {
        let _ = status_or_closed(&mut client).await;
        client = H1Client::connect(f.addr).await;
    }
    assert!(lines(&logs, "endpoint ejected").await.len() >= 3);
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 1.0);
    let before = lines(&logs, "endpoint restored").await.len();
    state.hold(view_services(&[cart(&[ok[0].addr])]), "\"4-a\"");
    state.tick();
    assert_eq!(ejected(&f.metrics.encode(), "shop/cart:80"), 0.0);
    assert_eq!(
        lines(&logs, "endpoint restored").await.len(),
        before,
        "an endpoint that left writes no line"
    );
}

#[tokio::test]
async fn cbr_23_no_retry_goes_to_an_ejected_pod_and_the_last_ejection_falls_back() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let pods = [
        closed_port().await,
        closed_port().await,
        closed_port().await,
    ];
    let state = mesh_state(
        &[cart_on(cluster.addr, &pods).outlier(30, 0, 1, 100)],
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "cluster");
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_failures"), 3.0, "{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total{", &[]),
        1.0,
        "{scrape}"
    );
    assert_eq!(
        state.window_retries_for_test("shop/cart"),
        3,
        "the budget counts what it counts without ejection"
    );
}

#[tokio::test]
async fn cbr_22_copies_to_a_resetting_mirror_pod_eject_it_and_never_reach_it_again() {
    let _logs = capture_logs();
    let primary = ModePod::start(Mode::Ok("primary")).await;
    let healthy = ModePod::start(Mode::Ok("mirror")).await;
    let bad = ModePod::start(Mode::Reset).await;
    let mut shop = cart(&[]);
    shop.routes = r#"{"name":"shop/mirror","rules":[{"backends":[{"port":80,"service":"shop/cart-v1","weight":1}],"filters":{"mirrors":[{"denominator":1,"numerator":1,"port":80,"service":"shop/mirror"}]},"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#.into();
    let services = [
        shop,
        Svc::new("shop/cart-v1", "", 80, &[primary.addr]).headless(),
        Svc::new("shop/mirror", "", 80, &[healthy.addr, bad.addr]).headless(),
    ];
    let state = mesh_state(&services, |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let copies = |f: &common::Fixture| {
        metric(
            &f.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["backend=\"shop/mirror:80\""],
        )
    };
    let mut sent = 0.0;
    for _ in 0..120 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        sent += 1.0;
        while copies(&f) < sent {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if bad.heads() >= 5 {
            break;
        }
    }
    assert_eq!(bad.heads(), 5, "five failed copies");
    assert_eq!(
        metric(
            &f.metrics.encode(),
            "mimic_mesh_ejections_total{",
            &["backend=\"shop/mirror:80\""],
        ),
        1.0,
        "counted on the mirror Service port"
    );
    for _ in 0..30 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        sent += 1.0;
        while copies(&f) < sent {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    assert_eq!(bad.heads(), 5, "no copy reaches it while it is ejected");
}

#[tokio::test]
async fn flt_16_with_every_pod_ejected_a_drawn_delay_is_neither_waited_nor_counted() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (x, y) = (
        ModePod::start(Mode::Reset).await,
        ModePod::start(Mode::Reset).await,
    );
    let delayed = r#"[{"delay":{"fixedMs":300,"percent":100},"match":{"path":{"type":"Exact","value":"/d"}}}]"#;
    let cart = cart_on(cluster.addr, &[x.addr, y.addr])
        .outlier(30, 0, 5, 100)
        .faults(delayed);
    let state = mesh_state(&[cart], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let label = format!("shop/cart:{}", cluster.addr.port());
    for _ in 0..30 {
        let mut client = H1Client::connect(f.addr).await;
        let _ = status_or_closed(&mut client).await;
    }
    let scrape = f.metrics.encode();
    assert_eq!(ejected(&scrape, &label), 2.0, "{scrape}");
    let mut client = H1Client::connect(f.addr).await;
    let began = Instant::now();
    let (head, _) = client.get("/d").await;
    let took = began.elapsed();
    assert!(
        head.to_ascii_lowercase()
            .contains("x-mimic-error: breaker-open"),
        "{head}"
    );
    assert!(took < Duration::from_millis(150), "{took:?}");
    let scrape = f.metrics.encode();
    assert!(
        !scrape
            .lines()
            .any(|l| l.starts_with("mimic_mesh_faults_total{")),
        "{scrape}"
    );
    logs.settled().await;
    let line = logs
        .access()
        .into_iter()
        .find(|l| l["path"] == "/d")
        .expect("the access line of /d");
    assert!(
        line.get("fault").is_none_or(serde_json::Value::is_null),
        "fault {}",
        line["fault"]
    );
}

#[tokio::test]
async fn cbr_22_a_mirror_pod_answering_500_is_not_ejected_and_all_ejected_copies_are_errors() {
    let _logs = capture_logs();
    let primary = ModePod::start(Mode::Ok("primary")).await;
    let five = ModePod::start(Mode::Status(500)).await;
    let bad = ModePod::start(Mode::Reset).await;
    let rules = |mirror: &str| {
        format!(
            r#"{{"name":"shop/mirror","rules":[{{"backends":[{{"port":80,"service":"shop/cart-v1","weight":1}}],"filters":{{"mirrors":[{{"denominator":1,"numerator":1,"port":80,"service":"{mirror}"}}]}},"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}]}}]}}"#
        )
    };
    let mut shop = cart(&[]);
    shop.routes = rules("shop/five");
    let state = mesh_state(
        &[
            shop,
            Svc::new("shop/cart-v1", "", 80, &[primary.addr]).headless(),
            Svc::new("shop/five", "", 80, &[five.addr])
                .headless()
                .outlier(30, 1, 5, 100),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 1..=20 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        while metric(
            &f.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["result=\"sent\""],
        ) < f64::from(n)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    assert_eq!(five.heads(), 20, "every copy reached it");
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));

    let mut shop = cart(&[]);
    shop.routes = rules("shop/bad");
    let state = mesh_state(
        &[
            shop,
            Svc::new("shop/cart-v1", "", 80, &[primary.addr]).headless(),
            Svc::new("shop/bad", "", 80, &[bad.addr])
                .headless()
                .outlier(30, 0, 5, 100),
        ],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 1..=12 {
        assert_eq!(status(&mut client, "GET", "/").await, 200);
        while metric(
            &f.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["result=\"error\""],
        ) < f64::from(n)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    assert_eq!(bad.heads(), 5, "ejected after five; the others are errors");
}

#[tokio::test]
async fn cbr_22_an_l4_balanced_service_ejects_a_refusing_pod_and_sends_it_nothing() {
    let _logs = capture_logs();
    let echo = EchoPod::start().await;
    let state = mesh_state(&[cart(&[echo.addr, closed_port().await])], |c| {
        c.avoid_failed_for = Duration::ZERO
    });
    let f = fixture_mesh(dst(), &state, None).await;
    let failed = |f: &common::Fixture| {
        metric(
            &f.metrics.encode(),
            "mimic_mesh_attempts_total{",
            &["result=\"connect_failed\""],
        )
    };
    for _ in 0..200 {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(b"\x00\x01binary").await.unwrap();
        let mut got = [0u8; 8];
        c.read_exact(&mut got).await.unwrap();
        if failed(&f) >= 5.0 {
            break;
        }
    }
    assert_eq!(failed(&f), 5.0, "ejected after five failed connects");
    assert_eq!(ejections(&f.metrics.encode(), "consecutive_failures"), 1.0);
    for _ in 0..30 {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(b"\x00\x01binary").await.unwrap();
        let mut got = [0u8; 8];
        c.read_exact(&mut got).await.unwrap();
    }
    assert_eq!(failed(&f), 5.0, "no connection while it is ejected");
}

#[tokio::test]
async fn cbr_20_an_l4_connection_with_every_pod_ejected_is_closed_without_a_byte() {
    let _logs = capture_logs();
    let cluster = EchoPod::start().await;
    let state = mesh_state(
        &[
            cart_on(cluster.addr, &[closed_port().await, closed_port().await])
                .outlier(30, 0, 1, 100),
        ],
        |c| c.avoid_failed_for = Duration::ZERO,
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    // The first connection ejects both pods and ends on the original destination.
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"\x00\x01binary").await.unwrap();
    let mut got = [0u8; 8];
    c.read_exact(&mut got).await.unwrap();
    let label = format!("shop/cart:{}", cluster.addr.port());
    assert_eq!(ejected(&f.metrics.encode(), &label), 2.0);
    let accepted = cluster.accepted();
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let _ = c.write_all(b"\x00\x01binary").await;
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut rest))
        .await
        .expect("closed in time");
    assert!(rest.is_empty(), "not a byte");
    assert_eq!(cluster.accepted(), accepted, "no fallback to the ClusterIP");
}

#[tokio::test]
async fn cbr_24_series_are_bounded_by_the_view_and_carry_no_request_data() {
    let logs = capture_logs();
    let ok = ModePod::start(Mode::Ok("ok")).await;
    let bad = ModePod::start(Mode::Reset).await;
    let mut many = Svc::new("shop/many", "127.0.0.2", 1000, &[ok.addr]);
    many.ports = (1000..1300u16).map(|p| (p, vec![ok.addr])).collect();
    let state = mesh_state(&[cart(&[ok.addr, bad.addr]), many], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let series = |f: &common::Fixture, name: &str| {
        f.metrics
            .encode()
            .lines()
            .filter(|l| l.starts_with(name))
            .count()
    };
    assert_eq!(series(&f, "mimic_mesh_ejected_endpoints{"), 301);
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..60 {
        let path = "/canary-path?canary-query=1";
        client
            .send(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: cart.shop.svc:80\r\nx-canary: canary-header\r\n\r\n"
                )
                .as_bytes(),
            )
            .await;
        client.read().await.expect("an answer");
    }
    assert!(series(&f, "mimic_mesh_ejections_total{") <= 2);
    let scrape = f.metrics.encode();
    let mut text = logs.text();
    logs.settled().await;
    text.push_str(&logs.text());
    for needle in ["canary-path", "canary-query", "canary-header"] {
        let hits = text
            .lines()
            .filter(|l| l.contains("endpoint ejected") || l.contains("endpoint restored"))
            .filter(|l| l.contains(needle))
            .count();
        assert_eq!(
            hits, 0,
            "a line of the outlier detection holds request data"
        );
        assert!(!scrape.contains(needle), "a label holds request data");
    }
    // The 300 ports leave: their series stay for 5 minutes, then go.
    state.hold(view_services(&[cart(&[ok.addr, bad.addr])]), "\"2-a\"");
    assert_eq!(series(&f, "mimic_mesh_ejected_endpoints{"), 301);
    state.evict(Instant::now() + Duration::from_secs(5 * 60 + 1));
    assert_eq!(series(&f, "mimic_mesh_ejected_endpoints{"), 1);
}

/// An `h2c` server that resets every stream with `reason`.
async fn h2_refuser(reason: h2::Reason, requests: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                let Ok(mut conn) = h2::server::handshake(stream).await else {
                    return;
                };
                while let Some(Ok((_, mut respond))) = conn.accept().await {
                    requests.fetch_add(1, Ordering::SeqCst);
                    respond.send_reset(reason);
                }
            });
        }
    });
    addr
}

/// An `h2c` server written frame by frame: on the first `HEADERS` it sends `GOAWAY` with a
/// last-stream-id of 0 (nothing was processed) and keeps the connection open.
async fn h2_goaway(requests: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut preface = [0u8; 24];
                if stream.read_exact(&mut preface).await.is_err() {
                    return;
                }
                let _ = stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await;
                loop {
                    let mut head = [0u8; 9];
                    if stream.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let len = usize::from(head[0]) << 16
                        | usize::from(head[1]) << 8
                        | usize::from(head[2]);
                    let mut payload = vec![0u8; len];
                    if stream.read_exact(&mut payload).await.is_err() {
                        return;
                    }
                    if head[3] == 1 {
                        requests.fetch_add(1, Ordering::SeqCst);
                        let ack = [0, 0, 0, 4, 1, 0, 0, 0, 0];
                        let goaway = [0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                        let _ = stream.write_all(&ack).await;
                        let _ = stream.write_all(&goaway).await;
                    }
                }
            });
        }
    });
    addr
}

type H2Sender = hyper::client::conn::http2::SendRequest<Empty<Bytes>>;

async fn h2_client(addr: SocketAddr) -> H2Sender {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    sender
}

/// One `GET` over `h2c`: the status, or the error.
async fn h2_status(sender: &mut H2Sender) -> Result<u16, String> {
    let request = hyper::Request::get("http://cart.shop.svc/")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let status = response.status().as_u16();
    response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(status)
}

#[tokio::test]
async fn cbr_15_over_h2c_a_resetting_pod_is_ejected_after_five_and_restored_after_its_time() {
    let logs = capture_logs();
    let (a, b) = (Pod::h2c("a", 0, None).await, Pod::h2c("b", 0, None).await);
    let resets = Arc::new(AtomicUsize::new(0));
    let bad = h2_refuser(h2::Reason::INTERNAL_ERROR, resets.clone()).await;
    let state = mesh_state(&[cart(&[a.addr, b.addr, bad]).outlier(1, 0, 5, 50)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut sender = h2_client(f.addr).await;
    for n in 0..100 {
        assert_eq!(h2_status(&mut sender).await, Ok(200), "request {n}");
    }
    assert_eq!(resets.load(Ordering::SeqCst), 5, "five heads, then none");
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_failures"), 1.0, "{scrape}");
    assert_eq!(ejected(&scrape, "shop/cart:80"), 1.0, "{scrape}");
    let ejected_lines = lines(&logs, "endpoint ejected").await;
    assert_eq!(ejected_lines.len(), 1, "{}", logs.text());
    assert_eq!(ejected_lines[0]["endpoint"], bad.to_string());
    assert_eq!(ejected_lines[0]["ejection_seconds"], 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    state.tick();
    let restored = lines(&logs, "endpoint restored").await;
    assert_eq!(restored.len(), 1, "{}", logs.text());
    assert_eq!(restored[0]["cause"], "elapsed");
    for n in 0..200 {
        assert_eq!(h2_status(&mut sender).await, Ok(200), "request {n}");
        if resets.load(Ordering::SeqCst) >= 10 {
            break;
        }
    }
    assert_eq!(resets.load(Ordering::SeqCst), 10, "five more once restored");
    let again = lines(&logs, "endpoint ejected").await;
    assert_eq!(again[1]["ejection_seconds"], 2);
}

#[tokio::test]
async fn cbr_17_refused_streams_and_goaways_are_never_ejected() {
    let logs = capture_logs();
    for goaway in [false, true] {
        let seen = Arc::new(AtomicUsize::new(0));
        let refuser = if goaway {
            h2_goaway(seen.clone()).await
        } else {
            h2_refuser(h2::Reason::REFUSED_STREAM, seen.clone()).await
        };
        let ok = Pod::h2c("ok", 0, None).await;
        let state = mesh_state(&[cart(&[ok.addr, refuser]).outlier(30, 1, 2, 100)], |_| {});
        let f = fixture_mesh(dst(), &state, None).await;
        let mut sender = h2_client(f.addr).await;
        for n in 0..200 {
            assert_eq!(h2_status(&mut sender).await, Ok(200), "request {n}");
        }
        assert!(
            seen.load(Ordering::SeqCst) > 5,
            "tried: {}",
            seen.load(Ordering::SeqCst)
        );
        let scrape = f.metrics.encode();
        assert!(
            !scrape.contains("mimic_mesh_ejections_total{"),
            "goaway {goaway}: {scrape}"
        );
    }
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
}

#[tokio::test]
async fn cbr_17_a_route_timeout_is_never_ejected() {
    let logs = capture_logs();
    let slow = Pod::http1("slow", 3000).await;
    let timed = r#"{"name":"shop/timed","rules":[{"backends":[{"port":80,"service":"shop/cart","weight":1}],"matches":[{"path":{"type":"PathPrefix","value":"/"}}],"timeouts":{"backendRequestMs":50}}]}"#;
    let mut shop = cart(&[slow.addr]).outlier(30, 1, 1, 100);
    shop.routes = timed.into();
    let state = mesh_state(&[shop], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..30 {
        assert_eq!(status(&mut client, "GET", "/").await, 504, "request {n}");
    }
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
}

/// A pod that answers only once it has the whole `content-length` body of a request, and counts
/// the connections that ended before that (the sidecar cut the request short).
struct BodyPod {
    addr: SocketAddr,
    heads: Arc<AtomicUsize>,
    cut: Arc<AtomicUsize>,
}

impl BodyPod {
    async fn start() -> BodyPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (heads, cut) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (counted, ended) = (heads.clone(), cut.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (counted, ended) = (counted.clone(), ended.clone());
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 8192];
                    // The body bytes still due before the answer.
                    let mut due: Option<usize> = None;
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                            _ => {
                                if due.is_some() || !pending.is_empty() {
                                    ended.fetch_add(1, Ordering::SeqCst);
                                }
                                return;
                            }
                        }
                        if due.is_none()
                            && let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let head = String::from_utf8_lossy(&pending[..p]).to_ascii_lowercase();
                            pending.drain(..p + 4);
                            counted.fetch_add(1, Ordering::SeqCst);
                            due = Some(
                                head.split("content-length: ")
                                    .nth(1)
                                    .and_then(|v| v.split("\r\n").next()?.trim().parse().ok())
                                    .unwrap_or(0),
                            );
                        }
                        if let Some(n) = due
                            && pending.len() >= n
                        {
                            pending.drain(..n);
                            due = None;
                            let answer = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                            if stream.write_all(answer.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        BodyPod { addr, heads, cut }
    }
}

#[tokio::test]
async fn cbr_17_a_request_the_app_abandons_is_never_ejected() {
    let logs = capture_logs();
    // The pod answers only with the whole body: an abandoned upload ends in a request that was
    // written, partly, and got no head, the case `AppLeft` keeps from counting as a `reset`.
    let pod = BodyPod::start().await;
    let state = mesh_state(&[cart(&[pod.addr]).outlier(30, 1, 1, 100)], |_| {});
    let f = fixture_mesh(dst(), &state, None).await;
    for n in 1..=30 {
        let mut leaving = TcpStream::connect(f.addr).await.unwrap();
        leaving
            .write_all(
                b"POST / HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 10\r\n\r\nabc",
            )
            .await
            .unwrap();
        for _ in 0..500 {
            if pod.heads.load(Ordering::SeqCst) >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            pod.heads.load(Ordering::SeqCst),
            n,
            "the pod got request {n}"
        );
        drop(leaving);
        // The pod saw the connection cut in the middle of the body.
        for _ in 0..500 {
            if pod.cut.load(Ordering::SeqCst) >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            pod.cut.load(Ordering::SeqCst),
            n,
            "request {n} was cut mid-body"
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!f.metrics.encode().contains("mimic_mesh_ejections_total{"));
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
    // The pod is still a candidate: a whole request is answered by it.
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"POST / HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 2\r\n\r\nhi")
        .await;
    let (head, body) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200") && body == b"ok", "{head}");
}

#[tokio::test]
async fn cbr_18_the_answers_of_the_caller_sidecar_count_on_no_pod() {
    let logs = capture_logs();
    // Every pod refuses to connect: the caller's `connect-failed` answers are no `5xx` of a pod.
    let state = mesh_state(
        &[cart(&[closed_port().await]).outlier(30, 3, 100, 100)],
        |_| {},
    );
    let f = fixture_mesh(dst(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..20 {
        assert_eq!(status(&mut client, "GET", "/").await, 503);
    }
    let scrape = f.metrics.encode();
    assert_eq!(ejections(&scrape, "consecutive_5xx"), 0.0, "{scrape}");
    assert!(lines(&logs, "endpoint ejected").await.is_empty());
}

#[tokio::test]
async fn cbr_20_after_the_first_ejection_ends_requests_are_tried_again() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (x, y) = (
        ModePod::start(Mode::Reset).await,
        ModePod::start(Mode::Reset).await,
    );
    let state = mesh_state(
        &[cart_on(cluster.addr, &[x.addr, y.addr]).outlier(1, 0, 5, 100)],
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    for _ in 0..30 {
        let mut client = H1Client::connect(f.addr).await;
        let _ = status_or_closed(&mut client).await;
    }
    let heads = x.heads() + y.heads();
    assert_eq!(
        status(&mut H1Client::connect(f.addr).await, "GET", "/").await,
        503
    );
    assert_eq!(x.heads() + y.heads(), heads, "nothing tried while ejected");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let _ = status_or_closed(&mut H1Client::connect(f.addr).await).await;
    assert!(x.heads() + y.heads() > heads, "tried again");
}

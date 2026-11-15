//! RTE-83 (the GET part), RTE-84, RTE-86 to RTE-89 and RTE-92: copies of requests without a body
//! sent to the mirrors of a rule (spec §7.9 "Routing" "Mirrors", §12.26).

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::mesh_pods::{H1Client, Pod, RawPod, cluster_ip};
use common::{capture_logs, closed_port, fixture_mesh};
use http_body_util::{BodyExt, Empty};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const POLICY: &str = r#""policy":{"loadBalancer":{"slowStartSeconds":0},"retries":3,"retryBudget":{"minPerSecond":10,"percent":20,"windowSeconds":10}}"#;

/// A Service of the view: its pods, its ClusterIPs and the routes it carries.
struct Svc {
    key: String,
    port: u16,
    ips: Vec<String>,
    pods: Vec<SocketAddr>,
    routes: String,
}

fn svc(key: &str, pods: &[SocketAddr]) -> Svc {
    Svc {
        key: key.into(),
        port: 80,
        ips: vec![],
        pods: pods.to_vec(),
        routes: String::new(),
    }
}

impl Svc {
    fn ip(mut self, ip: &str) -> Svc {
        self.ips = vec![ip.into()];
        self
    }

    fn port(mut self, port: u16) -> Svc {
        self.port = port;
        self
    }

    /// One route, `shop/mirror`, of these rules.
    fn rules(mut self, rules: &[String]) -> Svc {
        self.routes = format!(
            r#","routes":[{{"name":"shop/mirror","rules":[{}]}}]"#,
            rules.join(",")
        );
        self
    }

    fn json(&self) -> String {
        let pods: Vec<String> = self
            .pods
            .iter()
            .map(|p| format!(r#"{{"ip":"{}","port":{},"ready":true}}"#, p.ip(), p.port()))
            .collect();
        let ips: Vec<String> = self.ips.iter().map(|i| format!("\"{i}\"")).collect();
        format!(
            r#""{key}":{{"endpoints":{{"{port}":[{pods}]}},"generations":{{}},"ips":[{ips}],{POLICY},"ports":{{"{port}":{{"targetPort":8080}}}}{routes}}}"#,
            key = self.key,
            port = self.port,
            pods = pods.join(","),
            ips = ips.join(","),
            routes = self.routes,
        )
    }
}

fn view_of(services: &[Svc], mesh: bool) -> View {
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":{mesh},"mode":"none","services":{{{}}}}}"#,
        services.iter().map(Svc::json).collect::<Vec<_>>().join(",")
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

fn view(services: &[Svc]) -> View {
    view_of(services, true)
}

/// A mirror of every request (100/100) to port 80 of `service`.
fn mirror(service: &str) -> String {
    mirror_of(service, 80, (100, 100), false)
}

fn mirror_of(service: &str, port: u16, (n, d): (u32, u32), before: bool) -> String {
    let before = if before {
        r#""beforeModifier":true,"#
    } else {
        ""
    };
    format!(r#"{{{before}"denominator":{d},"numerator":{n},"port":{port},"service":"{service}"}}"#)
}

const MODIFIER: &str = r#"{"remove":["x-debug"],"set":[{"name":"x-env","value":"prod"}]}"#;

/// A rule on `path` to `backend` with these mirrors, request modifier and timeouts.
fn rule(
    path: &str,
    backend: &str,
    mirrors: &[String],
    modifier: Option<&str>,
    timeouts: Option<&str>,
) -> String {
    let mut filters = vec![];
    if !mirrors.is_empty() {
        filters.push(format!(r#""mirrors":[{}]"#, mirrors.join(",")));
    }
    if let Some(m) = modifier {
        filters.push(format!(r#""requestHeaders":{m}"#));
    }
    let filters = if filters.is_empty() {
        String::new()
    } else {
        format!(r#","filters":{{{}}}"#, filters.join(","))
    };
    let timeouts = timeouts.map_or_else(String::new, |t| format!(r#","timeouts":{t}"#));
    format!(
        r#"{{"backends":[{{"port":80,"service":"{backend}","weight":1}}]{filters},"matches":[{{"path":{{"type":"PathPrefix","value":"{path}"}}}}]{timeouts}}}"#
    )
}

/// The Service the app dials, routed to `shop/cart-v1` (`primary`), with one rule on `/`.
fn routed(primary: SocketAddr, mirrors: &[Svc], rule: String) -> View {
    routed_on(80, primary, mirrors, rule)
}

/// Like [`routed`], the Service the app dials is on `port`.
fn routed_on(port: u16, primary: SocketAddr, mirrors: &[Svc], rule: String) -> View {
    let mut all = vec![
        svc("shop/cart", &[])
            .ip("127.0.0.1")
            .port(port)
            .rules(&[rule]),
        svc("shop/cart-v1", &[primary]),
    ];
    all.extend(mirrors.iter().map(|m| Svc {
        key: m.key.clone(),
        port: m.port,
        ips: m.ips.clone(),
        pods: m.pods.clone(),
        routes: m.routes.clone(),
    }));
    view(&all)
}

fn state_with(view: View, configure: impl FnOnce(&mut MeshConfig)) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(7),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view, "\"1-a\"");
    state
}

fn metric(metrics: &str, name: &str, labels: &[&str]) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn mirrored(f: &common::Fixture, backend: &str, result: &str) -> f64 {
    metric(
        &f.metrics.encode(),
        "mimic_mesh_mirrored_total{",
        &[
            &format!("backend=\"{backend}\""),
            &format!("result=\"{result}\""),
        ],
    )
}

/// Waits until `cond` holds, for at most 10 s; how long that took.
async fn until(what: &str, mut cond: impl FnMut() -> bool) -> Duration {
    let began = Instant::now();
    while !cond() {
        assert!(began.elapsed() < Duration::from_secs(10), "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    began.elapsed()
}

const GET: &str = "GET /cart?a=1 HTTP/1.1\r\nHost: cart.shop.svc:80\r\nX-Debug: 1\r\nX-Keep: Mixed Case\r\nAccept: */*\r\n\r\n";
const OK_OWN: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\nx-pod: own\r\n\r\nown";

/// A pod that reads and never answers.
struct SilentPod {
    addr: SocketAddr,
    open: Arc<AtomicUsize>,
}

impl SilentPod {
    async fn start() -> SilentPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let open = Arc::new(AtomicUsize::new(0));
        let o = open.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                o.fetch_add(1, Ordering::SeqCst);
                let o = o.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                    o.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        SilentPod { addr, open }
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// What an `hyper` pod saw of a request.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    conn: usize,
    method: String,
    authority: String,
    target: String,
    headers: Vec<(String, String)>,
}

/// An HTTP/1 or `h2c` pod that records the requests it gets; the paths under `hold` are held
/// that long before they are answered.
struct SeenPod {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl SeenPod {
    async fn start(h2: bool, status: u16, hold: Option<(&'static str, Duration)>) -> SeenPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut conn = 0;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                conn += 1;
                let (log, id) = (log.clone(), conn);
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    async move {
                        let target = req
                            .uri()
                            .path_and_query()
                            .map_or(String::new(), |p| p.to_string());
                        let authority = req
                            .uri()
                            .authority()
                            .map(ToString::to_string)
                            .or_else(|| {
                                req.headers()
                                    .get("host")
                                    .and_then(|h| h.to_str().ok())
                                    .map(str::to_string)
                            })
                            .unwrap_or_default();
                        log.lock().unwrap().push(Seen {
                            conn: id,
                            method: req.method().to_string(),
                            authority,
                            target: target.clone(),
                            headers: req
                                .headers()
                                .iter()
                                .filter(|(n, _)| *n != "host")
                                .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_string()))
                                .collect(),
                        });
                        if let Some((prefix, wait)) = hold
                            && target.starts_with(prefix)
                        {
                            tokio::time::sleep(wait).await;
                        }
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .body(http_body_util::Full::new(Bytes::from_static(b"seen")))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(async move {
                    if h2 {
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    } else {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                });
            }
        });
        SeenPod { addr, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// One `h2c` request to `addr`: the status, the fields and the body.
async fn h2c_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, hyper::HeaderMap, String) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut request = Request::builder()
        .method(method)
        .uri(format!("http://cart.shop.svc{path}"));
    for (n, v) in fields {
        request = request.header(*n, *v);
    }
    let response = sender
        .send_request(request.body(Empty::new()).unwrap())
        .await
        .expect("an answer");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn without_date(head: &str) -> String {
    head.lines()
        .filter(|l| !l.to_lowercase().starts_with("date:"))
        .collect::<Vec<_>>()
        .join("\n")
}

// RTE-83: the copy on the wire.

#[tokio::test]
async fn rte_83_an_http1_get_is_copied_with_the_modified_fields() {
    let logs = capture_logs();
    let primary = RawPod::start(|_| OK_OWN.to_vec()).await;
    let shadow = RawPod::start(|_| {
        b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 6\r\nx-pod: shadow\r\n\r\nshadow"
            .to_vec()
    })
    .await;
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                Some(MODIFIER),
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.send(GET.as_bytes()).await;
    let (head, body) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("x-pod: own") && body == b"own", "{head}");
    until("the mirror got the copy", || shadow.counts.requests() == 1).await;
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    let (copy, original) = (shadow.received(), primary.received());
    assert_eq!(
        String::from_utf8_lossy(&copy),
        String::from_utf8_lossy(&original),
        "the copy is the request the backend got"
    );
    let wire = String::from_utf8_lossy(&copy).to_lowercase();
    assert!(wire.starts_with("get /cart?a=1 http/1.1\r\n"), "{wire}");
    assert!(wire.contains("host: cart.shop.svc:80"), "{wire}");
    assert!(
        wire.contains("x-env: prod") && !wire.contains("x-debug"),
        "{wire}"
    );
    assert!(wire.contains("X-Keep: Mixed Case".to_lowercase().as_str()));
    assert!(String::from_utf8_lossy(&copy).contains("X-Keep: Mixed Case\r\n"));
    assert_eq!(shadow.counts.requests(), 1, "exactly one request");
    let entry = &logs.access_exactly(1).await[0];
    assert_eq!(entry["mirrored"], true, "{entry}");
    assert_eq!(entry["route"], "shop/mirror", "{entry}");
    assert_eq!(entry["backend"], "shop/cart-v1:80", "{entry}");
    assert_eq!(entry["upstream"], primary.addr.to_string(), "{entry}");
}

#[tokio::test]
async fn rte_83_a_mirror_before_the_modifier_gets_the_fields_as_the_app_sent_them() {
    let _logs = capture_logs();
    let primary = RawPod::start(|_| OK_OWN.to_vec()).await;
    let shadow = RawPod::start(|_| b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec()).await;
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror_of("shop/cart-shadow", 80, (100, 100), true)],
                Some(MODIFIER),
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.send(GET.as_bytes()).await;
    client.read().await.expect("an answer");
    until("the copy", || shadow.counts.requests() == 1).await;
    assert_eq!(
        String::from_utf8_lossy(&shadow.received()),
        GET,
        "as the app sent it"
    );
    let backend = String::from_utf8_lossy(&primary.received()).into_owned();
    assert!(
        backend.contains("x-env: prod") && !backend.contains("X-Debug"),
        "{backend}"
    );
}

#[tokio::test]
async fn rte_83_what_a_mirror_answers_changes_nothing_and_is_not_retried() {
    let _logs = capture_logs();
    let primary = RawPod::start(|_| OK_OWN.to_vec()).await;
    let own_addr: Arc<OnceLock<SocketAddr>> = Arc::default();
    let reply = {
        let own_addr = own_addr.clone();
        let n = AtomicUsize::new(0);
        move |_: &[u8]| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                return b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n".to_vec();
            }
            let me = own_addr.get().map(ToString::to_string).unwrap_or_default();
            format!(
                "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nx-mimic-endpoint: {me}\r\ncontent-length: 0\r\n\r\n"
            )
            .into_bytes()
        }
    };
    let shadow = RawPod::start(reply).await;
    own_addr.set(shadow.addr).unwrap();
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for i in 1..=2 {
        let (head, body) = client.get("/x").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "own");
        until("the copy is counted", || {
            mirrored(&f, "shop/cart-shadow:80", "sent") == f64::from(i)
        })
        .await;
    }
    assert_eq!(shadow.counts.requests(), 2, "never retried");
    assert_eq!(primary.counts.requests(), 2);
    let scrape = f.metrics.encode();
    assert_eq!(metric(&scrape, "mimic_mesh_retries_total{", &[]), 0.0);
    assert_eq!(metric(&scrape, "mimic_mesh_attempts_total{", &[]), 2.0);
}

#[tokio::test]
async fn rte_83_an_h2c_get_is_copied_with_the_modified_fields() {
    let _logs = capture_logs();
    let primary = SeenPod::start(true, 200, None).await;
    let shadow = SeenPod::start(true, 500, None).await;
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                Some(MODIFIER),
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (status, _, body) = h2c_request(
        f.addr,
        "GET",
        "/cart?a=1",
        &[("x-debug", "1"), ("x-keep", "k")],
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "seen"));
    until("the copy", || shadow.seen().len() == 1).await;
    until("counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    let (copy, original) = (shadow.seen()[0].clone(), primary.seen()[0].clone());
    assert_eq!(
        (&copy.method, &copy.authority, &copy.target, &copy.headers),
        (
            &original.method,
            &original.authority,
            &original.target,
            &original.headers
        ),
    );
    assert_eq!(copy.authority, "cart.shop.svc");
    assert_eq!(copy.target, "/cart?a=1");
    assert!(copy.headers.contains(&("x-env".into(), "prod".into())));
    assert!(copy.headers.contains(&("x-keep".into(), "k".into())));
    assert!(!copy.headers.iter().any(|(n, _)| n == "x-debug"));
}

// RTE-84: mirrors never slow the primary.

enum Misbehaving {
    Silent,
    Slow,
    Resetting,
    Refusing,
    NoEndpoint,
}

impl Misbehaving {
    const ALL: [Misbehaving; 5] = [
        Misbehaving::Silent,
        Misbehaving::Slow,
        Misbehaving::Resetting,
        Misbehaving::Refusing,
        Misbehaving::NoEndpoint,
    ];

    /// The pods of the mirror Service, and what keeps them alive.
    async fn pods(&self) -> (Vec<SocketAddr>, Box<dyn std::any::Any>) {
        match self {
            Misbehaving::Silent => {
                let pod = SilentPod::start().await;
                (vec![pod.addr], Box::new(pod))
            }
            Misbehaving::Slow => {
                let pod = Pod::http1("slow", 2000).await;
                (vec![pod.addr], Box::new(pod))
            }
            Misbehaving::Resetting => {
                let pod = RawPod::start(|_| vec![]).await;
                (vec![pod.addr], Box::new(pod))
            }
            Misbehaving::Refusing => (vec![closed_port().await], Box::new(())),
            Misbehaving::NoEndpoint => (vec![], Box::new(())),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Misbehaving::Silent => "silent",
            Misbehaving::Slow => "slow",
            Misbehaving::Resetting => "resetting",
            Misbehaving::Refusing => "refusing",
            Misbehaving::NoEndpoint => "no endpoint",
        }
    }
}

/// Ten sequential requests through a rule to a primary of 20 ms: the heads without `date`, the
/// bodies, the durations, and the access lines.
async fn ten(
    mirrors: Option<(Vec<SocketAddr>, MeshConfig)>,
) -> (Vec<(String, Vec<u8>)>, Vec<Duration>) {
    let logs = capture_logs();
    let primary = Pod::http1("own", 20).await;
    let with_mirror = mirrors.is_some();
    let (services, config) = match mirrors {
        Some((pods, config)) => (vec![svc("shop/cart-shadow", &pods)], config),
        None => (vec![], MeshConfig::default()),
    };
    let mirrors_json = if services.is_empty() {
        vec![]
    } else {
        vec![mirror("shop/cart-shadow")]
    };
    let state = state_with(
        routed(
            primary.addr,
            &services,
            rule("/", "shop/cart-v1", &mirrors_json, None, None),
        ),
        |c| {
            *c = MeshConfig {
                enabled: true,
                seed: Some(7),
                ..config
            }
        },
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (mut answers, mut took) = (vec![], vec![]);
    for _ in 0..10 {
        let began = Instant::now();
        let (head, body) = client.get("/x").await;
        took.push(began.elapsed());
        answers.push((without_date(&head), body.into_bytes()));
    }
    for entry in logs.access_exactly(10).await {
        assert_eq!(entry["attempts"], 1, "{entry}");
        assert_eq!(entry["upstream"], primary.addr.to_string(), "{entry}");
    }
    if with_mirror {
        until("every copy is counted", || {
            metric(&f.metrics.encode(), "mimic_mesh_mirrored_total{", &[]) == 10.0
        })
        .await;
    }
    (answers, took)
}

fn median(mut d: Vec<Duration>) -> Duration {
    d.sort();
    d[d.len() / 2]
}

#[tokio::test]
async fn rte_84_a_misbehaving_mirror_changes_nothing_for_the_primary() {
    let (base, base_took) = ten(None).await;
    for kind in Misbehaving::ALL {
        let (pods, _keep) = kind.pods().await;
        let config = MeshConfig {
            mirror_timeout: Duration::from_millis(300),
            ..MeshConfig::default()
        };
        let (answers, took) = ten(Some((pods, config))).await;
        assert_eq!(
            answers,
            base,
            "the {} mirror changed an answer",
            kind.label()
        );
        assert!(
            median(took.clone()) <= median(base_took.clone()) + Duration::from_millis(50),
            "the {} mirror slowed the primary: {took:?} against {base_took:?}",
            kind.label()
        );
    }
}

#[tokio::test]
async fn rte_84_copies_end_at_their_deadline_or_at_once() {
    for kind in Misbehaving::ALL {
        let _logs = capture_logs();
        let (pods, _keep) = kind.pods().await;
        let primary = Pod::http1("own", 0).await;
        let state = state_with(
            routed(
                primary.addr,
                &[svc("shop/cart-shadow", &pods)],
                rule(
                    "/",
                    "shop/cart-v1",
                    &[mirror("shop/cart-shadow")],
                    None,
                    None,
                ),
            ),
            |c| c.mirror_timeout = Duration::from_millis(300),
        );
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        let began = Instant::now();
        let (head, _) = client.get("/x").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let took = until("the copy ended", || {
            mirrored(&f, "shop/cart-shadow:80", "error") == 1.0
        })
        .await;
        let took = began.elapsed().max(took);
        match kind {
            Misbehaving::Silent | Misbehaving::Slow => assert!(
                took >= Duration::from_millis(300) && took < Duration::from_millis(900),
                "the {} copy ended after {took:?}",
                kind.label()
            ),
            _ => assert!(
                took < Duration::from_millis(250),
                "the {} copy ended after {took:?}",
                kind.label()
            ),
        }
        assert_eq!(mirrored(&f, "shop/cart-shadow:80", "sent"), 0.0);
    }
}

#[tokio::test]
async fn rte_84_a_mirror_answering_after_50_ms_is_sent() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 50).await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |c| c.mirror_timeout = Duration::from_millis(300),
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    H1Client::connect(f.addr).await.get("/x").await;
    until("sent", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "error"), 0.0);
}

#[tokio::test]
async fn rte_84_a_primary_that_times_out_has_its_copy_sent() {
    let _logs = capture_logs();
    let (primary, shadow) = (
        Pod::http1("own", 5000).await,
        Pod::http1("shadow", 150).await,
    );
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                Some(r#"{"requestMs":600}"#),
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/x").await;
    assert!(head.starts_with("HTTP/1.1 504"), "{head}");
    until("sent", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "error"), 0.0);
}

// RTE-86: slots.

/// `n` concurrent `GET /x` on their own connections; all must be answered `200`.
async fn burst(addr: SocketAddr, n: usize) {
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            tokio::spawn(async move {
                let (head, _) = H1Client::connect(addr).await.get("/x").await;
                assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test]
async fn rte_86_copies_in_flight_are_bounded_per_mirror_port() {
    let _logs = capture_logs();
    let (primary, silent) = (Pod::http1("own", 0).await, SilentPod::start().await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[silent.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |c| {
            c.mirror_max_in_flight = 4;
            c.mirror_timeout = Duration::from_millis(1000);
        },
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    burst(f.addr, 10).await;
    until("4 copies are in flight", || silent.open() == 4).await;
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "skipped_limit"), 6.0);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_endpoint_in_flight{", &["cart-shadow"]),
        4.0
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(silent.open(), 4, "never more than the slots");
    // The copies end at their deadline: the slots are free again.
    until("the four copies ended", || {
        mirrored(&f, "shop/cart-shadow:80", "error") == 4.0
    })
    .await;
    H1Client::connect(f.addr).await.get("/x").await;
    until("the next request is mirrored", || {
        mirrored(&f, "shop/cart-shadow:80", "error") == 5.0
    })
    .await;
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "skipped_limit"), 6.0);
}

#[tokio::test]
async fn rte_86_copies_in_flight_are_bounded_in_total() {
    let _logs = capture_logs();
    let (primary, a, b) = (
        Pod::http1("own", 0).await,
        SilentPod::start().await,
        SilentPod::start().await,
    );
    let state = state_with(
        routed(
            primary.addr,
            &[
                svc("shop/shadow-a", &[a.addr]),
                svc("shop/shadow-b", &[b.addr]),
            ],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/shadow-a"), mirror("shop/shadow-b")],
                None,
                None,
            ),
        ),
        |c| {
            c.mirror_max_in_flight_total = 6;
            c.mirror_timeout = Duration::from_millis(1000);
        },
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    burst(f.addr, 10).await;
    let mut peak = 0;
    let began = Instant::now();
    while began.elapsed() < Duration::from_millis(500) {
        peak = peak.max(a.open() + b.open());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(peak, 6, "at most the total, and the total is used");
    let skipped = mirrored(&f, "shop/shadow-a:80", "skipped_limit")
        + mirrored(&f, "shop/shadow-b:80", "skipped_limit");
    assert_eq!(skipped, 14.0, "20 drawn, 6 slots");
}

// RTE-87: share and scope.

#[tokio::test]
async fn rte_87_a_share_of_the_requests_is_mirrored() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror_of("shop/cart-shadow", 80, (10, 100), false)],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10_000 {
        client.get("/x").await;
    }
    let seen = shadow.counts.requests();
    until("every copy ended", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == shadow.counts.requests() as f64
    })
    .await;
    let sent = mirrored(&f, "shop/cart-shadow:80", "sent");
    assert!((800.0..=1200.0).contains(&sent), "{sent} of 10000 mirrored");
    assert!(sent >= seen as f64);
    assert_eq!(shadow.counts.requests() as f64, sent, "the pod agrees");
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "skipped_limit"), 0.0);
}

#[tokio::test]
async fn rte_87_a_zero_share_mirrors_nothing_and_two_mirrors_each_get_a_copy() {
    let logs = capture_logs();
    let (primary, zero, a, b) = (
        Pod::http1("own", 0).await,
        Pod::http1("zero", 0).await,
        Pod::http1("a", 0).await,
        Pod::http1("b", 0).await,
    );
    let state = state_with(
        routed(
            primary.addr,
            &[
                svc("shop/zero", &[zero.addr]),
                svc("shop/shadow-a", &[a.addr]),
                svc("shop/shadow-b", &[b.addr]),
            ],
            rule(
                "/",
                "shop/cart-v1",
                &[
                    mirror_of("shop/zero", 80, (0, 100), false),
                    mirror("shop/shadow-a"),
                    mirror("shop/shadow-b"),
                ],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..20 {
        client.get("/x").await;
    }
    until("both mirrors got 20", || {
        a.counts.requests() == 20 && b.counts.requests() == 20
    })
    .await;
    until("counted", || {
        mirrored(&f, "shop/shadow-a:80", "sent") == 20.0
            && mirrored(&f, "shop/shadow-b:80", "sent") == 20.0
    })
    .await;
    assert_eq!(zero.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert!(
        !scrape
            .lines()
            .any(|l| l.starts_with("mimic_mesh_mirrored_total{") && l.contains("shop/zero")),
        "a mirror not drawn is not counted"
    );
    let lines = logs.access_exactly(20).await;
    assert!(lines.iter().all(|l| l["mirrored"] == true));
}

#[tokio::test]
async fn rte_87_without_a_drawn_backend_nothing_is_mirrored() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let shadows = [svc("shop/cart-shadow", &[shadow.addr])];
    let mirrors = [mirror("shop/cart-shadow")];
    // No route: no rule matches `/other`.
    let no_route = routed(
        primary.addr,
        &shadows,
        rule("/matched", "shop/cart-v1", &mirrors, None, None),
    );
    // No backend: the rule has none. Invalid backend: it names a Service the view does not hold.
    let no_backend = format!(
        r#"{{"backends":[],"filters":{{"mirrors":[{}]}},"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}]}}"#,
        mirrors[0]
    );
    let invalid = format!(
        r#"{{"backends":[{{"invalid":true,"weight":1}}],"filters":{{"mirrors":[{}]}},"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}]}}"#,
        mirrors[0]
    );
    for (name, view, path, status) in [
        ("no-route", no_route, "/other", "404"),
        (
            "no-backend",
            routed(primary.addr, &shadows, no_backend),
            "/x",
            "500",
        ),
        (
            "invalid-backend",
            routed(primary.addr, &shadows, invalid),
            "/x",
            "500",
        ),
    ] {
        let state = state_with(view, |_| {});
        let f = fixture_mesh(cluster_ip(), &state, None).await;
        let (head, _) = H1Client::connect(f.addr).await.get(path).await;
        assert!(head.contains(status), "{name}: {head}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(shadow.counts.requests(), 0, "{name}");
        let scrape = f.metrics.encode();
        assert_eq!(
            metric(&scrape, "mimic_mesh_mirrored_total{", &[]),
            0.0,
            "{name}"
        );
    }
}

#[tokio::test]
async fn rte_87_a_stale_view_and_mesh_false_mirror_nothing() {
    let _logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let build = || {
        routed_on(
            cluster.addr.port(),
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        )
    };
    let state = state_with(build(), |c| c.stale_after = Duration::from_millis(500));
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // Live: the app dials the ClusterIP of `shop/cart` (127.0.0.1), on the cluster pod's port.
    let live = build();
    state.hold(live, "\"2-a\"");
    let (head, body) = client.get("/x").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "own");
    until("one copy", || shadow.counts.requests() == 1).await;
    // Stale: the original destination answers, nothing is mirrored.
    tokio::time::sleep(Duration::from_millis(700)).await;
    state.tick();
    let (_, body) = client.get("/x").await;
    assert_eq!(body, "cluster");
    // `"mesh": false`: the same.
    state.hold(view_of(&[], false), "\"3-a\"");
    let (_, body) = client.get("/x").await;
    assert_eq!(body, "cluster");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.counts.requests(), 1, "only the live request");
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "sent"), 1.0);
}

#[tokio::test]
async fn rte_87_an_upgrade_and_an_l4_connection_mirror_nothing() {
    let _logs = capture_logs();
    let (echo, shadow) = (
        common::mesh_pods::EchoPod::start().await,
        Pod::http1("shadow", 0).await,
    );
    let state = state_with(
        routed(
            echo.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |c| c.opaque_ports = std::iter::once(80).collect(),
    );
    // Port 80 is opaque here: an L4 connection, relayed by bytes.
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut c = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /x HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(2), c.read(&mut buf)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.counts.requests(), 0);
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "sent"), 0.0);
}

#[tokio::test]
async fn rte_87_a_view_without_the_mirror_applies_from_the_next_request() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 300).await);
    let with = |mirrors: &[String]| {
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule("/", "shop/cart-v1", mirrors, None, None),
        )
    };
    let state = state_with(with(&[mirror("shop/cart-shadow")]), |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/x").await;
    until("the copy is in flight", || shadow.counts.requests() == 1).await;
    state.hold(with(&[]), "\"2-a\"");
    client.get("/x").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.counts.requests(), 1, "no copy under the new view");
    until("the copy under the old view finished", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
}

// RTE-88: one try, no side effects on the primary's accounting.

#[tokio::test]
async fn rte_88_a_refused_copy_makes_one_try_and_never_the_cluster_ip() {
    let _logs = capture_logs();
    let primary = Pod::http1("own", 0).await;
    let cluster = common::mesh_pods::EchoPod::start().await;
    let dead = closed_port().await;
    // The mirror Service's ClusterIP is the echo pod: a fallback would reach it.
    let shadow = svc("shop/cart-shadow", &[dead, closed_port().await])
        .ip("127.0.0.1")
        .port(cluster.addr.port());
    let state = state_with(
        routed(
            primary.addr,
            &[shadow],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror_of(
                    "shop/cart-shadow",
                    cluster.addr.port(),
                    (100, 100),
                    false,
                )],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..5 {
        client.get("/x").await;
    }
    let backend = format!("shop/cart-shadow:{}", cluster.addr.port());
    until("five errors", || mirrored(&f, &backend, "error") == 5.0).await;
    assert_eq!(cluster.accepted(), 0, "no ClusterIP try");
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total{", &[]),
        0.0,
        "{scrape}"
    );
    assert_eq!(metric(&scrape, "mimic_mesh_retries_total{", &[]), 0.0);
}

#[tokio::test]
async fn rte_88_the_primarys_accounting_is_that_of_a_request_without_mirror() {
    let _logs = capture_logs();
    let primary = Pod::http1("own", 0).await;
    let shadow = Pod::http1("shadow", 0).await;
    let state = state_with(
        routed(
            primary.addr,
            &[
                svc("shop/cart-shadow", &[shadow.addr]),
                svc("shop/gone", &[closed_port().await]),
            ],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow"), mirror("shop/gone")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..5 {
        client.get("/x").await;
    }
    until("every copy ended", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 5.0
            && mirrored(&f, "shop/gone:80", "error") == 5.0
    })
    .await;
    let scrape = f.metrics.encode();
    let series = |name: &str| -> Vec<String> {
        scrape
            .lines()
            .filter(|l| l.starts_with(name))
            .map(str::to_string)
            .collect()
    };
    for name in [
        "mimic_mesh_requests_total{",
        "mimic_mesh_attempts_total{",
        "mimic_mesh_endpoint_requests_total{",
    ] {
        let lines = series(name);
        assert!(!lines.is_empty(), "{name}");
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("cart-shadow") && !l.contains("shop/gone")),
            "{name}: {lines:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
                .sum::<f64>(),
            5.0,
            "{name}"
        );
    }
    for name in ["mimic_mesh_retries_total{", "mimic_mesh_fallbacks_total{"] {
        assert_eq!(metric(&scrape, name, &[]), 0.0, "{name}");
    }
    assert_eq!(state.window_retries_for_test("shop/cart-shadow"), 0);
}

#[tokio::test]
async fn rte_88_a_copy_is_not_routed_again() {
    let _logs = capture_logs();
    let (primary, shadow, other, third) = (
        Pod::http1("own", 0).await,
        Pod::http1("shadow", 0).await,
        Pod::http1("other", 0).await,
        Pod::http1("third", 0).await,
    );
    // The mirror Service has its own route to `shop/other`, with a mirror to `shop/third`.
    let shadow_svc = svc("shop/cart-shadow", &[shadow.addr])
        .ip("127.0.0.2")
        .rules(&[rule("/", "shop/other", &[mirror("shop/third")], None, None)]);
    let state = state_with(
        routed(
            primary.addr,
            &[
                shadow_svc,
                svc("shop/other", &[other.addr]),
                svc("shop/third", &[third.addr]),
            ],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..5 {
        client.get("/x").await;
    }
    until("five copies", || shadow.counts.requests() == 5).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!((other.counts.requests(), third.counts.requests()), (0, 0));
    assert_eq!(mirrored(&f, "shop/third:80", "sent"), 0.0);
}

#[tokio::test]
async fn rte_88_discovery_and_the_logs_hold_the_apps_exchange_once() {
    let logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..3 {
        client.get("/only-once").await;
    }
    until("three copies", || shadow.counts.requests() == 3).await;
    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 3",
    )
    .await;
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_discovery_exchanges_total{", &[]),
        3.0,
        "{scrape}"
    );
    logs.access_exactly(3).await;
}

// RTE-89: own pools, shared endpoint state.

async fn own_pools(h2: bool) {
    let _logs = capture_logs();
    let primary = SeenPod::start(h2, 200, None).await;
    let shadow = SeenPod::start(h2, 200, Some(("/m", Duration::from_secs(3)))).await;
    let rules = [
        rule(
            "/m",
            "shop/cart-v1",
            &[mirror("shop/cart-shadow")],
            None,
            None,
        ),
        rule("/direct", "shop/cart-shadow", &[], None, None),
    ];
    let state = state_with(
        view(&[
            svc("shop/cart", &[]).ip("127.0.0.1").rules(&rules),
            svc("shop/cart-v1", &[primary.addr]),
            svc("shop/cart-shadow", &[shadow.addr]),
        ]),
        |c| c.mirror_timeout = Duration::from_millis(2000),
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    if h2 {
        let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut tasks = vec![];
        for _ in 0..256 {
            let mut sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                let request = Request::get("http://cart.shop.svc/m")
                    .body(Empty::new())
                    .unwrap();
                sender
                    .send_request(request)
                    .await
                    .unwrap()
                    .into_body()
                    .collect()
                    .await
                    .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    } else {
        let tasks: Vec<_> = (0..256)
            .map(|_| {
                let addr = f.addr;
                tokio::spawn(async move { H1Client::connect(addr).await.get("/m").await })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
    }
    until("256 copies are held", || shadow.seen().len() == 256).await;
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_endpoint_in_flight{",
            &["backend=\"shop/cart-shadow:80\""]
        ),
        256.0,
        "the copies count in the pods' in-flight"
    );
    // A primary request to the same pods is answered as without mirror.
    let began = Instant::now();
    if h2 {
        let (status, _, _) = h2c_request(f.addr, "GET", "/direct", &[]).await;
        assert_eq!(status, 200);
    } else {
        let (head, _) = H1Client::connect(f.addr).await.get("/direct").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "{:?}",
        began.elapsed()
    );
    let seen = shadow.seen();
    let mut carried: BTreeMap<usize, (bool, bool)> = BTreeMap::new();
    for s in &seen {
        let entry = carried.entry(s.conn).or_default();
        if s.target.starts_with("/m") {
            entry.0 = true;
        } else {
            entry.1 = true;
        }
    }
    assert!(
        carried.values().all(|(copy, primary)| !(*copy && *primary)),
        "a connection carried a copy and a primary request: {carried:?}"
    );
    assert!(carried.values().any(|(_, primary)| *primary));
}

#[tokio::test]
async fn rte_89_copies_never_take_the_connections_of_primary_requests_http1() {
    own_pools(false).await;
}

#[tokio::test]
async fn rte_89_copies_never_take_the_connections_of_primary_requests_h2c() {
    own_pools(true).await;
}

#[tokio::test]
async fn rte_89_a_failed_connect_of_a_copy_makes_the_next_primary_request_try_another_pod() {
    let _logs = capture_logs();
    let (primary, live) = (Pod::http1("own", 0).await, Pod::http1("live", 0).await);
    let rules = [
        rule(
            "/m",
            "shop/cart-v1",
            &[mirror("shop/cart-shadow")],
            None,
            None,
        ),
        rule("/direct", "shop/cart-shadow", &[], None, None),
    ];
    let state = state_with(
        view(&[
            svc("shop/cart", &[]).ip("127.0.0.1").rules(&rules),
            svc("shop/cart-v1", &[primary.addr]),
            svc("shop/cart-shadow", &[closed_port().await, live.addr]),
        ]),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..50 {
        client.get("/m").await;
        if mirrored(&f, "shop/cart-shadow:80", "error") >= 1.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirrored(&f, "shop/cart-shadow:80", "error") >= 1.0,
        "a copy never met the dead pod"
    );
    for _ in 0..20 {
        let (head, body) = client.get("/direct").await;
        assert!(head.starts_with("HTTP/1.1 200") && body == "live", "{head}");
    }
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_attempts_total{",
            &["result=\"connect_failed\""]
        ),
        0.0,
        "the primary requests skipped the avoided pod: {scrape}"
    );
}

// RTE-92: label bounds.

#[tokio::test]
async fn rte_92_the_series_are_per_mirror_port_and_leave_5_minutes_after_the_view_drops_them() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let mirrors = |i: usize| mirror(&format!("shop/m{i}"));
    // 19 routes of one rule each, to 16 mirrors; 300 mirror Services in all.
    let build = |with: bool| {
        let mut services = vec![svc("shop/cart-v1", &[primary.addr])];
        let mut routes = vec![];
        for r in 0..19 {
            let list: Vec<String> = (r * 16..(r * 16 + 16).min(300)).map(mirrors).collect();
            let list = if with { list } else { vec![] };
            routes.push(format!(
                r#"{{"name":"shop/r{r}","rules":[{}]}}"#,
                rule(&format!("/p{r}"), "shop/cart-v1", &list, None, None)
            ));
        }
        let mut cart = svc("shop/cart", &[]).ip("127.0.0.1");
        cart.routes = format!(r#","routes":[{}]"#, routes.join(","));
        services.push(cart);
        for i in (0..300).filter(|_| with) {
            services.push(svc(&format!("shop/m{i}"), &[shadow.addr]));
        }
        view(&services)
    };
    let state = state_with(build(true), |c| c.mirror_max_in_flight_total = 1024);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for r in 0..19 {
        let (head, _) = client.get(&format!("/p{r}?token=zzz-query")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    until("every copy ended", || {
        metric(
            &f.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["result=\"sent\""],
        ) == 300.0
    })
    .await;
    let series = |scrape: &str| -> Vec<(String, String)> {
        scrape
            .lines()
            .filter(|l| l.starts_with("mimic_mesh_mirrored_total{"))
            .map(|l| {
                let value = |key: &str| {
                    l.split(&format!("{key}=\""))
                        .nth(1)
                        .unwrap()
                        .split('"')
                        .next()
                        .unwrap()
                        .to_string()
                };
                (value("backend"), value("result"))
            })
            .collect()
    };
    let scrape = f.metrics.encode();
    let all = series(&scrape);
    let backends: std::collections::BTreeSet<_> = all.iter().map(|(b, _)| b.clone()).collect();
    assert_eq!(backends.len(), 300, "one backend per mirror Service port");
    assert!(
        all.iter()
            .all(|(_, r)| ["sent", "error", "skipped_body", "skipped_limit"].contains(&r.as_str()))
    );
    assert!(
        !scrape
            .lines()
            .filter(|l| l.starts_with("mimic_mesh_mirrored_total{"))
            .any(|l| l.contains("/p1") || l.contains("zzz-query") || l.contains("shop/r")),
        "a label carries a request or route value"
    );
    // The view drops the mirror Services: the series stay 5 minutes.
    state.hold(build(false), "\"2-a\"");
    let now = Instant::now();
    state.evict(now + Duration::from_secs(200));
    assert_eq!(series(&f.metrics.encode()).len(), 300, "kept");
    state.evict(now + Duration::from_secs(301));
    assert!(series(&f.metrics.encode()).is_empty(), "gone");
}

// RTE-87: mode `mock` mirrors nothing.

#[tokio::test]
async fn rte_87_mode_mock_mirrors_nothing() {
    let _logs = capture_logs();
    let (primary, shadow) = (Pod::http1("own", 0).await, Pod::http1("shadow", 0).await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let rig = common::mock_rig::rig_with(
        common::mock_rig::users_set(5),
        100,
        1024,
        None,
        common::mock_rig::Options {
            mode: Some(mimic_core::Mode::Discovery),
            dst: Some(cluster_ip()),
            mesh: Some(state),
            ..common::mock_rig::Options::default()
        },
    )
    .await;
    let request = b"GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
    let mut client = H1Client::connect(rig.addr).await;
    client.send(request).await;
    let (head, body) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200") && body == b"own", "{head}");
    let sent = || {
        metric(
            &rig.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["result=\"sent\""],
        )
    };
    until("the balanced request is mirrored", || {
        shadow.counts.requests() == 1 && sent() == 1.0
    })
    .await;
    rig.mode
        .observe(mimic_sidecar::mode::Announced {
            mode: Ok(mimic_core::Mode::Mock),
            factor: None,
        })
        .await;
    client.send(request).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(
        head.to_ascii_lowercase().contains("x-mimic-match"),
        "{head}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        shadow.counts.requests(),
        1,
        "the mocked request has no copy"
    );
    assert_eq!(sent(), 1.0);
    let scrape = rig.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_mirrored_total{", &[]),
        1.0,
        "nothing else is counted"
    );
}

// RTE-83 (bodies), RTE-85, RTE-90: request bodies of copies.

/// What a `BodyPod` read of one request, once its body ended.
#[derive(Clone, Debug, Default)]
struct Got {
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    trailers: Vec<(String, String)>,
}

/// An HTTP/1 or `h2c` pod that reads whole bodies and answers once it has them, after `wait`.
struct BodyPod {
    addr: SocketAddr,
    got: Arc<Mutex<Vec<Got>>>,
    /// Requests whose head came in, complete or not.
    started: Arc<AtomicUsize>,
}

impl BodyPod {
    async fn start(h2: bool, wait: Duration) -> BodyPod {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (got, started) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
        );
        let (log, count) = (got.clone(), started.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, count) = (log.clone(), count.clone());
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (log, count) = (log.clone(), count.clone());
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        let (parts, mut body) = req.into_parts();
                        let mut seen = Got {
                            method: parts.method.to_string(),
                            headers: parts
                                .headers
                                .iter()
                                .filter(|(n, _)| *n != "host")
                                .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_string()))
                                .collect(),
                            ..Got::default()
                        };
                        while let Some(frame) = body.frame().await {
                            let frame = frame.map_err(std::io::Error::other)?;
                            match frame.into_data() {
                                Ok(data) => seen.body.extend_from_slice(&data),
                                Err(frame) => {
                                    for (n, v) in frame.into_trailers().unwrap_or_default().iter() {
                                        seen.trailers
                                            .push((n.to_string(), v.to_str().unwrap().to_string()));
                                    }
                                }
                            }
                        }
                        log.lock().unwrap().push(seen);
                        tokio::time::sleep(wait).await;
                        Ok::<_, std::io::Error>(Response::new(http_body_util::Full::new(
                            Bytes::from_static(b"seen"),
                        )))
                    }
                });
                tokio::spawn(async move {
                    if h2 {
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    } else {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                });
            }
        });
        BodyPod { addr, got, started }
    }

    fn got(&self) -> Vec<Got> {
        self.got.lock().unwrap().clone()
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn post_head(extra: &str) -> Vec<u8> {
    format!("POST /up HTTP/1.1\r\nHost: cart.shop.svc:80\r\nX-Debug: 1\r\n{extra}\r\n").into_bytes()
}

/// The head and the chunked encoding of `parts`, the last one ended.
fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut wire = Vec::new();
    for part in parts {
        wire.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        wire.extend_from_slice(part);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    wire
}

/// A primary and a mirror reading bodies behind a rule with the request modifier.
async fn body_rig(
    h2: bool,
    configure: impl FnOnce(&mut MeshConfig),
) -> (BodyPod, BodyPod, common::Fixture) {
    let (primary, shadow) = (
        BodyPod::start(h2, Duration::ZERO).await,
        BodyPod::start(h2, Duration::ZERO).await,
    );
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                Some(MODIFIER),
                None,
            ),
        ),
        configure,
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    (primary, shadow, f)
}

/// Samples `mimic_mesh_retry_buffer_bytes` every millisecond; the highest value seen so far.
fn peak_buffer(f: &common::Fixture) -> Arc<std::sync::atomic::AtomicU64> {
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (metrics, top) = (f.metrics.clone(), peak.clone());
    tokio::spawn(async move {
        loop {
            let now = metric(&metrics.encode(), "mimic_mesh_retry_buffer_bytes", &[]) as u64;
            top.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    peak
}

fn buffer_bytes(f: &common::Fixture) -> f64 {
    metric(&f.metrics.encode(), "mimic_mesh_retry_buffer_bytes", &[])
}

#[tokio::test]
async fn rte_83_an_http1_post_with_a_content_length_is_copied_with_its_body() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |_| {}).await;
    let body = payload(1024);
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("Content-Length: 1024\r\n")).await;
    client.send(&body).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    let (copy, original) = (shadow.got(), primary.got());
    assert_eq!((copy.len(), original.len()), (1, 1));
    assert_eq!(copy[0].method, "POST");
    assert_eq!(copy[0].body, body);
    assert_eq!(copy[0].headers, original[0].headers);
    assert!(copy[0].headers.contains(&("x-env".into(), "prod".into())));
    assert!(!copy[0].headers.iter().any(|(n, _)| n == "x-debug"));
    assert!(
        copy[0]
            .headers
            .contains(&("content-length".into(), "1024".into()))
    );
}

#[tokio::test]
async fn rte_83_an_http1_chunked_post_is_copied_with_its_body() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |_| {}).await;
    let body = payload(1024);
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(&post_head("Transfer-Encoding: chunked\r\n"))
        .await;
    client.send(&chunked(&[&body[..300], &body[300..]])).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    let (copy, original) = (shadow.got(), primary.got());
    assert_eq!(copy[0].body, body);
    assert_eq!(copy[0].headers, original[0].headers);
    assert!(
        copy[0]
            .headers
            .contains(&("transfer-encoding".into(), "chunked".into()))
    );
}

/// One `h2c` POST with `parts` as DATA frames and then `trailers`; the status.
async fn h2c_post(addr: SocketAddr, parts: Vec<Vec<u8>>, trailers: &[(&str, &str)]) -> StatusCode {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, http_body_util::StreamBody<
            futures::stream::Iter<
                std::vec::IntoIter<Result<hyper::body::Frame<Bytes>, std::convert::Infallible>>,
            >,
        >>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut frames: Vec<_> = parts
        .into_iter()
        .map(|p| Ok(hyper::body::Frame::data(Bytes::from(p))))
        .collect();
    if !trailers.is_empty() {
        let mut map = hyper::HeaderMap::new();
        for (n, v) in trailers {
            map.insert(
                hyper::header::HeaderName::from_bytes(n.as_bytes()).unwrap(),
                (*v).parse().unwrap(),
            );
        }
        frames.push(Ok(hyper::body::Frame::trailers(map)));
    }
    let request = Request::builder()
        .method("POST")
        .uri("http://cart.shop.svc/up")
        .header("x-debug", "1")
        .body(http_body_util::StreamBody::new(futures::stream::iter(
            frames,
        )))
        .unwrap();
    let response = sender.send_request(request).await.expect("an answer");
    let status = response.status();
    let _ = response.into_body().collect().await;
    status
}

#[tokio::test]
async fn rte_83_an_h2c_post_is_copied_with_its_body_and_trailers() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(true, |_| {}).await;
    let body = payload(1024);
    let status = h2c_post(
        f.addr,
        vec![body[..400].to_vec(), body[400..].to_vec()],
        &[("x-sum", "abc")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    let (copy, original) = (shadow.got(), primary.got());
    assert_eq!(copy[0].body, body);
    assert_eq!(
        copy[0].trailers,
        vec![("x-sum".to_string(), "abc".to_string())]
    );
    assert_eq!(copy[0].trailers, original[0].trailers);
    assert_eq!(copy[0].headers, original[0].headers);
    assert!(copy[0].headers.contains(&("x-env".into(), "prod".into())));
}

#[tokio::test]
async fn rte_85_a_body_of_exactly_the_buffer_is_mirrored_byte_for_byte() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |c| c.retry_buffer = 64 * 1024).await;
    let body = payload(64 * 1024);
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("Content-Length: 65536\r\n")).await;
    client.send(&body).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    assert_eq!(shadow.got()[0].body, body);
    assert_eq!(primary.got()[0].body, body);
    until("the buffer is given back", || buffer_bytes(&f) == 0.0).await;
}

#[tokio::test]
async fn rte_85_a_declared_body_over_the_buffer_is_skipped_at_the_head() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |c| c.retry_buffer = 64 * 1024).await;
    let peak = peak_buffer(&f);
    let body = payload(64 * 1024 + 1);
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("Content-Length: 65537\r\n")).await;
    assert!(
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") <= 1.0,
        "counted at most once"
    );
    until("skipped at the head", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    client.send(&body).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(primary.got()[0].body, body, "the primary has every byte");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.started(), 0);
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "sent"), 0.0);
    assert_eq!(peak.load(Ordering::SeqCst), 0, "nothing was held for it");
}

#[tokio::test]
async fn rte_85_a_chunked_body_is_skipped_once_it_passes_the_buffer() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |c| c.retry_buffer = 64 * 1024).await;
    let body = payload(80_000);
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(&post_head("Transfer-Encoding: chunked\r\n"))
        .await;
    client
        .send(&chunked(&[&body[..40_000], &body[40_000..]]))
        .await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(primary.got()[0].body, body, "the primary has every byte");
    until("skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    assert_eq!(shadow.started(), 0);
    until("the buffer is given back", || buffer_bytes(&f) == 0.0).await;
}

#[tokio::test]
async fn rte_85_a_body_is_skipped_when_the_total_is_held_by_other_requests() {
    let _logs = capture_logs();
    let (silent, shadow) = (
        SilentPod::start().await,
        BodyPod::start(false, Duration::ZERO).await,
    );
    let main = rule(
        "/",
        "shop/cart-v1",
        &[mirror("shop/cart-shadow")],
        None,
        None,
    );
    let hold = rule("/hold", "shop/silent", &[], None, None);
    let state = state_with(
        routed(
            silent.addr,
            &[
                svc("shop/cart-shadow", &[shadow.addr]),
                svc("shop/silent", &[silent.addr]),
            ],
            format!("{hold},{main}"),
        ),
        |c| {
            c.retry_buffer = 64 * 1024;
            c.retry_buffer_total = 100 * 1024;
        },
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body = payload(64 * 1024);
    let mut holder = H1Client::connect(f.addr).await;
    holder
        .send(b"POST /hold HTTP/1.1\r\nHost: cart.shop.svc:80\r\nContent-Length: 65536\r\n\r\n")
        .await;
    holder.send(&body).await;
    until("the total is in use", || buffer_bytes(&f) >= 64.0 * 1024.0).await;
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("Content-Length: 65536\r\n")).await;
    client.send(&body).await;
    until("skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    assert_eq!(shadow.started(), 0);
}

#[tokio::test]
async fn rte_85_a_client_that_leaves_in_the_middle_of_a_chunked_upload_sends_nothing() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(false, |_| {}).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(&post_head("Transfer-Encoding: chunked\r\n"))
        .await;
    client.send(b"3e8\r\n").await;
    client.send(&payload(1000)).await;
    until("the primary began", || primary.started() == 1).await;
    drop(client);
    until("skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.started(), 0);
    until("the buffer is given back", || buffer_bytes(&f) == 0.0).await;
}

#[tokio::test]
async fn rte_85_an_h2c_client_that_resets_its_stream_sends_nothing() {
    let _logs = capture_logs();
    let (primary, shadow, f) = body_rig(true, |_| {}).await;
    let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    type Frames = futures::stream::Chain<
        futures::stream::Iter<
            std::vec::IntoIter<Result<hyper::body::Frame<Bytes>, std::convert::Infallible>>,
        >,
        futures::stream::Pending<Result<hyper::body::Frame<Bytes>, std::convert::Infallible>>,
    >;
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, http_body_util::StreamBody<Frames>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    use futures::StreamExt;
    let frames = futures::stream::iter(vec![Ok(hyper::body::Frame::data(Bytes::from(payload(
        500,
    ))))])
    .chain(futures::stream::pending());
    let request = Request::builder()
        .method("POST")
        .uri("http://cart.shop.svc/up")
        .body(http_body_util::StreamBody::new(frames))
        .unwrap();
    let call = tokio::spawn(async move { sender.send_request(request).await });
    until("the primary began", || primary.started() == 1).await;
    call.abort();
    until("skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    assert_eq!(shadow.started(), 0);
}

#[tokio::test]
async fn rte_85_a_body_that_has_not_ended_at_the_deadline_is_skipped() {
    let _logs = capture_logs();
    let (primary, shadow, f) =
        body_rig(false, |c| c.mirror_timeout = Duration::from_millis(500)).await;
    let mut client = H1Client::connect(f.addr).await;
    let began = Instant::now();
    client
        .send(&post_head("Transfer-Encoding: chunked\r\n"))
        .await;
    client.send(b"5\r\nhello\r\n").await;
    until("the primary began", || primary.started() == 1).await;
    let took = until("skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    let took = took.max(began.elapsed());
    assert!(
        took >= Duration::from_millis(450) && took < Duration::from_millis(1200),
        "skipped after {took:?}"
    );
    // The client finishes late: nothing is sent, nothing is counted twice.
    client.send(b"0\r\n\r\n").await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow.started(), 0);
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "skipped_body"), 1.0);
    until("the buffer is given back", || buffer_bytes(&f) == 0.0).await;
}

#[tokio::test]
async fn rte_85_without_a_buffer_a_get_is_sent_and_a_post_with_a_body_is_skipped() {
    let _logs = capture_logs();
    let (_primary, shadow, f) = body_rig(false, |c| c.retry_buffer = 0).await;
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("")).await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the GET is sent", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    client.send(&post_head("Content-Length: 5\r\n")).await;
    client.send(b"hello").await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the POST with a body is skipped", || {
        mirrored(&f, "shop/cart-shadow:80", "skipped_body") == 1.0
    })
    .await;
    assert_eq!(shadow.got().len(), 1, "only the request without a body");
}

#[tokio::test]
async fn rte_85_three_mirrors_and_a_retried_primary_hold_one_copy_of_the_body() {
    let _logs = capture_logs();
    let primary = BodyPod::start(false, Duration::ZERO).await;
    let shadows = [
        BodyPod::start(false, Duration::ZERO).await,
        BodyPod::start(false, Duration::ZERO).await,
        BodyPod::start(false, Duration::ZERO).await,
    ];
    let services = [
        svc("shop/shadow-a", &[shadows[0].addr]),
        svc("shop/shadow-b", &[shadows[1].addr]),
        svc("shop/shadow-c", &[shadows[2].addr]),
    ];
    let dead = closed_port().await;
    let mut all = vec![
        svc("shop/cart", &[]).ip("127.0.0.1").rules(&[rule(
            "/",
            "shop/cart-v1",
            &[
                mirror("shop/shadow-a"),
                mirror("shop/shadow-b"),
                mirror("shop/shadow-c"),
            ],
            None,
            None,
        )]),
        svc("shop/cart-v1", &[dead, primary.addr]),
    ];
    all.extend(services);
    let state = state_with(view(&all), |c| {
        c.avoid_failed_for = Duration::from_millis(1)
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let peak = peak_buffer(&f);
    let body = payload(32 * 1024);
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        client.send(&post_head("Content-Length: 32768\r\n")).await;
        client.send(&body).await;
        let (head, _) = client.read().await.expect("an answer");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        // The next request starts once this one's copies are done, so that peaks do not add up.
        until("the buffer is back to 0", || buffer_bytes(&f) == 0.0).await;
    }
    for shadow in &shadows {
        until("every copy arrived", || shadow.got().len() == 10).await;
    }
    let scrape = f.metrics.encode();
    assert!(
        metric(&scrape, "mimic_mesh_retries_total", &[]) >= 1.0,
        "a try was retried"
    );
    let top = peak.load(Ordering::SeqCst);
    assert!(
        (32 * 1024..=64 * 1024).contains(&top),
        "the peak was {top}: one buffer of at most the request's share, not four copies"
    );
    until("the buffer is back to 0", || buffer_bytes(&f) == 0.0).await;
}

#[tokio::test]
async fn rte_90_a_client_that_goes_away_after_its_body_ended_does_not_cancel_the_copy() {
    let _logs = capture_logs();
    let primary = BodyPod::start(false, Duration::from_millis(500)).await;
    let shadow = BodyPod::start(false, Duration::from_millis(300)).await;
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[shadow.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body = payload(2048);
    let mut client = H1Client::connect(f.addr).await;
    client.send(&post_head("Content-Length: 2048\r\n")).await;
    client.send(&body).await;
    until("the primary has the body", || primary.got().len() == 1).await;
    drop(client);
    until("the copy is counted", || {
        mirrored(&f, "shop/cart-shadow:80", "sent") == 1.0
    })
    .await;
    assert_eq!(shadow.got()[0].body, body, "the mirror has the whole body");
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "skipped_body"), 0.0);
}

#[tokio::test]
async fn rte_90_pending_copies_leave_no_timer_slot_connection_or_byte() {
    let _logs = capture_logs();
    let (primary, silent) = (Pod::http1("own", 0).await, SilentPod::start().await);
    let state = state_with(
        routed(
            primary.addr,
            &[svc("shop/cart-shadow", &[silent.addr])],
            rule(
                "/",
                "shop/cart-v1",
                &[mirror("shop/cart-shadow")],
                None,
                None,
            ),
        ),
        |c| {
            c.mirror_max_in_flight = 1024;
            c.mirror_timeout = Duration::from_millis(500);
        },
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let addr = f.addr;
            tokio::spawn(async move {
                let mut client = H1Client::connect(addr).await;
                for _ in 0..50 {
                    client
                        .send(b"POST /x HTTP/1.1\r\nHost: cart.shop.svc:80\r\nContent-Length: 4\r\n\r\nbody")
                        .await;
                    let (head, _) = client.read().await.expect("an answer");
                    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    // Every primary ended: what is armed is at most one timer per copy still in flight.
    assert!(state.armed_timers_for_test() <= 1000);
    until("the copies ended", || {
        mirrored(&f, "shop/cart-shadow:80", "error") == 1000.0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(state.armed_timers_for_test(), 0);
    assert_eq!(buffer_bytes(&f), 0.0);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_endpoint_in_flight{", &["cart-shadow"]),
        0.0
    );
    assert_eq!(silent.open(), 0, "every copy connection was closed");
}

#[tokio::test]
async fn rte_90_sigterm_starts_no_copy_and_does_not_wait_for_the_pending_ones() {
    let _logs = capture_logs();
    let (primary, shadow) = (
        BodyPod::start(false, Duration::ZERO).await,
        BodyPod::start(false, Duration::ZERO).await,
    );
    let silent = SilentPod::start().await;
    let rules = format!(
        "{},{}",
        rule(
            "/silent",
            "shop/cart-v1",
            &[mirror("shop/silent")],
            None,
            None
        ),
        rule(
            "/",
            "shop/cart-v1",
            &[mirror("shop/cart-shadow")],
            None,
            None
        )
    );
    let state = state_with(
        routed(
            primary.addr,
            &[
                svc("shop/cart-shadow", &[shadow.addr]),
                svc("shop/silent", &[silent.addr]),
            ],
            rules,
        ),
        |c| c.mirror_timeout = Duration::from_secs(30),
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    // A copy in flight to a silent mirror, and a copy that waits for the end of its body.
    let mut silent_client = H1Client::connect(f.addr).await;
    silent_client
        .send(b"GET /silent HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await;
    silent_client.read().await.expect("an answer");
    until("a copy is in flight", || silent.open() == 1).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(&post_head("Transfer-Encoding: chunked\r\n"))
        .await;
    client.send(b"5\r\nhello\r\n").await;
    until("the primary began", || primary.started() == 1).await;
    f.shared.shutdown();
    // The body ends after the signal: no copy starts.
    client.send(b"0\r\n\r\n").await;
    let _ = client.read().await;
    drop((client, silent_client));
    let began = Instant::now();
    f.shared.drain(Duration::from_secs(1)).await;
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "the pending copies did not hold the drain"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(shadow.started(), 0, "no copy starts after the signal");
    assert_eq!(mirrored(&f, "shop/cart-shadow:80", "sent"), 0.0);
}

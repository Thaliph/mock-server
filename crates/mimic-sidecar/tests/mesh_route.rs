//! RTE-47 to RTE-53: the sidecar routes the requests of a Service port that has routes
//! (spec §7.9 "Routing"): the weighted draw, the match and its update, the answers of the
//! sidecar, the header modifiers on the wire and where routes do not apply (spec §12.24).

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::mesh_pods::{H1Client, Pod, RawPod, cluster_ip, h2c_calls};
use common::mock_rig::{Options, Rig, rig_with, users_set};
use common::{capture_logs, closed_port, fixture_mesh, fixture_mesh_dst};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_core::mesh::view::{Limits, View};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::mode::Announced;
use tokio::net::TcpListener;

/// A Service of a view: key, ClusterIP, port and pods.
struct Svc<'a> {
    key: &'a str,
    ip: &'a str,
    port: u16,
    pods: Vec<SocketAddr>,
    /// The `routes` member (a JSON array), for the Service the app calls.
    routes: &'a str,
    /// A second port of the Service, with no endpoint.
    also: Option<u16>,
    retries: u32,
    /// `percent`, `minPerSecond`, `windowSeconds` of the retry budget.
    budget: (u32, u32, u32),
    /// No ClusterIP: `ips` is empty.
    headless: bool,
}

fn svc<'a>(key: &'a str, ip: &'a str, port: u16, pods: &[SocketAddr]) -> Svc<'a> {
    Svc {
        key,
        ip,
        port,
        pods: pods.to_vec(),
        routes: "",
        also: None,
        retries: 3,
        budget: (20, 10, 10),
        headless: false,
    }
}

fn view(services: &[Svc]) -> View {
    let entries: Vec<String> = services
        .iter()
        .map(|s| {
            let pods: Vec<String> = s
                .pods
                .iter()
                .map(|p| format!(r#"{{"ip":"{}","port":{},"ready":true}}"#, p.ip(), p.port()))
                .collect();
            let routes = if s.routes.is_empty() {
                String::new()
            } else {
                format!(r#","routes":{}"#, s.routes)
            };
            let (more_endpoints, more_ports) = s.also.map_or_else(Default::default, |also| {
                (
                    format!(r#","{also}":[]"#),
                    format!(r#","{also}":{{"targetPort":8081}}"#),
                )
            });
            format!(
                r#""{key}":{{"endpoints":{{"{port}":[{pods}]{more_endpoints}}},"generations":{{}},"ips":[{ip}],"policy":{{"loadBalancer":{{"slowStartSeconds":0}},"retries":{retries},"retryBudget":{{"minPerSecond":{min},"percent":{percent},"windowSeconds":{window}}}}},"ports":{{"{port}":{{"targetPort":8080}}{more_ports}}}{routes}}}"#,
                key = s.key,
                port = s.port,
                pods = pods.join(","),
                ip = if s.headless {
                    String::new()
                } else {
                    format!("\"{}\"", s.ip)
                },
                retries = s.retries,
                percent = s.budget.0,
                min = s.budget.1,
                window = s.budget.2,
            )
        })
        .collect();
    let text = format!(
        r#"{{"cluster":"local","format":1,"generations":{{}},"mesh":true,"mode":"none","services":{{{}}}}}"#,
        entries.join(",")
    );
    View::from_json(&text, &Limits::default()).unwrap()
}

fn backend(service: &str, weight: u32) -> String {
    backend_on(service, 80, weight)
}

fn backend_on(service: &str, port: u16, weight: u32) -> String {
    format!(r#"{{"port":{port},"service":"{service}","weight":{weight}}}"#)
}

fn rule(matches: &str, backends: &[String], filters: &str) -> String {
    let filters = if filters.is_empty() {
        String::new()
    } else {
        format!(r#","filters":{filters}"#)
    };
    format!(
        r#"{{"backends":[{}]{filters},"matches":[{matches}]}}"#,
        backends.join(",")
    )
}

fn route(name: &str, ports: &str, rules: &[String]) -> String {
    format!(
        r#"{{"name":"{name}"{ports},"rules":[{}]}}"#,
        rules.join(",")
    )
}

fn routes(routes: &[String]) -> String {
    format!("[{}]", routes.join(","))
}

fn state_with(view: View) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(7),
        ..MeshConfig::default()
    });
    state.hold(view, "\"1-a\"");
    state
}

const CANARY: &str = "shop/cart-canary";

fn canary(weights: (u32, u32)) -> String {
    routes(&[route(
        CANARY,
        "",
        &[rule(
            "{}",
            &[
                backend("shop/cart-v1", weights.0),
                backend("shop/cart-v2", weights.1),
            ],
            "",
        )],
    )])
}

fn metric(metrics: &str, name: &str, labels: &[&str]) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(name) && labels.iter().all(|f| l.contains(f)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn requests(metrics: &str, labels: &[&str]) -> f64 {
    metric(metrics, "mimic_mesh_requests_total{", labels)
}

fn lower(head: &str) -> String {
    head.to_lowercase()
}

/// A view of the parent `shop/cart` on `cluster_ip()` with its own pod, and `cart-v1`, `cart-v2`.
fn canary_view(own: &Pod, v1: &Pod, v2: &Pod, routes: &str) -> View {
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = routes;
    view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr]),
    ])
}

async fn pods() -> (Pod, Pod, Pod) {
    (
        Pod::http1("own", 0).await,
        Pod::http1("v1", 0).await,
        Pod::http1("v2", 0).await,
    )
}

#[tokio::test]
async fn rte_47_a_90_10_canary_splits_http1_requests_and_counts_them_per_backend() {
    let (own, v1, v2) = pods().await;
    let state = state_with(canary_view(&own, &v1, &v2, &canary((90, 10))));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10_000 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    let served = v2.counts.requests();
    assert!((800..=1200).contains(&served), "cart-v2 served {served}");
    assert_eq!(v1.counts.requests() + served, 10_000);
    assert_eq!(own.counts.requests(), 0, "the Service's own pod got some");
    let scrape = f.metrics.encode();
    let labels = |b: &str| {
        vec![
            "service=\"shop/cart\"".to_string(),
            format!("route=\"{CANARY}\""),
            format!("backend=\"shop/{b}:80\""),
        ]
    };
    let count = |b: &str| {
        let l = labels(b);
        requests(&scrape, &l.iter().map(String::as_str).collect::<Vec<_>>())
    };
    assert_eq!(count("cart-v2") as usize, served);
    assert_eq!(count("cart-v1") as usize, v1.counts.requests());
    assert_eq!(count("cart-v1") + count("cart-v2"), 10_000.0);
}

#[tokio::test]
async fn rte_47_a_90_10_canary_splits_h2c_requests() {
    let own = Pod::h2c("own", 0, None).await;
    let (v1, v2) = (Pod::h2c("v1", 0, None).await, Pod::h2c("v2", 0, None).await);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    let routes = canary((90, 10));
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr]),
    ]));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let bodies = h2c_calls(f.addr, 10_000, 32).await;
    let served = bodies.iter().filter(|b| *b == "v2").count();
    assert!((800..=1200).contains(&served), "cart-v2 served {served}");
    assert_eq!(
        bodies.iter().filter(|b| *b == "v1").count() + served,
        10_000
    );
    assert_eq!(own.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert_eq!(
        requests(&scrape, &[&format!("route=\"{CANARY}\""), "cart-v2:80"]) as usize,
        served
    );
}

#[tokio::test]
async fn rte_47_the_access_line_names_the_route_rule_backend_and_pod() {
    let logs = capture_logs();
    let (own, v1, v2) = pods().await;
    let state = state_with(canary_view(&own, &v1, &v2, &canary((50, 50))));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..20 {
        client.get("/").await;
    }
    for entry in logs.access_exactly(20).await {
        assert_eq!(entry["route"], CANARY, "{entry}");
        assert_eq!(entry["rule"], 0, "{entry}");
        let backend = entry["backend"].as_str().unwrap();
        let pod = if backend == "shop/cart-v1:80" {
            v1.addr
        } else {
            assert_eq!(backend, "shop/cart-v2:80", "{entry}");
            v2.addr
        };
        assert_eq!(entry["upstream"], pod.to_string(), "{entry}");
    }
}

fn header_rule(backend_service: &str, name: &str) -> String {
    rule(
        &format!(r#"{{"headers":[{{"name":"{name}","value":"1"}}]}}"#),
        &[backend(backend_service, 1)],
        "",
    )
}

#[tokio::test]
async fn rte_48_header_and_path_matches_pick_the_backend_and_exact_beats_prefix() {
    let (own, v1, v2) = pods().await;
    let rules = routes(&[
        route(
            "shop/by-header",
            "",
            &[
                header_rule("shop/cart-v2", "x-canary"),
                rule("{}", &[backend("shop/cart-v1", 1)], ""),
            ],
        ),
        route(
            "shop/by-path",
            "",
            &[
                rule(
                    r#"{"path":{"type":"PathPrefix","value":"/cart"}}"#,
                    &[backend("shop/cart-v1", 1)],
                    "",
                ),
                rule(
                    r#"{"path":{"type":"Exact","value":"/cart/checkout"}}"#,
                    &[backend("shop/cart-v2", 1)],
                    "",
                ),
            ],
        ),
    ]);
    let state = state_with(canary_view(&own, &v1, &v2, &rules));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..100 {
        client
            .send(b"GET /x HTTP/1.1\r\nHost: cart.shop.svc\r\nX-Canary: 1\r\n\r\n")
            .await;
        assert_eq!(client.read().await.unwrap().1, b"v2");
        assert_eq!(client.get("/x").await.1, "v1");
    }
    // The Exact path wins over the PathPrefix one, and over the header-less catch-all.
    assert_eq!(client.get("/cart/checkout").await.1, "v2");
    assert_eq!(client.get("/cart/other").await.1, "v1");
    assert_eq!(own.counts.requests(), 0);
}

#[tokio::test]
async fn rte_48_a_new_view_applies_to_the_next_request_of_the_same_connection() {
    let (own, v1, v2) = pods().await;
    let state = state_with(canary_view(&own, &v1, &v2, &canary((100, 0))));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    assert_eq!(client.get("/").await.1, "v1");
    state.hold(canary_view(&own, &v1, &v2, &canary((0, 100))), "\"2-a\"");
    for _ in 0..100 {
        assert_eq!(client.get("/").await.1, "v2");
    }
    // A view without the route balances over the Service's own pods again.
    state.hold(canary_view(&own, &v1, &v2, ""), "\"3-a\"");
    assert_eq!(client.get("/").await.1, "own");
    // A route of another port leaves port 80 as it was.
    let other = routes(&[route(
        CANARY,
        r#","ports":[81]"#,
        &[rule("{}", &[backend("shop/cart-v2", 1)], "")],
    )]);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &other;
    parent.also = Some(81);
    state.hold(
        view(&[parent, svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr])]),
        "\"4-a\"",
    );
    assert_eq!(client.get("/").await.1, "own");
    assert_eq!(v2.counts.requests(), 100);
}

#[tokio::test]
async fn rte_49_the_sidecar_answers_no_route_no_backend_and_invalid_backend() {
    let logs = capture_logs();
    let (own, v1, v2) = pods().await;
    let invalid = r#"{"invalid":true,"weight":1}"#.to_string();
    let rules = routes(&[route(
        CANARY,
        "",
        &[
            rule(r#"{"path":{"type":"Exact","value":"/nothing"}}"#, &[], ""),
            rule(
                r#"{"path":{"type":"Exact","value":"/zero"}}"#,
                &[backend("shop/cart-v1", 0), backend("shop/cart-v2", 0)],
                "",
            ),
            rule(
                r#"{"path":{"type":"Exact","value":"/half"}}"#,
                &[backend("shop/cart-v1", 1), invalid],
                "",
            ),
        ],
    )]);
    let state = state_with(canary_view(&own, &v1, &v2, &rules));
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = client.get("/other").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    assert_eq!(lower(&head).matches("x-mimic-error").count(), 1, "{head}");
    assert!(lower(&head).contains("x-mimic-error: no-route"), "{head}");
    assert!(lower(&head).contains("content-length: 0") && body.is_empty());
    for path in ["/nothing", "/zero"] {
        let (head, _) = client.get(path).await;
        assert!(head.starts_with("HTTP/1.1 500"), "{path}: {head}");
        assert!(lower(&head).contains("x-mimic-error: no-backend"), "{head}");
    }
    let mut invalids = 0;
    for _ in 0..2000 {
        let (head, _) = client.get("/half").await;
        if head.starts_with("HTTP/1.1 500") {
            assert!(
                lower(&head).contains("x-mimic-error: invalid-backend"),
                "{head}"
            );
            invalids += 1;
        } else {
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        }
    }
    assert!((900..=1100).contains(&invalids), "invalid share {invalids}");
    assert_eq!(v1.counts.requests(), 2000 - invalids, "none reached a pod");
    assert_eq!(v2.counts.requests() + own.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert_eq!(
        requests(
            &scrape,
            &["route=\"none\"", "backend=\"none\"", "reason=\"no-route\""]
        ),
        1.0
    );
    assert_eq!(
        requests(
            &scrape,
            &[
                &format!("route=\"{CANARY}\""),
                "backend=\"none\"",
                "reason=\"no-backend\""
            ]
        ),
        2.0
    );
    assert_eq!(
        requests(
            &scrape,
            &["backend=\"invalid\"", "reason=\"invalid-backend\""]
        ) as usize,
        invalids
    );
    let answers = 3 + invalids;
    common::counted(
        &f.metrics,
        &format!(
            "mimic_discovery_exchanges_total{{direction=\"outbound\",result=\"synthetic\"}} {answers}"
        ),
    )
    .await;
    let recorded = discovery.report().to_json().unwrap();
    assert!(
        !recorded.contains("\"404\"") && !recorded.contains("\"500\""),
        "an answer of the sidecar was recorded: {recorded}"
    );
    let lines = logs.access_exactly(3 + 2000).await;
    for entry in lines
        .iter()
        .filter(|l| l["status"] == 404 || l["status"] == 500)
    {
        assert!(entry.get("backend").is_none_or(|b| b.is_null()), "{entry}");
        assert!(entry.get("upstream").is_none_or(|b| b.is_null()), "{entry}");
    }
}

/// A rig in `discovery` mode whose only mesh route would answer `no-route`.
async fn mock_rig(own: &Pod, v1: &Pod, v2: &Pod, stale: Option<Duration>) -> (Rig, MeshState) {
    let rules = routes(&[route(
        CANARY,
        "",
        &[rule(
            r#"{"path":{"type":"Exact","value":"/nothing"}}"#,
            &[backend("shop/cart-v2", 1)],
            "",
        )],
    )]);
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(1),
        stale_after: stale.unwrap_or(MeshConfig::default().stale_after),
        ..MeshConfig::default()
    });
    state.hold(canary_view(own, v1, v2, &rules), "\"1-a\"");
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            mode: Some(Mode::Discovery),
            dst: Some(cluster_ip()),
            mesh: Some(state.clone()),
            ..Options::default()
        },
    )
    .await;
    (rig, state)
}

#[tokio::test]
async fn rte_52_in_mode_mock_the_mock_answers_and_no_route_is_matched() {
    let (own, v1, v2) = pods().await;
    let (rig, _state) = mock_rig(&own, &v1, &v2, None).await;
    rig.mode
        .observe(Announced {
            mode: Ok(Mode::Mock),
            factor: None,
        })
        .await;
    let mut client = H1Client::connect(rig.addr).await;
    client
        .send(b"GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n")
        .await;
    let (head, _) = client.read().await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(lower(&head).contains("x-mimic-match"), "{head}");
    assert!(!lower(&head).contains("no-route"), "{head}");
    assert_eq!(own.counts.requests() + v2.counts.requests(), 0);
}

#[tokio::test]
async fn rte_52_a_stale_view_routes_nothing_and_mesh_false_routes_nothing() {
    let (own, _, v2) = (
        Pod::http1("own", 0).await,
        Pod::http1("v1", 0).await,
        Pod::http1("v2", 0).await,
    );
    let rules = routes(&[route(
        CANARY,
        "",
        &[rule(
            r#"{"path":{"type":"Exact","value":"/nothing"}}"#,
            &[backend("shop/cart-v2", 1)],
            "",
        )],
    )]);
    // The original destination is a server of its own.
    let cluster = Pod::http1("cluster", 0).await;
    let mut parent = svc("shop/cart", "127.0.0.1", cluster.addr.port(), &[own.addr]);
    parent.routes = &rules;
    let routed = view(&[parent, svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr])]);
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(1),
        stale_after: Duration::from_millis(200),
        ..MeshConfig::default()
    });
    state.hold(routed, "\"1-a\"");
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/other").await;
    assert!(head.starts_with("HTTP/1.1 404"), "routed: {head}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    state.tick();
    for _ in 0..3 {
        let (head, body) = client.get("/other").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "cluster", "the original destination");
    }
    assert_eq!(own.counts.requests() + v2.counts.requests(), 0);

    // `"mesh": false` routes nothing: no Service, so the original destination.
    state.hold(common::mesh_pods::view_text(&[], 0, false), "\"2-a\"");
    let (head, body) = client.get("/other").await;
    assert!(
        head.starts_with("HTTP/1.1 200") && body == "cluster",
        "{head}"
    );
}

#[tokio::test]
async fn rte_52_an_upgrade_request_goes_to_a_pod_of_the_service_itself() {
    let own = RawPod::start(|head| {
        if head.starts_with(b"GET /ws") {
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
                .to_vec()
        } else {
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec()
        }
    })
    .await;
    let (_, v1, v2) = pods().await;
    let rules = routes(&[route(
        CANARY,
        "",
        &[rule(
            r#"{"path":{"type":"Exact","value":"/nothing"}}"#,
            &[backend("shop/cart-v2", 1)],
            "",
        )],
    )]);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &rules;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr]),
    ]));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // A request that routes first, so that the connection is a mesh-routed one.
    assert_eq!(client.get("/nothing").await.1, "v2");
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 101");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert_eq!(own.counts.requests(), 1, "a pod of the Service itself");
    assert_eq!(v2.counts.requests(), 1);
    assert_eq!(v1.counts.requests(), 0);
}

const MODIFIERS: &str = r#"{"requestHeaders":{"add":[{"name":"x-add","value":"a"}],"remove":["x-gone"],"set":[{"name":"x-set","value":"v"}]},"responseHeaders":{"set":[{"name":"x-resp","value":"r"}]}}"#;

/// A route whose only rule sends everything to `cart-v2` with the modifiers above.
fn modified() -> String {
    routes(&[route(
        CANARY,
        "",
        &[rule("{}", &[backend("shop/cart-v2", 1)], MODIFIERS)],
    )])
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.to_lowercase().matches(needle).count()
}

#[tokio::test]
async fn rte_50_the_backend_gets_the_head_with_the_modifiers_applied_on_http1() {
    let v2 = RawPod::start(|_| {
        b"HTTP/1.1 200 OK\r\nX-Up: 1\r\nTrailer-Like: keep\r\ncontent-length: 2\r\n\r\nok".to_vec()
    })
    .await;
    let (own, v1, _) = pods().await;
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    let routes = modified();
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr]),
    ]));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /p?q=1 HTTP/1.1\r\nHost: cart.shop.svc\r\nX-Keep: 1\r\nx-gone: g\r\nX-Set: old\r\nAccept: */*\r\n\r\n")
        .await;
    let (head, body) = client.read().await.expect("an answer");
    assert_eq!(body, b"ok");
    assert!(lower(&head).contains("x-resp: r"), "{head}");
    assert!(
        head.contains("X-Up: 1") && head.contains("Trailer-Like: keep"),
        "{head}"
    );
    let seen = String::from_utf8(v2.received()).unwrap();
    assert!(seen.starts_with("GET /p?q=1 HTTP/1.1\r\n"), "{seen}");
    assert!(seen.contains("Host: cart.shop.svc\r\n"), "{seen}");
    assert!(
        seen.contains("X-Keep: 1\r\n") && seen.contains("Accept: */*\r\n"),
        "{seen}"
    );
    assert!(
        seen.find("X-Keep").unwrap() < seen.find("Accept").unwrap(),
        "order kept: {seen}"
    );
    assert_eq!(count(&seen, "x-gone"), 0, "{seen}");
    assert_eq!(count(&seen, "x-set: v"), 1, "{seen}");
    assert_eq!(count(&seen, "x-set"), 1, "{seen}");
    assert_eq!(count(&seen, "x-add: a"), 1, "{seen}");
}

/// A server that records the head of each request.
async fn spy_h2() -> (SocketAddr, Arc<Mutex<Vec<(hyper::HeaderMap, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<(hyper::HeaderMap, String)>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let log = log.clone();
                async move {
                    let authority = req.uri().authority().map(|a| a.to_string());
                    log.lock()
                        .unwrap()
                        .push((req.headers().clone(), authority.unwrap_or_default()));
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header("x-up", "1")
                            .body(Full::new(Bytes::from("ok")))
                            .unwrap(),
                    )
                }
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn rte_50_the_backend_gets_the_head_with_the_modifiers_applied_on_h2c() {
    let (v2, seen) = spy_h2().await;
    let own = Pod::h2c("own", 0, None).await;
    let v1 = Pod::h2c("v1", 0, None).await;
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    let routes = modified();
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2]),
    ]));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let stream = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::get("http://cart.shop.svc/p?q=1")
        .header("x-keep", "1")
        .header("x-gone", "g")
        .header("x-set", "old")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.headers()["x-resp"], "r");
    assert_eq!(response.headers()["x-up"], "1");
    let _ = response.into_body().collect().await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (head, authority) = &seen[0];
    assert_eq!(head["x-keep"], "1");
    assert!(head.get("x-gone").is_none());
    assert_eq!(head.get_all("x-set").iter().count(), 1);
    assert_eq!(head["x-set"], "v");
    assert_eq!(head.get_all("x-add").iter().count(), 1);
    assert_eq!(authority, "cart.shop.svc");
}

#[tokio::test]
async fn rte_50_answers_of_the_sidecar_carry_no_modified_field_and_a_retry_adds_once() {
    let spy = RawPod::start(|_| b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()).await;
    let (own, v1, _) = pods().await;
    let dead = closed_port().await;
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    let routes = routes(&[route(
        CANARY,
        "",
        &[
            rule(
                r#"{"path":{"type":"Exact","value":"/dead"}}"#,
                &[backend("shop/cart-v2", 1)],
                MODIFIERS,
            ),
            rule(
                r#"{"path":{"type":"Exact","value":"/retry"}}"#,
                &[backend("shop/cart-v1", 1)],
                MODIFIERS,
            ),
        ],
    )]);
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[dead, spy.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[dead]),
    ]));
    let _ = &v1;
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/other").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    assert_eq!(count(&head, "x-resp"), 0, "{head}");
    let (head, _) = client.get("/dead").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(lower(&head).contains("connect-failed"), "{head}");
    assert_eq!(count(&head, "x-resp"), 0, "{head}");
    for _ in 0..20 {
        let (head, _) = client.get("/retry").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    let seen = String::from_utf8(spy.received()).unwrap();
    assert_eq!(count(&seen, "get /retry"), 20, "{seen}");
    assert_eq!(count(&seen, "x-add: a"), 20, "one add line per request");
    assert!(
        metric(
            &f.metrics.encode(),
            "mimic_mesh_attempts_total{",
            &["connect_failed"]
        ) >= 1.0,
        "a first try was refused and retried"
    );
}

#[tokio::test]
async fn rte_50_discovery_records_the_request_the_app_sent_and_the_modified_response() {
    let v2 = RawPod::start(|_| b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()).await;
    let (own, v1, _) = pods().await;
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    let routes = modified();
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "10.96.0.2", 80, &[v2.addr]),
    ]));
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /p HTTP/1.1\r\nHost: cart.shop.svc\r\nX-Keep: 1\r\nx-gone: g\r\n\r\n")
        .await;
    let _ = client.read().await.unwrap();
    for _ in 0..100 {
        if discovery.report().exchanges() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let report = discovery.report().to_json().unwrap().to_lowercase();
    assert!(
        report.contains("x-gone"),
        "the request as the app sent it: {report}"
    );
    assert!(
        report.contains("x-resp"),
        "the response as the app got it: {report}"
    );
    assert!(
        !report.contains("x-set") && !report.contains("x-add"),
        "{report}"
    );
}

fn exact(path: &str) -> String {
    format!(r#"{{"path":{{"type":"Exact","value":"{path}"}}}}"#)
}

/// `/v1` goes to `cart-v1:80`, `/v2` to `cart-v2:<v2_port>`.
fn split_routes(v2_port: u16) -> String {
    routes(&[route(
        CANARY,
        "",
        &[
            rule(&exact("/v1"), &[backend("shop/cart-v1", 1)], ""),
            rule(&exact("/v2"), &[backend_on("shop/cart-v2", v2_port, 1)], ""),
        ],
    )])
}

#[tokio::test]
async fn rte_51_a_backend_falls_back_to_its_own_cluster_ip_and_never_to_another_backend() {
    let logs = capture_logs();
    let (own, v1, _) = pods().await;
    let v2_cluster = Pod::http1("v2-cluster", 0).await;
    let v2_port = v2_cluster.addr.port();
    let backend_label = format!("shop/cart-v2:{v2_port}");
    let routes = split_routes(v2_port);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &routes;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        svc("shop/cart-v2", "127.0.0.1", v2_port, &[closed_port().await]),
    ]));
    // The parent's own destination is not the one that must be used.
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..5 {
        let (head, body) = client.get("/v2").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "v2-cluster");
    }
    assert_eq!(v1.counts.requests() + own.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_fallbacks_total{",
            &[&format!("backend=\"{backend_label}\"")]
        ),
        5.0,
        "{scrape}"
    );
    for entry in logs.access_exactly(5).await {
        assert_eq!(entry["backend"], backend_label, "{entry}");
        assert_eq!(entry["upstream"], v2_cluster.addr.to_string(), "{entry}");
    }
}

#[tokio::test]
async fn rte_51_the_retries_of_the_drawn_backend_apply_and_zero_gives_one_try() {
    let _logs = capture_logs();
    let (own, v1, _) = pods().await;
    let v2_cluster = Pod::http1("v2-cluster", 0).await;
    let v2_port = v2_cluster.addr.port();
    let routes = split_routes(v2_port);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &routes;
    let mut v2 = svc("shop/cart-v2", "127.0.0.1", v2_port, &[closed_port().await]);
    v2.retries = 0;
    let state = state_with(view(&[
        parent,
        svc("shop/cart-v1", "10.96.0.1", 80, &[v1.addr]),
        v2,
    ]));
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/v2").await;
    assert!(head.starts_with("HTTP/1.1 503"), "one try only: {head}");
    assert!(lower(&head).contains("connect-failed"), "{head}");
    assert_eq!(
        v2_cluster.counts.requests(),
        0,
        "no fallback with 0 retries"
    );
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_attempts_total{", &["cart-v2"]),
        1.0,
        "{scrape}"
    );
}

#[tokio::test]
async fn rte_51_the_budget_is_the_drawn_backends_and_sidecar_answers_deposit_nothing() {
    let logs = capture_logs();
    let (own, v1, _) = pods().await;
    let live = Pod::http1("v1-live", 0).await;
    let routes = routes(&[route(
        CANARY,
        "",
        &[
            rule(&exact("/v1"), &[backend("shop/cart-v1", 1)], ""),
            rule(&exact("/v2"), &[backend("shop/cart-v2", 1)], ""),
            rule(&exact("/none"), &[], ""),
            rule(
                &exact("/invalid"),
                &[r#"{"invalid":true,"weight":1}"#.to_string()],
                "",
            ),
        ],
    )]);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &routes;
    let mut v2 = svc("shop/cart-v2", "10.96.0.2", 80, &[closed_port().await]);
    // No retry is ever allowed.
    v2.budget = (0, 0, 10);
    let state = state_with(view(&[
        parent,
        svc(
            "shop/cart-v1",
            "10.96.0.1",
            80,
            &[closed_port().await, live.addr],
        ),
        v2,
    ]));
    let _ = &v1;
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for path in ["/missing", "/none", "/invalid", "/none"] {
        let (head, _) = client.get(path).await;
        assert!(!head.starts_with("HTTP/1.1 200"), "{path}: {head}");
    }
    for _ in 0..20 {
        let (head, body) = client.get("/v1").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "v1-live");
    }
    let (head, _) = client.get("/v2").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    let scrape = f.metrics.encode();
    let exhausted = |b: &str| {
        metric(
            &scrape,
            "mimic_mesh_retry_budget_exhausted_total{",
            &[&format!("backend=\"{b}\"")],
        )
    };
    assert_eq!(exhausted("shop/cart-v2:80"), 1.0, "{scrape}");
    assert_eq!(exhausted("shop/cart-v1:80"), 0.0, "{scrape}");
    let text = logs.text();
    let warn = text
        .lines()
        .find(|l| l.contains("retry budget exhausted"))
        .unwrap_or_else(|| panic!("no warning: {text}"));
    assert!(warn.contains("\"backend\":\"shop/cart-v2:80\""), "{warn}");
    assert!(
        warn.contains("\"window_requests\":1,"),
        "the answers of the sidecar deposited: {warn}"
    );
}

#[tokio::test]
async fn rte_51_a_headless_backend_whose_pods_refuse_is_answered_503_without_a_fallback() {
    let _logs = capture_logs();
    let (own, _, _) = pods().await;
    let routes = routes(&[route(
        CANARY,
        "",
        &[rule(&exact("/v2"), &[backend("shop/cart-v2", 1)], "")],
    )]);
    let mut parent = svc("shop/cart", "127.0.0.1", 80, &[own.addr]);
    parent.routes = &routes;
    let mut v2 = svc(
        "shop/cart-v2",
        "10.96.0.2",
        80,
        &[closed_port().await, closed_port().await],
    );
    v2.headless = true;
    let state = state_with(view(&[parent, v2]));
    // The parent's destination answers: it must not be used.
    let parent_dst = Pod::http1("parent-dst", 0).await;
    let f = fixture_mesh(parent_dst.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/v2").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(lower(&head).contains("connect-failed"), "{head}");
    assert_eq!(parent_dst.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total{", &["cart-v2"]),
        0.0,
        "{scrape}"
    );
}

#[tokio::test]
async fn rte_53_route_labels_are_capped_at_256_and_evicted_5_minutes_after_the_route_left() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let ip = |i: usize| format!("10.{}.{}.{}", 96 + i / 60_000, (i / 250) % 240, i % 250 + 1);
    let names: Vec<(String, String, String)> = (0..300)
        .map(|i| (format!("shop/s{i}"), ip(i), format!("shop/r{i}")))
        .collect();
    let build = |renamed: Option<&str>| {
        let routes: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, (service, _, name))| {
                let name = match (i, renamed) {
                    (0, Some(new)) => new,
                    _ => name,
                };
                routes(&[route(name, "", &[rule("{}", &[backend(service, 1)], "")])])
            })
            .collect();
        let services: Vec<Svc> = names
            .iter()
            .zip(&routes)
            .map(|((service, ip, _), routes)| {
                let mut s = svc(service, ip, 80, &[pod.addr]);
                s.routes = routes;
                s
            })
            .collect();
        view(&services)
    };
    let state = state_with(build(None));
    let next = Arc::new(Mutex::new(0usize));
    let addrs: Vec<SocketAddr> = names
        .iter()
        .map(|(_, ip, _)| format!("{ip}:80").parse().unwrap())
        .collect();
    let dst = {
        let (next, addrs) = (next.clone(), addrs.clone());
        Arc::new(move |_: &_| Ok(addrs[*next.lock().unwrap()]))
    };
    let f = fixture_mesh_dst(dst, &state, None).await;
    let visit = |i: usize, path: &'static str| {
        let next = next.clone();
        let addr = f.addr;
        async move {
            *next.lock().unwrap() = i;
            let mut client = H1Client::connect(addr).await;
            let (head, _) = client.get(path).await;
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        }
    };
    for i in 0..300 {
        visit(i, "/secret-path?token=zzz-query").await;
    }
    let routes_of = |scrape: &str| -> std::collections::BTreeSet<String> {
        scrape
            .lines()
            .filter(|l| l.starts_with("mimic_mesh_requests_total{"))
            .filter_map(|l| l.split("route=\"").nth(1)?.split('"').next())
            .map(str::to_string)
            .collect()
    };
    let scrape = f.metrics.encode();
    let seen = routes_of(&scrape);
    assert_eq!(seen.len(), 257, "256 routes and other");
    assert!(seen.contains("other"));
    assert_eq!(requests(&scrape, &["route=\"other\""]), 44.0);
    for line in scrape.lines().filter(|l| l.starts_with("mimic_mesh_")) {
        assert!(
            !line.contains("secret-path") && !line.contains("zzz-query"),
            "{line}"
        );
    }
    // Route 0 leaves the view, another takes its place.
    state.hold(build(Some("shop/renamed")), "\"2-a\"");
    let now = std::time::Instant::now();
    state.evict(now + Duration::from_secs(200));
    assert!(
        routes_of(&f.metrics.encode()).contains("shop/r0"),
        "kept 200 s"
    );
    state.evict(now + Duration::from_secs(301));
    let scrape = f.metrics.encode();
    assert!(!routes_of(&scrape).contains("shop/r0"), "{scrape}");
    visit(0, "/").await;
    let scrape = f.metrics.encode();
    assert_eq!(
        requests(&scrape, &["route=\"shop/renamed\""]),
        1.0,
        "{scrape}"
    );
}

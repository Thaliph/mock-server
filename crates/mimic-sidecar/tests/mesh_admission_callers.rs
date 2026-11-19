//! ADM-29: meshed callers in process. Each callee pod is an app behind its own inbound meshed
//! sidecar under a contract; the caller's view lists the apps' addresses as the pods, and its
//! test-only `MeshConfig.redirect` dials each of them at its inbound sidecar, as the callee
//! pod's own redirection does in a cluster. A refusal names the app (the original destination
//! of its sidecar), so the caller trusts it (RTY-13) and retries it on another pod.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;

use common::admission::{contract, inbound, meshed};
use common::mesh_pods::{
    BINARY, EchoPod, H1Client, Pod, Svc, cluster_ip, echoed, h1_request, h2_connect, h2_send,
    mesh_state, payload, retries,
};
use common::{Fixture, capture_logs, closed_port, fixture_mesh, metric};
use mimic_sidecar::mesh::MeshState;

const PODS: usize = 3;
const CLIENTS: usize = 6;
const REQUESTS: usize = 10;
const ERRORS: [&str; 2] = ["overloaded", "queue-timeout"];

/// The callee pods: their apps (100 ms each), their meshed states under `contract(1, 1000)`
/// and their inbound sidecars; and the caller, which balances over the apps' addresses and
/// dials each at its pod's sidecar.
struct Tier {
    apps: Vec<Pod>,
    _callees: Vec<(MeshState, Fixture)>,
    caller: Fixture,
}

async fn tier(h2c: bool) -> Tier {
    let mut apps = Vec::new();
    let mut callees = Vec::new();
    for i in 0..PODS {
        let name = format!("p{i}");
        let app = if h2c {
            Pod::h2c(&name, 100, None).await
        } else {
            Pod::http1(&name, 100).await
        };
        let state = meshed(Some(&format!("cart-{i}")), |_| {});
        state.hold(contract(1, 1000), "\"1-a\"");
        let f = inbound(app.addr, &state, |s| s).await;
        apps.push(app);
        callees.push((state, f));
    }
    let addrs: Vec<SocketAddr> = apps.iter().map(|app| app.addr).collect();
    let redirect: BTreeMap<SocketAddr, SocketAddr> = addrs
        .iter()
        .zip(&callees)
        .map(|(app, (_, f))| (*app, f.addr))
        .collect();
    let state = mesh_state(&[Svc::new("shop/cart", "127.0.0.1", 80, &addrs)], |c| {
        c.redirect = redirect;
    });
    let caller = fixture_mesh(cluster_ip(), &state, None).await;
    Tier {
        apps,
        _callees: callees,
        caller,
    }
}

/// One answer as a client saw it: the status, `x-mimic-error`, `x-mimic-endpoint`, the body.
type Got = (u16, Option<String>, Option<String>, Vec<u8>);

/// The value of `name` in an HTTP/1 head.
fn field(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

/// Even requests are `POST /p` with 1 KiB, odd ones `GET /p` without a body.
fn request(n: usize) -> (&'static str, Vec<u8>) {
    if n % 2 == 0 {
        ("POST", payload(1024))
    } else {
        ("GET", Vec::new())
    }
}

/// Checks every answer, the apps' contract and bodies, and the caller's retries.
fn check(tier: &Tier, answers: &[Got]) {
    let apps: Vec<String> = tier.apps.iter().map(|app| app.addr.to_string()).collect();
    let mut ok = 0;
    for (status, error, endpoint, body) in answers {
        match status {
            200 => {
                ok += 1;
                let body = String::from_utf8_lossy(body);
                assert!(["p0", "p1", "p2"].contains(&body.as_ref()), "{body}");
            }
            503 => {
                assert!(
                    error.as_deref().is_some_and(|e| ERRORS.contains(&e)),
                    "{error:?}"
                );
                assert!(
                    endpoint.as_ref().is_some_and(|e| apps.contains(e)),
                    "{endpoint:?} not one of {apps:?}"
                );
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(answers.len(), CLIENTS * REQUESTS);
    for app in &tier.apps {
        assert!(
            app.counts
                .max_in_flight
                .load(std::sync::atomic::Ordering::SeqCst)
                <= 1
        );
        for (method, body) in app.counts.seen.lock().unwrap().iter() {
            match method.as_str() {
                "POST" => assert_eq!(body, &payload(1024), "a POST body was cut"),
                "GET" => assert!(body.is_empty(), "a GET with a body"),
                other => panic!("unexpected method {other}"),
            }
        }
    }
    assert!(tier.apps.iter().any(|app| {
        app.counts
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            == 1
    }));
    let admitted: usize = tier.apps.iter().map(|app| app.counts.requests()).sum();
    assert_eq!(admitted, ok, "a refused request reached an app");
    let scrape = tier.caller.metrics.encode();
    let retried = retries(&scrape, "overloaded") + retries(&scrape, "queue_timeout");
    assert!(retried >= 1.0, "no refusal was retried:\n{scrape}");
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total", &[]),
        0.0,
        "{scrape}"
    );
}

#[tokio::test]
async fn adm_29_a_redirect_changes_where_a_pod_is_dialled_not_how_it_is_named() {
    let logs = capture_logs();
    let p = closed_port().await;
    let pod = Pod::http1("a", 0).await;
    let state = mesh_state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[p])], |c| {
        c.redirect = BTreeMap::from([(p, pod.addr)]);
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, body) = H1Client::connect(f.addr).await.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "a");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["upstream"], p.to_string(), "{}", lines[0]);
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_endpoint_requests_total{",
            &[&format!("endpoint=\"{p}\"")]
        ),
        1.0,
        "{scrape}"
    );
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_endpoint_requests_total{",
            &[&format!("endpoint=\"{}\"", pod.addr)]
        ),
        0.0,
        "{scrape}"
    );

    // L4: one pod per connection, dialled through the redirect too.
    let echo = EchoPod::start().await;
    let state = mesh_state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[p])], |c| {
        c.redirect = BTreeMap::from([(p, echo.addr)]);
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    assert_eq!(echo.accepted(), 1);
}

#[tokio::test]
async fn adm_29_http1_callers_retry_refusals_on_another_pod() {
    let _logs = capture_logs();
    let tier = tier(false).await;
    let mut tasks = Vec::new();
    for _ in 0..CLIENTS {
        let addr = tier.caller.addr;
        tasks.push(tokio::spawn(async move {
            let mut client = H1Client::connect(addr).await;
            let mut got = Vec::new();
            for n in 0..REQUESTS {
                let (method, body) = request(n);
                let (head, body) = h1_request(&mut client, method, &body).await;
                let status = head
                    .split(' ')
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                got.push((
                    status,
                    field(&head, "x-mimic-error"),
                    field(&head, "x-mimic-endpoint"),
                    body,
                ));
            }
            got
        }));
    }
    let mut answers = Vec::new();
    for task in tasks {
        answers.extend(task.await.unwrap());
    }
    check(&tier, &answers);
}

#[tokio::test]
async fn adm_29_h2c_callers_retry_refusals_on_another_pod() {
    let _logs = capture_logs();
    let tier = tier(true).await;
    let mut tasks = Vec::new();
    for _ in 0..CLIENTS {
        let addr = tier.caller.addr;
        tasks.push(tokio::spawn(async move {
            let mut sender = h2_connect(addr).await;
            let mut got = Vec::new();
            for n in 0..REQUESTS {
                let (method, body) = request(n);
                let (status, headers, body) =
                    h2_send(&mut sender, method, body).await.expect("an answer");
                let value = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                got.push((
                    status,
                    value("x-mimic-error"),
                    value("x-mimic-endpoint"),
                    body,
                ));
            }
            got
        }));
    }
    let mut answers = Vec::new();
    for task in tasks {
        answers.extend(task.await.unwrap());
    }
    check(&tier, &answers);
}

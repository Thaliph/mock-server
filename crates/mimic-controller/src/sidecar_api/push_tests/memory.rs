//! SHR-18: the controller's peak memory with pushes onto reports at the store limits. The work
//! runs in a child process (this test binary again), so the peak is its own and the other tests
//! of the binary do not move it.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;
use crate::ui::{UiMetrics, router as ui_router};

const CHILD: &str = "MIMIC_SHR_18_CHILD";
const RSS_LIMIT: u64 = 640 * 1024 * 1024;
/// Keys of the JSON bodies of the exchanges: schemas near their caps, pushes of about 28 MB.
const KEYS: usize = 40;
const DEPLOYMENTS: [&str; 2] = ["pay", "web"];

/// The default limits of the push route (spec §10.2).
fn defaults() -> crate::push_config::PushConfig {
    crate::push_config::PushConfig::from_env(&mimic_runtime::config::Env::from_pairs([]))
        .expect("the defaults are valid")
}

fn options() -> Options {
    let defaults = defaults();
    Options {
        max_report: defaults.max_report,
        budget: defaults.body_budget,
        max_push: 1024,
        max_endpoints: 512,
        max_authorities: 128,
        sample_budget: 4 << 20,
        ..Options::default()
    }
}

/// Peak resident set size of this process, in bytes.
fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM");
    kb * 1024
}

fn wide_body(seed: usize, status: u16) -> String {
    let mut out = String::from("{");
    for key in 0..KEYS {
        if key > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"field{key}\":\"value-{seed}-{status}-{key}\""));
    }
    out.push('}');
    out
}

/// A report at the store limits: 512 inbound endpoints (64 ports x 8 methods) and 512 outbound
/// ones (128 authorities x 4 methods), six statuses each, with JSON bodies.
fn full_report(salt: usize) -> Report {
    const METHODS: [&str; 8] = [
        "GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE",
    ];
    let limits = Limits {
        max_endpoints: 512,
        max_authorities: 128,
        sample_budget: 4 << 20,
        ..LIMITS
    };
    let mut report = Report::new(Some(pod_of("pay")));
    let headers = [("content-type", "application/json")];
    for n in 0..1024usize {
        let (direction, scope, port, method) = if n < 512 {
            let port = 8000 + u16::try_from(n / 8).unwrap();
            (
                Direction::Inbound,
                ScopeKey::Port(port),
                port,
                METHODS[n % 8],
            )
        } else {
            let m = n - 512;
            let authority = format!("a{}.example.com:443", m / 4);
            (
                Direction::Outbound,
                ScopeKey::Authority(authority),
                443,
                METHODS[m % 4],
            )
        };
        for status in [200, 201, 202, 400, 404, 500] {
            let body = wide_body(n + salt, status);
            let exchange = Exchange {
                direction,
                port,
                method,
                path: "/resource",
                status,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation {
                    query: "",
                    req_headers: &headers,
                    req_body: Some(body.as_bytes()),
                    resp_headers: &headers,
                    resp_body: Some(body.as_bytes()),
                },
            };
            let via = Via {
                tls: direction == Direction::Outbound,
                sni: None,
                dst: None,
            };
            report.record_at(
                scope.clone(),
                &via,
                &limits,
                &Redactor::default(),
                &exchange,
            );
        }
    }
    report.stamp(today());
    report
}

/// The push body, written by hand: no second copy of the report as a JSON value.
fn full_body(sequence: u64, salt: usize) -> Vec<u8> {
    let json = full_report(salt).to_json().expect("serialises");
    assert!(json.len() < defaults().max_report, "{} bytes", json.len());
    format!("{{\"session\":\"{SESSION}\",\"sequence\":{sequence},\"report\":{json}}}").into_bytes()
}

async fn get(router: &Router, path: &str) -> (u16, usize) {
    let mut request = Request::builder()
        .uri(path)
        .header("host", "localhost:8080")
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.len())
}

async fn work(dir: &str) {
    let mut s = setup(options());
    s.ns = std::fs::read_to_string(format!("{dir}/ns")).expect("the namespace");
    let s = Arc::new(s);
    let mut pushes = Vec::new();
    for deployment in DEPLOYMENTS {
        let s = s.clone();
        let path = format!("{dir}/{deployment}.json");
        pushes.push(tokio::spawn(async move {
            let body = std::fs::read(path).expect("the body");
            // A sidecar re-posts a push that got the `503` of the body budget.
            for _ in 0..600 {
                let answer = push(&s, deployment, body.clone()).await;
                if answer.status != 503 {
                    return answer.status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            503
        }));
    }
    for push in pushes {
        assert_eq!(push.await.expect("the push ran"), 200);
    }
    // A document of each kind per deployment, rendered at once.
    let ui = ui_router(s.api.store.clone(), UiMetrics::default());
    let mut reads = Vec::new();
    for deployment in DEPLOYMENTS {
        for id in ["inbound-8000", "outbound-a0.example.com%3A443"] {
            let ui = ui.clone();
            let path = format!("/v1/workloads/{}/{deployment}/openapi/{id}", s.ns);
            reads.push(tokio::spawn(async move { get(&ui, &path).await }));
        }
    }
    for read in reads {
        let (status, length) = read.await.expect("the render ran");
        assert_eq!(status, 200);
        assert!(length > 1000);
    }
    let peak = peak_rss();
    eprintln!("PEAK_MIB={}", peak >> 20);
    assert!(
        peak < RSS_LIMIT,
        "peak RSS {} MiB over {} MiB",
        peak >> 20,
        RSS_LIMIT >> 20
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shr_18_child_work() {
    if let Some(dir) = std::env::var_os(CHILD) {
        work(dir.to_str().expect("a UTF-8 path")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shr_18_two_max_size_pushes_onto_full_reports_stay_under_512_mib() {
    // Everything but the measured work happens here: the bodies are built, and both
    // deployments are first filled to the store limits (a first push each), so that the
    // measured pushes merge into full stored reports.
    let dir = std::env::temp_dir().join(unique("mimic-shr18"));
    std::fs::create_dir(&dir).expect("a scratch directory");
    let s = setup(options());
    std::fs::write(dir.join("ns"), &s.ns).expect("the namespace");
    for (i, deployment) in DEPLOYMENTS.into_iter().enumerate() {
        let first = push(&s, deployment, full_body(1, i)).await;
        assert_eq!(first.status, 200);
        // Other exchanges (other sample content) of the same endpoints.
        std::fs::write(
            dir.join(format!("{deployment}.json")),
            full_body(2, i + 1000),
        )
        .expect("a body");
    }
    let exe = std::env::current_exe().expect("the test binary");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "sidecar_api::push_tests::memory::shr_18_child_work",
            "--nocapture",
        ])
        .env(CHILD, &dir)
        // As the chart sets it on the controller.
        .env("MALLOC_ARENA_MAX", "2")
        .output()
        .expect("the child runs");
    std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines().filter(|l| l.starts_with("PEAK_MIB")) {
        eprintln!("{line}");
    }
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{stderr}"
    );
}

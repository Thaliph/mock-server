//! SHR-18 and SHR-23: the controller's peak memory with pushes onto reports at the store limits.
//! The work runs in a child process (this test binary again), so the peak is its own and the
//! other tests of the binary do not move it.
//!
//! Measurement conditions (SHR-23): each body is built once in the parent and held once by the
//! child (a re-post shares it), `MALLOC_ARENA_MAX=2` as the chart sets it, the stored reports are
//! at the store limits before the measured pushes, and the peak is `VmHWM` after the pushes and a
//! document of each kind per deployment.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;
use crate::ui::{UiMetrics, router as ui_router};

const CHILD: &str = "MIMIC_SHR_18_CHILD";
/// The body budget of the child, in bytes (the defaults when unset), and whether a `503` is a
/// failure (SHR-23: both pushes are read at once) instead of a re-post (SHR-18).
const BUDGET: &str = "MIMIC_SHR_BUDGET";
const NO_503: &str = "MIMIC_SHR_NO_503";
/// PUL-12: also 32 pulls of each deployment during the pushes and again after them.
const PULLS: &str = "MIMIC_PUL_12";
const MIB: u64 = 1 << 20;
/// SHR-23: two concurrent writes, with a `64Mi` body budget.
const CONCURRENT_LIMIT: u64 = 640 * MIB;
/// PUL-12: the 640 MiB of SHR-18 plus the 32 MiB egress budget.
const EGRESS_LIMIT: u64 = 672 * MIB;
/// SHR-23: the defaults at most 70% of B (509 MiB, measured before #339), 356 MiB.
const DEFAULTS_LIMIT: u64 = 356 * MIB;
/// Keys of the JSON bodies of the exchanges: schemas near their caps, pushes of about 28 MB.
const KEYS: usize = 40;
const DEPLOYMENTS: [&str; 2] = ["pay", "web"];

/// The default limits of the push route (spec §10.2).
fn defaults() -> crate::push_config::PushConfig {
    crate::push_config::PushConfig::from_env(&mimic_runtime::config::Env::from_pairs([]))
        .expect("the defaults are valid")
}

fn options(budget: u64) -> Options {
    let defaults = defaults();
    Options {
        max_report: defaults.max_report,
        budget,
        // One push reaches the store limits (512 endpoints per direction): harsher than
        // production, where the sidecars' pushes are smaller and the limit is reached by merging.
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

/// One pull of the egress route: its status and size; the body is dropped as it is counted.
async fn pull_size(s: &Setup, deployment: &str) -> (u16, usize) {
    let mut request = Request::builder()
        .uri(format!("/v1/workloads/{deployment}/egress?format=1"))
        .header(
            "authorization",
            format!("Bearer {}", token(&s.ns, deployment, "t")),
        )
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(Peer(PEER.parse().unwrap()));
    let response = s.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.len())
}

/// 32 concurrent pulls of each deployment.
fn pulls(s: &Arc<Setup>) -> Vec<tokio::task::JoinHandle<u16>> {
    let mut tasks = Vec::new();
    for deployment in DEPLOYMENTS {
        for _ in 0..32 {
            let s = s.clone();
            tasks.push(tokio::spawn(async move {
                let (status, _) = pull_size(&s, deployment).await;
                assert!(
                    [200, 503, 507].contains(&status),
                    "a pull of {deployment} got {status}"
                );
                status
            }));
        }
    }
    tasks
}

async fn work(dir: &str, budget: u64, concurrent: bool, pulled: bool) {
    let mut s = setup(options(budget));
    s.ns = std::fs::read_to_string(format!("{dir}/ns")).expect("the namespace");
    let s = Arc::new(s);
    let mut reads = Vec::new();
    if pulled {
        reads.extend(pulls(&s));
    }
    let mut pushes = Vec::new();
    for deployment in DEPLOYMENTS {
        let s = s.clone();
        let path = format!("{dir}/{deployment}.json");
        pushes.push(tokio::spawn(async move {
            // The body is held once; a re-post shares it (a clone of `Bytes` copies nothing), so
            // that the peak is the controller's (SHR-23).
            let body = axum::body::Bytes::from(std::fs::read(path).expect("the body"));
            // A sidecar re-posts a push that got the `503` of the body budget.
            for _ in 0..600 {
                let answer = push(&s, deployment, body.clone()).await;
                assert!(
                    answer.status != 503 || !concurrent,
                    "the push of {deployment} waited for the budget"
                );
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
    if pulled {
        // The pushes changed the digests: pulls at new digests again.
        reads.extend(pulls(&s));
    }
    let mut statuses = BTreeMap::new();
    for read in reads {
        *statuses
            .entry(read.await.expect("the pull ran"))
            .or_insert(0) += 1;
    }
    if pulled {
        eprintln!("PULLS={statuses:?}");
    }
    // A document of each kind per deployment, rendered at once.
    let ui = ui_router(s.api.store.clone(), UiMetrics::default());
    let mut renders = Vec::new();
    for deployment in DEPLOYMENTS {
        for id in ["inbound-8000", "outbound-a0.example.com%3A443"] {
            let ui = ui.clone();
            let path = format!("/v1/workloads/{}/{deployment}/openapi/{id}", s.ns);
            renders.push(tokio::spawn(async move { get(&ui, &path).await }));
        }
    }
    for read in renders {
        let (status, length) = read.await.expect("the render ran");
        assert_eq!(status, 200);
        assert!(length > 1000);
    }
    let peak = peak_rss();
    eprintln!("PEAK_MIB={}", peak.div_ceil(MIB));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shr_18_child_work() {
    if let Some(dir) = std::env::var_os(CHILD) {
        let budget = std::env::var(BUDGET)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| defaults().body_budget);
        let concurrent = std::env::var_os(NO_503).is_some();
        let pulled = std::env::var_os(PULLS).is_some();
        work(
            dir.to_str().expect("a UTF-8 path"),
            budget,
            concurrent,
            pulled,
        )
        .await;
    }
}

/// Fills both deployments to the store limits, then runs the measured pushes in a child process;
/// the peak RSS of the child, in bytes.
async fn measure(budget: Option<u64>, pulled: bool) -> u64 {
    // Everything but the measured work happens here: the bodies are built, and both
    // deployments are first filled to the store limits (a first push each), so that the
    // measured pushes merge into full stored reports.
    let dir = std::env::temp_dir().join(unique("mimic-shr18"));
    std::fs::create_dir(&dir).expect("a scratch directory");
    let s = setup(options(defaults().body_budget));
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
    // The first pushes are over: the parent's memory is not the child's.
    drop(s);
    let exe = std::env::current_exe().expect("the test binary");
    let mut command = std::process::Command::new(exe);
    command
        .args([
            "--exact",
            "sidecar_api::push_tests::memory::shr_18_child_work",
            "--nocapture",
        ])
        .env(CHILD, &dir)
        // As the chart sets it on the controller.
        .env("MALLOC_ARENA_MAX", "2");
    if let Some(budget) = budget {
        command.env(BUDGET, budget.to_string()).env(NO_503, "1");
    }
    if pulled {
        command.env(PULLS, "1");
    }
    let output = command.output().expect("the child runs");
    std::fs::remove_dir_all(&dir).expect("the scratch directory goes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{stderr}"
    );
    let peak = stderr
        .lines()
        .find_map(|l| l.strip_prefix("PEAK_MIB="))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .expect("the child reports its peak");
    eprintln!("PEAK_MIB={peak}");
    if let Some(line) = stderr.lines().find(|l| l.starts_with("PULLS=")) {
        eprintln!("{line}");
    }
    peak * MIB
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shr_18_two_max_size_pushes_onto_full_reports_stay_under_640_mib() {
    let peak = measure(None, false).await;
    assert!(peak < 640 * MIB, "peak RSS {} MiB", peak / MIB);
    // SHR-23: at most 70% of B.
    assert!(
        peak <= DEFAULTS_LIMIT,
        "peak RSS {} MiB over {} MiB",
        peak / MIB,
        DEFAULTS_LIMIT / MIB
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shr_23_two_concurrent_writes_stay_under_640_mib() {
    let peak = measure(Some(64 * MIB), false).await;
    assert!(
        peak < CONCURRENT_LIMIT,
        "peak RSS {} MiB over {} MiB",
        peak / MIB,
        CONCURRENT_LIMIT / MIB
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pul_12_pushes_and_32_pulls_per_deployment_stay_under_672_mib() {
    let peak = measure(None, true).await;
    assert!(
        peak < EGRESS_LIMIT,
        "peak RSS {} MiB over {} MiB",
        peak / MIB,
        EGRESS_LIMIT / MIB
    );
}

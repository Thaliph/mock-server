//! A mock-mode sidecar in process, with a recorded set, for the mocked h2c and gRPC tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Logs, capture_logs};
use mimic_core::Mode;
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction as CoreDirection, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::discovery::CaptureBudget;
use mimic_sidecar::egress::Snapshot;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mock::Mock;
use mimic_sidecar::mode::{ModeConfig, ModeState};
use mimic_sidecar::red::ProxyMetrics;
use mimic_sidecar::tls::ca::CaState;
use mimic_sidecar::tls::intercept::{Interception, TlsMetrics};
use tokio::net::TcpListener;

pub const AUTHORITY: &str = "users.test:8080";

pub fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

pub struct Ex<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub head_ms: u64,
    pub grpc: bool,
    pub resp_body: Option<&'a str>,
}

pub fn record(report: &mut Report, ex: &Ex<'_>) {
    let resp = [(
        "content-type",
        if ex.grpc {
            "application/grpc"
        } else {
            "application/json"
        },
    )];
    report.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &Via {
            tls: false,
            sni: None,
            dst: None,
        },
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: CoreDirection::Outbound,
            port: 8080,
            method: ex.method,
            path: ex.path,
            status: ex.status,
            head_ms: ex.head_ms,
            host: None,
            grpc: ex.grpc,
            observation: Observation {
                query: "",
                req_headers: &[],
                req_body: None,
                resp_headers: &resp,
                resp_body: ex.resp_body.map(str::as_bytes),
            },
        },
    );
}

/// `GET /users/{users_id}` (12 exchanges of `head_ms` ms) and the gRPC method
/// `/pkg.Greeter/Hello`.
pub fn users_set(head_ms: u64) -> EgressSet {
    let mut report = Report::new(None);
    for id in 1..=12 {
        let path = format!("/users/{id}");
        let body = format!("{{\"id\":{id},\"name\":\"user\"}}");
        record(
            &mut report,
            &Ex {
                method: "GET",
                path: &path,
                status: 200,
                head_ms,
                grpc: false,
                resp_body: Some(&body),
            },
        );
    }
    for _ in 0..3 {
        record(
            &mut report,
            &Ex {
                method: "POST",
                path: "/pkg.Greeter/Hello",
                status: 200,
                head_ms,
                grpc: true,
                resp_body: None,
            },
        );
    }
    EgressSet::project(&report)
}

pub struct Rig {
    pub addr: SocketAddr,
    pub metrics: Arc<Metrics>,
    pub logs: Logs,
    pub mode: ModeState,
    pub snapshot: Snapshot,
    pub mock: Mock,
    pub accepted: Arc<AtomicUsize>,
    pub shared: Arc<Shared>,
}

/// A destination that counts the connections it accepts.
async fn destination() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            drop(s);
        }
    });
    (addr, accepted)
}

pub async fn rig(set: EgressSet, factor: u32, max_pending: usize) -> Rig {
    rig_with_budget(set, factor, max_pending, None).await
}

/// Like [`rig`], keeping the request bodies being matched in `budget` (as `main.rs` shares
/// the capture budget of discovery).
pub async fn rig_with_budget(
    set: EgressSet,
    factor: u32,
    max_pending: usize,
    budget: Option<CaptureBudget>,
) -> Rig {
    rig_full(set, factor, max_pending, budget, None).await
}

/// TLS interception of the sidecar: the `MIMIC_TLS_EXCLUDE` value and the intermediate.
pub struct TlsSetup {
    pub exclude: &'static str,
    pub ca: Option<Arc<CaState>>,
}

pub async fn rig_full(
    set: EgressSet,
    factor: u32,
    max_pending: usize,
    budget: Option<CaptureBudget>,
    tls: Option<TlsSetup>,
) -> Rig {
    let (dst, accepted) = destination().await;
    let logs = capture_logs();
    let mut metrics = Metrics::new("sidecar", "test");
    let config = ModeConfig {
        mode: Mode::Mock,
        factor,
        ..ModeConfig::default()
    };
    let mode = ModeState::start(&config, &mut metrics).await;
    let snapshot = Snapshot::new(mode.clone());
    snapshot.load(set, "\"1\"");
    let mock = match budget {
        Some(budget) => {
            Mock::with_budget(snapshot.clone(), max_pending, Redactor::default(), budget)
        }
        None => Mock::new(snapshot.clone(), max_pending, Redactor::default()),
    };
    mock.register(&mut metrics);
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let original: OriginalDst = Arc::new(move |_| Ok(dst));
    let mut shared = Shared::new(vec![addr], tunnel, original)
        .with_proxy_metrics(proxy)
        .with_mock(mock.clone());
    if let Some(tls) = tls {
        let metrics = TlsMetrics::register(&mut metrics);
        shared = shared.with_tls(Interception::new(
            Exclusions::parse(tls.exclude).unwrap(),
            tls.ca,
            Vec::<PathBuf>::new(),
            PathBuf::new(),
            metrics,
        ));
    }
    let shared = Arc::new(shared);
    spawn_listener(listener, Direction::Outbound, shared.clone());
    Rig {
        addr,
        metrics: Arc::new(metrics),
        logs,
        mode,
        snapshot,
        mock,
        accepted,
        shared,
    }
}

impl Rig {
    pub fn counter(&self, series: &str) -> u64 {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with(series))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
}

//! Banc du mode mock (spec §5.3, objectifs O2 et O5), ignoré par défaut.
//!
//! Le sidecar tourne dans un processus séparé (binaire du crate), épinglé sur les cœurs 0 et 1
//! quand `taskset` est disponible ; le banc (amont factice et client) tourne dans ce processus,
//! épinglé sur les cœurs 2 et 3 :
//!
//! `cargo test --release -p mimic-proxy --test mock_load -- --ignored --nocapture --test-threads 1`
//!
//! Scénarios :
//! - `mock_throughput` : store rempli en record (`BENCH_STORE` échanges sortants, 20 000 par
//!   défaut), bascule en mock, puis `BENCH_N` requêtes mockées à 16 et 64 concurrentes ;
//!   débit, p50, p99, CPU du sidecar ;
//! - `mock_partial` : mock-partial avec 5 % de requêtes sans correspondance (relayées puis
//!   enregistrées) ;
//! - `mock_latency_fidelity` : dépendance sans délai, puis à `sleep(2 ms)` ; latence vue par le
//!   client en record, durée enregistrée, puis latence rejouée (`latency_factor: 1`).

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn user(
    State(delay_us): State<u64>,
    Path(id): Path<u64>,
) -> impl axum::response::IntoResponse {
    if delay_us > 0 {
        tokio::time::sleep(Duration::from_micros(delay_us)).await;
    }
    let items: Vec<Value> = (0..8)
        .map(|i| json!({"sku": format!("SKU-{id}-{i}"), "qty": i, "price": 12.5 * i as f64}))
        .collect();
    (
        [
            ("content-type", "application/json"),
            ("x-request-id", "abc"),
        ],
        axum::Json(json!({
            "id": id,
            "name": format!("user{id}"),
            "status": "active",
            "created_at": "2026-10-17T10:00:00Z",
            "items": items,
        })),
    )
}

async fn upstream(delay_us: u64) -> SocketAddr {
    let app = Router::new()
        .route("/users/{id}", get(user))
        .with_state(delay_us);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Sidecar {
    child: Child,
    outbound: SocketAddr,
    admin: SocketAddr,
    _dir: tempfile::TempDir,
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn sidecar(mode: &str, latency_factor: f64) -> Sidecar {
    let dir = tempfile::tempdir().unwrap();
    let (pi, po, pa) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "mode: {mode}\ninstance: bench-0\nworkload:\n  namespace: ns\n  name: bench\n\
         inbound:\n  listen: 127.0.0.1:{pi}\noutbound:\n  listen: 127.0.0.1:{po}\n\
         admin:\n  listen: 127.0.0.1:{pa}\nredact:\n  salt: bench\n\
         capture:\n  max_exchanges: 100000\n  max_store_bytes: 1073741824\n\
         mock:\n  latency_factor: {latency_factor}\n"
    );
    let path = dir.path().join("mimic.yaml");
    std::fs::write(&path, yaml).unwrap();
    // `BENCH_SIDECAR` : autre binaire à mesurer (comparaison avant/après).
    let bin = std::env::var("BENCH_SIDECAR")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_mimic-proxy").to_string());
    let bin = bin.as_str();
    let taskset = Command::new("taskset")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_ok();
    let mut cmd = if taskset {
        let mut c = Command::new("taskset");
        c.args(["-c", "0,1", bin]);
        c
    } else {
        Command::new(bin)
    };
    let child = cmd
        .arg("--config")
        .arg(&path)
        .env("MIMIC_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let s = Sidecar {
        child,
        outbound: format!("127.0.0.1:{po}").parse().unwrap(),
        admin: format!("127.0.0.1:{pa}").parse().unwrap(),
        _dir: dir,
    };
    let c = direct();
    for _ in 0..250 {
        if c.get(format!("http://{}/healthz", s.admin))
            .send()
            .await
            .is_ok()
        {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("sidecar pas prêt");
}

/// Épingle le banc (tous ses threads) sur les cœurs 2 et 3, loin du sidecar.
fn pin_bench() {
    let _ = Command::new("taskset")
        .args(["-a", "-p", "-c", "2,3", &std::process::id().to_string()])
        .stdout(Stdio::null())
        .status();
}

fn direct() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn via(s: &Sidecar) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", s.outbound)).unwrap())
        .build()
        .unwrap()
}

/// Temps CPU (utime + stime) du processus, en millisecondes.
fn cpu_ms(pid: u32) -> f64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let after = stat.rsplit_once(')').map(|x| x.1).unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    (ticks(11) + ticks(12)) as f64 * 10.0
}

fn rss_mib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
}

struct Report {
    lat_us: Vec<u64>,
    wall: Duration,
    errors: usize,
}

impl Report {
    fn pct(&self, p: f64) -> u64 {
        let s = &self.lat_us;
        if s.is_empty() {
            return 0;
        }
        let i = ((p / 100.0) * s.len() as f64).ceil() as usize;
        s[i.saturating_sub(1).min(s.len() - 1)]
    }

    fn rps(&self) -> f64 {
        self.lat_us.len() as f64 / self.wall.as_secs_f64()
    }
}

/// `n` requêtes à concurrence `conc` ; `path(i)` donne le chemin de la i-ème.
async fn load(
    client: &reqwest::Client,
    base: &str,
    n: usize,
    conc: usize,
    path: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Report {
    let next = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let path = Arc::new(path);
    let started = Instant::now();
    let mut tasks = vec![];
    for _ in 0..conc {
        let (client, base, next, errors, path) = (
            client.clone(),
            base.to_string(),
            next.clone(),
            errors.clone(),
            path.clone(),
        );
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::with_capacity(n / conc + 1);
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let t = Instant::now();
                match client.get(format!("{base}{}", path(i))).send().await {
                    // Un 404 relayé (mock-partial) est une réponse ; 5xx et 599 sont des erreurs.
                    Ok(r) if r.status().as_u16() < 500 => {
                        let _ = r.bytes().await;
                        lat.push(t.elapsed().as_micros() as u64);
                    }
                    Ok(r) => {
                        let _ = r.bytes().await;
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            lat
        }));
    }
    let mut lat_us = vec![];
    for t in tasks {
        lat_us.extend(t.await.unwrap());
    }
    lat_us.sort_unstable();
    Report {
        lat_us,
        wall: started.elapsed(),
        errors: errors.load(Ordering::Relaxed),
    }
}

fn users(i: usize) -> String {
    format!("/users/{}", i % 50 + 1)
}

async fn measured(
    s: &Sidecar,
    label: &str,
    base: &str,
    n: usize,
    conc: usize,
    path: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Report {
    let pid = s.child.id();
    let c0 = cpu_ms(pid);
    let r = load(&via(s), base, n, conc, path).await;
    // Laisse finir les enregistrements en tâche de fond.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cpu = cpu_ms(pid) - c0;
    println!(
        "{label:<36} n={:<6} err={:<4} p50={:>6}us p99={:>6}us max={:>7}us rps={:>8.0} cpu={:>5.0}ms/1k rss={}MiB",
        r.lat_us.len(),
        r.errors,
        r.pct(50.0),
        r.pct(99.0),
        r.lat_us.last().copied().unwrap_or(0),
        r.rps(),
        cpu / (r.lat_us.len().max(1) as f64 / 1000.0),
        rss_mib(pid),
    );
    r
}

async fn put_mode(s: &Sidecar, mode: &str) {
    let r = direct()
        .put(format!("http://{}/mode", s.admin))
        .body(mode.to_string())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
}

/// Remplit le store en record, attend les enregistrements, puis passe dans `mode`.
async fn fill(s: &Sidecar, base: &str, n: usize, mode: &str) {
    let r = load(&via(s), base, n, 16, users).await;
    assert_eq!(r.errors, 0);
    let c = direct();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let st: Value = c
            .get(format!("http://{}/api/status", s.admin))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let stored = st["store"]["local"].as_u64().unwrap_or(0) as usize;
        if stored >= n || Instant::now() > deadline {
            println!("store : {stored} échanges");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let t = Instant::now();
    put_mode(s, mode).await;
    println!("bascule en {mode} : {} us", t.elapsed().as_micros());
    let t = Instant::now();
    let r = load(&via(s), base, 1, 1, users).await;
    assert_eq!(r.errors, 0);
    println!("première requête mockée : {} us", t.elapsed().as_micros());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc de charge, lancé à la main"]
async fn mock_throughput() {
    pin_bench();
    let app = upstream(0).await;
    let base = format!("http://{app}");
    let s = sidecar("record", 0.0).await;
    fill(&s, &base, env("BENCH_STORE", 20_000), "mock").await;
    let n = env("BENCH_N", 20_000);
    let _ = load(&via(&s), &base, 1000, 16, users).await;
    for conc in [16, 64] {
        let r = measured(&s, &format!("mock C={conc}"), &base, n, conc, users).await;
        assert_eq!(r.errors, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc de charge, lancé à la main"]
async fn mock_partial() {
    pin_bench();
    let app = upstream(0).await;
    let base = format!("http://{app}");
    let s = sidecar("record", 0.0).await;
    fill(&s, &base, env("BENCH_STORE", 20_000), "mock-partial").await;
    let n = env("BENCH_N", 5_000);
    measured(
        &s,
        "mock-partial 0 % sans correspondance",
        &base,
        n,
        8,
        users,
    )
    .await;
    measured(
        &s,
        "mock-partial 5 % sans correspondance",
        &base,
        n,
        8,
        |i| {
            if i % 20 == 0 {
                format!("/orders/x{i}/lines")
            } else {
                users(i)
            }
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc de charge, lancé à la main"]
async fn mock_latency_fidelity() {
    pin_bench();
    let n = env("BENCH_N", 3_000);
    for delay_us in [0u64, 2_000] {
        let app = upstream(delay_us).await;
        let base = format!("http://{app}");
        let s = sidecar("record", 1.0).await;
        let _ = load(&via(&s), &base, 200, 1, users).await;
        let rec = measured(
            &s,
            &format!("record, amont +{delay_us}us"),
            &base,
            n,
            1,
            users,
        )
        .await;
        // Durées enregistrées par le sidecar (celles que le mock rejoue).
        let list: Vec<Value> = direct()
            .get(format!(
                "http://{}/exchanges?direction=outbound&limit=10000",
                s.admin
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut stored: Vec<u64> = list.iter().filter_map(|e| e["total_us"].as_u64()).collect();
        stored.sort_unstable();
        let stored_p50 = stored.get(stored.len() / 2).copied().unwrap_or(0);
        put_mode(&s, "mock").await;
        let _ = load(&via(&s), &base, 1, 1, users).await;
        let rep = measured(
            &s,
            &format!("mock, amont +{delay_us}us"),
            &base,
            n,
            1,
            users,
        )
        .await;
        println!(
            "fidélité amont +{delay_us}us : vu en record p50={}us, enregistré p50={stored_p50}us, rejoué p50={}us p99={}us, écart {:+.1} %",
            rec.pct(50.0),
            rep.pct(50.0),
            rep.pct(99.0),
            (rep.pct(50.0) as f64 / stored_p50.max(1) as f64 - 1.0) * 100.0
        );
    }
}

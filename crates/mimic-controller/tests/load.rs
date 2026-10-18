//! Banc de charge du contrôleur (spec §5.5.2, hors critères 11.2), ignoré par défaut.
//!
//! Le contrôleur tourne dans son propre runtime tokio (threads `ctl-rt`, dont on mesure le
//! temps CPU) ; des sidecars simulés rejouent le protocole réel (§5.4, §5.5) : battement,
//! publication d'un segment sous son identifiant, liste des segments, import de ceux des pairs.
//!
//! Lancement :
//! `cargo test --release -p mimic-controller --test load -- --ignored --nocapture --test-threads 1 <scénario>`
//! Paramètres par variables d'environnement : `LOAD_PODS`, `LOAD_WORKLOADS`, `LOAD_SECS`,
//! `LOAD_SPEEDUP`, `LOAD_EXCHANGES`, `LOAD_BODY`, `LOAD_CTL_WORKERS`, `LOAD_UI_MS`,
//! `LOAD_PRELOAD`, `LOAD_PRELOAD_AGE_DAYS`, `LOAD_SINCE` (1 : liste incrémentale, comme le
//! sidecar), `LOAD_SEGMENTS`, `LOAD_BIG_EXCHANGES`, `LOAD_JOIN_SEGMENTS`,
//! `LOAD_DISCOVERY_SECS`, `LOAD_MAX_TOTAL`, `LOAD_GC_SEGMENTS`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mimic_controller::config::{AuthConfig, ControllerConfig, Identity};
use mimic_controller::reconcile::MimicWorkloadSpec;
use serde_json::{Value, json};

const NS: &str = "load";
/// Délai d'un appel côté sidecar (`control.rs`, `TIMEOUT`).
const SIDECAR_TIMEOUT: Duration = Duration::from_secs(5);

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Latences par opération, en microsecondes.
#[derive(Default)]
struct Stats {
    ops: Mutex<BTreeMap<&'static str, Vec<u64>>>,
    errors: Mutex<BTreeMap<String, u64>>,
    bytes_in: AtomicU64,
}

impl Stats {
    fn record(&self, op: &'static str, d: Duration) {
        self.ops
            .lock()
            .unwrap()
            .entry(op)
            .or_default()
            .push(d.as_micros() as u64);
    }

    fn error(&self, what: String) {
        *self.errors.lock().unwrap().entry(what).or_default() += 1;
    }

    fn report(&self, wall: Duration) {
        let ops = self.ops.lock().unwrap();
        println!(
            "{:<22} {:>8} {:>9} {:>9} {:>9} {:>9}",
            "opération", "nombre", "débit/s", "p50 ms", "p99 ms", "max ms"
        );
        for (op, v) in ops.iter() {
            let mut v = v.clone();
            v.sort_unstable();
            let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
            println!(
                "{:<22} {:>8} {:>9.1} {:>9.2} {:>9.2} {:>9.2}",
                op,
                v.len(),
                v.len() as f64 / wall.as_secs_f64(),
                p(0.5),
                p(0.99),
                p(1.0)
            );
        }
        let errors = self.errors.lock().unwrap();
        if !errors.is_empty() {
            println!("erreurs : {errors:?}");
        }
        println!(
            "octets téléchargés par les sidecars : {:.1} Mo ({:.2} Mo/s)",
            self.bytes_in.load(Ordering::Relaxed) as f64 / 1e6,
            self.bytes_in.load(Ordering::Relaxed) as f64 / 1e6 / wall.as_secs_f64()
        );
    }
}

/// Temps CPU (s) des threads du runtime du contrôleur, lu dans `/proc/self/task`.
fn ctl_cpu_secs() -> f64 {
    let mut ticks = 0u64;
    for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let p = entry.path();
        let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
        if !comm.starts_with("ctl-rt") {
            continue;
        }
        let stat = std::fs::read_to_string(p.join("stat")).unwrap_or_default();
        // Champs après le nom entre parenthèses : utime et stime sont les 14e et 15e.
        let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or_default();
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() > 13 {
            ticks += f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
        }
    }
    ticks as f64 / 100.0
}

/// Mémoire résidente du processus (Mo) ; le banc et le contrôleur la partagent.
fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .unwrap_or_default()
        / 1024.0
}

/// Échange réaliste : headers, query, body JSON de `body` octets.
fn exchange(id: &str, n: u64, at: u64, body: usize) -> Value {
    let outbound = n % 2 == 1;
    let (authority, method, path) = match n % 6 {
        0 => ("api:8080", "GET", format!("/items/{}", n % 997)),
        1 => ("payments.saas.io:443", "POST", "/v1/charges".to_string()),
        2 => ("api:8080", "POST", "/orders".to_string()),
        3 => ("stock.internal:8080", "GET", format!("/stock/{}", n % 991)),
        4 => ("api:8080", "GET", format!("/users/{}/cart", n % 983)),
        _ => ("payments.saas.io:443", "GET", format!("/v1/charges/ch_{n}")),
    };
    let payload = format!(
        "{{\"id\":{n},\"name\":\"item-{n}\",\"tags\":[\"a\",\"b\"],\"pad\":\"{}\"}}",
        "x".repeat(body)
    );
    let b64 = base64_encode(payload.as_bytes());
    json!({
        "id": id, "direction": if outbound { "outbound" } else { "inbound" },
        "started_at_ms": at, "scheme": "http", "authority": authority,
        "method": method, "path": path, "query": [["page", (n % 5).to_string()]],
        "req_headers": [
            {"name": "authorization", "value": "<redacted:sha256:0123abcd>"},
            {"name": "content-type", "value": "application/json"},
            {"name": "user-agent", "value": "load/1.0"}
        ],
        "req_body": {"kind": "empty"},
        "status": if n.is_multiple_of(50) { 500 } else { 200 },
        "resp_headers": [{"name": "content-type", "value": "application/json"}],
        "resp_body": {"kind": "captured", "data": b64},
        "ttfb_us": 1000 + n % 500, "total_us": 2000 + n % 900, "trace_id": null
    })
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let v = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn segment_body(pod: &str, counter: &mut u64, count: u64, body: usize) -> String {
    let now = now_ms();
    let mut s = String::new();
    for _ in 0..count {
        *counter += 1;
        let id = format!("{pod}-{counter:012}");
        s.push_str(&exchange(&id, *counter, now, body).to_string());
        s.push('\n');
    }
    s
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Ctl {
    rt: Option<tokio::runtime::Runtime>,
    running: Option<mimic_controller::Running>,
    dir: tempfile::TempDir,
}

impl Ctl {
    async fn start(
        tokens: BTreeMap<String, Identity>,
        workers: usize,
        max_exchanges: usize,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("ctl-rt")
            .enable_all()
            .build()
            .unwrap();
        let config = ControllerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ui_listen: "127.0.0.1:0".parse().unwrap(),
            store: format!("file://{}", dir.path().display()),
            auth: AuthConfig::Static { tokens },
            max_exchanges_per_workload: max_exchanges,
            discovery_interval: Duration::from_secs(env("LOAD_DISCOVERY_SECS", 30)),
            max_exchanges_total: env("LOAD_MAX_TOTAL", 1_000_000) as usize,
            ..ControllerConfig::default()
        };
        let running = rt
            .spawn(mimic_controller::start(config))
            .await
            .unwrap()
            .unwrap();
        Self {
            rt: Some(rt),
            running: Some(running),
            dir,
        }
    }

    fn api(&self, path: &str) -> String {
        format!("http://{}{path}", self.running.as_ref().unwrap().api_addr())
    }

    fn ui(&self, path: &str) -> String {
        format!("http://{}{path}", self.running.as_ref().unwrap().ui_addr())
    }

    /// Applique une ressource `MimicWorkload` dans le runtime du contrôleur.
    async fn apply_spec(&self, wl: &str, mode: &str) {
        let ctl = self.running.as_ref().unwrap().state().clone();
        let wl = wl.to_string();
        let mode = mode.to_string();
        self.rt
            .as_ref()
            .unwrap()
            .spawn(async move {
                ctl.apply_spec(
                    NS,
                    &wl,
                    Some(MimicWorkloadSpec {
                        mode,
                        snapshot: None,
                    }),
                    Some(1),
                )
                .await
            })
            .await
            .unwrap();
    }
}

impl Drop for Ctl {
    fn drop(&mut self) {
        drop(self.running.take());
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(SIDECAR_TIMEOUT)
        .build()
        .unwrap()
}

fn tokens(pods: &[(String, String)]) -> BTreeMap<String, Identity> {
    pods.iter()
        .map(|(wl, pod)| {
            (
                format!("tok-{pod}"),
                Identity {
                    namespace: NS.into(),
                    pod: pod.clone(),
                    workload: wl.clone(),
                },
            )
        })
        .collect()
}

async fn timed(
    stats: &Stats,
    op: &'static str,
    req: reqwest::RequestBuilder,
) -> Option<bytes::Bytes> {
    let t = Instant::now();
    match req.send().await {
        Ok(r) => {
            let status = r.status();
            match r.bytes().await {
                Ok(b) if status.is_success() => {
                    stats.record(op, t.elapsed());
                    Some(b)
                }
                Ok(_) => {
                    stats.error(format!("{op}: {status}"));
                    None
                }
                Err(e) => {
                    stats.error(format!(
                        "{op}: {}",
                        if e.is_timeout() { "délai" } else { "corps" }
                    ));
                    None
                }
            }
        }
        Err(e) => {
            stats.error(format!(
                "{op}: {}",
                if e.is_timeout() {
                    "délai"
                } else {
                    "connexion"
                }
            ));
            None
        }
    }
}

/// Un sidecar simulé : battements toutes les `hb`, synchronisation toutes les `share`.
#[allow(clippy::too_many_arguments)]
async fn sidecar(
    base: String,
    wl: String,
    pod: String,
    hb: Duration,
    share: Duration,
    per_segment: u64,
    body: usize,
    stop: Instant,
    stats: Arc<Stats>,
) {
    let http = client();
    let token = format!("tok-{pod}");
    let prefix = format!("{base}/v1/workloads/{NS}/{wl}");
    let mut imported = BTreeSet::new();
    let mut counter = 0u64;
    // Liste incrémentale (`since_ms`), comme le sidecar (SHR-06) ; ignorée par un
    // contrôleur qui ne la connaît pas.
    let incremental = env("LOAD_SINCE", 0) == 1;
    let mut listed_upto: Option<i64> = None;
    // Décalage initial : les pods ne démarrent pas tous à la même milliseconde.
    let spread = share.min(Duration::from_secs(10)).as_millis().max(1) as u64;
    let jitter = if share > Duration::from_secs(60) {
        Duration::ZERO
    } else {
        Duration::from_millis(fastrand_u64(&pod) % spread)
    };
    tokio::time::sleep(jitter).await;
    let mut next_share = Instant::now();
    let mut next_hb = Instant::now();
    while Instant::now() < stop {
        if Instant::now() >= next_hb {
            next_hb += hb;
            let req = http
                .post(format!("{prefix}/heartbeat"))
                .bearer_auth(&token)
                .json(&json!({"instance": pod, "mode": "record", "snapshot": null}));
            timed(&stats, "heartbeat", req).await;
        }
        if Instant::now() >= next_share {
            next_share += share;
            if per_segment > 0 {
                let seg = segment_body(&pod, &mut counter, per_segment, body);
                let id = ulid::Ulid::generate();
                let req = http
                    .post(format!("{prefix}/segments?instance={pod}&segment={id}"))
                    .bearer_auth(&token)
                    .body(seg);
                timed(&stats, "post_segment", req).await;
            }
            let url = match listed_upto {
                Some(ms) if incremental => format!("{prefix}/segments?since_ms={}", ms - 60_000),
                _ => format!("{prefix}/segments"),
            };
            let req = http.get(url).bearer_auth(&token);
            if let Some(b) = timed(&stats, "list_segments", req).await {
                stats.bytes_in.fetch_add(b.len() as u64, Ordering::Relaxed);
                let list: Vec<Value> = serde_json::from_slice(&b).unwrap_or_default();
                let oldest = now_ms().saturating_sub(7 * 24 * 3600 * 1000) as i64;
                if let Some(max) = list.iter().filter_map(|s| s["modified_ms"].as_i64()).max() {
                    listed_upto = Some(listed_upto.map_or(max, |m| m.max(max)));
                }
                for s in list {
                    let key = s["key"].as_str().unwrap_or_default().to_string();
                    // Les segments préchargés (`-old`) figurent l'historique déjà importé.
                    let old = s["modified_ms"].as_i64().unwrap_or_default() < oldest
                        || s["instance"].as_str().unwrap_or_default().contains("-old");
                    if s["instance"] == pod.as_str() || old || imported.contains(&key) {
                        continue;
                    }
                    let req = http
                        .get(format!("{prefix}/segments/{key}"))
                        .bearer_auth(&token);
                    if let Some(b) = timed(&stats, "get_segment", req).await {
                        stats.bytes_in.fetch_add(b.len() as u64, Ordering::Relaxed);
                        imported.insert(key);
                    }
                }
            }
        }
        let wake = next_hb.min(next_share).min(stop);
        tokio::time::sleep_until(tokio::time::Instant::from_std(wake)).await;
    }
}

fn fastrand_u64(seed: &str) -> u64 {
    seed.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

/// Régime établi : `LOAD_PODS` sidecars répartis sur `LOAD_WORKLOADS` workloads, temps
/// compressé d'un facteur `LOAD_SPEEDUP` (battement 2 s et partage 10 s divisés d'autant).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_steady_state() {
    let pods_n = env("LOAD_PODS", 100);
    let wls_n = env("LOAD_WORKLOADS", 20);
    let secs = env("LOAD_SECS", 30);
    let speedup = env("LOAD_SPEEDUP", 5);
    let per_segment = env("LOAD_EXCHANGES", 20);
    let body = env("LOAD_BODY", 300) as usize;
    let workers = env("LOAD_CTL_WORKERS", 2) as usize;
    let ui_every_ms = env("LOAD_UI_MS", 0);
    let pods: Vec<(String, String)> = (0..pods_n)
        .map(|i| {
            let w = i % wls_n;
            (format!("wl{w}"), format!("wl{w}-p{i}"))
        })
        .collect();
    let ctl = Ctl::start(tokens(&pods), workers, 200_000).await;
    let preload = env("LOAD_PRELOAD", 0);
    if preload > 0 {
        let t = Instant::now();
        for w in 0..wls_n {
            write_segments(
                ctl.dir.path(),
                &format!("wl{w}"),
                preload,
                Some(env("LOAD_PRELOAD_AGE_DAYS", 8)),
            );
        }
        println!(
            "préchargement : {preload} segments anciens par workload en {:.1} s",
            t.elapsed().as_secs_f64()
        );
    }
    let base = ctl.api("");
    let hb = Duration::from_millis(2000 / speedup);
    let share = Duration::from_millis(10_000 / speedup);
    let stats = Arc::new(Stats::default());
    let start = Instant::now();
    let stop = start + Duration::from_secs(secs);
    let cpu0 = ctl_cpu_secs();
    let mut tasks = vec![];
    for (wl, pod) in pods {
        tasks.push(tokio::spawn(sidecar(
            base.clone(),
            wl,
            pod,
            hb,
            share,
            per_segment,
            body,
            stop,
            stats.clone(),
        )));
    }
    // L'interface (ou publish_status) parcourt tous les workloads périodiquement.
    let ui_stats = stats.clone();
    let ui_url = ctl.ui("/v1/workloads");
    let page_url = ctl.ui(&format!("/ui/{NS}/wl0"));
    let ui_task = tokio::spawn(async move {
        let http = client();
        while Instant::now() < stop {
            tokio::time::sleep(Duration::from_millis(5000 / speedup)).await;
            timed(&ui_stats, "ui_workloads", http.get(&ui_url)).await;
            if ui_every_ms > 0 {
                timed(&ui_stats, "ui_page_wl0", http.get(&page_url)).await;
            }
        }
    });
    for t in tasks {
        t.await.unwrap();
    }
    ui_task.await.unwrap();
    let wall = start.elapsed();
    let cpu = ctl_cpu_secs() - cpu0;
    println!(
        "\n== régime établi : {pods_n} pods, {wls_n} workloads, {secs} s (x{speedup}), {per_segment} échanges/segment, {workers} threads contrôleur"
    );
    stats.report(wall);
    let segs = count_files(ctl.dir.path());
    let state = ctl.running.as_ref().unwrap().state();
    let kept: usize = (0..wls_n)
        .map(|w| state.exchange_count(NS, &format!("wl{w}")))
        .sum();
    println!(
        "CPU contrôleur : {cpu:.1} s sur {:.1} s, soit {:.2} cœur ; RSS processus : {:.0} Mo ; échanges en mémoire : {kept} ; segments sur le support : {segs}",
        wall.as_secs_f64(),
        cpu / wall.as_secs_f64(),
        rss_mb()
    );
}

fn count_files(dir: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Coût de `GET …/segments` selon le nombre de segments déjà rangés pour le workload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_segment_listing_growth() {
    let pods = vec![("wl0".to_string(), "wl0-p0".to_string())];
    let http = client();
    let stats = Stats::default();
    println!("\n== liste des segments selon leur nombre");
    println!(
        "{:>8} {:>12} {:>12}",
        "segments", "latence ms", "réponse Mo"
    );
    for target in env_list("LOAD_SEGMENTS", &[1_000, 10_000, 50_000]) {
        // Contrôleur neuf : le support est déjà rempli à son démarrage, comme après un
        // redémarrage (le premier appel construit l'index, il n'est pas compté).
        let ctl = Ctl::start(tokens(&pods), 2, 200_000).await;
        write_segments(ctl.dir.path(), "wl0", target, None);
        let _ = timed(
            &stats,
            "first_list",
            http.get(ctl.api(&format!("/v1/workloads/{NS}/wl0/segments")))
                .bearer_auth("tok-wl0-p0"),
        )
        .await;
        let mut best = Duration::MAX;
        let mut size = 0;
        for _ in 0..3 {
            let t = Instant::now();
            let b = timed(
                &stats,
                "list_segments",
                http.get(ctl.api(&format!("/v1/workloads/{NS}/wl0/segments")))
                    .bearer_auth("tok-wl0-p0"),
            )
            .await
            .unwrap();
            best = best.min(t.elapsed());
            size = b.len();
        }
        println!(
            "{target:>8} {:>12.1} {:>12.2}",
            best.as_secs_f64() * 1000.0,
            size as f64 / 1e6
        );
    }
}

/// Écrit `n` segments vides sur le support (disposition du §5.4), datés de `age_days` jours.
fn write_segments(root: &std::path::Path, wl: &str, n: u64, age_days: Option<u64>) {
    let dir = root.join(format!("{NS}/{wl}/exchanges"));
    let mtime = age_days.map(|d| std::time::SystemTime::now() - Duration::from_secs(d * 86_400));
    for i in 0..n {
        let inst = format!("{wl}-old{}", i % 5);
        std::fs::create_dir_all(dir.join(&inst)).unwrap();
        let path = dir
            .join(&inst)
            .join(format!("{}.jsonl", ulid::Ulid::generate()));
        std::fs::write(&path, "\n").unwrap();
        if let Some(t) = mtime {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
}

/// Arrivée d'un pod dans un workload qui a déjà `LOAD_JOIN_SEGMENTS` segments récents :
/// sa première synchronisation importe tout l'historique, un GET par segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_pod_join() {
    let n = env("LOAD_JOIN_SEGMENTS", 8_640);
    let per_segment = env("LOAD_EXCHANGES", 20);
    let body = env("LOAD_BODY", 300) as usize;
    let pods = vec![("wl0".to_string(), "wl0-new".to_string())];
    let ctl = Ctl::start(tokens(&pods), 2, 200_000).await;
    let dir = ctl.dir.path().join(format!("{NS}/wl0/exchanges/wl0-peer"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut counter = 0;
    for _ in 0..n {
        let seg = segment_body("wl0-peer", &mut counter, per_segment, body);
        std::fs::write(dir.join(format!("{}.jsonl", ulid::Ulid::generate())), seg).unwrap();
    }
    let stats = Arc::new(Stats::default());
    let t = Instant::now();
    let cpu0 = ctl_cpu_secs();
    // Une seule synchronisation : partage plus long que la durée du test.
    sidecar(
        ctl.api(""),
        "wl0".into(),
        "wl0-new".into(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        0,
        body,
        Instant::now() + Duration::from_millis(100),
        stats.clone(),
    )
    .await;
    let wall = t.elapsed();
    println!(
        "\n== arrivée d'un pod : {n} segments de {per_segment} échanges importés en {:.1} s, CPU contrôleur {:.1} s",
        wall.as_secs_f64(),
        ctl_cpu_secs() - cpu0
    );
    stats.report(wall);
}

fn env_list(name: &str, default: &[u64]) -> Vec<u64> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| default.to_vec())
}

/// Gros workload : mémoire par échange, inférence, requêtes, snapshot, et effet sur les
/// battements du même workload et d'un autre pendant ces calculs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_big_workload() {
    let total = env("LOAD_BIG_EXCHANGES", 200_000);
    let per_segment = 1_000;
    let body = env("LOAD_BODY", 300) as usize;
    let pods = vec![
        ("big".to_string(), "big-p0".to_string()),
        ("small".to_string(), "small-p0".to_string()),
    ];
    let ctl = Ctl::start(
        tokens(&pods),
        env("LOAD_CTL_WORKERS", 2) as usize,
        total as usize,
    )
    .await;
    let http = client();
    let stats = Arc::new(Stats::default());
    // Workload déjà vu dans l'interface : ses échanges sont gardés en mémoire dès l'ingestion.
    http.get(ctl.ui(&format!("/v1/workloads/{NS}/big/requests?limit=1")))
        .send()
        .await
        .unwrap();
    let rss0 = rss_mb();
    let mut counter = 0u64;
    let t = Instant::now();
    let mut sent_bytes = 0usize;
    for _ in 0..total / per_segment {
        let seg = segment_body("big-p0", &mut counter, per_segment, body);
        sent_bytes += seg.len();
        let r = http
            .post(ctl.api(&format!("/v1/workloads/{NS}/big/segments?instance=big-p0")))
            .bearer_auth("tok-big-p0")
            .body(seg)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
    }
    println!(
        "\n== gros workload : {total} échanges ({:.0} Mo de JSONL) ingérés en {:.1} s",
        sent_bytes as f64 / 1e6,
        t.elapsed().as_secs_f64()
    );
    let rss1 = rss_mb();
    let kept = ctl
        .running
        .as_ref()
        .unwrap()
        .state()
        .exchange_count(NS, "big");
    println!(
        "RSS : {rss0:.0} -> {rss1:.0} Mo pour {kept} échanges gardés, soit {:.2} Ko par échange",
        (rss1 - rss0) * 1024.0 / kept.max(1) as f64
    );

    // Pingueurs de battement : même workload et autre workload, toutes les 20 ms.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pingers = vec![];
    for (wl, pod, op) in [
        ("big", "big-p0", "hb_same_workload"),
        ("small", "small-p0", "hb_other_workload"),
    ] {
        let url = ctl.api(&format!("/v1/workloads/{NS}/{wl}/heartbeat"));
        let stats = stats.clone();
        let stop = stop.clone();
        pingers.push(tokio::spawn(async move {
            let http = client();
            while !stop.load(Ordering::Relaxed) {
                let req = http
                    .post(&url)
                    .bearer_auth(format!("tok-{pod}"))
                    .json(&json!({"instance": pod, "mode": "record", "snapshot": null}));
                timed(&stats, op, req).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let slow = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap();
    let ui = |p: &str| ctl.ui(&format!("/v1/workloads/{NS}/big{p}"));
    let step = async |name: &str, url: String| {
        let rss_before = rss_mb();
        let t = Instant::now();
        let r = slow.get(url).send().await.unwrap();
        let status = r.status();
        let len = r.bytes().await.unwrap().len();
        println!(
            "{name:<38} {:>9.0} ms  {status}  {:>8.1} Ko  RSS max vu {:.0} Mo (avant {rss_before:.0})",
            t.elapsed().as_secs_f64() * 1000.0,
            len as f64 / 1024.0,
            rss_mb()
        );
    };
    step("operations (1er appel, inférence)", ui("/operations")).await;
    step("operations (cache)", ui("/operations")).await;
    // Un seul nouveau segment invalide le cache : nouvelle inférence complète.
    let seg = segment_body("big-p0", &mut counter, 10, body);
    http.post(ctl.api(&format!("/v1/workloads/{NS}/big/segments?instance=big-p0")))
        .bearer_auth("tok-big-p0")
        .body(seg)
        .send()
        .await
        .unwrap();
    step("operations (après 1 segment)", ui("/operations")).await;
    step("requests?limit=50", ui("/requests?limit=50")).await;
    step(
        "requests?limit=100000&format=k6",
        ui("/requests?limit=100000&format=k6"),
    )
    .await;
    step("page /ui (après 1 segment)", {
        let seg = segment_body("big-p0", &mut counter, 10, body);
        http.post(ctl.api(&format!("/v1/workloads/{NS}/big/segments?instance=big-p0")))
            .bearer_auth("tok-big-p0")
            .body(seg)
            .send()
            .await
            .unwrap();
        ctl.ui(&format!("/ui/{NS}/big"))
    })
    .await;
    let t = Instant::now();
    ctl.apply_spec("big", "mock").await;
    println!(
        "{:<38} {:>9.0} ms",
        "bascule en mock (snapshot)",
        t.elapsed().as_secs_f64() * 1000.0
    );
    stop.store(true, Ordering::Relaxed);
    for p in pingers {
        p.await.unwrap();
    }
    println!("battements pendant ces calculs :");
    stats.report(Duration::from_secs(1));
}

/// Passe de maintenance (CTL-10, CTL-11) sur un support vieilli : `LOAD_GC_SEGMENTS`
/// segments de 20 échanges datés du dernier jour (à compacter par heure) et autant d'il y a
/// 8 jours (hors rétention), pendant que des battements continuent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_maintenance() {
    let n = env("LOAD_GC_SEGMENTS", 8_640);
    let body = env("LOAD_BODY", 300) as usize;
    let pods = vec![("wl0".to_string(), "wl0-p0".to_string())];
    let ctl = Ctl::start(tokens(&pods), 2, 200_000).await;
    let dir = ctl.dir.path().join(format!("{NS}/wl0/exchanges/wl0-peer"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut counter = 0;
    let now = std::time::SystemTime::now();
    for i in 0..n {
        // Dernier jour : réparti sur 24 heures révolues ; 8 jours : hors rétention.
        for age in [3_600 + i * 82_800 / n.max(1), 8 * 86_400] {
            let seg = segment_body("wl0-peer", &mut counter, 20, body);
            let path = dir.join(format!("{}.jsonl", ulid::Ulid::generate()));
            std::fs::write(&path, seg).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(age))
                .unwrap();
        }
    }
    let files0 = count_files(ctl.dir.path());
    let bytes0 = dir_bytes(ctl.dir.path());
    let stats = Arc::new(Stats::default());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pinger = {
        let url = ctl.api(&format!("/v1/workloads/{NS}/wl0/heartbeat"));
        let (stats, stop) = (stats.clone(), stop.clone());
        tokio::spawn(async move {
            let http = client();
            while !stop.load(Ordering::Relaxed) {
                let req = http
                    .post(&url)
                    .bearer_auth("tok-wl0-p0")
                    .json(&json!({"instance": "wl0-p0", "mode": "record", "snapshot": null}));
                timed(&stats, "heartbeat", req).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let state = ctl.running.as_ref().unwrap().state().clone();
    let t = Instant::now();
    let cpu0 = ctl_cpu_secs();
    ctl.rt
        .as_ref()
        .unwrap()
        .spawn(async move { state.maintain().await })
        .await
        .unwrap();
    let wall = t.elapsed();
    let cpu = ctl_cpu_secs() - cpu0;
    stop.store(true, Ordering::Relaxed);
    pinger.await.unwrap();
    println!(
        "\n== maintenance : {files0} fichiers ({:.0} Mo) -> {} fichiers ({:.0} Mo) en {:.1} s, CPU contrôleur {cpu:.1} s",
        bytes0 as f64 / 1e6,
        count_files(ctl.dir.path()),
        dir_bytes(ctl.dir.path()) as f64 / 1e6,
        wall.as_secs_f64()
    );
    stats.report(wall);
}

fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(e.path());
            } else {
                n += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    n
}

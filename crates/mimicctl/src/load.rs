//! `mimicctl load` (spec §5.9) : client HTTP en boucle fermée et minuteries autour du cœur
//! pur `mimic_core::load`. Les appels au cluster (contrôleur, `metrics.k8s.io`) sont dans
//! `main.rs` ; ils arrivent ici par le `Sampler`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::load::{
    Corpus, Criteria, Histogram, PodSample, ReplayRequest, StepResult, StepSummary, parse_cpu,
    parse_memory, saturation, summarize,
};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::watch;

/// Pause après un échec de connexion, pour ne pas boucler sur une cible injoignable.
const RECONNECT_PAUSE: Duration = Duration::from_millis(10);

/// Cible du tir : `http://hôte[:port][/préfixe]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub authority: String,
    /// Préfixe de chemin, sans `/` final.
    pub prefix: String,
    pub http2: bool,
}

impl Target {
    pub fn parse(url: &str, http2: bool) -> Result<Self, String> {
        let rest = match url.split_once("://") {
            Some(("http", rest)) => rest,
            Some(("https", _)) => {
                return Err(format!(
                    "--target {url} : https n'est pas pris en charge (v1)"
                ));
            }
            _ => return Err(format!("--target {url} : URL http://hôte[:port] attendue")),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains(['?', '#', '@']) {
            return Err(format!("--target {url} : hôte invalide"));
        }
        Ok(Target {
            authority: authority.into(),
            prefix: path.trim_end_matches('/').into(),
            http2,
        })
    }

    /// URI d'une requête : forme d'origine en HTTP/1.1, absolue en HTTP/2 (`:authority`).
    pub fn uri(&self, path_and_query: &str) -> String {
        if self.http2 {
            format!("http://{}{}{path_and_query}", self.authority, self.prefix)
        } else {
            format!("{}{path_and_query}", self.prefix)
        }
    }

    fn addr(&self) -> String {
        if self
            .authority
            .rsplit_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
            && !self.authority.ends_with(']')
        {
            self.authority.clone()
        } else {
            format!("{}:80", self.authority)
        }
    }
}

/// Fin d'un palier fixe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Exactement ce nombre de requêtes lancées.
    Requests(u64),
    Duration(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Fixed {
        concurrency: u32,
        stop: Stop,
    },
    /// Paliers joués chacun `step_duration`, arrêtés au premier saturé.
    Ramp {
        steps: Vec<u32>,
        step_duration: Duration,
    },
}

/// Usage des conteneurs d'un pod : `(conteneur, cœurs, octets)`.
pub type Containers = Vec<(String, f64, u64)>;
/// Relevé des pods du workload : `(pod, conteneurs)`.
pub type Usage = Vec<(String, Containers)>;
pub type Sampler =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Usage, String>> + Send>> + Send + Sync>;

pub struct Options {
    pub timeout: Duration,
    pub criteria: Criteria,
    pub sample_interval: Duration,
    pub sampler: Option<Sampler>,
    /// Une ligne par palier sur la sortie d'erreur.
    pub progress: bool,
}

pub struct Outcome {
    pub steps: Vec<StepSummary>,
    pub notes: Vec<String>,
}

/// Joue le plan : un palier fixe, ou la montée jusqu'au premier palier saturé.
pub async fn run(corpus: Arc<Corpus>, target: &Target, plan: &Plan, opts: &Options) -> Outcome {
    let (steps, stop): (Vec<u32>, Box<dyn Fn() -> Stop>) = match plan {
        Plan::Fixed { concurrency, stop } => {
            let stop = *stop;
            (vec![*concurrency], Box::new(move || stop))
        }
        Plan::Ramp {
            steps,
            step_duration,
        } => {
            let d = *step_duration;
            (steps.clone(), Box::new(move || Stop::Duration(d)))
        }
    };
    let mut out = Outcome {
        steps: vec![],
        notes: vec![],
    };
    let mut launched = 0u64;
    let mut sampler = opts.sampler.clone();
    for concurrency in steps {
        let (result, sampled) = step(
            corpus.clone(),
            target,
            concurrency,
            stop(),
            launched,
            opts,
            sampler.clone(),
        )
        .await;
        if let Err(e) = sampled {
            out.notes
                .push(format!("relevés de ressources indisponibles : {e}"));
            sampler = None;
        }
        launched += result.ok + result.errors;
        let summary = summarize(&result);
        if opts.progress {
            eprintln!(
                "mimicctl: palier concurrence {} : {} requêtes, {:.1} req/s, erreurs {:.1} %, p99 {:.1} ms",
                summary.concurrency,
                summary.requests,
                summary.rps,
                summary.error_rate * 100.0,
                summary.p99_us as f64 / 1000.0
            );
        }
        let saturated = saturation(out.steps.last(), &summary, &opts.criteria).is_some();
        out.steps.push(summary);
        if saturated {
            break;
        }
    }
    out
}

/// Un palier : `concurrency` tâches, chacune sur sa connexion, et le relevé des ressources.
async fn step(
    corpus: Arc<Corpus>,
    target: &Target,
    concurrency: u32,
    stop: Stop,
    offset: u64,
    opts: &Options,
    sampler: Option<Sampler>,
) -> (StepResult, Result<(), String>) {
    let start = Instant::now();
    let deadline = match stop {
        Stop::Duration(d) => Some(start + d),
        Stop::Requests(_) => None,
    };
    let limit = match stop {
        Stop::Requests(n) => Some(n),
        Stop::Duration(_) => None,
    };
    let next = Arc::new(AtomicU64::new(0));
    let (done_tx, done_rx) = watch::channel(false);
    let sampling =
        sampler.map(|s| tokio::spawn(sample_loop(s, start, opts.sample_interval, done_rx)));
    let workers: Vec<_> = (0..concurrency)
        .map(|_| {
            let w = Worker {
                corpus: corpus.clone(),
                target: target.clone(),
                next: next.clone(),
                offset,
                limit,
                deadline,
                timeout: opts.timeout,
            };
            tokio::spawn(w.run())
        })
        .collect();
    let mut result = StepResult {
        concurrency,
        ..StepResult::default()
    };
    for w in workers {
        if let Ok((latency, ok, errors)) = w.await {
            result.latency.merge(&latency);
            result.ok += ok;
            result.errors += errors;
        }
    }
    result.duration_us = start.elapsed().as_micros() as u64;
    let _ = done_tx.send(true);
    let mut sampled = Ok(());
    if let Some(task) = sampling {
        match task.await {
            Ok(Ok(samples)) => result.samples = samples,
            Ok(Err(e)) => sampled = Err(e),
            Err(e) => sampled = Err(e.to_string()),
        }
    }
    (result, sampled)
}

/// Relevés toutes les `interval` jusqu'à la fin du palier ; s'arrête à la première erreur.
async fn sample_loop(
    sampler: Sampler,
    start: Instant,
    interval: Duration,
    mut done: watch::Receiver<bool>,
) -> Result<Vec<PodSample>, String> {
    let mut samples = vec![];
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = done.changed() => return Ok(samples),
        }
        let at_us = start.elapsed().as_micros() as u64;
        for (pod, containers) in sampler().await? {
            samples.push(PodSample {
                at_us,
                pod,
                containers,
            });
        }
    }
}

enum Sender {
    H1(hyper::client::conn::http1::SendRequest<Full<Bytes>>),
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
}

struct Worker {
    corpus: Arc<Corpus>,
    target: Target,
    next: Arc<AtomicU64>,
    offset: u64,
    limit: Option<u64>,
    deadline: Option<Instant>,
    timeout: Duration,
}

impl Worker {
    async fn run(self) -> (Histogram, u64, u64) {
        let (mut latency, mut ok, mut errors) = (Histogram::new(), 0, 0);
        let mut sender: Option<Sender> = None;
        loop {
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            if self.limit.is_some_and(|max| n >= max) {
                break;
            }
            let req = self.corpus.nth(self.offset + n);
            // Une connexion que le serveur a fermée (keep-alive refusé ou expiré) n'est pas
            // une erreur : la requête, pas encore envoyée, part sur une nouvelle connexion.
            let mut attempts = 0;
            loop {
                attempts += 1;
                if sender.as_ref().is_none_or(Sender::is_closed) {
                    match tokio::time::timeout(self.timeout, self.connect()).await {
                        Ok(Ok(s)) => sender = Some(s),
                        _ => {
                            errors += 1;
                            sender = None;
                            tokio::time::sleep(RECONNECT_PAUSE).await;
                            break;
                        }
                    }
                }
                let Some(s) = sender.as_mut() else { break };
                let started = Instant::now();
                match tokio::time::timeout(self.timeout, send(s, &self.target, req)).await {
                    Ok(Ok(status)) => {
                        latency.record(started.elapsed().as_micros() as u64);
                        if status < 500 {
                            ok += 1;
                        } else {
                            errors += 1;
                        }
                    }
                    Ok(Err(Failure::NotSent)) if attempts < 2 => {
                        sender = None;
                        continue;
                    }
                    // Erreur ou délai dépassé : la connexion est dans un état inconnu.
                    _ => {
                        errors += 1;
                        sender = None;
                    }
                }
                break;
            }
        }
        (latency, ok, errors)
    }

    async fn connect(&self) -> Result<Sender, String> {
        let stream = TcpStream::connect(self.target.addr())
            .await
            .map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        let io = TokioIo::new(stream);
        if self.target.http2 {
            let (s, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(conn);
            Ok(Sender::H2(s))
        } else {
            let (s, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(conn);
            Ok(Sender::H1(s))
        }
    }
}

impl Sender {
    fn is_closed(&self) -> bool {
        match self {
            Sender::H1(s) => s.is_closed(),
            Sender::H2(s) => s.is_closed(),
        }
    }
}

/// Échec d'une requête.
enum Failure {
    /// La connexion s'est fermée avant l'envoi : la requête peut partir sur une autre.
    NotSent,
    Failed,
}

/// Envoie la requête et lit toute la réponse : la latence va jusqu'au dernier octet.
async fn send(sender: &mut Sender, target: &Target, r: &ReplayRequest) -> Result<u16, Failure> {
    let mut req = hyper::Request::builder()
        .method(r.method.as_str())
        .uri(target.uri(&r.path_and_query));
    if !target.http2 {
        req = req.header("host", target.authority.as_str());
    }
    let mut req = req
        .body(Full::new(Bytes::from(r.body.clone())))
        .map_err(|_| Failure::Failed)?;
    let headers = req.headers_mut();
    for (name, value) in &r.headers {
        // Un header enregistré invalide pour hyper n'est pas rejoué.
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
    let not_sent = |e: hyper::Error| {
        if e.is_canceled() || e.is_closed() {
            Failure::NotSent
        } else {
            Failure::Failed
        }
    };
    let resp = match sender {
        Sender::H1(s) => {
            s.ready().await.map_err(|_| Failure::NotSent)?;
            s.send_request(req).await
        }
        Sender::H2(s) => {
            s.ready().await.map_err(|_| Failure::NotSent)?;
            s.send_request(req).await
        }
    }
    .map_err(not_sent)?;
    let status = resp.status().as_u16();
    resp.into_body()
        .collect()
        .await
        .map_err(|_| Failure::Failed)?;
    Ok(status)
}

/// Refus du tir si le workload n'est pas isolé de ses dépendances (LOD-10) : mode souhaité
/// `mock` et tous ses pods sélectionnés alignés. `workload` est la fiche du contrôleur
/// (`/v1/workloads`).
pub fn isolation(workload: &Value) -> Result<(), String> {
    let mode = workload["desired"]["mode"].as_str().unwrap_or("inconnu");
    if mode != "mock" {
        return Err(format!(
            "mode souhaité {mode} : le tir frapperait les vraies dépendances ; \
             `mimicctl mode <workload> mock` d'abord, ou --allow-live-dependencies"
        ));
    }
    let selected = workload["selected_pods"].as_u64().unwrap_or(0);
    let aligned = workload["aligned"].as_u64().unwrap_or(0);
    if selected == 0 {
        return Err("aucun pod sélectionné connu du contrôleur".into());
    }
    if aligned < selected {
        return Err(format!(
            "pods alignés {aligned}/{selected} : attendre que tous servent le mock \
             (`mimicctl status`), ou --allow-live-dependencies"
        ));
    }
    Ok(())
}

/// Pods sélectionnés du workload, d'après la fiche du contrôleur.
pub fn selected_pods(workload: &Value) -> Vec<String> {
    workload["pods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["selected"].as_bool().unwrap_or(false))
        .filter_map(|p| p["instance"].as_str().map(String::from))
        .collect()
}

/// Un objet `PodMetrics` de `metrics.k8s.io` : le pod et l'usage de chaque conteneur.
pub fn pod_usage(metrics: &Value) -> Option<(String, Containers)> {
    let pod = metrics["metadata"]["name"].as_str()?;
    let containers = metrics["containers"]
        .as_array()?
        .iter()
        .filter_map(|c| {
            Some((
                c["name"].as_str()?.to_string(),
                parse_cpu(c["usage"]["cpu"].as_str()?)?,
                parse_memory(c["usage"]["memory"].as_str()?)?,
            ))
        })
        .collect();
    Some((pod.to_string(), containers))
}

/// Durée `500ms`, `60s`, `2m`, `1h`, non nulle.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let t = text.trim();
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let n: u64 = t[..end]
        .parse()
        .map_err(|_| format!("durée « {text} » : nombre attendu"))?;
    let d = match &t[end..] {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return Err(format!("durée « {text} » : unité ms, s, m ou h attendue")),
    };
    if d.is_zero() {
        return Err(format!("durée « {text} » : non nulle"));
    }
    Ok(d)
}

/// Proportion `0.3` ou `30%`, positive.
pub fn parse_ratio(text: &str) -> Result<f64, String> {
    let t = text.trim();
    let v = match t.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().map(|v| v / 100.0),
        None => t.parse::<f64>(),
    }
    .map_err(|_| format!("« {text} » : proportion attendue (0.3 ou 30%)"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("« {text} » : proportion positive attendue"));
    }
    Ok(v)
}

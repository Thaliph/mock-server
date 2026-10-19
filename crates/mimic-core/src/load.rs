//! Tir de charge par rejeu des requêtes enregistrées (spec §5.9) : requêtes à rejouer,
//! paliers de concurrence, histogramme de latence, saturation, lecture des quantités
//! Kubernetes, agrégation des relevés de ressources, recommandation et rapport.
//!
//! Tout est pur : le client HTTP, les minuteries et les appels au cluster sont dans
//! `mimicctl`.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::model::{Body, Direction, Exchange};
use crate::replay::NOT_REPLAYED;

/// Conteneurs qui ne sont pas l'application : Mimic et les proxys des mesh (§3.3).
pub const EXCLUDED_CONTAINERS: &[&str] = &["mimic", "istio-proxy", "linkerd-proxy"];

/// Une requête prête à rejouer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRequest {
    pub method: String,
    /// Chemin et query, encodée.
    pub path_and_query: String,
    /// Headers en minuscules, dans l'ordre d'enregistrement, ceux de `--header` en tête.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Requêtes entrantes enregistrées, dans l'ordre d'enregistrement.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    requests: Vec<ReplayRequest>,
    discarded: usize,
}

impl Corpus {
    /// Requêtes entrantes des échanges, triées par date d'enregistrement ; `extra` ajoute ou
    /// remplace des headers (`--header`). Un échange au body tronqué ou non capturé est
    /// écarté : le rejouer enverrait une requête incomplète.
    pub fn new<E: Borrow<Exchange>>(exchanges: &[E], extra: &[(String, String)]) -> Self {
        let mut list: Vec<&Exchange> = exchanges
            .iter()
            .map(Borrow::borrow)
            .filter(|e| e.direction == Direction::Inbound)
            .collect();
        list.sort_by(|a, b| (a.started_at_ms, &a.id).cmp(&(b.started_at_ms, &b.id)));
        let mut corpus = Corpus::default();
        for e in list {
            let body = match &e.req_body {
                Body::Empty => vec![],
                Body::Captured { data } => data.clone(),
                Body::Truncated { .. } => {
                    corpus.discarded += 1;
                    continue;
                }
                Body::Skipped { size } if *size > 0 => {
                    corpus.discarded += 1;
                    continue;
                }
                Body::Skipped { .. } => vec![],
            };
            corpus.requests.push(request(e, body, extra));
        }
        corpus
    }

    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Échanges écartés (body tronqué ou non capturé).
    pub fn truncated(&self) -> usize {
        self.discarded
    }

    /// La n-ième requête lancée : le corpus en boucle. Le corpus ne doit pas être vide.
    pub fn nth(&self, n: u64) -> &ReplayRequest {
        &self.requests[(n % self.requests.len() as u64) as usize]
    }
}

fn request(e: &Exchange, body: Vec<u8>, extra: &[(String, String)]) -> ReplayRequest {
    let mut path_and_query = e.path.clone();
    for (i, (k, v)) in e.query.iter().enumerate() {
        path_and_query.push(if i == 0 { '?' } else { '&' });
        encode_into(&mut path_and_query, k);
        path_and_query.push('=');
        encode_into(&mut path_and_query, v);
    }
    let mut headers: Vec<(String, String)> = extra.to_vec();
    for h in &e.req_headers {
        let name = h.name.to_ascii_lowercase();
        if NOT_REPLAYED.contains(&name.as_str())
            || h.value.contains("<redacted:")
            || extra.iter().any(|(n, _)| *n == name)
        {
            continue;
        }
        headers.push((name, h.value.clone()));
    }
    ReplayRequest {
        method: e.method.clone(),
        path_and_query,
        headers,
        body,
    }
}

/// Encodage d'un composant de query : tout sauf les caractères non réservés.
fn encode_into(out: &mut String, s: &str) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

/// `nom: valeur` (`--header`) : nom en minuscules, valeur sans les espaces autour.
pub fn parse_header(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once(':')
        .ok_or_else(|| format!("header « {text} » : nom: valeur attendu"))?;
    let name = name.trim().to_ascii_lowercase();
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if name.is_empty() || !name.bytes().all(token) {
        return Err(format!("header « {text} » : nom invalide"));
    }
    Ok((name, value.trim().to_string()))
}

/// Paliers de la montée : `start`, multiplié par `factor` (arrondi, toujours croissant),
/// jusqu'à `max`, joué en dernier.
pub fn ramp(start: u32, factor: f64, max: u32) -> Result<Vec<u32>, String> {
    if start == 0 {
        return Err("--start : au moins 1".into());
    }
    if max < start {
        return Err(format!("--max ({max}) inférieur à --start ({start})"));
    }
    if !factor.is_finite() || factor <= 1.0 {
        return Err(format!("--factor ({factor}) : strictement supérieur à 1"));
    }
    let mut steps = vec![start];
    let mut c = start;
    while c < max {
        let next = ((f64::from(c) * factor).round() as u32).max(c + 1);
        c = next.min(max);
        steps.push(c);
    }
    Ok(steps)
}

/// Valeurs exactes en dessous, puis 128 classes par puissance de deux : précision relative
/// meilleure que 1/128.
const LINEAR: u64 = 256;
const SUB_BITS: u32 = 7;
/// Au-delà (près de 52 jours en microsecondes), la valeur est plafonnée.
const MAX_VALUE: u64 = (1 << 42) - 1;

/// Histogramme log-linéaire de latences en microsecondes, fusionnable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Histogram {
    counts: Vec<u64>,
    count: u64,
    max: u64,
}

impl Histogram {
    pub fn new() -> Self {
        Self::default()
    }

    fn index(v: u64) -> usize {
        if v < LINEAR {
            return v as usize;
        }
        let msb = 63 - v.leading_zeros();
        let shift = msb - SUB_BITS;
        let mant = v >> shift;
        (LINEAR + u64::from(shift - 1) * (1 << SUB_BITS) + (mant - (1 << SUB_BITS))) as usize
    }

    /// Valeur représentative d'une classe : son milieu.
    fn value(index: usize) -> u64 {
        let i = index as u64;
        if i < LINEAR {
            return i;
        }
        let shift = (i - LINEAR) / (1 << SUB_BITS) + 1;
        let mant = (i - LINEAR) % (1 << SUB_BITS) + (1 << SUB_BITS);
        let low = mant << shift;
        low + ((1 << shift) - 1) / 2
    }

    pub fn record(&mut self, us: u64) {
        let v = us.min(MAX_VALUE);
        let i = Self::index(v);
        if self.counts.len() <= i {
            self.counts.resize(i + 1, 0);
        }
        self.counts[i] += 1;
        self.count += 1;
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, other: &Histogram) {
        if self.counts.len() < other.counts.len() {
            self.counts.resize(other.counts.len(), 0);
        }
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.count += other.count;
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// Quantile au rang le plus proche ; 0 sans mesure.
    pub fn quantile(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::value(i).min(self.max);
            }
        }
        self.max
    }
}

/// Relevé de `metrics.k8s.io` pour un pod : par conteneur, cœurs et octets.
#[derive(Debug, Clone, PartialEq)]
pub struct PodSample {
    /// Instant du relevé depuis le début du palier.
    pub at_us: u64,
    pub pod: String,
    pub containers: Vec<(String, f64, u64)>,
}

/// Mesures brutes d'un palier.
#[derive(Debug, Clone, Default)]
pub struct StepResult {
    pub concurrency: u32,
    pub duration_us: u64,
    pub ok: u64,
    pub errors: u64,
    pub latency: Histogram,
    pub samples: Vec<PodSample>,
}

/// Consommation de l'application pendant un palier, par pod.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub pods: usize,
    /// CPU moyen par pod.
    pub cpu_cores: f64,
    /// Mémoire maximale (working set) d'un pod.
    pub memory_bytes: u64,
}

/// Raison de la saturation d'un palier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Saturation {
    Errors,
    Latency,
    Plateau,
}

/// Résumé d'un palier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepSummary {
    pub concurrency: u32,
    pub duration_s: f64,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    /// Succès par seconde.
    pub rps: f64,
    pub error_rate: f64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub usage: Option<Usage>,
}

pub fn summarize(step: &StepResult) -> StepSummary {
    let requests = step.ok + step.errors;
    let duration_s = step.duration_us as f64 / 1e6;
    StepSummary {
        concurrency: step.concurrency,
        duration_s,
        requests,
        ok: step.ok,
        errors: step.errors,
        rps: if duration_s > 0.0 {
            step.ok as f64 / duration_s
        } else {
            0.0
        },
        error_rate: if requests > 0 {
            step.errors as f64 / requests as f64
        } else {
            0.0
        },
        p50_us: step.latency.quantile(0.5),
        p95_us: step.latency.quantile(0.95),
        p99_us: step.latency.quantile(0.99),
        max_us: step.latency.max(),
        usage: aggregate(&step.samples, step.duration_us),
    }
}

/// Critères de saturation (`--max-error-rate`, `--max-p99`, `--plateau`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criteria {
    pub max_error_rate: f64,
    pub max_p99_us: Option<u64>,
    pub plateau: f64,
}

/// Saturation d'un palier, par rapport au précédent.
pub fn saturation(
    prev: Option<&StepSummary>,
    cur: &StepSummary,
    c: &Criteria,
) -> Option<Saturation> {
    if cur.error_rate > c.max_error_rate {
        return Some(Saturation::Errors);
    }
    if c.max_p99_us.is_some_and(|max| cur.p99_us > max) {
        return Some(Saturation::Latency);
    }
    match prev {
        Some(p) if cur.concurrency > p.concurrency && cur.rps < p.rps * (1.0 + c.plateau) => {
            Some(Saturation::Plateau)
        }
        _ => None,
    }
}

/// Palier de capacité et premier palier saturé, par leur rang.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub capacity: Option<usize>,
    pub saturated: Option<(usize, Saturation)>,
}

pub fn verdict(steps: &[StepSummary], c: &Criteria) -> Verdict {
    let mut saturated = None;
    for (i, s) in steps.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| &steps[p]);
        if let Some(kind) = saturation(prev, s, c) {
            saturated = Some((i, kind));
            break;
        }
    }
    let end = saturated.map_or(steps.len(), |(i, _)| i);
    let capacity = steps[..end]
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.rps.total_cmp(&b.rps))
        .map(|(i, _)| i);
    Verdict {
        capacity,
        saturated,
    }
}

/// Nombre suivi d'un suffixe : le nombre et le suffixe.
fn split_quantity(text: &str) -> Option<(f64, &str)> {
    let text = text.trim();
    let end = text
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(text.len());
    let value: f64 = text[..end].parse().ok()?;
    Some((value, &text[end..]))
}

/// Quantité de CPU Kubernetes (`250m`, `1`, `1500000n`), en cœurs.
pub fn parse_cpu(text: &str) -> Option<f64> {
    let (v, suffix) = split_quantity(text)?;
    let scale = match suffix {
        "" => 1.0,
        "m" => 1e-3,
        "u" => 1e-6,
        "n" => 1e-9,
        _ => return None,
    };
    Some(v * scale)
}

/// Quantité de mémoire Kubernetes (`128Mi`, `1Gi`, `500k`, `12345`), en octets.
pub fn parse_memory(text: &str) -> Option<u64> {
    let (v, suffix) = split_quantity(text)?;
    let scale: f64 = match suffix {
        "" => 1.0,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        _ => return None,
    };
    Some((v * scale).round() as u64)
}

/// Consommation de l'application pendant un palier : relevés après le premier tiers, sans
/// les conteneurs de Mimic ni des mesh ; CPU moyen par pod puis entre pods, mémoire
/// maximale. `None` sans relevé retenu.
pub fn aggregate(samples: &[PodSample], step_duration_us: u64) -> Option<Usage> {
    let warmup = step_duration_us / 3;
    let mut cpu: BTreeMap<&str, (f64, u32)> = BTreeMap::new();
    let mut memory = 0;
    for s in samples.iter().filter(|s| s.at_us >= warmup) {
        let app = s
            .containers
            .iter()
            .filter(|(name, _, _)| !EXCLUDED_CONTAINERS.contains(&name.as_str()));
        let (cores, bytes) = app.fold((0.0, 0), |(c, b), (_, cc, bb)| (c + cc, b + bb));
        let e = cpu.entry(&s.pod).or_default();
        e.0 += cores;
        e.1 += 1;
        memory = memory.max(bytes);
    }
    if cpu.is_empty() {
        return None;
    }
    let pods = cpu.len();
    let cpu_cores = cpu.values().map(|(c, n)| c / f64::from(*n)).sum::<f64>() / pods as f64;
    Some(Usage {
        pods,
        cpu_cores,
        memory_bytes: memory,
    })
}

/// Taille recommandée d'un pod, au palier de capacité.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub cpu_millicores: u64,
    pub memory_mib: u64,
    pub rps_per_pod: f64,
    pub rps_per_core: Option<f64>,
}

/// Arrondi supérieur, sans compter l'erreur d'arrondi des flottants comme une unité.
fn ceil(x: f64) -> u64 {
    (x - 1e-6).ceil().max(0.0) as u64
}

pub fn recommend(
    step: &StepSummary,
    cpu_headroom: f64,
    memory_headroom: f64,
) -> Option<Recommendation> {
    let usage = step.usage.as_ref()?;
    let rps_per_pod = step.rps / usage.pods.max(1) as f64;
    Some(Recommendation {
        cpu_millicores: ceil(usage.cpu_cores * 1000.0 * (1.0 + cpu_headroom)).max(1),
        memory_mib: ceil(usage.memory_bytes as f64 * (1.0 + memory_headroom) / 1_048_576.0).max(1),
        rps_per_pod,
        rps_per_core: (usage.cpu_cores > 0.0).then(|| rps_per_pod / usage.cpu_cores),
    })
}

/// Rapport d'un tir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub target: String,
    /// Requêtes du corpus.
    pub corpus: usize,
    /// Échanges écartés (body tronqué ou non capturé).
    pub discarded: usize,
    pub criteria: Criteria,
    pub cpu_headroom: f64,
    pub memory_headroom: f64,
    pub steps: Vec<StepSummary>,
    pub verdict: Verdict,
    pub recommendation: Option<Recommendation>,
    pub notes: Vec<String>,
}

impl Report {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: &str,
        corpus: usize,
        discarded: usize,
        steps: Vec<StepSummary>,
        criteria: &Criteria,
        cpu_headroom: f64,
        memory_headroom: f64,
        notes: Vec<String>,
    ) -> Self {
        let verdict = verdict(&steps, criteria);
        let recommendation = verdict
            .capacity
            .and_then(|i| recommend(&steps[i], cpu_headroom, memory_headroom));
        Report {
            target: target.into(),
            corpus,
            discarded,
            criteria: criteria.clone(),
            cpu_headroom,
            memory_headroom,
            steps,
            verdict,
            recommendation,
            notes,
        }
    }
}

fn ms(us: u64) -> String {
    format!("{:.1} ms", us as f64 / 1000.0)
}

fn percent(x: f64) -> String {
    format!("{:.1} %", x * 100.0)
}

const COLUMNS: [&str; 12] = [
    "palier",
    "concurrence",
    "durée",
    "requêtes",
    "débit/s",
    "erreurs",
    "p50",
    "p95",
    "p99",
    "max",
    "CPU/pod",
    "mémoire/pod",
];

fn rows(r: &Report) -> Vec<[String; 12]> {
    r.steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (cpu, mem) = match &s.usage {
                Some(u) => (
                    format!("{:.0}m", u.cpu_cores * 1000.0),
                    format!("{:.0}Mi", u.memory_bytes as f64 / 1_048_576.0),
                ),
                None => ("?".into(), "?".into()),
            };
            [
                (i + 1).to_string(),
                s.concurrency.to_string(),
                format!("{:.0} s", s.duration_s),
                s.requests.to_string(),
                format!("{:.1}", s.rps),
                percent(s.error_rate),
                ms(s.p50_us),
                ms(s.p95_us),
                ms(s.p99_us),
                ms(s.max_us),
                cpu,
                mem,
            ]
        })
        .collect()
}

fn header_line(r: &Report) -> String {
    let mut line = format!(
        "Tir de charge de {} : {} requête(s) enregistrée(s) rejouée(s) en boucle",
        r.target, r.corpus
    );
    if r.discarded > 0 {
        let _ = write!(
            line,
            " ({} écartée(s), body tronqué ou non capturé)",
            r.discarded
        );
    }
    line.push('.');
    line
}

fn criteria_line(c: &Criteria) -> String {
    let mut line = format!("Saturation : erreurs > {}", percent(c.max_error_rate));
    if let Some(p99) = c.max_p99_us {
        let _ = write!(line, ", p99 > {}", ms(p99));
    }
    let _ = write!(
        line,
        ", débit en hausse de moins de {}.",
        percent(c.plateau)
    );
    line
}

fn verdict_lines(r: &Report) -> Vec<String> {
    let mut lines = vec![];
    match r.verdict.capacity {
        Some(i) => {
            let s = &r.steps[i];
            lines.push(format!(
                "Capacité : palier {}, concurrence {}, {:.1} req/s, p99 {}.",
                i + 1,
                s.concurrency,
                s.rps,
                ms(s.p99_us)
            ));
        }
        None => lines.push("Capacité : aucun palier non saturé.".into()),
    }
    match r.verdict.saturated {
        Some((i, kind)) => {
            let s = &r.steps[i];
            let why = match kind {
                Saturation::Errors => format!(
                    "erreurs {} > {}",
                    percent(s.error_rate),
                    percent(r.criteria.max_error_rate)
                ),
                Saturation::Latency => format!(
                    "p99 {} > {}",
                    ms(s.p99_us),
                    ms(r.criteria.max_p99_us.unwrap_or(0))
                ),
                Saturation::Plateau => {
                    let prev = &r.steps[i - 1];
                    let gain = if prev.rps > 0.0 {
                        s.rps / prev.rps - 1.0
                    } else {
                        0.0
                    };
                    format!(
                        "plateau, débit {:+.1} % < {} avec une concurrence de {} au lieu de {}",
                        gain * 100.0,
                        percent(r.criteria.plateau),
                        s.concurrency,
                        prev.concurrency
                    )
                }
            };
            lines.push(format!(
                "Saturation : palier {}, concurrence {} : {why}.",
                i + 1,
                s.concurrency
            ));
        }
        None => lines.push("Saturation : non atteinte.".into()),
    }
    match &r.recommendation {
        Some(rec) => {
            let mut line = format!(
                "Recommandation par pod (marges CPU {}, mémoire {}) : requests.cpu {}m, \
                 requests.memory et limits.memory {}Mi ; {:.1} req/s par pod",
                percent(r.cpu_headroom),
                percent(r.memory_headroom),
                rec.cpu_millicores,
                rec.memory_mib,
                rec.rps_per_pod
            );
            if let Some(per_core) = rec.rps_per_core {
                let _ = write!(line, ", {per_core:.0} req/s par cœur");
            }
            line.push('.');
            lines.push(line);
        }
        None if r.verdict.capacity.is_some() => {
            lines.push(
                "Recommandation : aucun relevé de CPU et de mémoire au palier de capacité.".into(),
            );
        }
        None => {}
    }
    lines
}

/// Rapport texte, colonnes alignées.
pub fn render_text(r: &Report) -> String {
    let rows = rows(r);
    let mut widths = COLUMNS.map(|c| c.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = w - cell.chars().count();
            out.push_str(cell);
            if i + 1 < cells.len() {
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push('\n');
        out
    };
    let mut out = format!("{}\n{}\n\n", header_line(r), criteria_line(&r.criteria));
    out.push_str(&line(&COLUMNS.map(String::from)));
    for row in &rows {
        out.push_str(&line(row));
    }
    out.push('\n');
    for l in verdict_lines(r) {
        out.push_str(&l);
        out.push('\n');
    }
    if !r.notes.is_empty() {
        out.push_str("Remarques :\n");
        for n in &r.notes {
            let _ = writeln!(out, "- {n}");
        }
    }
    out
}

/// Rapport Markdown, pour le résumé d'un job GitHub Actions.
pub fn render_markdown(r: &Report) -> String {
    let mut out = format!(
        "## Tir de charge\n\n{}\n{}\n\n| {} |\n|{}\n",
        header_line(r),
        criteria_line(&r.criteria),
        COLUMNS.join(" | "),
        "---|".repeat(COLUMNS.len())
    );
    for row in rows(r) {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }
    out.push('\n');
    for l in verdict_lines(r) {
        let _ = writeln!(out, "- {l}");
    }
    if !r.notes.is_empty() {
        out.push_str("\nRemarques :\n\n");
        for n in &r.notes {
            let _ = writeln!(out, "- {n}");
        }
    }
    out
}

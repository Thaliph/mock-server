//! The recorder thread: the only consumer of the queue and the only writer of the local report
//! (spec §6). Parsing of the captured heads, body classification, the core `Report::record`,
//! metrics and logs all happen here, never on a proxy task.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{
    Direction, Exchange, Limits, Outcome, Report, Sampled, Tunneled, Via,
};
use mimic_core::discovery::scope::{ScopeKey, authority_key};

use super::Handoff;
use super::capture::{Item, Part, Record, Tunnel};
use super::decode;
use super::metrics::DiscoveryMetrics;
use super::redactions::{self, Counts};

/// Longest authority kept, as in the codec.
const MAX_AUTHORITY: usize = 1024;
/// Old templates named in a `template absorbed` line.
const MAX_OLD_LOGGED: usize = 16;

/// Direction, scope (a port, or the authority of an outbound endpoint), method and template.
pub(super) type Key = (Direction, ScopeKey, String, String);

/// The endpoints of `report`.
pub(super) fn keys(report: &Report) -> BTreeSet<Key> {
    report
        .endpoints()
        .into_iter()
        .map(|e| {
            let scope = match e.authority {
                Some(authority) => ScopeKey::Authority(authority.to_string()),
                None => ScopeKey::Port(e.port),
            };
            (e.direction, scope, e.method.to_string(), e.template)
        })
        .collect()
}

/// Sets the gauges for the current report together with the pending one: sample bytes, and the
/// distinct endpoints. Called under the report's lock, so that a freeze and a record cannot
/// overwrite each other with stale values.
pub(super) fn publish(
    metrics: &DiscoveryMetrics,
    handoff: &Handoff,
    current: &Report,
    current_keys: &BTreeSet<Key>,
) {
    metrics.set_sample_bytes(held_samples(handoff, current));
    let pending_keys = handoff
        .pending_keys
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let egress_keys = handoff
        .egress_keys
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let all: BTreeSet<&Key> = current_keys
        .iter()
        .chain(pending_keys.iter())
        .chain(egress_keys.iter())
        .collect();
    for d in [Direction::Inbound, Direction::Outbound] {
        let n = all.iter().filter(|(k, ..)| *k == d).count();
        metrics.set_endpoints(label(d), n);
    }
}

/// Sample bytes held: the current report, the pending one and the egress report.
fn held_samples(handoff: &Handoff, current: &Report) -> usize {
    current
        .sample_bytes()
        .saturating_add(handoff.pending_samples.load(Ordering::Acquire))
        .saturating_add(handoff.egress_samples.load(Ordering::Acquire))
}

pub(super) struct Recorder {
    pub(super) report: Arc<Mutex<Report>>,
    /// The outbound exchanges (spec §6.1): frozen with the report when shipping.
    egress: Arc<Mutex<Report>>,
    pub(super) limits: Limits,
    pub(super) redactor: Redactor,
    pub(super) metrics: DiscoveryMetrics,
    /// What the shipper hands over: sample bytes of the pending report, and the freezes.
    handoff: Arc<Handoff>,
    epoch: u64,
    /// Endpoints of the report as of the last change of its shape.
    known: BTreeSet<Key>,
    /// Likewise for the egress report.
    egress_known: BTreeSet<Key>,
    /// The endpoints this process has recorded, across freezes (the shape survives a push,
    /// spec §7.2), for `mimic_discovery_last_new_endpoint_timestamp_seconds`: a template stands
    /// for the literals it covers, and the set is bounded.
    seen: BTreeSet<Key>,
    sample_budget_full: bool,
    /// The directions whose limit was logged: each logs it once.
    endpoint_limit_logged: BTreeSet<&'static str>,
    authority_limit_logged: bool,
    sample_bytes: usize,
}

/// The scope of an outbound exchange (spec §6.1): its target authority, else the SNI, else the
/// destination.
fn egress_key(record: &Record, host: Option<&str>) -> ScopeKey {
    ScopeKey::Authority(authority_key(
        host,
        if record.tls { 443 } else { 80 },
        record.sni.as_deref(),
        record.dst,
    ))
}

fn label(direction: Direction) -> &'static str {
    match direction {
        Direction::Inbound => "inbound",
        Direction::Outbound => "outbound",
    }
}

/// The request as the head says it.
struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

fn lines(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .take_while(|l| !l.is_empty())
        .collect()
}

fn header_pairs(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .skip(1)
        // A folded continuation line is part of the previous value, which the framer rejects.
        .filter(|l| !l.starts_with([' ', '\t']))
        .filter_map(|l| {
            let (name, value) = l.split_once(':')?;
            Some((
                name.to_string(),
                value.trim_matches([' ', '\t']).to_string(),
            ))
        })
        .collect()
}

/// Method, path and query of a request line; `None` for `CONNECT` or a line that is not one.
fn parse_request(raw: &[u8]) -> Option<Request> {
    let lines = lines(raw);
    let mut parts = lines.first()?.splitn(3, ' ');
    let (method, target) = (parts.next()?, parts.next()?);
    if method == "CONNECT" {
        return None;
    }
    // Absolute form (`http://host/path`): what follows the authority.
    let target = match target.split_once("://") {
        Some((scheme, rest))
            if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            rest.find(['/', '?', '#']).map_or("/", |i| &rest[i..])
        }
        _ => target,
    };
    let target = target.split('#').next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = if path.is_empty() { "/" } else { path };
    Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: header_pairs(&lines),
    })
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn media(headers: &[(String, String)]) -> String {
    header(headers, "content-type")
        .and_then(|v| v.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_json(media: &str) -> bool {
    media == "application/json" || media.ends_with("+json")
}

/// The `result` of `mimic_discovery_bodies_total` for a part, and the decoded bytes to infer
/// from.
fn classify<'a>(
    part: &'a Part,
    headers: &[(String, String)],
) -> Option<(&'static str, Option<Cow<'a, [u8]>>)> {
    Some(match part {
        Part::Absent => return None,
        Part::Empty => ("empty", None),
        Part::Budget => ("budget", None),
        Part::TooLarge => ("too_large", None),
        Part::Data(bytes) => {
            let bytes = match decode::decode(bytes, header(headers, "content-encoding")) {
                Ok(bytes) => bytes,
                Err(failure) => return Some((failure.result(), None)),
            };
            let media = media(headers);
            let result = if is_json(&media) {
                if serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_ok() {
                    "parsed"
                } else {
                    "invalid"
                }
            } else if media == "application/x-www-form-urlencoded" {
                "parsed"
            } else {
                "opaque"
            };
            (result, Some(bytes))
        }
    })
}

fn status_ok(record: &Record) -> bool {
    (100..=599).contains(&record.status)
}

/// How many parameters `template` has when it matches `path`, `None` when it does not.
fn matches(template: &str, path: &str) -> Option<usize> {
    // No allocation: it runs for every endpoint the recorder knows, at each new one.
    let (mut t, mut p) = (template.split('/'), path.split('/'));
    let mut params = 0;
    loop {
        match (t.next(), p.next()) {
            (None, None) => return Some(params),
            (Some(a), Some(b)) => {
                if a.starts_with('{') && a.ends_with('}') {
                    params += 1;
                } else if a != b {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

impl Recorder {
    pub(super) fn new(
        report: Arc<Mutex<Report>>,
        egress: Arc<Mutex<Report>>,
        limits: Limits,
        redactor: Redactor,
        metrics: DiscoveryMetrics,
        handoff: Arc<Handoff>,
    ) -> Recorder {
        Recorder {
            report,
            egress,
            limits,
            redactor,
            metrics,
            handoff,
            epoch: 0,
            known: BTreeSet::new(),
            egress_known: BTreeSet::new(),
            seen: BTreeSet::new(),
            sample_budget_full: false,
            endpoint_limit_logged: BTreeSet::new(),
            authority_limit_logged: false,
            sample_bytes: 0,
        }
    }

    pub(super) fn run(mut self, queue: Receiver<Item>) {
        for item in queue {
            match item {
                Item::Exchange(record) => {
                    let direction = label(record.direction);
                    self.guarded(direction, |recorder| recorder.record(&record));
                }
                Item::Tunnel(tunnel) => {
                    self.guarded("outbound", |recorder| recorder.tunnel(&tunnel));
                }
            }
        }
    }

    /// Counts a connection without plaintext HTTP in the `tunnels` of its authority.
    fn tunnel(&mut self, tunnel: &Tunnel) {
        let key = ScopeKey::Authority(authority_key(
            tunnel.target.as_deref(),
            if tunnel.tls { 443 } else { 80 },
            tunnel.sni.as_deref(),
            tunnel.dst,
        ));
        let (counted, authorities) = {
            let mut egress = self.egress.lock().unwrap_or_else(PoisonError::into_inner);
            let counted = egress.record_tunnel(&self.limits, key, tunnel.kind);
            (counted, egress.authorities())
        };
        self.metrics.set_authorities(authorities);
        match counted {
            Tunneled::Counted => self.metrics.tunnel(tunnel.kind, "recorded"),
            Tunneled::AuthorityLimit => {
                self.metrics.tunnel(tunnel.kind, "authority_limit");
                self.warn_authority_limit();
            }
            // The kinds are the ones of `TUNNEL_KINDS` and the key a valid authority.
            Tunneled::Invalid => {}
        }
    }

    fn warn_authority_limit(&mut self) {
        if !self.authority_limit_logged {
            self.authority_limit_logged = true;
            tracing::warn!(
                target: "discovery",
                limit = self.limits.max_authorities,
                "authority limit reached"
            );
        }
    }

    /// Runs `f` for one exchange. A panic is logged and counted, the exchange is skipped and
    /// the recorder goes on: it is the only writer of the report. The report is updated by
    /// one merge at the end of an exchange, so it is not left half-written.
    fn guarded(&mut self, direction: &'static str, f: impl FnOnce(&mut Self)) {
        if catch_unwind(AssertUnwindSafe(|| f(self))).is_err() {
            self.metrics.exchange(direction, "panic");
            tracing::error!(target: "discovery", direction, "recorder panic");
        }
    }

    fn record(&mut self, record: &Record) {
        let direction = label(record.direction);
        let Some(request) = parse_request(&record.request_head).filter(|_| status_ok(record))
        else {
            self.metrics.exchange(direction, "invalid");
            return;
        };
        let response_lines = lines(&record.response_head);
        let response_headers = header_pairs(&response_lines);
        let request_body = classify(&record.request, &request.headers);
        let response_body = classify(&record.response, &response_headers);
        let req: Vec<(&str, &str)> = request
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let resp: Vec<(&str, &str)> = response_headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let authority = record.authority.as_deref().map(|a| {
            let mut end = a.len().min(MAX_AUTHORITY);
            while !a.is_char_boundary(end) {
                end -= 1;
            }
            &a[..end]
        });
        let exchange = Exchange {
            direction: record.direction,
            port: record.port,
            method: &request.method,
            path: &request.path,
            status: record.status,
            head_ms: record.head_ms,
            host: authority,
            grpc: media(&request.headers).starts_with("application/grpc"),
            observation: Observation {
                query: &request.query,
                req_headers: &req,
                req_body: request_body
                    .as_ref()
                    .and_then(|(_, bytes)| bytes.as_deref()),
                resp_headers: &resp,
                resp_body: response_body
                    .as_ref()
                    .and_then(|(_, bytes)| bytes.as_deref()),
            },
        };
        let outbound = record.direction == Direction::Outbound;
        let (outcome, endpoints, sample_bytes, limits, epoch) = if outbound {
            self.record_egress(record, &exchange)
        } else {
            let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            // Read under the lock, where the shipper writes them: the pending report's samples
            // and the egress report's (at most half of the budget) share the budget.
            let epoch = self.handoff.epoch.load(Ordering::Acquire);
            let limits = Limits {
                sample_budget: self
                    .limits
                    .sample_budget
                    .saturating_sub(self.handoff.pending_samples.load(Ordering::Acquire))
                    .saturating_sub(self.handoff.egress_samples.load(Ordering::Acquire)),
                ..self.limits
            };
            let outcome = report.record(&limits, &self.redactor, &exchange);
            let new_endpoint = matches!(
                outcome,
                Outcome::Recorded {
                    new_endpoint: true,
                    ..
                }
            );
            let endpoints = new_endpoint.then(|| keys(&report));
            // The gauges cover the pending report too.
            match &endpoints {
                Some(keys) => publish(&self.metrics, &self.handoff, &report, keys),
                None => self
                    .metrics
                    .set_sample_bytes(held_samples(&self.handoff, &report)),
            }
            (outcome, endpoints, report.sample_bytes(), limits, epoch)
        };
        self.restart_shape(epoch);
        match outcome {
            Outcome::Invalid => self.metrics.exchange(direction, "invalid"),
            Outcome::NotFound => self.metrics.exchange(direction, "not_found"),
            Outcome::AuthorityLimit => {
                self.metrics.exchange(direction, "authority_limit");
                self.warn_authority_limit();
            }
            Outcome::EndpointLimit => {
                self.metrics.exchange(direction, "endpoint_limit");
                if self.endpoint_limit_logged.insert(direction) {
                    tracing::warn!(
                        target: "discovery",
                        direction,
                        max_endpoints = self.limits.max_endpoints,
                        "endpoint limit reached"
                    );
                }
            }
            Outcome::Recorded { sample, .. } => {
                for (part, body) in [("request", &request_body), ("response", &response_body)] {
                    if let Some((result, _)) = body {
                        self.metrics.body(direction, part, result);
                    }
                }
                self.count_redactions(
                    &request,
                    &response_headers,
                    request_body.as_ref().and_then(|(_, b)| b.as_deref()),
                    response_body.as_ref().and_then(|(_, b)| b.as_deref()),
                );
                self.samples(direction, sample, sample_bytes, limits.sample_budget);
                if let Some(endpoints) = endpoints {
                    let scope = outbound.then(|| egress_key(record, exchange.host));
                    self.shape_changed(record, &request, scope.as_ref(), endpoints);
                }
                // Last: the exchange counts as recorded once everything about it is visible.
                self.metrics.exchange(direction, "recorded");
            }
        }
        if !outbound {
            self.sample_bytes = sample_bytes;
        }
    }

    /// Records an outbound exchange into the egress report (spec §6.1). Its samples hold at
    /// most half of the budget, and never more than inbound leaves.
    fn record_egress(
        &mut self,
        record: &Record,
        exchange: &Exchange<'_>,
    ) -> (Outcome, Option<BTreeSet<Key>>, usize, Limits, u64) {
        let key = egress_key(record, exchange.host);
        let (inbound_bytes, pending_egress, epoch) = {
            let report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            (
                report
                    .sample_bytes()
                    .saturating_add(self.handoff.pending_samples.load(Ordering::Acquire)),
                self.handoff.pending_egress_samples.load(Ordering::Acquire),
                self.handoff.epoch.load(Ordering::Acquire),
            )
        };
        // The pending report's outbound samples count against the half; all its samples
        // count against the budget (`inbound_bytes` holds them).
        let cap = (self.limits.sample_budget / 2)
            .saturating_sub(pending_egress)
            .min(self.limits.sample_budget.saturating_sub(inbound_bytes));
        // The core takes half of `sample_budget` for outbound samples.
        let limits = Limits {
            sample_budget: cap.saturating_mul(2),
            ..self.limits
        };
        let via = Via {
            tls: record.tls,
            sni: record.sni.as_deref(),
            dst: Some(record.dst),
        };
        let (outcome, endpoints, sample_bytes) = {
            let mut egress = self.egress.lock().unwrap_or_else(PoisonError::into_inner);
            let outcome = egress.record_at(key, &via, &limits, &self.redactor, exchange);
            self.metrics.set_authorities(egress.authorities());
            let new_endpoint = matches!(
                outcome,
                Outcome::Recorded {
                    new_endpoint: true,
                    ..
                }
            );
            let endpoints = new_endpoint.then(|| keys(&egress));
            self.handoff
                .egress_samples
                .store(egress.sample_bytes(), Ordering::Release);
            if let Some(keys) = &endpoints {
                *self
                    .handoff
                    .egress_keys
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = keys.clone();
            }
            (outcome, endpoints, egress.sample_bytes())
        };
        {
            let report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            if endpoints.is_some() {
                publish(&self.metrics, &self.handoff, &report, &keys(&report));
            } else {
                self.metrics
                    .set_sample_bytes(held_samples(&self.handoff, &report));
            }
        }
        let limits = Limits {
            sample_budget: cap,
            ..self.limits
        };
        (outcome, endpoints, sample_bytes, limits, epoch)
    }

    fn samples(
        &mut self,
        direction: &'static str,
        sample: Sampled,
        sample_bytes: usize,
        sample_budget: usize,
    ) {
        // A rejection says nothing about room: only an exchange that was not rejected re-arms.
        if sample != Sampled::Budget && rearmed(sample_bytes, sample_budget) {
            self.sample_budget_full = false;
        }
        match sample {
            Sampled::Admitted => self.metrics.sample(direction, "kept"),
            Sampled::Budget => {
                self.metrics.sample(direction, "budget");
                if !self.sample_budget_full {
                    self.sample_budget_full = true;
                    tracing::warn!(
                        target: "discovery",
                        direction,
                        sample_budget,
                        "sample budget full"
                    );
                }
            }
            Sampled::None => {}
        }
    }

    /// The shipper froze the report since the last exchange: the tree starts anew, so the
    /// shape known so far is not the report's any more (the shipper has set the gauges).
    fn restart_shape(&mut self, epoch: u64) {
        if epoch == self.epoch {
            return;
        }
        self.epoch = epoch;
        self.known.clear();
        // A freeze takes the egress report with it.
        self.egress_known.clear();
    }

    /// A new endpoint was recorded: the template tree changed, and may have absorbed a node.
    fn shape_changed(
        &mut self,
        record: &Record,
        request: &Request,
        authority: Option<&ScopeKey>,
        now: BTreeSet<Key>,
    ) {
        let direction = label(record.direction);
        let known = if record.direction == Direction::Outbound {
            &mut self.egress_known
        } else {
            &mut self.known
        };
        let scope = authority.cloned().unwrap_or(ScopeKey::Port(record.port));
        let covered = self.seen.iter().any(|(d, s, method, template)| {
            *d == record.direction
                && *s == scope
                && *method == request.method
                && matches(template, &request.path).is_some()
        });
        if !covered {
            self.metrics.new_endpoint(direction);
        }
        let removed: Vec<&Key> = known.difference(&now).collect();
        // What the tree lost to absorption leaves the set; the endpoint of this exchange
        // joins it, and a template drops the literals it covers.
        for key in &removed {
            self.seen.remove(*key);
        }
        let own = now
            .iter()
            .filter(|(d, s, method, _)| {
                *d == record.direction && *s == scope && *method == request.method
            })
            .filter_map(|key| Some((matches(&key.3, &request.path)?, key)))
            .min()
            .map(|(_, key)| key.clone());
        if let Some(own) = own {
            if own.3.contains('{') {
                self.seen.retain(|(d, s, method, template)| {
                    !(*d == own.0
                        && *s == own.1
                        && *method == own.2
                        && *template != own.3
                        && matches(&own.3, template).is_some())
                });
            }
            if !covered && self.seen.len() < self.limits.max_endpoints.saturating_mul(4) {
                self.seen.insert(own);
            }
        }
        if removed.is_empty() {
            for (d, scope, method, template) in now.difference(known) {
                match scope {
                    ScopeKey::Port(port) => tracing::info!(
                        target: "discovery",
                        direction = label(*d),
                        port,
                        method,
                        template,
                        "endpoint discovered"
                    ),
                    ScopeKey::Authority(authority) => tracing::info!(
                        target: "discovery",
                        direction = label(*d),
                        authority,
                        method,
                        template,
                        "endpoint discovered"
                    ),
                }
            }
        } else {
            let template = now
                .iter()
                .filter(|(d, scope, method, _)| {
                    *d == record.direction
                        && *method == request.method
                        && match authority {
                            Some(authority) => scope == authority,
                            None => *scope == ScopeKey::Port(record.port),
                        }
                })
                .filter_map(|(.., template)| Some((matches(template, &request.path)?, template)))
                .min()
                .map(|(_, template)| template.as_str())
                .unwrap_or_default();
            let old: Vec<String> = removed
                .iter()
                .filter(|(_, scope, ..)| authority.is_none_or(|a| scope == a))
                .take(MAX_OLD_LOGGED)
                .map(|(_, _, method, template)| format!("{method} {template}"))
                .collect();
            let authority = authority.map(|a| a.to_string());
            tracing::info!(
                target: "discovery",
                direction,
                authority = authority.as_deref(),
                port = record.port,
                method = %request.method,
                template,
                old = %old.join(", "),
                old_count = removed.len(),
                "template absorbed"
            );
        }
        *known = now;
    }

    fn count_redactions(
        &self,
        request: &Request,
        response_headers: &[(String, String)],
        request_body: Option<&[u8]>,
        response_body: Option<&[u8]>,
    ) {
        let mut counts = Counts::new();
        redactions::path(&mut counts, &request.path);
        redactions::pairs(&mut counts, &self.redactor, "query", &request.query);
        redactions::headers(&mut counts, &self.redactor, &request.headers);
        redactions::headers(&mut counts, &self.redactor, response_headers);
        for (part, headers) in [
            (request_body, request.headers.as_slice()),
            (response_body, response_headers),
        ] {
            let Some(bytes) = part else { continue };
            let media = media(headers);
            if is_json(&media) {
                redactions::json(&mut counts, &self.redactor, bytes);
            } else if media == "application/x-www-form-urlencoded" {
                redactions::pairs(
                    &mut counts,
                    &self.redactor,
                    "body",
                    &String::from_utf8_lossy(bytes),
                );
            }
        }
        for ((location, rule), n) in counts {
            self.metrics.redaction(location, rule, n);
        }
    }
}

/// Whether the samples fell far enough below the budget (under 7/8 of it) for the
/// `sample budget full` line to be due again when the budget fills anew. A merge that swaps a
/// sample for a slightly smaller one does not count.
fn rearmed(sample_bytes: usize, sample_budget: usize) -> bool {
    sample_bytes < sample_budget - sample_budget / 8
}

#[cfg(test)]
mod tests {
    use mimic_runtime::metrics::Metrics;

    use super::*;

    fn recorder(metrics: &DiscoveryMetrics) -> Recorder {
        Recorder::new(
            Arc::new(Mutex::new(Report::new(None))),
            Arc::new(Mutex::new(Report::new(None))),
            Limits {
                max_literals: 32,
                max_endpoints: 4,
                max_authorities: 64,
                sample_budget: 1 << 20,
            },
            Redactor::default(),
            metrics.clone(),
            Arc::default(),
        )
    }

    #[test]
    fn dsc_18_a_recorder_panic_is_counted_and_the_recorder_goes_on() {
        let metrics = DiscoveryMetrics::default();
        let mut registry = Metrics::new("sidecar", "test");
        metrics.register(&mut registry);
        let mut recorder = recorder(&metrics);
        recorder.guarded("inbound", |_| panic!("boom"));
        let mut ran = false;
        recorder.guarded("inbound", |_| ran = true);
        assert!(ran);
        let text = registry.encode();
        assert!(
            text.contains(
                "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"panic\"} 1"
            ),
            "{text}"
        );
    }

    #[test]
    fn dsc_18_the_budget_warning_is_rearmed_only_when_samples_were_freed() {
        let budget = 1000;
        assert!(!rearmed(1000, budget));
        // Merging replaces a sample with a slightly smaller one: still full.
        assert!(!rearmed(990, budget));
        assert!(!rearmed(875, budget));
        assert!(rearmed(874, budget));
        assert!(rearmed(0, budget));
    }
}

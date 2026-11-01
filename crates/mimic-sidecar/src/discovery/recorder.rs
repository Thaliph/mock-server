//! The recorder thread: the only consumer of the queue and the only writer of the local report
//! (spec §6). Parsing of the captured heads, body classification, the core `Report::record`,
//! metrics and logs all happen here, never on a proxy task.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Outcome, Report, Sampled};

use super::capture::{Part, Record};
use super::decode;
use super::metrics::DiscoveryMetrics;
use super::redactions::{self, Counts};

/// Longest authority kept, as in the codec.
const MAX_AUTHORITY: usize = 1024;
/// Old templates named in a `template absorbed` line.
const MAX_OLD_LOGGED: usize = 16;

type Key = (Direction, u16, String, String);

pub(super) struct Recorder {
    pub(super) report: Arc<Mutex<Report>>,
    pub(super) limits: Limits,
    pub(super) redactor: Redactor,
    pub(super) metrics: DiscoveryMetrics,
    /// Endpoints of the report as of the last change of its shape.
    known: BTreeSet<Key>,
    sample_budget_full: bool,
    endpoint_limit_logged: bool,
    sample_bytes: usize,
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
    let (t, p): (Vec<_>, Vec<_>) = (template.split('/').collect(), path.split('/').collect());
    if t.len() != p.len() {
        return None;
    }
    let mut params = 0;
    for (a, b) in t.iter().zip(&p) {
        if a.starts_with('{') && a.ends_with('}') {
            params += 1;
        } else if a != b {
            return None;
        }
    }
    Some(params)
}

impl Recorder {
    pub(super) fn new(
        report: Arc<Mutex<Report>>,
        limits: Limits,
        redactor: Redactor,
        metrics: DiscoveryMetrics,
    ) -> Recorder {
        Recorder {
            report,
            limits,
            redactor,
            metrics,
            known: BTreeSet::new(),
            sample_budget_full: false,
            endpoint_limit_logged: false,
            sample_bytes: 0,
        }
    }

    pub(super) fn run(mut self, queue: Receiver<Record>) {
        for record in queue {
            let direction = label(record.direction);
            self.guarded(direction, |recorder| recorder.record(&record));
        }
    }

    /// Runs `f` (stub).
    fn guarded(&mut self, _direction: &'static str, f: impl FnOnce(&mut Self)) {
        f(self);
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
        let (outcome, endpoints, sample_bytes) = {
            let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
            let outcome = report.record(&self.limits, &self.redactor, &exchange);
            let new_endpoint = matches!(
                outcome,
                Outcome::Recorded {
                    new_endpoint: true,
                    ..
                }
            );
            let endpoints = new_endpoint.then(|| {
                report
                    .endpoints()
                    .into_iter()
                    .map(|e| (e.direction, e.port, e.method.to_string(), e.template))
                    .collect::<BTreeSet<Key>>()
            });
            (outcome, endpoints, report.sample_bytes())
        };
        match outcome {
            Outcome::Invalid => self.metrics.exchange(direction, "invalid"),
            Outcome::NotFound => self.metrics.exchange(direction, "not_found"),
            Outcome::EndpointLimit => {
                self.metrics.exchange(direction, "endpoint_limit");
                if !self.endpoint_limit_logged {
                    self.endpoint_limit_logged = true;
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
                self.samples(direction, sample, sample_bytes);
                if let Some(endpoints) = endpoints {
                    self.shape_changed(record, &request, endpoints);
                }
                // Last: the exchange counts as recorded once everything about it is visible.
                self.metrics.exchange(direction, "recorded");
            }
        }
        self.metrics.set_sample_bytes(sample_bytes);
        self.sample_bytes = sample_bytes;
    }

    fn samples(&mut self, direction: &'static str, sample: Sampled, sample_bytes: usize) {
        if sample_bytes < self.sample_bytes {
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
                        sample_budget = self.limits.sample_budget,
                        "sample budget full"
                    );
                }
            }
            Sampled::None => {}
        }
    }

    /// A new endpoint was recorded: the template tree changed, and may have absorbed a node.
    fn shape_changed(&mut self, record: &Record, request: &Request, now: BTreeSet<Key>) {
        let direction = label(record.direction);
        let removed: Vec<&Key> = self.known.difference(&now).collect();
        if removed.is_empty() {
            for (d, port, method, template) in now.difference(&self.known) {
                tracing::info!(
                    target: "discovery",
                    direction = label(*d),
                    port,
                    method,
                    template,
                    "endpoint discovered"
                );
            }
        } else {
            let template = now
                .iter()
                .filter(|(d, port, method, _)| {
                    *d == record.direction && *port == record.port && *method == request.method
                })
                .filter_map(|(.., template)| Some((matches(template, &request.path)?, template)))
                .min()
                .map(|(_, template)| template.as_str())
                .unwrap_or_default();
            let old: Vec<String> = removed
                .iter()
                .take(MAX_OLD_LOGGED)
                .map(|(_, _, method, template)| format!("{method} {template}"))
                .collect();
            tracing::info!(
                target: "discovery",
                direction,
                port = record.port,
                method = %request.method,
                template,
                old = %old.join(", "),
                old_count = removed.len(),
                "template absorbed"
            );
        }
        for d in [Direction::Inbound, Direction::Outbound] {
            let n = now.iter().filter(|(k, ..)| *k == d).count();
            if n > 0 || self.known.iter().any(|(k, ..)| *k == d) {
                self.metrics.set_endpoints(label(d), n);
            }
        }
        self.known = now;
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

/// Whether the samples fell far enough below the budget for a new `sample budget full` line
/// to be due when it fills again (stub).
fn rearmed(_sample_bytes: usize, _sample_budget: usize) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use mimic_runtime::metrics::Metrics;

    use super::*;

    fn recorder(metrics: &DiscoveryMetrics) -> Recorder {
        Recorder::new(
            Arc::new(Mutex::new(Report::new(None))),
            Limits {
                max_literals: 32,
                max_endpoints: 4,
                sample_budget: 1 << 20,
            },
            Redactor::default(),
            metrics.clone(),
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

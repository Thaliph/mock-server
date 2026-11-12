//! The long-poll client of `GET /v1/workloads/<deployment>/mesh` (spec §7.9): one request at a
//! time, the token read again for each, the view validated before it replaces the one held.

use std::future::Future;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use mimic_core::Mode;
use mimic_core::mesh::view::{View, etag};
use tokio::time::sleep;

use super::{MeshConfig, MeshState, retry_delay};
use crate::discovery::{Target, retry_after};
use crate::mode::{Announced, ModeState};
use crate::tls::ca::api_client;

/// After a `200` or `304` the next request waits this long: a controller that answers at once
/// every time is not hammered.
const MIN_GAP: Duration = Duration::from_millis(100);

/// What a request came to.
enum Fetched {
    Applied(Arc<View>),
    /// A `304`, or a `200` with the view held.
    NotModified,
    Failed(Failure),
}

struct Failure {
    /// The `result` label.
    result: &'static str,
    status: Option<u16>,
    reason: Option<String>,
    retry_after: Option<Duration>,
}

impl Failure {
    fn error(status: Option<u16>, reason: Option<String>, retry_after: Option<Duration>) -> Self {
        Failure {
            result: "error",
            status,
            reason,
            retry_after,
        }
    }

    fn invalid(reason: impl Into<String>) -> Self {
        Failure {
            result: "invalid",
            status: None,
            reason: Some(reason.into()),
            retry_after: None,
        }
    }
}

enum Body {
    TooLarge,
    Error(String),
}

/// Follows the view of one deployment.
pub struct Follower {
    http: reqwest::Client,
    target: Target,
    state: MeshState,
    mode: ModeState,
    max_bytes: usize,
    /// Failures in a row.
    failures: u32,
    /// The result of the streak whose warning is written.
    warned: Option<&'static str>,
}

impl Follower {
    /// The client, or `None` when the mesh is off or there is no controller to ask.
    pub fn new(
        config: MeshConfig,
        state: MeshState,
        mode: ModeState,
    ) -> Result<Option<Follower>, String> {
        if !config.enabled {
            tracing::info!(target: "mesh", "mesh off: no mesh view is pulled");
            return Ok(None);
        }
        let Some(target) = config.target else {
            tracing::info!(target: "mesh", "mesh view pull disabled: no controller URL");
            return Ok(None);
        };
        Ok(Some(Follower {
            http: api_client(&target.ca, config.timeout)?,
            target,
            state,
            mode,
            max_bytes: config.limits.max_view_bytes,
            failures: 0,
            warned: None,
        }))
    }

    /// Follows until `stop` completes; a request in flight is abandoned.
    pub async fn run(mut self, stop: impl Future<Output = ()>) {
        tokio::select! {
            () = stop => {}
            () = self.cycle() => {}
        }
    }

    async fn cycle(&mut self) {
        loop {
            let wait = self.pull().await;
            sleep(wait).await;
        }
    }

    /// One request; how long to wait before the next.
    async fn pull(&mut self) -> Duration {
        match self.fetch().await {
            Fetched::Applied(view) => {
                self.state.pull("ok");
                self.state.save(&view).await;
                // The mode is applied like a pulled one: saved, then switched (spec §8).
                if let Ok(mode) = view.mode.parse::<Mode>() {
                    self.mode
                        .observe(Announced {
                            mode: Ok(mode),
                            factor: None,
                        })
                        .await;
                }
                self.succeeded()
            }
            Fetched::NotModified => {
                self.state.pull("not_modified");
                self.state.touch();
                self.succeeded()
            }
            Fetched::Failed(failure) => self.failed(failure),
        }
    }

    fn succeeded(&mut self) -> Duration {
        self.failures = 0;
        if self.warned.take().is_some() {
            tracing::info!(target: "mesh", "mesh view pull recovered");
        }
        MIN_GAP
    }

    fn failed(&mut self, failure: Failure) -> Duration {
        self.state.pull(failure.result);
        // One warning per streak of the same kind of failure.
        if self.warned != Some(failure.result) {
            self.warned = Some(failure.result);
            match failure.result {
                "invalid" => {
                    tracing::warn!(target: "mesh", reason = failure.reason, "mesh view rejected");
                }
                "older" => {
                    tracing::warn!(target: "mesh", "mesh view older than the one held, kept the held one");
                }
                _ => tracing::warn!(
                    target: "mesh",
                    status = failure.status,
                    error = failure.reason,
                    "mesh view pull failed"
                ),
            }
        }
        let delay = retry_delay(self.failures, rand::random_range(0.5..=1.0));
        self.failures = self.failures.saturating_add(1);
        delay.max(failure.retry_after.unwrap_or_default())
    }

    /// The token is read again for each request: the kubelet rotates it.
    async fn fetch(&mut self) -> Fetched {
        let token = match tokio::fs::read_to_string(&self.target.token_file).await {
            Ok(token) => token,
            Err(e) => {
                let error = format!("token file: {}", e.kind());
                return Fetched::Failed(Failure::error(None, Some(error), None));
            }
        };
        let url = format!(
            "{}/v1/workloads/{}/mesh?format=1",
            self.target.url, self.target.workload
        );
        let mut request = self
            .http
            .get(url)
            .bearer_auth(token.trim())
            .header(reqwest::header::ACCEPT_ENCODING, "gzip");
        if let Some(etag) = self.state.etag() {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        // Without the URL: it is the configured one, and nothing else is useful here.
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                let error = e.without_url().to_string();
                return Fetched::Failed(Failure::error(None, Some(error), None));
            }
        };
        let status = response.status().as_u16();
        match status {
            200 => self.read(response).await,
            304 => Fetched::NotModified,
            _ => {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after);
                Fetched::Failed(Failure::error(Some(status), None, wait))
            }
        }
    }

    /// Reads the body of a `200`, at most `max_bytes` of it before and after decompression, and
    /// checks it. The buffers are freed before the view is returned.
    async fn read(&self, response: reqwest::Response) -> Fetched {
        let header = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // `reqwest` may have decoded it already, and then says nothing of it.
        let gzip = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
        let cap = self.max_bytes;
        let body = match read_capped(response, cap).await {
            Ok(body) => body,
            Err(Body::TooLarge) => return Fetched::Failed(Failure::invalid("view too large")),
            Err(Body::Error(error)) => {
                return Fetched::Failed(Failure::error(None, Some(error), None));
            }
        };
        let limits = self.state.limits().clone();
        // On a blocking thread: parsing a large view must not stall the proxy's workers.
        let parsed = tokio::task::spawn_blocking(move || {
            let bytes = if gzip { gunzip(&body, cap)? } else { body };
            let text = String::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())?;
            let view = View::from_json(&text, &limits)?;
            let etag = header.unwrap_or_else(|| etag(&text));
            Ok::<_, String>((view, etag))
        })
        .await;
        match parsed {
            Ok(Ok((view, etag))) => self.offer(Arc::new(view), &etag),
            Ok(Err(reason)) => Fetched::Failed(Failure::invalid(reason)),
            Err(e) => Fetched::Failed(Failure::error(None, Some(e.to_string()), None)),
        }
    }

    fn offer(&self, view: Arc<View>, etag: &str) -> Fetched {
        if self.state.etag().as_deref() == Some(etag) {
            return Fetched::NotModified;
        }
        if self.state.offer(view.clone(), etag) {
            Fetched::Applied(view)
        } else {
            Fetched::Failed(Failure {
                result: "older",
                status: None,
                reason: None,
                retry_after: None,
            })
        }
    }
}

/// `body` decompressed, refused past `cap` bytes.
fn gunzip(body: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| "not gzip".to_string())?;
    if out.len() > cap {
        return Err(format!("view over {cap} bytes"));
    }
    Ok(out)
}

/// The body of `response`, read chunk by chunk and refused as soon as it goes over `cap`.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, Body> {
    if response
        .content_length()
        .is_some_and(|n| usize::try_from(n).map_or(true, |n| n > cap))
    {
        return Err(Body::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Body::Error(e.without_url().to_string()))?
    {
        if bytes.len() + chunk.len() > cap {
            return Err(Body::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

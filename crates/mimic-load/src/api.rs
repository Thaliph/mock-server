//! The client of the load API (spec §7.8 "Load API"): plan, targets, results.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use mimic_core::load::summary::PodResults;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::config::Config;

/// Slack over the plan limit for the rest of the plan answer.
const PLAN_SLACK: usize = 64 << 10;
const MAX_TARGETS_ANSWER: usize = 4 << 20;
const MAX_POST_ANSWER: usize = 64 << 10;
/// Time allowed to read the plan, which can be large.
const PLAN_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a call has no answer. Never carries a URL, a token or a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// `404`: the run is unknown or ended.
    Gone,
    Status(u16),
    /// The answer is over the limit of its route.
    TooLarge,
    Transport(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Gone => f.write_str("run unknown or ended (404)"),
            ApiError::Status(code) => write!(f, "status {code}"),
            ApiError::TooLarge => f.write_str("answer too large"),
            ApiError::Transport(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodParams {
    pub concurrency: u64,
    pub rate: Option<u64>,
    pub duration_ms: u64,
    pub ramp_up_ms: u64,
    pub timeout_ms: u64,
    pub start_timeout_ms: u64,
}

#[derive(Deserialize)]
pub struct PlanAnswer {
    pub plan: Box<RawValue>,
    pub ticket: String,
    pub worker: u64,
    pub workers: u64,
    pub params: PodParams,
}

#[derive(Debug, Deserialize)]
pub struct TargetEntry {
    pub ip: String,
    pub pod: String,
}

#[derive(Debug, Deserialize)]
pub struct TargetsAnswer {
    pub stop: bool,
    pub targets: Vec<TargetEntry>,
}

impl TargetsAnswer {
    /// The targets whose address parses.
    pub fn addresses(&self) -> Vec<(IpAddr, String)> {
        self.targets
            .iter()
            .filter_map(|t| Some((t.ip.parse().ok()?, t.pod.clone())))
            .collect()
    }
}

#[derive(Serialize)]
pub struct Post<'a> {
    #[serde(rename = "final")]
    pub final_post: bool,
    pub pod: &'a str,
    pub reason: Option<&'static str>,
    pub results: &'a PodResults,
}

#[derive(Deserialize)]
struct PostAnswer {
    stop: bool,
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
    max_plan: usize,
    timeout: Duration,
}

impl Api {
    pub fn new(config: &Config) -> Result<Api, String> {
        // Already installed by an earlier call: the existing provider is the same one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let anchor =
            reqwest::Certificate::from_pem(config.ca.as_bytes()).map_err(|e| e.to_string())?;
        let http = reqwest::Client::builder()
            .tls_certs_only([anchor])
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Api {
            http,
            base: format!(
                "{}/v1/load/{}/{}",
                config.url, config.run.name, config.run.uid
            ),
            token_file: config.token_file.clone(),
            max_plan: config.max_plan,
            timeout: config.interval,
        })
    }

    /// The token is read again for each call: the kubelet rotates it.
    async fn token(&self) -> Result<String, ApiError> {
        let token = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|e| ApiError::Transport(format!("token file: {}", e.kind())))?;
        Ok(token.trim().to_string())
    }

    async fn call(
        &self,
        method: reqwest::Method,
        route: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
        cap: usize,
    ) -> Result<Vec<u8>, ApiError> {
        let token = self.token().await?;
        let mut request = self
            .http
            .request(method, format!("{}/{route}", self.base))
            .bearer_auth(token)
            .timeout(timeout);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        // Without the URL: it is the configured one, and nothing else is useful here.
        let mut response = request
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?;
        match response.status().as_u16() {
            200 => {}
            404 => return Err(ApiError::Gone),
            code => return Err(ApiError::Status(code)),
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?
        {
            if bytes.len() + chunk.len() > cap {
                return Err(ApiError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    pub async fn plan(&self) -> Result<PlanAnswer, ApiError> {
        let bytes = self
            .call(
                reqwest::Method::GET,
                "plan",
                None,
                PLAN_TIMEOUT,
                self.max_plan + PLAN_SLACK,
            )
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::Transport("invalid plan answer".into()))
    }

    pub async fn targets(&self) -> Result<TargetsAnswer, ApiError> {
        let bytes = self
            .call(
                reqwest::Method::GET,
                "targets",
                None,
                self.timeout,
                MAX_TARGETS_ANSWER,
            )
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::Transport("invalid targets answer".into()))
    }

    /// Posts the cumulative results; the answer says whether to stop.
    pub async fn post(&self, post: &Post<'_>) -> Result<bool, ApiError> {
        let body = serde_json::to_vec(post)
            .map_err(|_| ApiError::Transport("results not serialisable".into()))?;
        let bytes = self
            .call(
                reqwest::Method::POST,
                "results",
                Some(body),
                self.timeout,
                MAX_POST_ANSWER,
            )
            .await?;
        let answer: PostAnswer = serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::Transport("invalid results answer".into()))?;
        Ok(answer.stop)
    }
}

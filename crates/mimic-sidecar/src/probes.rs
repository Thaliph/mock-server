//! `/app-health/<container>/<livez|readyz|startupz>` (spec §4, INJ-18): replays the app's
//! original `httpGet` probes, which the webhook rewrote to point at the sidecar admin port.
//! The sidecar runs as UID 1337, so these requests are not captured by the redirection.

use std::collections::BTreeMap;
use std::net::Ipv6Addr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap as RequestHeaders, StatusCode, header};
use axum::routing::get;
use mimic_runtime::config::{ConfigError, Env};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, redirect};
use serde::Deserialize;

const PROBES_VAR: &str = "MIMIC_APP_PROBES";
const POD_IP_VAR: &str = "MIMIC_POD_IP";
const KINDS: [&str; 3] = ["livez", "readyz", "startupz"];
/// Kubernetes' default `timeoutSeconds`.
const DEFAULT_TIMEOUT_SECONDS: u64 = 1;
/// Headers of the kubelet's request that the app would have received.
const KUBELET_HEADERS: [header::HeaderName; 2] = [header::USER_AGENT, header::ACCEPT];

/// One original probe, as the webhook writes it in `MIMIC_APP_PROBES`: the fields of a
/// Kubernetes `HTTPGetAction` with the named port resolved, and the probe's `timeoutSeconds`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProbe {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    path: String,
    port: u16,
    #[serde(default = "http")]
    scheme: String,
    #[serde(default)]
    http_headers: Vec<RawHeader>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawHeader {
    name: String,
    value: String,
}

fn http() -> String {
    "HTTP".to_string()
}

#[derive(Debug)]
struct Probe {
    url: String,
    headers: HeaderMap,
    timeout: Duration,
}

/// The configured probes and the client that replays them.
pub struct Probes {
    probes: BTreeMap<String, Probe>,
    client: Client,
}

impl Probes {
    /// Reads `MIMIC_APP_PROBES` (default `{}`) and `MIMIC_POD_IP`, the default probe host.
    pub fn from_env(env: &Env) -> Result<Probes, ConfigError> {
        let raw = env.optional(PROBES_VAR).unwrap_or("{}");
        // The value can carry header values (credentials): it is never echoed.
        let invalid = |reason: String| ConfigError::invalid(PROBES_VAR, "<redacted>", reason);
        let parsed: BTreeMap<String, RawProbe> = serde_json::from_str(raw).map_err(|e| {
            // serde_json's message can quote input: keep only the position.
            invalid(format!(
                "not a valid probe set (line {}, column {})",
                e.line(),
                e.column()
            ))
        })?;
        let pod_ip = env.optional(POD_IP_VAR);
        let mut probes = BTreeMap::new();
        for (key, raw) in parsed {
            let probe = Probe::build(&key, raw, pod_ip).map_err(invalid)?;
            probes.insert(key, probe);
        }
        let client = client().map_err(|e| invalid(format!("HTTP client: {e}")))?;
        Ok(Probes { probes, client })
    }

    async fn run(&self, probe: &Probe, kubelet: &RequestHeaders) -> StatusCode {
        let mut headers = HeaderMap::new();
        for name in &KUBELET_HEADERS {
            if let Some(value) = kubelet.get(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
        for (name, value) in &probe.headers {
            headers.append(name.clone(), value.clone());
        }
        let response = self
            .client
            .get(&probe.url)
            .headers(headers)
            .timeout(probe.timeout)
            .send()
            .await;
        match response {
            Ok(r) => {
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
            }
            Err(e) => {
                tracing::debug!(url = %probe.url, error = %e, "app probe failed");
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }
}

impl Probe {
    fn build(key: &str, raw: RawProbe, pod_ip: Option<&str>) -> Result<Probe, String> {
        match key.split_once('/') {
            Some((container, kind)) if !container.is_empty() && KINDS.contains(&kind) => {}
            _ => {
                return Err(format!(
                    "key {key:?} is not <container>/<livez|readyz|startupz>"
                ));
            }
        }
        let scheme = match raw.scheme.as_str() {
            "HTTP" => "http",
            "HTTPS" => "https",
            other => return Err(format!("{key}: unsupported scheme {other:?}")),
        };
        if raw.port == 0 {
            return Err(format!("{key}: port must be 1-65535"));
        }
        let host = match (raw.host.as_deref().filter(|h| !h.is_empty()), pod_ip) {
            (Some(host), _) | (None, Some(host)) => host,
            (None, None) => return Err(format!("{key}: no host and {POD_IP_VAR} is unset")),
        };
        let host = match host.parse::<Ipv6Addr>() {
            Ok(_) => format!("[{host}]"),
            Err(_) => host.to_string(),
        };
        let path = match raw.path.as_str() {
            "" => "/",
            p if p.starts_with('/') => p,
            _ => return Err(format!("{key}: path must start with '/'")),
        };
        let url = format!("{scheme}://{host}:{}{path}", raw.port);
        reqwest::Url::parse(&url).map_err(|e| format!("{key}: invalid URL: {e}"))?;
        let mut headers = HeaderMap::new();
        for h in raw.http_headers {
            let name = HeaderName::from_bytes(h.name.as_bytes())
                .map_err(|_| format!("{key}: invalid header name"))?;
            let value = HeaderValue::from_str(&h.value)
                .map_err(|_| format!("{key}: invalid value for header {name}"))?;
            headers.append(name, value);
        }
        let seconds = raw
            .timeout_seconds
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        Ok(Probe {
            url,
            headers,
            timeout: Duration::from_secs(seconds),
        })
    }
}

/// Like the kubelet: no certificate verification, no redirect, a fresh connection per probe,
/// and no proxy from the environment.
fn client() -> Result<Client, reqwest::Error> {
    // Already installed by an earlier call: the existing provider is the same one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .no_proxy()
        .build()
}

/// Routes to merge into the admin router. Any `/app-health/` path that is not a configured
/// probe answers 404.
pub fn router(probes: Arc<Probes>) -> Router {
    Router::new()
        .route("/app-health/{container}/{kind}", get(forward))
        .with_state(probes)
}

async fn forward(
    State(probes): State<Arc<Probes>>,
    Path((container, kind)): Path<(String, String)>,
    headers: RequestHeaders,
) -> StatusCode {
    match probes.probes.get(&format!("{container}/{kind}")) {
        Some(probe) => probes.run(probe, &headers).await,
        None => StatusCode::NOT_FOUND,
    }
}

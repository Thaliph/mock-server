//! `GET /v1/workloads/<deployment>/mesh?format=1` (spec §7.2): the mesh view of the caller's own
//! deployment, as a long-poll. Admission, authentication and authorisation are those of the
//! other routes: the namespace is the token's, and a pod reads its own deployment's view only.
//! What the view holds, and what a namespace that did not consent never shows, is decided by
//! `mimic_core::mesh` (MSH-01 to MSH-04).

use axum::body::Bytes;
use axum::http::HeaderValue;
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY,
};

use super::*;
use crate::mesh::{Answer as Held, Entry, Hub};
use crate::tls::Hold;

pub(super) const ENDPOINT: &str = "/v1/workloads/{deployment}/mesh";

/// The held-request budget is spent.
const WATCHES_FULL: Refusal = Refusal {
    retry: true,
    ..refusal(StatusCode::SERVICE_UNAVAILABLE, "watch_limit")
};

enum Answered {
    View(Arc<Entry>, bool),
    NotModified(String),
}

/// The body of a `200`: the view itself, never a copy.
struct Plain(Arc<Entry>);

impl AsRef<[u8]> for Plain {
    fn as_ref(&self) -> &[u8] {
        self.0.json.as_bytes()
    }
}

struct Zipped(Arc<Entry>);

impl AsRef<[u8]> for Zipped {
    fn as_ref(&self) -> &[u8] {
        self.0.gzip()
    }
}

/// Whether `Accept-Encoding` lists gzip with a quality above 0.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or_default().trim();
            let quality = parts
                .find_map(|p| p.trim().strip_prefix("q="))
                .map_or(Some(1.0), |q| q.trim().parse::<f32>().ok());
            (coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip"))
                && quality.is_some_and(|q| q > 0.0)
        })
}

/// `format=1`, nothing else.
fn format_ok(query: Option<&str>) -> bool {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("format="))
        == Some("1")
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}

impl SidecarApi {
    pub(super) async fn mesh(
        &self,
        deployment: &str,
        headers: &HeaderMap,
        query: Option<&str>,
        peer: Option<std::net::IpAddr>,
        hold: Option<Hold>,
    ) -> Response {
        // Off: no view, and nothing read or reviewed for the request (design §3.4).
        let Some(hub) = &self.mesh else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let mut caller = Caller::default();
        let outcome = self
            .authorize_and_serve(hub, deployment, headers, query, peer, hold, &mut caller)
            .await;
        let result = match &outcome {
            Ok(Answered::View(..)) => "ok",
            Ok(Answered::NotModified(_)) => "not_modified",
            Err(refusal) => refusal.result,
        };
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(ENDPOINT, &caller, result, reason);
        match outcome {
            Ok(Answered::View(entry, gzip)) => {
                let (body, encoding) = if gzip {
                    (Body::from(Bytes::from_owner(Zipped(entry.clone()))), true)
                } else {
                    (Body::from(Bytes::from_owner(Plain(entry.clone()))), false)
                };
                let mut response = (
                    [
                        (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                        (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                        (VARY, HeaderValue::from_static("Accept-Encoding")),
                    ],
                    body,
                )
                    .into_response();
                if encoding {
                    response
                        .headers_mut()
                        .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                }
                with_etag(response, &entry.etag)
            }
            Ok(Answered::NotModified(etag)) => {
                with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag)
            }
            Err(refusal) => refusal.into_response(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn authorize_and_serve(
        &self,
        hub: &Arc<Hub>,
        deployment: &str,
        headers: &HeaderMap,
        query: Option<&str>,
        peer: Option<std::net::IpAddr>,
        hold: Option<Hold>,
        caller: &mut Caller,
    ) -> Result<Answered, Refusal> {
        self.admit(peer)?;
        let identity = self.authenticate(headers, peer, caller).await?;
        if !valid_name(deployment) {
            return Err(invalid("deployment"));
        }
        caller.deployment = deployment.to_string();
        let pod = identity.pod.as_deref().ok_or(FORBIDDEN)?;
        if !pod_of_deployment(pod, deployment) {
            return Err(FORBIDDEN);
        }
        if !format_ok(query) {
            return Err(invalid("format"));
        }
        let namespace = identity.namespace.as_str();
        // Every value of every header (RFC 9110 §13.1.2).
        let held: Vec<&str> = headers
            .get_all(IF_NONE_MATCH)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        let mode = || self.workloads.effective(namespace, deployment).mode();
        // Only an authorised caller, with a share of the watch budget, leaves the listener's cap.
        let start = || {
            if let Some(hold) = &hold {
                hold.start();
            }
        };
        match hub
            .answer(namespace, deployment, &mode, &held, &start)
            .await
        {
            Held::View(entry) => {
                caller.bytes = Some(entry.json.len());
                let gzip = accepts_gzip(headers);
                if gzip {
                    hub.compress(&entry);
                }
                Ok(Answered::View(entry, gzip))
            }
            Held::NotModified(etag) => {
                caller.bytes = Some(0);
                Ok(Answered::NotModified(etag))
            }
            Held::Busy => {
                caller.error = Some("watch budget spent".to_string());
                Err(WATCHES_FULL)
            }
            Held::NotReady => {
                caller.error = Some("mesh watches not synced".to_string());
                Err(UNAVAILABLE)
            }
        }
    }
}

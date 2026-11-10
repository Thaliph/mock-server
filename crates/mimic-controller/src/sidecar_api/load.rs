//! Load API (spec §7.8): the plan, the targets and the results of a run, for the pods of its
//! generator Job. Same listener, admission order, rate limit and `TokenReview` limits as the
//! sidecar API; the token has the audience `mimic.platform.io/load`.
//!
//! Nothing here logs or returns a ticket, a header, a body or an address of the run.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::Api;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use mimic_core::identity::generated_suffix;
use mimic_core::load::gate::{Target, targets};
use mimic_core::load::job::job_name;
use mimic_core::load::schedule::{pods, split};
use mimic_core::load::summary::PodResults;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::{
    Caller, FORBIDDEN, NOT_FOUND, Peer, Refusal, ReviewCache, Reviewer, SidecarApi, TOO_LARGE,
    UNAVAILABLE, content_length, invalid, read_exact, valid_name,
};
use crate::store::StoreError;
use crate::store::load::{MAX_POST, StoredPost};

/// Audience of the generator's projected token (spec §7.8).
pub const LOAD_AUDIENCE: &str = "mimic.platform.io/load";

/// What the load API needs to know of a `MimicLoadTest` that is running (spec §7.8): the
/// reconciler keeps the index in line with the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub deployment: String,
    pub concurrency: u64,
    pub rate: Option<u64>,
    pub duration_ms: u64,
    pub ramp_up_ms: u64,
    pub timeout_ms: u64,
    pub start_timeout_ms: u64,
    /// `MIMIC_LOAD_POD_CONCURRENCY` when the Job was built.
    pub pod_concurrency: u64,
    /// `spec.stop`.
    pub stop: bool,
}

/// The `MimicLoadTest`s of the cluster by `(namespace, name)`.
#[derive(Default)]
pub struct RunIndex {
    runs: RwLock<BTreeMap<(String, String), RunInfo>>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the reconciler of the next step fills the index")
)]
impl RunIndex {
    pub fn apply(&self, run: RunInfo) {
        self.runs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((run.namespace.clone(), run.name.clone()), run);
    }

    pub fn remove(&self, namespace: &str, name: &str) {
        self.runs
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(namespace.to_string(), name.to_string()));
    }

    pub fn get(&self, namespace: &str, name: &str) -> Option<RunInfo> {
        self.runs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
    }
}

/// The `EndpointSlice`s of the cluster by `(namespace, name)`.
#[derive(Default)]
pub struct SliceCache {
    slices: RwLock<BTreeMap<(String, String), EndpointSlice>>,
    synced: std::sync::atomic::AtomicBool,
}

impl SliceCache {
    pub fn apply(&self, slice: EndpointSlice) {
        let (Some(namespace), Some(name)) = (&slice.metadata.namespace, &slice.metadata.name)
        else {
            return;
        };
        let key = (namespace.clone(), name.clone());
        self.slices
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, slice);
    }

    /// Replaces the whole content after a (re)list and marks the cache synced.
    pub fn replace(&self, slices: impl IntoIterator<Item = EndpointSlice>) {
        self.slices
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        for slice in slices {
            self.apply(slice);
        }
        self.mark_synced();
    }

    pub fn mark_synced(&self) {
        self.synced.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the first listing arrived: before it, an empty target list says nothing.
    pub fn is_synced(&self) -> bool {
        self.synced.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn remove(&self, namespace: &str, name: &str) {
        self.slices
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(namespace.to_string(), name.to_string()));
    }

    /// The targets of `deployment` in `namespace` (spec §7.8 "Targets").
    pub fn targets(&self, namespace: &str, deployment: &str) -> Vec<Target> {
        let slices: Vec<EndpointSlice> = self
            .slices
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, slice)| slice.clone())
            .collect();
        targets(&slices, deployment)
    }
}

/// The load routes' own state: tokens of another audience, the runs and the slices.
pub struct LoadApi {
    /// Reviews for the audience [`LOAD_AUDIENCE`].
    pub reviewer: Arc<dyn Reviewer>,
    /// Not shared with the sidecar routes: a cached sidecar token must not open these.
    pub reviews: ReviewCache,
    pub runs: Arc<RunIndex>,
    pub slices: Arc<SliceCache>,
}

/// Endpoints a pod's results may hold: the plan limit (`MIMIC_LOAD_MAX_ENDPOINTS`, at most 256).
const MAX_RESULT_ENDPOINTS: usize = 256;
/// Reasons a generator gives for its final post (spec §7.8 "Load API").
const REASONS: [&str; 4] = ["Completed", "ModeLeft", "Unverified", "NoTargets"];

/// Keeps `cache` in line with the `EndpointSlice`s of the cluster, for ever.
pub async fn watch_slices(client: kube::Client, cache: Arc<SliceCache>) {
    let api = Api::<EndpointSlice>::all(client);
    let mut events =
        std::pin::pin!(watcher::watcher(api, watcher::Config::default()).default_backoff());
    // A relist replaces the whole content: slices deleted meanwhile must not stay.
    let mut listing: Vec<EndpointSlice> = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => listing.clear(),
            Ok(Event::InitApply(slice)) => listing.push(slice),
            Ok(Event::InitDone) => {
                cache.replace(std::mem::take(&mut listing));
                tracing::info!("EndpointSlice cache synced");
            }
            Ok(Event::Apply(slice)) => cache.apply(slice),
            Ok(Event::Delete(slice)) => {
                if let (Some(namespace), Some(name)) =
                    (&slice.metadata.namespace, &slice.metadata.name)
                {
                    cache.remove(namespace, name);
                }
            }
            Err(e) => tracing::warn!(error = %e, "EndpointSlice watch error"),
        }
    }
}

/// The index of generator pod `pod` of the Job `job` with `workers` pods:
/// `<job>-<index>-<5 chars>`.
fn worker_index(pod: &str, job: &str, workers: u64) -> Option<u64> {
    let rest = pod.strip_prefix(job)?.strip_prefix('-')?;
    let (index, suffix) = rest.split_once('-')?;
    let plain = !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && (index == "0" || !index.starts_with('0'));
    let index: u64 = index.parse().ok().filter(|_| plain)?;
    (generated_suffix(suffix) && index < workers).then_some(index)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    concurrency: u64,
    rate: Option<u64>,
    duration_ms: u64,
    ramp_up_ms: u64,
    timeout_ms: u64,
    start_timeout_ms: u64,
}

#[derive(Serialize)]
struct PlanAnswer<'a> {
    plan: &'a RawValue,
    ticket: &'a str,
    worker: u64,
    workers: u64,
    params: Params,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Posted {
    #[serde(rename = "final")]
    final_post: bool,
    pod: String,
    reason: Option<String>,
    results: PodResults,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Plan,
    Targets,
    Results,
}

impl Route {
    fn parse(route: &str) -> Option<Route> {
        match route {
            "plan" => Some(Route::Plan),
            "targets" => Some(Route::Targets),
            "results" => Some(Route::Results),
            _ => None,
        }
    }

    /// The `endpoint` label and log field: never the run's name.
    fn endpoint(self) -> &'static str {
        match self {
            Route::Plan => "/v1/load/plan",
            Route::Targets => "/v1/load/targets",
            Route::Results => "/v1/load/results",
        }
    }

    fn method(self) -> Method {
        match self {
            Route::Results => Method::POST,
            _ => Method::GET,
        }
    }
}

fn json_response(body: Vec<u8>) -> Response {
    (StatusCode::OK, [(CONTENT_TYPE, "application/json")], body).into_response()
}

impl SidecarApi {
    /// `/v1/load/<name>/<uid>/<route>` (spec §7.8 "Load API").
    pub(super) async fn load_route(
        &self,
        name: &str,
        uid: &str,
        route: &str,
        request: axum::extract::Request,
    ) -> Response {
        let Some(route) = Route::parse(route).filter(|_| self.load.is_some()) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if *request.method() != route.method() {
            let allow = route.method();
            return (StatusCode::METHOD_NOT_ALLOWED, [(ALLOW, allow.as_str())]).into_response();
        }
        let peer = request.extensions().get::<Peer>().map(|peer| peer.0);
        let headers = request.headers().clone();
        let mut caller = Caller::default();
        let outcome = self
            .load_serve(
                route,
                name,
                uid,
                &headers,
                peer,
                request.into_body(),
                &mut caller,
            )
            .await;
        let result = outcome.as_ref().map_or_else(|r| r.result, |_| "ok");
        let reason = outcome.as_ref().err().and_then(|r| r.reason);
        self.record(route.endpoint(), &caller, result, reason);
        match outcome {
            Ok(response) => response,
            Err(refusal) => refusal.into_response(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn load_serve(
        &self,
        route: Route,
        name: &str,
        uid: &str,
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::IpAddr>,
        body: axum::body::Body,
        caller: &mut Caller,
    ) -> Result<Response, Refusal> {
        let load = self.load.as_ref().ok_or(NOT_FOUND)?;
        self.admit(peer)?;
        let identity = self
            .authenticate_with(&*load.reviewer, &load.reviews, headers, peer, caller)
            .await?;
        // The run is looked up in the token's own namespace: another namespace never sees it.
        if !valid_name(name) || !valid_name(uid) {
            return Err(NOT_FOUND);
        }
        let run = load
            .runs
            .get(&identity.namespace, name)
            .filter(|run| run.uid == uid)
            .ok_or(NOT_FOUND)?;
        caller.deployment = run.deployment.clone();
        let workers = pods(run.concurrency, run.pod_concurrency);
        let pod = identity.pod.as_deref().ok_or(FORBIDDEN)?;
        let worker = worker_index(pod, &job_name(uid), workers).ok_or(FORBIDDEN)?;
        let (namespace, deployment) = (&identity.namespace, &run.deployment);
        let unavailable = |caller: &mut Caller, error: &str| {
            caller.error = Some(error.to_string());
            UNAVAILABLE
        };
        // The effective mode leaving `mock` stops the run as `spec.stop` does.
        let stopped = run.stop || !self.workloads.effective(namespace, deployment).mock;

        match route {
            Route::Plan => {
                let stored = match self.store.load_run(namespace, deployment, uid).await {
                    Ok(Some(stored)) => stored,
                    Ok(None) => return Err(NOT_FOUND),
                    Err(_) => return Err(unavailable(caller, "store unavailable")),
                };
                let plan = RawValue::from_string(stored.plan)
                    .map_err(|_| unavailable(caller, "stored plan unreadable"))?;
                let shares = split(run.concurrency, run.rate, run.pod_concurrency);
                let share = usize::try_from(worker)
                    .ok()
                    .and_then(|i| shares.get(i))
                    .ok_or(FORBIDDEN)?;
                let answer = PlanAnswer {
                    plan: &plan,
                    ticket: &stored.ticket,
                    worker,
                    workers,
                    params: Params {
                        concurrency: share.concurrency,
                        rate: share.rate,
                        duration_ms: run.duration_ms,
                        ramp_up_ms: run.ramp_up_ms,
                        timeout_ms: run.timeout_ms,
                        start_timeout_ms: run.start_timeout_ms,
                    },
                };
                let bytes = serde_json::to_vec(&answer)
                    .map_err(|_| unavailable(caller, "plan not serialisable"))?;
                Ok(json_response(bytes))
            }
            Route::Targets => {
                let stop = match self.store.load_active(namespace, deployment, uid).await {
                    Ok(Some(stop)) => stop,
                    Ok(None) => return Err(NOT_FOUND),
                    Err(_) => return Err(unavailable(caller, "store unavailable")),
                };
                let targets: Vec<serde_json::Value> = load
                    .slices
                    .targets(namespace, deployment)
                    .iter()
                    .map(|t| serde_json::json!({"ip": t.ip.to_string(), "pod": t.pod}))
                    .collect();
                let answer = serde_json::json!({"stop": stop || stopped, "targets": targets});
                Ok(json_response(answer.to_string().into_bytes()))
            }
            Route::Results => {
                // Authorised: only now is the body announced, bounded and read.
                let length = content_length(headers)?;
                caller.bytes = Some(length);
                if length > MAX_POST {
                    return Err(TOO_LARGE);
                }
                let reservation = self.budget.reserve(length as u64).ok_or_else(|| {
                    caller.error = Some("body budget spent".to_string());
                    UNAVAILABLE
                })?;
                let bytes = read_exact(body, length).await?;
                drop(reservation);
                // Parse errors quote the offending value: they are never logged or returned.
                let posted: Posted =
                    serde_json::from_slice(&bytes).map_err(|_| invalid("malformed"))?;
                if posted.pod != pod {
                    return Err(invalid("pod"));
                }
                if posted
                    .reason
                    .as_deref()
                    .is_some_and(|reason| !REASONS.contains(&reason))
                {
                    return Err(invalid("reason"));
                }
                if posted.results.endpoints.len() > MAX_RESULT_ENDPOINTS {
                    return Err(invalid("results"));
                }
                let stored = StoredPost {
                    final_post: posted.final_post,
                    reason: posted.reason,
                    results: posted.results,
                };
                match self
                    .store
                    .load_post(namespace, deployment, uid, pod, &stored)
                    .await
                {
                    Ok(Some(stop)) => Ok(json_response(
                        serde_json::json!({"stop": stop || stopped})
                            .to_string()
                            .into_bytes(),
                    )),
                    Ok(None) => Err(NOT_FOUND),
                    Err(StoreError::Name) => Err(invalid("name")),
                    Err(_) => Err(unavailable(caller, "store unavailable")),
                }
            }
        }
    }
}

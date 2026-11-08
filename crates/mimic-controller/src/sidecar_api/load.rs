//! Load API (spec §7.8): the plan, the targets and the results of a run, for the pods of its
//! generator Job. Same listener, admission order, rate limit and `TokenReview` limits as the
//! sidecar API; the token has the audience `mimic.platform.io/load`.
//!
//! Nothing here logs or returns a ticket, a header, a body or an address of the run.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use k8s_openapi::api::discovery::v1::EndpointSlice;
use mimic_core::load::gate::{Target, targets};

use super::{ReviewCache, Reviewer};

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

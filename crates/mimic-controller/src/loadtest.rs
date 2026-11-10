//! `MimicLoadTest` reconciler (spec §7.8 "Lifecycle"): the gate, the Valkey admission, the Job
//! and NetworkPolicy of a run, its status, and its end (stop, mode leaving `mock`, generator
//! results, Job failure). Both replicas run it with no leader: every step is idempotent, and the
//! Valkey scripts of `store::load` decide who wins.
//!
//! Nothing here logs or puts in a status a ticket, a credential or a value of the report.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use mimic_core::load::plan::{CredentialField, plan_limit};
use mimic_core::load::validate::Limits;
use mimic_runtime::config::{ConfigError, Env, parse_duration};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::push_config::{KIB, MIB, number, size, usize_of};
use crate::sidecar_api::load::{RunIndex, SliceCache};
use crate::store::Store;
use crate::tls::CertSlot;
use crate::tls_ca::SeedSlot;
use crate::workloads::WorkloadCache;

mod cluster;
mod reconcile;

pub use cluster::{KubeCluster, Object, watch};

#[cfg(test)]
mod tests;

/// The generator's reporting period (`MIMIC_LOAD_REPORT_INTERVAL`, spec §10.6).
pub const REPORT_INTERVAL_MS: u64 = 5_000;

/// The NetworkPolicy peers of a run (`MIMIC_LOAD_NETWORK_POLICY`, spec §10.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Network {
    pub controller_labels: BTreeMap<String, String>,
    pub dns_namespace: String,
    pub dns_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub metrics_from: Vec<Value>,
}

/// The `MIMIC_LOAD_*` settings of the controller (spec §10.2).
#[derive(Debug, Clone)]
pub struct Config {
    pub max_runs: u64,
    pub max_namespace_runs: u64,
    pub max_pending: usize,
    pub store_budget: u64,
    pub limits: Limits,
    pub max_endpoints: usize,
    pub max_plan: usize,
    pub start_timeout: Duration,
    pub resources: Value,
    /// `MIMIC_INJECT_SIDECAR_IMAGE`: the generator runs from the `mimic` image.
    pub image: String,
    pub controller_url: String,
    pub controller_namespace: String,
    pub controller_port: u16,
    pub network: Option<Network>,
    /// `MIMIC_STATUS_INTERVAL`.
    pub status_interval: Duration,
    /// `MIMIC_STATUS_POD_WINDOW`.
    pub pod_window: Duration,
}

const DEFAULT_RESOURCES: &str =
    r#"{"requests":{"cpu":"500m","memory":"128Mi"},"limits":{"cpu":"2","memory":"256Mi"}}"#;

/// What the process around the load settings provides.
pub struct Context<'a> {
    pub image: &'a str,
    pub controller_url: &'a str,
    pub controller_namespace: &'a str,
    pub controller_port: u16,
    pub status_interval: Duration,
    pub pod_window: Duration,
}

impl Config {
    pub fn from_env(env: &Env, context: &Context<'_>) -> Result<Config, ConfigError> {
        let max_runs: u64 = number(env, "MIMIC_LOAD_MAX_RUNS", 0, 0, 100)?;
        let namespace_ceiling = if max_runs == 0 { 100 } else { max_runs };
        let var = "MIMIC_LOAD_MAX_PLAN";
        let max_plan = match env.optional(var) {
            Some(value) => plan_limit(value).map_err(|e| ConfigError::invalid(var, value, e))?,
            None => 8 << 20,
        };
        let var = "MIMIC_LOAD_STORE_BUDGET";
        let store_budget = size(
            env,
            var,
            64 * MIB,
            MIB.max(max_plan as u64 + MIB),
            4096 * MIB,
        )?;
        let var = "MIMIC_LOAD_START_TIMEOUT";
        let start_timeout = env.duration_or(var, Duration::from_secs(300))?;
        if !(Duration::from_secs(5)..=Duration::from_secs(3600)).contains(&start_timeout) {
            return Err(ConfigError::invalid(
                var,
                env.optional(var).unwrap_or_default(),
                "expected a duration from 5s to 1h",
            ));
        }
        let var = "MIMIC_LOAD_MAX_DURATION";
        let max_duration = env.duration_or(var, Duration::from_secs(1800))?;
        if !(Duration::from_secs(10)..=Duration::from_secs(86_400)).contains(&max_duration) {
            return Err(ConfigError::invalid(
                var,
                env.optional(var).unwrap_or_default(),
                "expected a duration from 10s to 24h",
            ));
        }
        let var = "MIMIC_LOAD_RESOURCES";
        let resources = env.optional(var).unwrap_or(DEFAULT_RESOURCES);
        let resources: Value = serde_json::from_str(resources)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| ConfigError::invalid(var, resources, "expected a JSON object"))?;
        let var = "MIMIC_LOAD_NETWORK_POLICY";
        let network = match env.optional(var) {
            Some(value) => Some(
                serde_json::from_str(value).map_err(|e| ConfigError::invalid(var, value, e))?,
            ),
            None => None,
        };
        Ok(Config {
            max_runs,
            max_namespace_runs: number(env, "MIMIC_LOAD_MAX_NAMESPACE_RUNS", 2, 1, 100)
                .and_then(|n| {
                    if n <= namespace_ceiling {
                        Ok(n)
                    } else {
                        Err(ConfigError::invalid(
                            "MIMIC_LOAD_MAX_NAMESPACE_RUNS",
                            &n.to_string(),
                            "at most MIMIC_LOAD_MAX_RUNS",
                        ))
                    }
                })?,
            max_pending: number(env, "MIMIC_LOAD_MAX_PENDING", 16, 1, 1000)?,
            store_budget,
            limits: Limits {
                max_concurrency: number(env, "MIMIC_LOAD_MAX_CONCURRENCY", 256, 1, 4096)?,
                max_rate: number(env, "MIMIC_LOAD_MAX_RATE", 2000, 1, 100_000)?,
                max_duration_ms: u64::try_from(max_duration.as_millis()).unwrap_or(u64::MAX),
                pod_concurrency: number(env, "MIMIC_LOAD_POD_CONCURRENCY", 128, 1, 4096)?,
            },
            max_endpoints: usize_of(number(env, "MIMIC_LOAD_MAX_ENDPOINTS", 64, 1, 256)?),
            max_plan,
            start_timeout,
            resources,
            image: context.image.to_string(),
            controller_url: context.controller_url.to_string(),
            controller_namespace: context.controller_namespace.to_string(),
            controller_port: context.controller_port,
            network,
            status_interval: context.status_interval,
            pod_window: context.pod_window,
        })
    }
}

/// The spec of a `MimicLoadTest`, parsed (spec §7.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSpec {
    pub concurrency: u64,
    pub rate: Option<u64>,
    pub duration_ms: u64,
    pub ramp_up_ms: u64,
    pub timeout_ms: u64,
    pub include: Option<BTreeSet<String>>,
    pub exclude: BTreeSet<String>,
    pub credentials: Vec<CredentialField>,
    pub secret_name: Option<String>,
}

/// A `MimicLoadTest` as the reconciler sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Test {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub deployment: String,
    /// `metadata.creationTimestamp`, Unix milliseconds.
    pub created_ms: u64,
    pub spec: TestSpec,
    /// `spec.stop`.
    pub stop: bool,
    /// The `status` the cluster holds (`Null` when none).
    pub status: Value,
}

impl Test {
    pub fn phase(&self) -> &str {
        self.status
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// `Succeeded`, `Failed` or `Aborted`.
    pub fn ended(&self) -> bool {
        matches!(self.phase(), "Succeeded" | "Failed" | "Aborted")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSpec {
    duration: String,
    concurrency: Option<u64>,
    rate: Option<u64>,
    ramp_up: Option<String>,
    timeout: Option<String>,
    endpoints: Option<RawEndpoints>,
    credentials: Option<RawCredentials>,
    stop: Option<bool>,
}

#[derive(Deserialize)]
struct RawEndpoints {
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCredentials {
    secret_name: String,
    fields: Vec<CredentialField>,
}

fn millis(text: &str) -> Option<u64> {
    u64::try_from(parse_duration(text)?.as_millis()).ok()
}

/// A [`Test`] from the object the cluster holds; `None` when its spec does not parse (the CRD
/// schema refuses such an object).
pub fn parse(object: &Object) -> Option<Test> {
    let spec: RawSpec = serde_json::from_value(object.data.get("spec")?.clone()).ok()?;
    let deployment = object.data.get("spec")?.get("deployment")?.as_str()?;
    let created = object.metadata.creation_timestamp.as_ref()?;
    let (secret_name, credentials) = match spec.credentials {
        Some(c) => (Some(c.secret_name), c.fields),
        None => (None, Vec::new()),
    };
    Some(Test {
        namespace: object.metadata.namespace.clone()?,
        name: object.metadata.name.clone()?,
        uid: object.metadata.uid.clone()?,
        deployment: deployment.to_string(),
        created_ms: u64::try_from(created.0.as_millisecond()).ok()?,
        spec: TestSpec {
            concurrency: spec.concurrency.unwrap_or(10),
            rate: spec.rate,
            duration_ms: millis(&spec.duration)?,
            ramp_up_ms: match spec.ramp_up {
                Some(text) => millis(&text)?,
                None => 0,
            },
            timeout_ms: match spec.timeout {
                Some(text) => millis(&text)?,
                None => 10_000,
            },
            include: spec
                .endpoints
                .as_ref()
                .and_then(|e| e.include.clone())
                .map(|list| list.into_iter().collect()),
            exclude: spec
                .endpoints
                .and_then(|e| e.exclude)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            credentials,
            secret_name,
        },
        stop: spec.stop.unwrap_or(false),
        status: object.data.get("status").cloned().unwrap_or(Value::Null),
    })
}

/// The `MimicLoadTest`s of the cluster by `(namespace, name)`.
#[derive(Default)]
pub struct Tests {
    items: RwLock<BTreeMap<(String, String), Test>>,
}

impl Tests {
    pub fn apply(&self, test: Test) {
        self.items
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((test.namespace.clone(), test.name.clone()), test);
    }

    pub fn remove(&self, namespace: &str, name: &str) -> Option<Test> {
        self.items
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(namespace.to_string(), name.to_string()))
    }

    /// Every test, oldest first (then by namespace and name).
    pub fn list(&self) -> Vec<Test> {
        let mut tests: Vec<Test> = self
            .items
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        tests.sort_by(|a, b| {
            (a.created_ms, &a.namespace, &a.name).cmp(&(b.created_ms, &b.namespace, &b.name))
        });
        tests
    }
}

/// What the reconciler asks of the cluster.
pub type ClusterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Job,
    NetworkPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Complete,
    Failed,
    Gone,
}

pub trait Cluster: Send + Sync {
    /// Creates the object; `Ok(false)` when it exists already.
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        kind: Kind,
        object: &'a Value,
    ) -> ClusterFuture<'a, bool>;
    /// Deletes by name (propagation `Background`); an absent object is success.
    fn delete<'a>(&'a self, namespace: &'a str, kind: Kind, name: &'a str)
    -> ClusterFuture<'a, ()>;
    fn job_state<'a>(&'a self, namespace: &'a str, name: &'a str) -> ClusterFuture<'a, JobState>;
    /// Server-side apply of the `status` of a `MimicLoadTest`.
    fn apply_status<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        status: &'a Value,
    ) -> ClusterFuture<'a, ()>;
}

type Labels = Vec<(String, String)>;

/// `mimic_load_runs_total{phase,reason}` and `mimic_load_active_runs`.
#[derive(Clone, Default)]
pub struct LoadMetrics {
    runs: Family<Labels, Counter>,
    active: Gauge,
}

impl LoadMetrics {
    pub fn register(registry: &mut Registry) -> LoadMetrics {
        let metrics = LoadMetrics::default();
        registry.register(
            "load_runs",
            "Load test runs ended",
            metrics.runs.clone(),
        );
        registry.register(
            "load_active_runs",
            "Load test runs this replica sees running",
            metrics.active.clone(),
        );
        metrics
    }
}

/// What a replica remembers of a run between passes.
#[derive(Default)]
pub(crate) struct RunState {
    /// The parsed plan, for the endpoint rows of the summary.
    pub plan: Option<Arc<mimic_core::load::plan::Plan>>,
    pub start_ms: Option<u64>,
    pub last_status_ms: Option<u64>,
    pub last_job_check_ms: Option<u64>,
}

/// The reconciler.
pub struct Reconciler {
    pub config: Config,
    pub store: Arc<Store>,
    pub workloads: Arc<WorkloadCache>,
    pub tests: Arc<Tests>,
    pub runs: Arc<RunIndex>,
    pub slices: Arc<SliceCache>,
    pub cluster: Arc<dyn Cluster>,
    pub seed: Arc<SeedSlot>,
    pub cert: Arc<CertSlot>,
    pub metrics: LoadMetrics,
    pub(crate) state: Mutex<BTreeMap<String, RunState>>,
}

/// The reconciliation period; the stop of a run is seen within it (LDT-10).
pub const TICK: Duration = Duration::from_millis(500);

/// Bytes of a results post a run reserves per pod (spec §7.8 "Admission").
pub(crate) const POST_RESERVE: u64 = 256 * KIB;

fn status_of(phase: &str, reason: &str, message: &str) -> Value {
    json!({"phase": phase, "reason": reason, "message": message})
}

/// The summary as stored and shown, at most [`CAP_SUMMARY`] bytes: past it the per-endpoint rows
/// go and the total stays (spec §7.8 "End").
pub fn summary_json(summary: &mimic_core::load::summary::Summary) -> String {
    String::new()
}

//! One pass of the reconciler: each `MimicLoadTest` is `Pending` (gate, admission, Job) or
//! `Running` (end conditions, status); the Valkey scripts decide between the replicas.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimic_core::Mode;
use mimic_core::load::gate::{self, Gate};
use mimic_core::load::job::{JobInput, PolicyInput, job, job_name, network_policy};
use mimic_core::load::plan::{self, Plan};
use mimic_core::load::schedule::pods;
use mimic_core::load::summary::{PodResults, Summary, summary};
use mimic_core::load::ticket::{self, Payload};
use mimic_core::load::validate::{Params, validate};
use serde_json::{Value, json};

use super::{
    JobState, Kind, POST_RESERVE, REPORT_INTERVAL_MS, Reconciler, RunState, TICK, Test, status_of,
    summary_json,
};
use crate::sidecar_api::load::RunInfo;
use crate::store::load::{Admit, AdmitError, AdmitLimits, Ended, MAX_SUMMARY};
use crate::store::{Half, ReadError};

/// A call to the cluster that takes longer is abandoned.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the Job of a running run is looked at.
const JOB_CHECK_MS: u64 = 5_000;

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn rfc3339(ms: u64) -> String {
    let nanos = i128::from(ms) * 1_000_000;
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn clock_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, ms)
}

impl Reconciler {
    /// Passes every [`TICK`], for ever.
    pub async fn run(&self) {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.tick(clock_ms()).await;
        }
    }

    /// One pass at `now_ms` (Unix milliseconds).
    pub async fn tick(&self, now_ms: u64) {
        let mut pending: BTreeMap<String, usize> = BTreeMap::new();
        let mut running = 0;
        let mut live = Vec::new();
        for test in self.tests.list() {
            if test.ended() {
                self.runs.remove(&test.namespace, &test.name);
                continue;
            }
            live.push(test.uid.clone());
            self.runs.apply(self.run_info(&test));
            if test.phase() == "Running" {
                running += 1;
                self.running(&test, now_ms).await;
            } else {
                let rank = pending.entry(test.namespace.clone()).or_default();
                let mine = *rank;
                *rank += 1;
                self.pending(&test, mine, now_ms).await;
            }
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|uid, _| live.contains(uid));
        self.metrics.active.set(running);
    }

    fn run_info(&self, test: &Test) -> RunInfo {
        RunInfo {
            namespace: test.namespace.clone(),
            name: test.name.clone(),
            uid: test.uid.clone(),
            deployment: test.deployment.clone(),
            concurrency: test.spec.concurrency,
            rate: test.spec.rate,
            duration_ms: test.spec.duration_ms,
            ramp_up_ms: test.spec.ramp_up_ms,
            timeout_ms: test.spec.timeout_ms,
            start_timeout_ms: ms(self.config.start_timeout),
            pod_concurrency: self.config.limits.pod_concurrency,
            stop: test.stop,
        }
    }

    fn with_state<T>(&self, uid: &str, f: impl FnOnce(&mut RunState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(state.entry(uid.to_string()).or_default())
    }

    /// Applies `status` unless the cluster holds it already.
    async fn set_status(&self, test: &Test, status: Value) {
        if test.status == status {
            return;
        }
        let call = self
            .cluster
            .apply_status(&test.namespace, &test.name, &status);
        let result = tokio::time::timeout(API_TIMEOUT, call)
            .await
            .unwrap_or_else(|_| Err("no answer in time".to_string()));
        if let Err(error) = result {
            tracing::warn!(
                namespace = test.namespace,
                name = test.name,
                error,
                "load test status not applied"
            );
        }
    }

    /// A run that ends before it was admitted.
    async fn fail(&self, test: &Test, reason: &str, message: &str, now_ms: u64) {
        let mut status = status_of("Failed", reason, message);
        status["endTime"] = json!(rfc3339(now_ms));
        if let Some(skipped) = test.status.get("skipped") {
            status["skipped"] = skipped.clone();
        }
        self.set_status(test, status).await;
        self.count("Failed", reason);
    }

    fn count(&self, phase: &str, reason: &str) {
        self.metrics
            .runs
            .get_or_create(&vec![
                ("phase".to_string(), phase.to_string()),
                ("reason".to_string(), reason.to_string()),
            ])
            .inc();
    }

    async fn pending(&self, test: &Test, rank: usize, now_ms: u64) {
        let cfg = &self.config;
        if cfg.max_runs == 0 {
            return self
                .fail(
                    test,
                    "Disabled",
                    "load tests are disabled (MIMIC_LOAD_MAX_RUNS=0)",
                    now_ms,
                )
                .await;
        }
        let params = Params {
            concurrency: test.spec.concurrency,
            rate: test.spec.rate,
            duration_ms: test.spec.duration_ms,
            ramp_up_ms: test.spec.ramp_up_ms,
            timeout_ms: test.spec.timeout_ms,
        };
        if let Err(invalid) = validate(&params, &cfg.limits) {
            return self.fail(test, "Invalid", &invalid.message, now_ms).await;
        }
        if test.stop {
            let mut status = status_of("Aborted", "Stopped", "stopped before it started");
            status["endTime"] = json!(rfc3339(now_ms));
            self.set_status(test, status).await;
            self.count("Aborted", "Stopped");
            return;
        }
        if rank >= cfg.max_pending {
            let message = "too many Pending runs in the namespace (MIMIC_LOAD_MAX_PENDING)";
            return self.fail(test, "TooManyPending", message, now_ms).await;
        }
        let (waiting, failed, message) = if !self.slices.is_synced() {
            (
                "WaitingForTargets",
                "NoTargets",
                "waiting for the EndpointSlice cache",
            )
        } else {
            let targets = self.slices.targets(&test.namespace, &test.deployment);
            let pulls = match self.store.pulls(&test.namespace, &test.deployment).await {
                Ok(pulls) => pulls,
                Err(ReadError::Unavailable) => return,
                Err(_) => BTreeMap::new(),
            };
            let mode = self
                .workloads
                .effective(&test.namespace, &test.deployment)
                .mode();
            match gate::gate(mode, &targets, &pulls, now_ms, ms(cfg.pod_window)) {
                Gate::Open => return self.admit(test, targets.len(), now_ms).await,
                Gate::WaitingForMock => (
                    "WaitingForMock",
                    "NotMock",
                    if mode == Mode::Mock {
                        "waiting for every target pod to pull the mock mode"
                    } else {
                        "waiting for the deployment to be in mock mode"
                    },
                ),
                Gate::WaitingForTargets => (
                    "WaitingForTargets",
                    "NoTargets",
                    "waiting for a ready pod behind a Service",
                ),
            }
        };
        if now_ms.saturating_sub(test.created_ms) >= ms(cfg.start_timeout) {
            self.fail(test, failed, message, now_ms).await;
        } else {
            self.set_status(test, status_of("Pending", waiting, message))
                .await;
        }
    }

    async fn admit(&self, test: &Test, targets: usize, now_ms: u64) {
        let cfg = &self.config;
        let (ns, dep) = (&test.namespace, &test.deployment);
        let stored = match self.store.stored_report(ns, dep, Half::Inbound).await {
            Ok(stored) => stored,
            Err(ReadError::Unavailable) => return,
            Err(ReadError::NotFound) => {
                return self
                    .fail(
                        test,
                        "NoReport",
                        "no stored report for the deployment",
                        now_ms,
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .fail(test, "NoReport", "the stored report is unreadable", now_ms)
                    .await;
            }
        };
        let spec = plan::Spec {
            include: test.spec.include.clone(),
            exclude: test.spec.exclude.clone(),
            credentials: test.spec.credentials.clone(),
            max_endpoints: cfg.max_endpoints,
            max_plan: cfg.max_plan,
        };
        let built = tokio::task::spawn_blocking(move || {
            let report = stored.parse()?;
            let planned = plan::plan(&report, &spec);
            let json = planned.plan.to_json().ok()?;
            let ports: Vec<u16> = planned.plan.endpoints.iter().map(|e| e.port).collect();
            Some((json, ports, planned.skipped))
        })
        .await
        .ok()
        .flatten();
        let Some((plan_json, ports, skipped)) = built else {
            return self
                .fail(test, "NoReport", "the stored report is unreadable", now_ms)
                .await;
        };
        let skipped: BTreeMap<&str, u64> = skipped.into_iter().collect();
        if ports.is_empty() {
            let mut status = status_of(
                "Failed",
                "NoEndpoints",
                "no endpoint of the report can be loaded",
            );
            status["skipped"] = json!(skipped);
            status["endTime"] = json!(rfc3339(now_ms));
            self.set_status(test, status).await;
            self.count("Failed", "NoEndpoints");
            return;
        }
        // Without the seed or the webhook CA the run waits: neither is a reason to fail it.
        let (Some(seed), Some(ca)) = (self.seed.get(), self.cert.ca_pem()) else {
            return;
        };
        let Ok(root) = crate::tls_ca::derive_root(&seed, ns, dep) else {
            tracing::error!(
                namespace = ns.as_str(),
                deployment = dep.as_str(),
                "load ticket root not derived"
            );
            return;
        };
        let deadline_s =
            test.spec.duration_ms.div_ceil(1000) + ms(cfg.start_timeout).div_ceil(1000) + 60;
        let deadline_ms = now_ms + deadline_s * 1000;
        let payload = Payload {
            deployment: dep.clone(),
            exp: deadline_ms / 1000,
            namespace: ns.clone(),
            uid: test.uid.clone(),
        };
        let ticket = ticket::issue(&payload, |input| root.sign(input));
        let workers = pods(test.spec.concurrency, cfg.limits.pod_concurrency);
        let spec_json = json!({
            "name": test.name, "deployment": dep, "concurrency": test.spec.concurrency,
            "rate": test.spec.rate, "durationMs": test.spec.duration_ms,
            "rampUpMs": test.spec.ramp_up_ms, "timeoutMs": test.spec.timeout_ms,
        })
        .to_string();
        let admitted = self
            .store
            .load_admit(&Admit {
                namespace: ns,
                deployment: dep,
                uid: &test.uid,
                spec: &spec_json,
                plan: &plan_json,
                ticket: &ticket,
                now_ms,
                deadline_ms,
                reserve: plan_json.len() as u64 + workers * POST_RESERVE + MAX_SUMMARY as u64,
                limits: AdmitLimits {
                    max_runs: cfg.max_runs,
                    max_namespace_runs: cfg.max_namespace_runs,
                    budget: cfg.store_budget,
                },
            })
            .await;
        let (reason, message) = match admitted {
            Ok(()) => {
                return self
                    .start(test, &ports, targets, &skipped, &ca, now_ms)
                    .await;
            }
            Err(AdmitError::Unavailable) => return,
            Err(AdmitError::Busy) => ("Busy", "the deployment has an active run"),
            Err(AdmitError::NamespaceBusy) => (
                "NamespaceBusy",
                "the namespace has too many active runs (MIMIC_LOAD_MAX_NAMESPACE_RUNS)",
            ),
            Err(AdmitError::ClusterBusy) => (
                "ClusterBusy",
                "the cluster has too many active runs (MIMIC_LOAD_MAX_RUNS)",
            ),
            Err(AdmitError::StoreFull) => (
                "StoreFull",
                "the run does not fit the load store budget (MIMIC_LOAD_STORE_BUDGET)",
            ),
            Err(AdmitError::Name) => (
                "Invalid",
                "the namespace, deployment or uid is not a valid name",
            ),
        };
        self.fail(test, reason, message, now_ms).await;
    }

    /// Creates the NetworkPolicy and the Job of an admitted run (both idempotent).
    async fn start(
        &self,
        test: &Test,
        ports: &[u16],
        targets: usize,
        skipped: &BTreeMap<&str, u64>,
        ca: &str,
        now_ms: u64,
    ) {
        let cfg = &self.config;
        let ns = &test.namespace;
        let created = async {
            if let Some(network) = &cfg.network {
                let policy = network_policy(&PolicyInput {
                    namespace: ns,
                    name: &test.name,
                    uid: &test.uid,
                    deployment: &test.deployment,
                    ports,
                    controller_namespace: &cfg.controller_namespace,
                    controller_labels: &network.controller_labels,
                    controller_port: cfg.controller_port,
                    dns_namespace: &network.dns_namespace,
                    dns_labels: &network.dns_labels,
                    metrics_from: &network.metrics_from,
                });
                self.create(ns, Kind::NetworkPolicy, &policy).await?;
            }
            let object = job(&JobInput {
                namespace: ns,
                name: &test.name,
                uid: &test.uid,
                deployment: &test.deployment,
                concurrency: test.spec.concurrency,
                duration_ms: test.spec.duration_ms,
                pod_concurrency: cfg.limits.pod_concurrency,
                start_timeout_ms: ms(cfg.start_timeout),
                report_interval_ms: REPORT_INTERVAL_MS,
                max_plan: cfg.max_plan,
                image: &cfg.image,
                resources: &cfg.resources,
                controller_url: &cfg.controller_url,
                controller_ca: ca,
                secret_name: test.spec.secret_name.as_deref(),
            });
            self.create(ns, Kind::Job, &object).await
        }
        .await;
        match created {
            Ok(new) => {
                let mut status = status_of("Running", "", "");
                status["startTime"] = json!(rfc3339(now_ms));
                status["targets"] = json!(targets);
                status["pods"] = json!(0);
                status["skipped"] = json!(skipped);
                self.with_state(&test.uid, |s| s.start_ms = Some(now_ms));
                self.set_status(test, status).await;
                if new {
                    tracing::info!(
                        namespace = ns.as_str(),
                        name = test.name,
                        deployment = test.deployment,
                        uid = test.uid,
                        concurrency = test.spec.concurrency,
                        rate = test.spec.rate.unwrap_or(0),
                        duration_ms = test.spec.duration_ms,
                        targets,
                        "load test started"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    namespace = ns.as_str(),
                    name = test.name,
                    error,
                    "load test Job not created"
                );
                // The run holds a slot: do not keep it for ever behind a refused Job.
                if now_ms.saturating_sub(test.created_ms) >= ms(cfg.start_timeout) {
                    self.end(
                        test,
                        "Failed",
                        "JobFailed",
                        "the Job could not be created",
                        now_ms,
                        true,
                    )
                    .await;
                }
            }
        }
    }

    async fn create(&self, ns: &str, kind: Kind, object: &Value) -> Result<bool, String> {
        tokio::time::timeout(API_TIMEOUT, self.cluster.create(ns, kind, object))
            .await
            .unwrap_or_else(|_| Err("no answer in time".to_string()))
    }

    async fn delete_objects(&self, test: &Test) {
        let name = job_name(&test.uid);
        let mut kinds = vec![Kind::Job];
        if self.config.network.is_some() {
            kinds.push(Kind::NetworkPolicy);
        }
        for kind in kinds {
            let call = self.cluster.delete(&test.namespace, kind, &name);
            let result = tokio::time::timeout(API_TIMEOUT, call)
                .await
                .unwrap_or_else(|_| Err("no answer in time".to_string()));
            if let Err(error) = result {
                tracing::warn!(
                    namespace = test.namespace,
                    name = test.name,
                    error,
                    "load test object not deleted"
                );
            }
        }
    }

    async fn running(&self, test: &Test, now_ms: u64) {
        let (ns, dep, uid) = (&test.namespace, &test.deployment, &test.uid);
        match self.store.load_active(ns, dep, uid).await {
            Ok(Some(_)) => {}
            Ok(None) => return self.recover(test, now_ms).await,
            Err(_) => return,
        }
        if test.stop {
            return self
                .end(
                    test,
                    "Aborted",
                    "Stopped",
                    "stopped by spec.stop",
                    now_ms,
                    true,
                )
                .await;
        }
        if !self.workloads.effective(ns, dep).mock {
            let message = "the deployment left mock mode";
            return self
                .end(test, "Aborted", "ModeLeft", message, now_ms, true)
                .await;
        }
        let Ok(posts) = self.store.load_posts(ns, dep, uid).await else {
            return;
        };
        let workers = pods(test.spec.concurrency, self.config.limits.pod_concurrency);
        let verdict =
            posts
                .values()
                .filter(|p| p.final_post)
                .find_map(|p| match p.reason.as_deref() {
                    Some("ModeLeft") => Some((
                        "Aborted",
                        "ModeLeft",
                        "a generator saw the target leave mock mode",
                    )),
                    Some("Unverified") => Some((
                        "Aborted",
                        "Unverified",
                        "a target did not answer as the deployment under test",
                    )),
                    Some("NoTargets") => Some(("Failed", "NoTargets", "no verified target")),
                    _ => None,
                });
        if let Some((phase, reason, message)) = verdict {
            return self.end(test, phase, reason, message, now_ms, true).await;
        }
        if posts.len() as u64 >= workers && posts.values().all(|p| p.final_post) {
            return self
                .end(test, "Succeeded", "Completed", "", now_ms, false)
                .await;
        }
        let due = self.with_state(uid, |s| {
            let due = s
                .last_job_check_ms
                .is_none_or(|at| now_ms.saturating_sub(at) >= JOB_CHECK_MS);
            if due {
                s.last_job_check_ms = Some(now_ms);
            }
            due
        });
        if due {
            let job = job_name(uid);
            let call = self.cluster.job_state(ns, &job);
            match tokio::time::timeout(API_TIMEOUT, call).await {
                Ok(Ok(JobState::Failed)) => {
                    return self
                        .end(
                            test,
                            "Failed",
                            "JobFailed",
                            "the generator Job failed",
                            now_ms,
                            false,
                        )
                        .await;
                }
                Ok(Ok(JobState::Gone)) => {
                    return self
                        .end(
                            test,
                            "Failed",
                            "JobFailed",
                            "the generator Job is gone",
                            now_ms,
                            false,
                        )
                        .await;
                }
                Ok(Ok(JobState::Complete)) => {
                    return self
                        .end(test, "Succeeded", "Completed", "", now_ms, false)
                        .await;
                }
                _ => {}
            }
        }
        let interval = ms(self.config.status_interval);
        let report = self.with_state(uid, |s| {
            let due = s
                .last_status_ms
                .is_none_or(|at| now_ms.saturating_sub(at) >= interval);
            if due {
                s.last_status_ms = Some(now_ms);
            }
            due
        });
        if !report {
            return;
        }
        let Some(plan) = self.plan_of(test).await else {
            return;
        };
        let start = self.start_of(test, now_ms).await;
        let results: Vec<PodResults> = posts.values().map(|p| p.results.clone()).collect();
        let summary = summary(&plan, &results, now_ms.saturating_sub(start));
        let mut status = test.status.clone();
        status["phase"] = json!("Running");
        status["pods"] = json!(posts.len());
        status["targets"] = json!(self.slices.targets(ns, dep).len());
        status["results"] = serde_json::from_str(&summary_json(&summary)).unwrap_or(Value::Null);
        self.set_status(test, status).await;
    }

    async fn plan_of(&self, test: &Test) -> Option<Arc<Plan>> {
        if let Some(plan) = self.with_state(&test.uid, |s| s.plan.clone()) {
            return Some(plan);
        }
        let run = self
            .store
            .load_run(&test.namespace, &test.deployment, &test.uid)
            .await
            .ok()??;
        let plan = Arc::new(Plan::from_json(&run.plan, self.config.max_plan).ok()?);
        self.with_state(&test.uid, |s| s.plan = Some(plan.clone()));
        Some(plan)
    }

    async fn start_of(&self, test: &Test, now_ms: u64) -> u64 {
        if let Some(start) = self.with_state(&test.uid, |s| s.start_ms) {
            return start;
        }
        let stored = self
            .store
            .load_fields(&test.namespace, &test.deployment, &test.uid)
            .await
            .ok()
            .and_then(|f| f.get("start")?.parse::<u64>().ok())
            .unwrap_or(now_ms);
        self.with_state(&test.uid, |s| s.start_ms = Some(stored));
        stored
    }

    /// Ends a run: stops its generators and removes its Job (`cleanup`), keeps the results as
    /// the summary, releases the slot, applies the final status.
    #[allow(clippy::too_many_arguments)]
    async fn end(
        &self,
        test: &Test,
        phase: &str,
        reason: &str,
        message: &str,
        now_ms: u64,
        cleanup: bool,
    ) {
        let (ns, dep, uid) = (&test.namespace, &test.deployment, &test.uid);
        if cleanup {
            // The flag first: a generator that polls before its pod is gone stops.
            if self.store.load_stop(ns, dep, uid).await.is_err() {
                tracing::warn!(
                    namespace = ns.as_str(),
                    name = test.name,
                    "load test stop flag not set"
                );
            }
            self.delete_objects(test).await;
        }
        let posts = self
            .store
            .load_posts(ns, dep, uid)
            .await
            .unwrap_or_default();
        let plan = self.plan_of(test).await.unwrap_or_default();
        let start = self.start_of(test, now_ms).await;
        let results: Vec<PodResults> = posts.values().map(|p| p.results.clone()).collect();
        let summary: Summary = summary(&plan, &results, now_ms.saturating_sub(start));
        let text = summary_json(&summary);
        let ended = Ended {
            phase,
            reason,
            summary: &text,
            end_ms: now_ms,
        };
        match self.store.load_end(ns, dep, uid, &ended).await {
            Ok(true) => {
                self.count(phase, reason);
                let errors: u64 = summary.total.errors.values().sum();
                tracing::info!(
                    namespace = ns.as_str(),
                    name = test.name,
                    deployment = dep.as_str(),
                    uid = uid.as_str(),
                    phase,
                    reason,
                    requests = summary.total.requests,
                    errors,
                    "load test ended"
                );
            }
            Ok(false) => {}
            Err(_) => return,
        }
        let mut status = status_of(phase, reason, message);
        status["startTime"] = test.status.get("startTime").cloned().unwrap_or(Value::Null);
        status["endTime"] = json!(rfc3339(now_ms));
        status["pods"] = json!(posts.len());
        for field in ["targets", "skipped"] {
            if let Some(value) = test.status.get(field) {
                status[field] = value.clone();
            }
        }
        status["results"] = serde_json::from_str(&text).unwrap_or(Value::Null);
        self.set_status(test, status).await;
        self.with_state(uid, |s| *s = RunState::default());
    }

    /// The run ended elsewhere (or its status apply failed): the status follows the store.
    async fn recover(&self, test: &Test, now_ms: u64) {
        let fields = match self
            .store
            .load_fields(&test.namespace, &test.deployment, &test.uid)
            .await
        {
            Ok(fields) => fields,
            Err(_) => return,
        };
        let (Some(phase), Some(reason)) = (fields.get("phase"), fields.get("reason")) else {
            return self
                .fail(test, "JobFailed", "the run state is gone", now_ms)
                .await;
        };
        let end_ms = fields
            .get("end")
            .and_then(|e| e.parse().ok())
            .unwrap_or(now_ms);
        let mut status = status_of(phase, reason, "");
        status["startTime"] = test.status.get("startTime").cloned().unwrap_or(Value::Null);
        status["endTime"] = json!(rfc3339(end_ms));
        for field in ["targets", "pods", "skipped"] {
            if let Some(value) = test.status.get(field) {
                status[field] = value.clone();
            }
        }
        status["results"] = fields
            .get("summary")
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        self.set_status(test, status).await;
    }

    /// A `MimicLoadTest` was deleted: the Job goes (also by owner reference) and the slot is
    /// released.
    pub async fn deleted(&self, test: &Test) {
        self.runs.remove(&test.namespace, &test.name);
        self.with_state(&test.uid, |s| *s = RunState::default());
        if test.ended() {
            return;
        }
        let _ = self
            .store
            .load_stop(&test.namespace, &test.deployment, &test.uid)
            .await;
        self.delete_objects(test).await;
        let ended = Ended {
            phase: "Aborted",
            reason: "Stopped",
            summary: "{}",
            end_ms: clock_ms(),
        };
        let _ = self
            .store
            .load_end(&test.namespace, &test.deployment, &test.uid, &ended)
            .await;
    }
}

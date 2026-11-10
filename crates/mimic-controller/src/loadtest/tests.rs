//! F11 step 7: the reconciler (LDT-07, LDT-09, LDT-10, the status part of LDT-14, the signing
//! part of LDT-20). The tests need the Valkey of `MIMIC_TEST_VALKEY_URL`; those that count the
//! runs or the reserved bytes of the whole cluster use a database of their own.

use std::time::{Duration, SystemTime};

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits as ReportLimits, Report};
use mimic_core::load::job::job_name;
use mimic_core::load::summary::{EndpointRow, PodResults, Stats, Summary};
use mimic_core::load::ticket;
use mimic_runtime::metrics::Metrics;
use p256::ecdsa::signature::Verifier;
use p256::pkcs8::DecodePublicKey;
use serde_json::{Value, json};

use super::*;
use crate::store::load::{MAX_SUMMARY, StoredPost};
use crate::store::{Push, StoreConfig, StoreMetrics};
use crate::tls_ca::{Seed, derive_root};
use crate::workloads::MockSettings;

const LIMITS: ReportLimits = ReportLimits {
    max_literals: 32,
    max_endpoints: 256,
    max_authorities: 64,
    sample_budget: 1 << 20,
};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", hex(6))
}

fn valkey(db: Option<u8>) -> redis::Client {
    let url = std::env::var("MIMIC_TEST_VALKEY_URL")
        .unwrap_or_else(|_| panic!("MIMIC_TEST_VALKEY_URL is not set: start a Valkey >= 9.0"));
    let url = match db {
        Some(db) => format!("{}/{db}", url.trim_end_matches('/')),
        None => url,
    };
    redis::Client::open(url).expect("a valid URL")
}

/// A cluster that remembers what it was asked.
#[derive(Default)]
struct FakeCluster {
    objects: Mutex<BTreeMap<(String, &'static str, String), Value>>,
    deleted: Mutex<Vec<(String, &'static str, String)>>,
    statuses: Mutex<BTreeMap<(String, String), Value>>,
    states: Mutex<BTreeMap<String, JobState>>,
}

fn kind_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Job => "Job",
        Kind::NetworkPolicy => "NetworkPolicy",
    }
}

impl Cluster for FakeCluster {
    fn create<'a>(&'a self, ns: &'a str, kind: Kind, object: &'a Value) -> ClusterFuture<'a, bool> {
        Box::pin(async move {
            let name = object["metadata"]["name"].as_str().unwrap_or_default();
            let key = (ns.to_string(), kind_of(kind), name.to_string());
            let mut objects = self.objects.lock().unwrap();
            Ok(objects.insert(key, object.clone()).is_none())
        })
    }

    fn delete<'a>(&'a self, ns: &'a str, kind: Kind, name: &'a str) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let key = (ns.to_string(), kind_of(kind), name.to_string());
            self.objects.lock().unwrap().remove(&key);
            self.deleted.lock().unwrap().push(key);
            Ok(())
        })
    }

    fn job_state<'a>(&'a self, _ns: &'a str, name: &'a str) -> ClusterFuture<'a, JobState> {
        Box::pin(async move {
            Ok(*self
                .states
                .lock()
                .unwrap()
                .get(name)
                .unwrap_or(&JobState::Running))
        })
    }

    fn apply_status<'a>(
        &'a self,
        ns: &'a str,
        name: &'a str,
        status: &'a Value,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            self.statuses
                .lock()
                .unwrap()
                .insert((ns.to_string(), name.to_string()), status.clone());
            Ok(())
        })
    }
}

impl FakeCluster {
    fn jobs(&self) -> Vec<String> {
        self.of("Job")
    }

    fn of(&self, kind: &str) -> Vec<String> {
        let objects = self.objects.lock().unwrap();
        objects
            .keys()
            .filter(|k| k.1 == kind)
            .map(|k| k.2.clone())
            .collect()
    }

    fn status(&self, ns: &str, name: &str) -> Value {
        self.statuses
            .lock()
            .unwrap()
            .get(&(ns.to_string(), name.to_string()))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn was_deleted(&self, kind: &str, name: &str) -> bool {
        self.deleted
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.1 == kind && d.2 == name)
    }
}

struct Fx {
    rec: Reconciler,
    cluster: Arc<FakeCluster>,
    metrics: Metrics,
}

fn config() -> Config {
    Config {
        max_runs: 1000,
        max_namespace_runs: 1000,
        max_pending: 16,
        store_budget: 1 << 40,
        limits: Limits {
            max_concurrency: 256,
            max_rate: 2000,
            max_duration_ms: 1_800_000,
            pod_concurrency: 128,
        },
        max_endpoints: 64,
        max_plan: 8 << 20,
        start_timeout: Duration::from_secs(300),
        resources: json!({"limits": {"memory": "256Mi"}}),
        image: "mimic@sha256:00".to_string(),
        controller_url: "https://mimic.mimic-system.svc:8444".to_string(),
        controller_namespace: "mimic-system".to_string(),
        controller_port: 8444,
        network: Some(Network {
            controller_labels: BTreeMap::from([("app".to_string(), "mimic".to_string())]),
            dns_namespace: "kube-system".to_string(),
            dns_labels: BTreeMap::from([("k8s-app".to_string(), "kube-dns".to_string())]),
            metrics_from: vec![],
        }),
        status_interval: Duration::from_secs(30),
        pod_window: Duration::from_secs(300),
    }
}

/// A reconciler on the Valkey database `db` (flushed when given), with its own fake cluster.
async fn fx(db: Option<u8>, edit: impl FnOnce(&mut Config)) -> Fx {
    let client = valkey(db);
    if db.is_some() {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        redis::cmd("FLUSHDB")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }
    fx_on(client, edit)
}

/// A second replica on the same Valkey and the same cluster.
fn fx_on(client: redis::Client, edit: impl FnOnce(&mut Config)) -> Fx {
    let mut metrics = Metrics::new("controller", "test");
    let store = Store::new(
        client,
        StoreConfig {
            ttl: Duration::from_secs(3600),
            limits: LIMITS,
            max_deployments: 50,
        },
        StoreMetrics::register(metrics.registry_mut()),
    );
    let seed = Arc::new(SeedSlot::new());
    seed.set(Seed::generate(SystemTime::now()));
    let cert = Arc::new(CertSlot::new());
    cert.set_ca_pem(b"-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n");
    let cluster = Arc::new(FakeCluster::default());
    let mut config = config();
    edit(&mut config);
    let rec = Reconciler {
        config,
        store,
        workloads: Arc::new(WorkloadCache::synced_with(&[])),
        tests: Arc::new(Tests::default()),
        runs: Arc::new(RunIndex::default()),
        slices: Arc::new(SliceCache::default()),
        cluster: cluster.clone(),
        seed,
        cert,
        metrics: LoadMetrics::register(metrics.registry_mut()),
        state: Mutex::new(BTreeMap::new()),
    };
    Fx {
        rec,
        cluster,
        metrics,
    }
}

impl Fx {
    /// Another replica: same Valkey, same cluster, same objects.
    fn replica(&self, client: redis::Client) -> Fx {
        let mut other = fx_on(client, |c| *c = self.rec.config.clone());
        other.rec.cluster = self.cluster.clone();
        other.rec.tests = self.rec.tests.clone();
        other.rec.workloads = self.rec.workloads.clone();
        other.rec.slices = self.rec.slices.clone();
        other.rec.seed = self.rec.seed.clone();
        other
    }

    /// A deployment with a mock (or discovery) `MimicWorkload`, one ready pod, a stored report
    /// of one endpoint (`GET /a`) and a pull of that pod in the mode.
    async fn deployment(&self, ns: &str, dep: &str, mock: bool) {
        self.deployment_with(ns, dep, mock, false).await;
    }

    async fn deployment_with(&self, ns: &str, dep: &str, mock: bool, grpc: bool) {
        let name = format!("{dep}-w");
        self.rec.workloads.apply(ns, &name, dep, vec![], vec![]);
        self.rec.workloads.set_mock(
            ns,
            &name,
            MockSettings {
                mock,
                ..MockSettings::default()
            },
        );
        let pod = format!("{dep}-5d9c8f7b6-bcdfg");
        let slice: k8s_openapi::api::discovery::v1::EndpointSlice = serde_json::from_value(json!({
            "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
            "metadata": {"name": format!("{dep}-s"), "namespace": ns},
            "addressType": "IPv4",
            "endpoints": [{"addresses": ["10.0.0.7"], "conditions": {"ready": true},
                           "targetRef": {"kind": "Pod", "name": pod}}],
        }))
        .unwrap();
        self.rec.slices.apply(slice);
        self.rec.slices.mark_synced();
        self.pull(ns, dep, &pod, if mock { "mock" } else { "discovery" })
            .await;
        self.report(ns, dep, grpc).await;
    }

    async fn pull(&self, ns: &str, dep: &str, pod: &str, mode: &str) {
        let mut conn = self.conn().await;
        redis::cmd("HSET")
            .arg(format!("mimic:v1:{{{ns}/{dep}}}:pulls"))
            .arg(pod)
            .arg(format!("{mode} {}", now_ms()))
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }

    async fn conn(&self) -> redis::aio::MultiplexedConnection {
        self.rec.store.connection().await.unwrap()
    }

    async fn report(&self, ns: &str, dep: &str, grpc: bool) {
        let pod = format!("{dep}-5d9c8f7b6-bcdfg");
        let mut report = Report::new(Some(pod.clone()));
        report.record(
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "GET",
                path: "/a",
                status: 200,
                head_ms: 3,
                host: Some("svc"),
                grpc,
                observation: Observation::default(),
            },
        );
        report.stamp(now_ms() / 86_400_000);
        let push = Push {
            pod,
            seq: None,
            json: report.to_json().unwrap(),
        };
        self.rec.store.push(ns, dep, push).await.expect("pushed");
    }

    /// A `MimicLoadTest` created at `created`.
    fn test(&self, ns: &str, name: &str, dep: &str, created: u64) -> Test {
        let test = Test {
            namespace: ns.to_string(),
            name: name.to_string(),
            uid: format!("{}-6a3f-4c58-9e11-2f7d3a8b4c60", hex(4)),
            deployment: dep.to_string(),
            created_ms: created,
            spec: TestSpec {
                concurrency: 10,
                rate: None,
                duration_ms: 60_000,
                ramp_up_ms: 0,
                timeout_ms: 10_000,
                include: None,
                exclude: BTreeSet::new(),
                credentials: vec![],
                secret_name: None,
            },
            stop: false,
            status: Value::Null,
        };
        self.rec.tests.apply(test.clone());
        test
    }

    /// The test as the cluster would hold it after the last status apply.
    fn settle(&self, test: &Test) -> Test {
        let mut test = self
            .rec
            .tests
            .list()
            .into_iter()
            .find(|t| t.name == test.name && t.namespace == test.namespace)
            .unwrap();
        test.status = self.cluster.status(&test.namespace, &test.name);
        self.rec.tests.apply(test.clone());
        test
    }

    /// One pass, then the cluster's status is what the next pass reads.
    async fn pass(&self, now: u64) {
        self.rec.tick(now).await;
        for test in self.rec.tests.list() {
            self.settle(&test);
        }
    }

    fn status(&self, test: &Test) -> Value {
        self.cluster.status(&test.namespace, &test.name)
    }

    fn phase(&self, test: &Test) -> (String, String) {
        let s = self.status(test);
        (
            s["phase"].as_str().unwrap_or("").to_string(),
            s["reason"].as_str().unwrap_or("").to_string(),
        )
    }

    fn edit(&self, test: &Test, change: impl FnOnce(&mut Test)) {
        let mut current = self
            .rec
            .tests
            .list()
            .into_iter()
            .find(|t| t.name == test.name && t.namespace == test.namespace)
            .unwrap();
        change(&mut current);
        self.rec.tests.apply(current);
    }

    async fn post(
        &self,
        test: &Test,
        index: u64,
        final_post: bool,
        reason: Option<&str>,
        requests: u64,
    ) {
        let mut stats = Stats::default();
        for _ in 0..requests {
            stats.response(200, 1500);
        }
        let post = StoredPost {
            final_post,
            reason: reason.map(str::to_string),
            results: PodResults {
                endpoints: vec![stats],
            },
        };
        self.rec
            .store
            .load_post(
                &test.namespace,
                &test.deployment,
                &test.uid,
                &index.to_string(),
                &post,
            )
            .await
            .unwrap();
    }

    async fn active(&self, test: &Test) -> Option<bool> {
        self.rec
            .store
            .load_active(&test.namespace, &test.deployment, &test.uid)
            .await
            .unwrap()
    }
}

fn running_fixture_ns() -> String {
    unique("shop")
}

#[tokio::test]
async fn ldt_07_discovery_waits_then_not_mock_after_the_start_timeout() {
    let f = fx(None, |c| c.start_timeout = Duration::from_secs(5)).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", false).await;
    let t0 = now_ms();
    let test = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&test), ("Pending".into(), "WaitingForMock".into()));
    assert!(f.cluster.jobs().is_empty());
    f.pass(t0 + 6000).await;
    assert_eq!(f.phase(&test), ("Failed".into(), "NotMock".into()));
    assert!(f.cluster.jobs().is_empty());
    assert_eq!(f.active(&test).await, None);
}

#[tokio::test]
async fn ldt_07_a_deployment_without_a_target_waits_then_has_no_targets() {
    let f = fx(None, |c| c.start_timeout = Duration::from_secs(5)).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    f.rec.slices.replace(vec![]);
    let t0 = now_ms();
    let test = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    assert_eq!(
        f.phase(&test),
        ("Pending".into(), "WaitingForTargets".into())
    );
    f.pass(t0 + 6000).await;
    assert_eq!(f.phase(&test), ("Failed".into(), "NoTargets".into()));
}

#[tokio::test]
async fn ldt_07_mock_and_mock_pulls_create_the_job_and_the_run_is_running() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let mut test = f.test(&ns, "t1", "pay", t0);
    test.spec.concurrency = 200;
    f.rec.tests.apply(test.clone());
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&test), ("Running".into(), "".into()));
    let name = job_name(&test.uid);
    assert_eq!(f.cluster.jobs(), std::slice::from_ref(&name));
    assert_eq!(f.cluster.of("NetworkPolicy"), std::slice::from_ref(&name));
    let job = f.cluster.objects.lock().unwrap()[&(ns.clone(), "Job", name)].clone();
    assert_eq!(job["spec"]["parallelism"], 2);
    assert_eq!(
        job["spec"]["template"]["spec"]["containers"][0]["image"],
        "mimic@sha256:00"
    );
    let status = f.status(&test);
    assert!(
        status["startTime"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z'))
    );
    assert_eq!(status["targets"], 1);
    assert_eq!(f.active(&test).await, Some(false));
    // The load API knows the run.
    assert_eq!(
        f.rec.runs.get(&ns, "t1").map(|r| r.uid),
        Some(test.uid.clone())
    );
    // One start line, with the fields of the criterion and no ticket.
    f.pass(t0 + 2000).await;
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let started: Vec<Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["message"] == "load test started" && v["namespace"] == ns.as_str())
        .collect();
    assert_eq!(started.len(), 1, "{raw}");
    assert_eq!(started[0]["concurrency"], 200);
    assert_eq!(started[0]["deployment"], "pay");
    assert_eq!(started[0]["uid"], test.uid.as_str());
    assert!(!raw.contains("v1."), "{raw}");
}

#[tokio::test]
async fn ldt_20_the_ticket_is_signed_by_the_current_root_of_the_deployment() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let first = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    let stored = f
        .rec
        .store
        .load_run(&ns, "pay", &first.uid)
        .await
        .unwrap()
        .unwrap();
    let parsed = ticket::parse(&stored.ticket).expect("a ticket");
    assert_eq!(parsed.payload.namespace, ns);
    assert_eq!(parsed.payload.deployment, "pay");
    assert_eq!(parsed.payload.uid, first.uid);
    // exp: the run's deadline in seconds (duration + start timeout + 60 s after the start).
    assert!(parsed.payload.exp * 1000 >= t0 + 60_000 + 300_000 + 60_000);
    let verifies = |seed: &Seed, parsed: &ticket::Parsed| {
        let root = derive_root(seed, &ns, "pay").unwrap();
        let key = p256::ecdsa::VerifyingKey::from_public_key_der(&root.public_key_der()).unwrap();
        let signature = p256::ecdsa::Signature::from_slice(&parsed.signature).unwrap();
        key.verify(&parsed.signed, &signature).is_ok()
    };
    let seed = f.rec.seed.get().unwrap();
    assert!(verifies(&seed, &parsed));
    // After a CA rotation the next run is signed by the new root, not the old one.
    let rotated = Seed::generate(SystemTime::now());
    f.rec.seed.set(rotated);
    let second = f.test(&ns, "t2", "other", t0);
    f.deployment(&ns, "other", true).await;
    f.pass(t0 + 2000).await;
    let stored = f
        .rec
        .store
        .load_run(&ns, "other", &second.uid)
        .await
        .unwrap()
        .unwrap();
    let parsed = ticket::parse(&stored.ticket).expect("a ticket");
    let current = f.rec.seed.get().unwrap();
    let root = derive_root(&current, &ns, "other").unwrap();
    let key = p256::ecdsa::VerifyingKey::from_public_key_der(&root.public_key_der()).unwrap();
    let signature = p256::ecdsa::Signature::from_slice(&parsed.signature).unwrap();
    assert!(key.verify(&parsed.signed, &signature).is_ok());
    let old = derive_root(&seed, &ns, "other").unwrap();
    let old = p256::ecdsa::VerifyingKey::from_public_key_der(&old.public_key_der()).unwrap();
    assert!(old.verify(&parsed.signed, &signature).is_err());
}

#[tokio::test]
async fn ldt_07_a_second_run_on_the_deployment_is_busy_and_a_third_in_the_namespace_too() {
    let f = fx(None, |c| c.max_namespace_runs = 2).await;
    let ns = running_fixture_ns();
    for dep in ["d1", "d2", "d3"] {
        f.deployment(&ns, dep, true).await;
    }
    let t0 = now_ms();
    let a = f.test(&ns, "a", "d1", t0);
    let b = f.test(&ns, "b", "d1", t0 + 1);
    let c = f.test(&ns, "c", "d2", t0 + 2);
    let d = f.test(&ns, "d", "d3", t0 + 3);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&a), ("Running".into(), "".into()));
    assert_eq!(f.phase(&b).0, "Failed");
    assert_eq!(f.phase(&b).1, "Busy");
    assert_eq!(f.phase(&c), ("Running".into(), "".into()));
    assert_eq!(f.phase(&d), ("Failed".into(), "NamespaceBusy".into()));
    assert_eq!(f.cluster.jobs().len(), 2);
}

#[tokio::test]
async fn ldt_07_past_max_runs_the_run_is_cluster_busy() {
    let f = fx(Some(6), |c| c.max_runs = 1).await;
    let (ns1, ns2) = (running_fixture_ns(), running_fixture_ns());
    f.deployment(&ns1, "pay", true).await;
    f.deployment(&ns2, "pay", true).await;
    let t0 = now_ms();
    let a = f.test(&ns1, "a", "pay", t0);
    let b = f.test(&ns2, "b", "pay", t0 + 1);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&a).0, "Running");
    assert_eq!(f.phase(&b), ("Failed".into(), "ClusterBusy".into()));
}

#[tokio::test]
async fn ldt_07_the_store_budget_refuses_a_run_that_does_not_fit_until_the_first_ended() {
    // One plan of a few hundred bytes reserves plan + 256 KiB + 192 KiB: 700 KiB holds one.
    let f = fx(Some(7), |c| c.store_budget = 700 << 10).await;
    let (ns1, ns2) = (running_fixture_ns(), running_fixture_ns());
    f.deployment(&ns1, "pay", true).await;
    f.deployment(&ns2, "pay", true).await;
    let t0 = now_ms();
    let a = f.test(&ns1, "a", "pay", t0);
    let b = f.test(&ns2, "b", "pay", t0 + 1);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&a).0, "Running");
    assert_eq!(f.phase(&b), ("Failed".into(), "StoreFull".into()));
    // Once the first ended the same request is admitted.
    f.edit(&a, |t| t.stop = true);
    f.pass(t0 + 2000).await;
    assert_eq!(f.phase(&a), ("Aborted".into(), "Stopped".into()));
    let c = f.test(&ns2, "c", "pay", t0 + 3);
    f.pass(t0 + 3000).await;
    assert_eq!(f.phase(&c).0, "Running");
}

#[tokio::test]
async fn ldt_07_the_third_pending_run_of_a_namespace_fails_and_the_first_two_wait() {
    let f = fx(None, |c| c.max_pending = 2).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", false).await;
    let t0 = now_ms();
    let a = f.test(&ns, "a", "pay", t0);
    let b = f.test(&ns, "b", "pay", t0 + 1);
    let c = f.test(&ns, "c", "pay", t0 + 2);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&a).0, "Pending");
    assert_eq!(f.phase(&b).0, "Pending");
    assert_eq!(f.phase(&c), ("Failed".into(), "TooManyPending".into()));
}

#[tokio::test]
async fn ldt_07_two_replicas_create_one_job_per_run_and_admit_at_most_max_runs() {
    let f = fx(Some(8), |c| c.max_runs = 2).await;
    let g = f.replica(valkey(Some(8)));
    let nss: Vec<String> = (0..4).map(|_| running_fixture_ns()).collect();
    let t0 = now_ms();
    let mut tests = Vec::new();
    for (i, ns) in nss.iter().enumerate() {
        f.deployment(ns, "pay", true).await;
        tests.push(f.test(ns, "t", "pay", t0 + i as u64));
    }
    tokio::join!(f.rec.tick(t0 + 1000), g.rec.tick(t0 + 1000));
    let jobs = f.cluster.jobs();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let running = nss
        .iter()
        .filter(|ns| f.cluster.status(ns, "t")["phase"] == "Running")
        .count();
    let busy = nss
        .iter()
        .filter(|ns| f.cluster.status(ns, "t")["reason"] == "ClusterBusy")
        .count();
    assert_eq!((running, busy), (2, 2));
}

#[tokio::test]
async fn ldt_07_invalid_values_no_report_no_endpoints_and_disabled_fail_with_their_reason() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let mut big = f.test(&ns, "big", "pay", t0);
    big.spec.concurrency = 257;
    f.rec.tests.apply(big.clone());
    // No stored report.
    f.rec.workloads.apply(&ns, "w2", "bare", vec![], vec![]);
    f.rec.workloads.set_mock(
        &ns,
        "w2",
        MockSettings {
            mock: true,
            ..MockSettings::default()
        },
    );
    f.pull(&ns, "bare", "bare-5d9c8f7b6-bcdfg", "mock").await;
    let slice: k8s_openapi::api::discovery::v1::EndpointSlice = serde_json::from_value(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": {"name": "bare-s", "namespace": ns}, "addressType": "IPv4",
        "endpoints": [{"addresses": ["10.0.0.8"], "conditions": {"ready": true},
                       "targetRef": {"kind": "Pod", "name": "bare-5d9c8f7b6-bcdfg"}}],
    }))
    .unwrap();
    f.rec.slices.apply(slice);
    let bare = f.test(&ns, "bare", "bare", t0 + 1);
    // Only a gRPC endpoint.
    f.deployment_with(&ns, "rpc", true, true).await;
    let rpc = f.test(&ns, "rpc", "rpc", t0 + 2);
    f.pass(t0 + 1000).await;
    let invalid = f.status(&big);
    assert_eq!(
        (invalid["phase"].as_str(), invalid["reason"].as_str()),
        (Some("Failed"), Some("Invalid"))
    );
    assert!(
        invalid["message"]
            .as_str()
            .unwrap()
            .contains("spec.concurrency")
    );
    assert_eq!(f.phase(&bare), ("Failed".into(), "NoReport".into()));
    assert_eq!(f.phase(&rpc), ("Failed".into(), "NoEndpoints".into()));
    assert_eq!(f.status(&rpc)["skipped"], json!({"grpc": 1}));
    assert!(f.cluster.jobs().is_empty());

    let off = fx(None, |c| c.max_runs = 0).await;
    off.deployment(&ns, "pay", true).await;
    let t = off.test(&ns, "t", "pay", t0);
    off.pass(t0 + 1000).await;
    assert_eq!(off.phase(&t), ("Failed".into(), "Disabled".into()));
}

#[tokio::test]
async fn ldt_09_posts_of_two_pods_merge_into_the_status_and_the_run_completes() {
    let f = fx(None, |c| c.status_interval = Duration::from_secs(1)).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let mut test = f.test(&ns, "t1", "pay", t0);
    test.spec.concurrency = 200;
    f.rec.tests.apply(test.clone());
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&test).0, "Running");
    f.post(&test, 0, false, None, 5).await;
    f.post(&test, 1, false, None, 7).await;
    f.pass(t0 + 10_000).await;
    let status = f.status(&test);
    assert_eq!(status["phase"], "Running");
    assert_eq!(status["pods"], 2);
    assert_eq!(status["results"]["total"]["requests"], 12);
    assert_eq!(status["results"]["endpoints"][0]["template"], "/a");
    // The same posts again change nothing.
    f.post(&test, 0, false, None, 5).await;
    f.pass(t0 + 12_000).await;
    assert_eq!(f.status(&test)["results"]["total"]["requests"], 12);
    // Both final: Completed, with the merged results.
    f.post(&test, 0, true, Some("Completed"), 5).await;
    f.post(&test, 1, true, Some("Completed"), 7).await;
    f.pass(t0 + 14_000).await;
    let status = f.status(&test);
    assert_eq!(
        (status["phase"].as_str(), status["reason"].as_str()),
        (Some("Succeeded"), Some("Completed"))
    );
    assert_eq!(status["results"]["total"]["requests"], 12);
    assert!(status["endTime"].as_str().is_some());
    // The stored run keeps the summary only; the slot and the reservation are gone.
    let mut conn = f.conn().await;
    let key = format!("mimic:v1:{{{ns}/pay}}:load:{}", test.uid);
    let mut fields: Vec<String> = redis::cmd("HKEYS")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .unwrap();
    fields.sort();
    assert_eq!(
        fields,
        ["end", "phase", "reason", "spec", "start", "stop", "summary"]
    );
    let member = format!("{ns}/pay/{}", test.uid);
    let reserved: Option<String> = redis::cmd("HGET")
        .arg("mimic:v1:loads:reserved")
        .arg(&member)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(reserved, None);
    let late = StoredPost {
        final_post: false,
        reason: None,
        results: PodResults::default(),
    };
    assert_eq!(
        f.rec
            .store
            .load_post(&ns, "pay", &test.uid, "0", &late)
            .await
            .unwrap(),
        None
    );
    // One end line and one count.
    assert!(
        f.metrics
            .encode()
            .contains("load_runs_total{phase=\"Succeeded\",reason=\"Completed\"} 1")
    );
}

#[tokio::test]
async fn ldt_09_a_final_post_with_a_reason_ends_the_run_as_that_reason() {
    for (reason, phase, ended) in [
        ("ModeLeft", "Aborted", "ModeLeft"),
        ("Unverified", "Aborted", "Unverified"),
        ("NoTargets", "Failed", "NoTargets"),
    ] {
        let f = fx(None, |_| {}).await;
        let ns = running_fixture_ns();
        f.deployment(&ns, "pay", true).await;
        let t0 = now_ms();
        let test = f.test(&ns, "t1", "pay", t0);
        f.pass(t0 + 1000).await;
        f.post(&test, 0, true, Some(reason), 3).await;
        f.pass(t0 + 2000).await;
        assert_eq!(f.phase(&test), (phase.into(), ended.into()));
        assert_eq!(f.status(&test)["results"]["total"]["requests"], 3);
        assert!(f.cluster.was_deleted("Job", &job_name(&test.uid)));
    }
}

#[tokio::test]
async fn ldt_09_a_failed_job_fails_the_run_with_the_results_posted_so_far() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let test = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    f.post(&test, 0, false, None, 4).await;
    f.cluster
        .states
        .lock()
        .unwrap()
        .insert(job_name(&test.uid), JobState::Failed);
    f.pass(t0 + 2000).await;
    assert_eq!(f.phase(&test), ("Failed".into(), "JobFailed".into()));
    assert_eq!(f.status(&test)["results"]["total"]["requests"], 4);
    assert_eq!(f.active(&test).await, None);
}

#[test]
fn ldt_09_a_summary_over_the_cap_keeps_the_total_and_drops_the_rows() {
    let row = |n: usize| EndpointRow {
        method: "GET".into(),
        port: 80,
        template: format!("/{}", "x".repeat(n)),
        row: mimic_core::load::summary::summary(
            &mimic_core::load::plan::Plan::default(),
            &[],
            1000,
        )
        .total,
    };
    let small = Summary {
        total: row(0).row,
        endpoints: vec![row(10)],
    };
    assert_eq!(summary_json(&small), serde_json::to_string(&small).unwrap());
    let big = Summary {
        total: row(0).row,
        endpoints: (0..64).map(|_| row(4096)).collect(),
    };
    let json = summary_json(&big);
    assert!(json.len() < MAX_SUMMARY, "{}", json.len());
    let parsed: Summary = serde_json::from_str(&json).unwrap();
    assert!(parsed.endpoints.is_empty());
    assert_eq!(parsed.total, big.total);
}

#[tokio::test]
async fn ldt_10_the_mode_leaving_mock_deletes_the_job_and_frees_the_slot() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let test = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    assert_eq!(f.phase(&test).0, "Running");
    f.rec
        .workloads
        .set_mock(&ns, "pay-w", MockSettings::default());
    f.pass(t0 + 1500).await;
    assert_eq!(f.phase(&test), ("Aborted".into(), "ModeLeft".into()));
    let name = job_name(&test.uid);
    assert!(f.cluster.was_deleted("Job", &name));
    assert!(f.cluster.was_deleted("NetworkPolicy", &name));
    // The stop flag was set (and is kept) and the run is no longer active.
    let fields = f
        .rec
        .store
        .load_fields(&ns, "pay", &test.uid)
        .await
        .unwrap();
    assert_eq!(fields["stop"], "1");
    assert_eq!(f.active(&test).await, None);
    // A new run on the deployment is admitted once it is back in mock.
    f.rec.workloads.set_mock(
        &ns,
        "pay-w",
        MockSettings {
            mock: true,
            ..MockSettings::default()
        },
    );
    f.pull(&ns, "pay", "pay-5d9c8f7b6-bcdfg", "mock").await;
    let next = f.test(&ns, "t2", "pay", t0 + 2000);
    f.pass(t0 + 3000).await;
    assert_eq!(f.phase(&next).0, "Running");
}

#[tokio::test]
async fn ldt_10_spec_stop_aborts_as_stopped_and_deleting_the_object_frees_the_slot() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let test = f.test(&ns, "t1", "pay", t0);
    f.pass(t0 + 1000).await;
    f.edit(&test, |t| t.stop = true);
    f.pass(t0 + 1500).await;
    assert_eq!(f.phase(&test), ("Aborted".into(), "Stopped".into()));
    assert!(f.cluster.was_deleted("Job", &job_name(&test.uid)));
    assert_eq!(
        f.rec
            .store
            .load_fields(&ns, "pay", &test.uid)
            .await
            .unwrap()["stop"],
        "1"
    );
    // Deleted object: the run is released, the index forgets it.
    let other = f.test(&ns, "t2", "pay", t0 + 2000);
    f.pass(t0 + 3000).await;
    assert_eq!(f.phase(&other).0, "Running");
    let gone = f.rec.tests.remove(&ns, "t2").unwrap();
    f.rec.deleted(&gone).await;
    assert_eq!(f.active(&other).await, None);
    assert!(f.rec.runs.get(&ns, "t2").is_none());
    assert!(f.cluster.was_deleted("Job", &job_name(&other.uid)));
}

#[tokio::test]
async fn ldt_14_no_ticket_or_credential_name_value_reaches_the_status() {
    let f = fx(None, |_| {}).await;
    let ns = running_fixture_ns();
    f.deployment(&ns, "pay", true).await;
    let t0 = now_ms();
    let mut test = f.test(&ns, "t1", "pay", t0);
    test.spec.secret_name = Some("canary-secret-name".into());
    test.spec.credentials = vec![CredentialField {
        field: "header/authorization".into(),
        key: "token".into(),
    }];
    f.rec.tests.apply(test.clone());
    f.pass(t0 + 1000).await;
    f.post(&test, 0, true, Some("Completed"), 2).await;
    f.pass(t0 + 2000).await;
    let stored = f
        .rec
        .store
        .load_fields(&ns, "pay", &test.uid)
        .await
        .unwrap();
    let status = f.status(&test).to_string();
    for text in [status, stored["summary"].clone(), stored["spec"].clone()] {
        assert!(!text.contains("v1.") && !text.contains("Bearer"), "{text}");
    }
    assert_eq!(f.phase(&test).0, "Succeeded");
}

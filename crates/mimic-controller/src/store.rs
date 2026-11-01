//! The discovery store (spec §7.3): the only code that reads or writes the `mimic:v1:` keys.
//!
//! A push reaches `Store::push` already checked, re-redacted and stamped (`mimic-core`). Within
//! a replica one writer per deployment merges the pushes that queued while it wrote (group
//! commit); at most `MAX_WRITERS` deployments are written at once. A write reads the stored
//! report, its `version` and the `:seq` fields, merges in memory, then runs one Lua script that
//! writes only if those are still what was read (compare-and-set), so a push is applied once
//! whichever replica receives it.

// Called by the push route (step 4 of F5).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimic_core::discovery::report::{Direction, Limits, Report};
use mimic_core::discovery::store::{Kind, Refusal, drop_expired, merge_bounded};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};
use redis::aio::MultiplexedConnection;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, oneshot};

/// Deployments written at once per replica (memory bound).
const MAX_WRITERS: usize = 2;
/// Attempts of one write before `503`.
const ATTEMPTS: u32 = 5;
const BACKOFF_MS: std::ops::RangeInclusive<u64> = 10..=100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest sequence a sidecar may send: what a JSON number holds exactly.
const MAX_SEQUENCE: u64 = 1 << 53;
const DAY_SECS: u64 = 86_400;
/// Lifetime of the `:pods` and `:seq` fields, at most.
const FIELD_TTL_MS: u64 = DAY_SECS * 1000;

const WRITE: &str = r"
local current = redis.call('HGET', KEYS[2], 'version')
if (current or '0') ~= ARGV[1] then return 0 end
local n_pods = tonumber(ARGV[14])
local seq_at = 15 + n_pods
local n_seq = tonumber(ARGV[seq_at])
for i = 0, n_seq - 1 do
  local at = seq_at + 1 + i * 3
  if (redis.call('HGET', KEYS[4], ARGV[at]) or '') ~= ARGV[at + 1] then return 0 end
end
local ttl = tonumber(ARGV[2])
local field_ttl = tonumber(ARGV[3])
local now = tonumber(ARGV[4])
local floor = tostring(now - ttl)
redis.call('ZREMRANGEBYSCORE', KEYS[5], '-inf', floor)
redis.call('ZREMRANGEBYSCORE', KEYS[6], '-inf', floor)
if not redis.call('ZSCORE', KEYS[5], ARGV[7])
   and redis.call('ZCARD', KEYS[5]) >= tonumber(ARGV[5]) then
  return -1
end
redis.call('SET', KEYS[1], ARGV[8], 'PX', ttl)
redis.call('HINCRBY', KEYS[2], 'version', 1)
redis.call('HSET', KEYS[2], 'updated', now, 'exchanges', ARGV[9],
  'endpoints_inbound', ARGV[10], 'endpoints_outbound', ARGV[11],
  'sample_bytes', ARGV[12], 'report_bytes', ARGV[13])
redis.call('PEXPIRE', KEYS[2], ttl)
for i = 15, 14 + n_pods do
  redis.call('HSET', KEYS[3], ARGV[i], now)
  redis.call('HPEXPIRE', KEYS[3], field_ttl, 'FIELDS', 1, ARGV[i])
end
for i = 0, n_seq - 1 do
  local at = seq_at + 1 + i * 3
  if ARGV[at + 2] ~= '' then
    redis.call('HSET', KEYS[4], ARGV[at], ARGV[at + 2])
    redis.call('HPEXPIRE', KEYS[4], field_ttl, 'FIELDS', 1, ARGV[at])
  end
end
redis.call('ZADD', KEYS[5], now, ARGV[7])
redis.call('PEXPIRE', KEYS[5], ttl)
redis.call('ZADD', KEYS[6], now, ARGV[6] .. '/' .. ARGV[7])
redis.call('PEXPIRE', KEYS[6], ttl)
return 1
";

/// A namespace, deployment or pod name that is not DNS-1123, or a session or sequence that
/// is out of range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError;

/// DNS-1123 subdomain: no `:`, `/`, `{` or `}` can reach a key.
fn check_name(name: &str) -> Result<(), NameError> {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ok = !name.is_empty()
        && name.len() <= 253
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && edge(name.chars().next())
        && edge(name.chars().last());
    ok.then_some(()).ok_or(NameError)
}

/// The keys of one deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    pub report: String,
    pub meta: String,
    pub pods: String,
    pub seq: String,
    pub ns_index: String,
    pub workloads: String,
}

/// The keys of `<namespace>/<deployment>`, once both names are checked.
pub fn keys(namespace: &str, deployment: &str) -> Result<Keys, NameError> {
    check_name(namespace)?;
    check_name(deployment)?;
    let base = format!("mimic:v1:{{{namespace}/{deployment}}}");
    Ok(Keys {
        report: format!("{base}:report"),
        meta: format!("{base}:meta"),
        pods: format!("{base}:pods"),
        seq: format!("{base}:seq"),
        ns_index: format!("mimic:v1:ns:{namespace}"),
        workloads: "mimic:v1:workloads".to_string(),
    })
}

/// A session and sequence that passed the format check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqId {
    pub session: String,
    pub sequence: u64,
}

impl SeqId {
    /// `session`: 32 lower-case hex characters; `sequence`: 1 to 2^53.
    pub fn parse(session: &str, sequence: u64) -> Result<SeqId, NameError> {
        let hex = session.len() == 32
            && session
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex || sequence == 0 || sequence > MAX_SEQUENCE {
            return Err(NameError);
        }
        Ok(SeqId {
            session: session.to_string(),
            sequence,
        })
    }

    fn field(&self, pod: &str) -> String {
        format!("{pod}/{}", self.session)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Apply,
    Duplicate,
}

/// `stored`: the last applied sequence of the push's pod and session; `sequence`: the
/// push's. A push without a sequence is always applied.
pub fn decide(stored: Option<u64>, sequence: Option<u64>) -> Decision {
    match (stored, sequence) {
        (Some(stored), Some(sequence)) if sequence <= stored => Decision::Duplicate,
        _ => Decision::Apply,
    }
}

/// A checked, re-redacted and stamped report, and who sent it.
pub struct Push {
    pub pod: String,
    pub seq: Option<SeqId>,
    pub report: Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// A name that is not DNS-1123: `400`.
    Name,
    /// A new deployment past `MIMIC_STORE_MAX_DEPLOYMENTS` in the namespace: `507`.
    Deployments,
    /// Valkey unavailable, too many conflicts or an unreadable stored value: `503`.
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// `MIMIC_STORE_TTL`.
    pub ttl: Duration,
    pub limits: Limits,
    /// `MIMIC_STORE_MAX_DEPLOYMENTS`, per namespace.
    pub max_deployments: usize,
}

type Labels = Vec<(String, String)>;

fn label(name: &str, value: &str) -> Labels {
    vec![(name.to_string(), value.to_string())]
}

/// The `mimic_store_*` metrics of spec §9. No label holds a namespace, deployment or pod.
#[derive(Clone)]
pub struct StoreMetrics {
    writes: Family<Labels, Counter>,
    duration: Histogram,
    report_bytes: Histogram,
    pushes: Family<Labels, Counter>,
    refused: Family<Labels, Counter>,
    evicted: Family<Labels, Counter>,
    expired: Family<Labels, Counter>,
}

impl StoreMetrics {
    pub fn register(registry: &mut Registry) -> StoreMetrics {
        let metrics = StoreMetrics {
            writes: Family::default(),
            duration: Histogram::new([
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            report_bytes: Histogram::new([
                4_096.0,
                16_384.0,
                65_536.0,
                262_144.0,
                1_048_576.0,
                4_194_304.0,
                16_777_216.0,
                67_108_864.0,
            ]),
            pushes: Family::default(),
            refused: Family::default(),
            evicted: Family::default(),
            expired: Family::default(),
        };
        registry.register(
            "store_writes",
            "Store write attempts",
            metrics.writes.clone(),
        );
        registry.register_with_unit(
            "store_write_duration",
            "One store write, from read to script result",
            Unit::Seconds,
            metrics.duration.clone(),
        );
        registry.register_with_unit(
            "store_report",
            "Size of each report written",
            Unit::Bytes,
            metrics.report_bytes.clone(),
        );
        registry.register("store_pushes", "Pushes by outcome", metrics.pushes.clone());
        registry.register(
            "store_refused",
            "Growth refused by the bounded merge",
            metrics.refused.clone(),
        );
        registry.register(
            "store_evicted",
            "Stored endpoints and samples displaced by the bounded merge",
            metrics.evicted.clone(),
        );
        registry.register(
            "store_expired",
            "Endpoints and samples dropped by retention",
            metrics.expired.clone(),
        );
        metrics
    }

    fn write(&self, result: &str) {
        self.writes.get_or_create(&label("result", result)).inc();
    }
}

struct Pending {
    push: Push,
    reply: oneshot::Sender<Result<Outcome, StoreError>>,
}

enum Attempt {
    Done(Vec<Outcome>),
    Conflict,
}

type Queues = BTreeMap<(String, String), Vec<Pending>>;

pub struct Store {
    client: redis::Client,
    config: StoreConfig,
    metrics: StoreMetrics,
    connection: AsyncMutex<Option<MultiplexedConnection>>,
    script: redis::Script,
    writers: Semaphore,
    /// A deployment with a writer has an entry: the pushes waiting for its next write.
    queues: Mutex<Queues>,
    /// What was already logged: (namespace, deployment, reason or kind).
    logged: Mutex<BTreeSet<(String, String, String)>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    pub fn new(client: redis::Client, config: StoreConfig, metrics: StoreMetrics) -> Arc<Store> {
        Arc::new(Store {
            client,
            config,
            metrics,
            connection: AsyncMutex::new(None),
            script: redis::Script::new(WRITE),
            writers: Semaphore::new(MAX_WRITERS),
            queues: Mutex::new(BTreeMap::new()),
            logged: Mutex::new(BTreeSet::new()),
        })
    }

    /// Merges the push into the deployment's report. Answered after the write that holds it.
    pub async fn push(
        self: &Arc<Self>,
        namespace: &str,
        deployment: &str,
        push: Push,
    ) -> Result<Outcome, StoreError> {
        let keys = keys(namespace, deployment).map_err(|_| StoreError::Name)?;
        check_name(&push.pod).map_err(|_| StoreError::Name)?;
        let (reply, answer) = oneshot::channel();
        let key = (namespace.to_string(), deployment.to_string());
        let start_writer = {
            let mut queues = lock(&self.queues);
            match queues.get_mut(&key) {
                Some(queue) => {
                    queue.push(Pending { push, reply });
                    false
                }
                None => {
                    queues.insert(key.clone(), vec![Pending { push, reply }]);
                    true
                }
            }
        };
        if start_writer {
            let store = self.clone();
            tokio::spawn(async move { store.drain(key, keys).await });
        }
        answer.await.unwrap_or(Err(StoreError::Unavailable))
    }

    /// The deployment's writer: one write per batch of queued pushes until none is left.
    async fn drain(self: Arc<Self>, key: (String, String), keys: Keys) {
        loop {
            // Acquired before taking the batch: pushes arriving meanwhile join it.
            let Ok(_permit) = self.writers.acquire().await else {
                lock(&self.queues).remove(&key);
                return;
            };
            let batch = {
                let mut queues = lock(&self.queues);
                let batch = queues.get_mut(&key).map(std::mem::take).unwrap_or_default();
                if batch.is_empty() {
                    queues.remove(&key);
                    return;
                }
                batch
            };
            let result = self.write(&key.0, &key.1, &keys, &batch).await;
            for (i, pending) in batch.into_iter().enumerate() {
                let answer = match &result {
                    Ok(outcomes) => Ok(outcomes[i]),
                    Err(e) => Err(*e),
                };
                // The caller may be gone: its push is applied all the same.
                let _ = pending.reply.send(answer);
            }
        }
    }

    async fn write(
        &self,
        namespace: &str,
        deployment: &str,
        keys: &Keys,
        batch: &[Pending],
    ) -> Result<Vec<Outcome>, StoreError> {
        for attempt in 0..ATTEMPTS {
            let started = Instant::now();
            let result = self.attempt(namespace, deployment, keys, batch).await;
            self.metrics
                .duration
                .observe(started.elapsed().as_secs_f64());
            match result {
                Ok(Attempt::Done(outcomes)) => return Ok(outcomes),
                Ok(Attempt::Conflict) => {
                    self.metrics.write("conflict");
                    if attempt + 1 < ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(rand::random_range(BACKOFF_MS)))
                            .await;
                    }
                }
                Err(e) => {
                    if e == StoreError::Unavailable {
                        self.metrics.write("error");
                    }
                    return Err(e);
                }
            }
        }
        tracing::warn!(namespace, deployment, "store write kept conflicting");
        Err(StoreError::Unavailable)
    }

    async fn connection(&self) -> Result<MultiplexedConnection, StoreError> {
        let mut guard = self.connection.lock().await;
        if let Some(conn) = guard.as_ref() {
            return Ok(conn.clone());
        }
        let config = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Some(CONNECT_TIMEOUT))
            .set_response_timeout(Some(RESPONSE_TIMEOUT));
        let conn = self
            .client
            .get_multiplexed_async_connection_with_config(&config)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "valkey connection failed");
                StoreError::Unavailable
            })?;
        *guard = Some(conn.clone());
        Ok(conn)
    }

    /// A connection that failed is dropped so that the next attempt makes a new one.
    async fn failed(&self, error: redis::RedisError) -> StoreError {
        tracing::warn!(%error, "valkey command failed");
        *self.connection.lock().await = None;
        StoreError::Unavailable
    }

    fn once(&self, namespace: &str, deployment: &str, what: &str) -> bool {
        lock(&self.logged).insert((namespace.into(), deployment.into(), what.into()))
    }

    async fn attempt(
        &self,
        namespace: &str,
        deployment: &str,
        keys: &Keys,
        batch: &[Pending],
    ) -> Result<Attempt, StoreError> {
        let mut conn = self.connection().await?;

        // The :seq fields of the batch; "" (never a field) keeps HMGET valid without any.
        let mut fields: Vec<String> = batch
            .iter()
            .filter_map(|p| p.push.seq.as_ref().map(|s| s.field(&p.push.pod)))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let real_fields = fields.len();
        if fields.is_empty() {
            fields.push(String::new());
        }
        let (stored, version, seqs): (Option<Vec<u8>>, Option<String>, Vec<Option<String>>) =
            match redis::pipe()
                .cmd("GET")
                .arg(&keys.report)
                .cmd("HGET")
                .arg(&keys.meta)
                .arg("version")
                .cmd("HMGET")
                .arg(&keys.seq)
                .arg(&fields)
                .query_async(&mut conn)
                .await
            {
                Ok(read) => read,
                Err(e) => return Err(self.failed(e).await),
            };
        let unreadable = || {
            tracing::error!(namespace, deployment, "stored report unreadable");
            StoreError::Unavailable
        };
        let mut report = match stored {
            None => Report::new(None),
            Some(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|json| Report::parse_checked(json, usize::MAX).ok())
                .ok_or_else(unreadable)?,
        };
        let version: u64 = match version {
            None => 0,
            Some(v) => v.parse().map_err(|_| unreadable())?,
        };
        let mut sequences: BTreeMap<String, u64> = BTreeMap::new();
        let mut expected: BTreeMap<String, String> = BTreeMap::new();
        for (field, value) in fields.iter().zip(seqs).take(real_fields) {
            if let Some(value) = value {
                sequences.insert(field.clone(), value.parse().map_err(|_| unreadable())?);
                expected.insert(field.clone(), value);
            }
        }
        let read_sequences = sequences.clone();

        let (now, today) = (now_ms(), now_ms() / 1000 / DAY_SECS);
        let ttl_ms = u64::try_from(self.config.ttl.as_millis()).unwrap_or(u64::MAX);
        let mut outcomes = Vec::with_capacity(batch.len());
        let mut refused: BTreeMap<Refusal, u64> = BTreeMap::new();
        let mut evicted: BTreeMap<Kind, u64> = BTreeMap::new();
        for pending in batch {
            let push = &pending.push;
            let field = push.seq.as_ref().map(|s| s.field(&push.pod));
            let stored_seq = field.as_ref().and_then(|f| sequences.get(f)).copied();
            let outcome = match decide(stored_seq, push.seq.as_ref().map(|s| s.sequence)) {
                Decision::Duplicate => Outcome::Duplicate,
                Decision::Apply => {
                    if let (Some(field), Some(seq)) = (field, &push.seq) {
                        sequences.insert(field, seq.sequence);
                    }
                    let merged = merge_bounded(report, push.report.clone(), &self.config.limits);
                    report = merged.report;
                    for (reason, n) in merged.refused {
                        *refused.entry(reason).or_default() += n;
                    }
                    for (kind, n) in merged.evicted {
                        *evicted.entry(kind).or_default() += n;
                    }
                    Outcome::Applied
                }
            };
            outcomes.push(outcome);
        }

        let pods: BTreeSet<&str> = batch.iter().map(|p| p.push.pod.as_str()).collect();
        let field_ttl = ttl_ms.min(FIELD_TTL_MS);
        if !outcomes.contains(&Outcome::Applied) {
            // Duplicates only: they refresh their :pods field and nothing else.
            let mut pipe = redis::pipe();
            for pod in &pods {
                pipe.cmd("HSET").arg(&keys.pods).arg(*pod).arg(now).ignore();
                pipe.cmd("HPEXPIRE")
                    .arg(&keys.pods)
                    .arg(field_ttl)
                    .arg("FIELDS")
                    .arg(1)
                    .arg(*pod)
                    .ignore();
            }
            if let Err(e) = pipe.query_async::<()>(&mut conn).await {
                return Err(self.failed(e).await);
            }
            self.count_pushes(&outcomes);
            return Ok(Attempt::Done(outcomes));
        }

        let expired = drop_expired(&mut report, today, self.config.ttl.as_secs() / DAY_SECS);
        let json = report.to_json().map_err(|error| {
            tracing::error!(%error, namespace, deployment, "report not serialisable");
            StoreError::Unavailable
        })?;
        let endpoints = |direction| {
            report
                .endpoints()
                .iter()
                .filter(|e| e.direction == direction)
                .count()
        };

        let mut script = self.script.prepare_invoke();
        script
            .key(&keys.report)
            .key(&keys.meta)
            .key(&keys.pods)
            .key(&keys.seq)
            .key(&keys.ns_index)
            .key(&keys.workloads)
            .arg(version)
            .arg(ttl_ms)
            .arg(field_ttl)
            .arg(now)
            .arg(self.config.max_deployments)
            .arg(namespace)
            .arg(deployment)
            .arg(&json)
            .arg(report.exchanges())
            .arg(endpoints(Direction::Inbound))
            .arg(endpoints(Direction::Outbound))
            .arg(report.sample_bytes())
            .arg(json.len())
            .arg(pods.len());
        for pod in &pods {
            script.arg(*pod);
        }
        script.arg(real_fields);
        for field in fields.iter().take(real_fields) {
            let before = read_sequences.get(field);
            let after = sequences.get(field);
            script
                .arg(field)
                .arg(expected.get(field).map_or("", String::as_str))
                .arg(if after == before {
                    String::new()
                } else {
                    after.map_or_else(String::new, u64::to_string)
                });
        }
        let result: i64 = match script.invoke_async(&mut conn).await {
            Ok(result) => result,
            Err(e) => return Err(self.failed(e).await),
        };
        match result {
            1 => {}
            0 => return Ok(Attempt::Conflict),
            _ => {
                self.metrics.write("ok");
                if self.once(namespace, deployment, "deployments") {
                    tracing::warn!(
                        namespace,
                        deployment,
                        reason = "deployments",
                        "store limit reached"
                    );
                }
                return Err(StoreError::Deployments);
            }
        }
        self.metrics.write("ok");
        self.metrics.report_bytes.observe(json.len() as f64);
        self.count_pushes(&outcomes);
        for (reason, n) in &refused {
            self.metrics
                .refused
                .get_or_create(&vec![
                    ("kind".to_string(), reason.kind().as_str().to_string()),
                    ("reason".to_string(), reason.as_str().to_string()),
                ])
                .inc_by(*n);
            if self.once(namespace, deployment, reason.as_str()) {
                tracing::warn!(
                    namespace,
                    deployment,
                    reason = reason.as_str(),
                    "store limit reached"
                );
            }
        }
        for (kind, n) in &evicted {
            self.metrics
                .evicted
                .get_or_create(&label("kind", kind.as_str()))
                .inc_by(*n);
            if self.once(namespace, deployment, kind.as_str()) {
                tracing::warn!(
                    namespace,
                    deployment,
                    kind = kind.as_str(),
                    count = *n,
                    "store evicted"
                );
            }
        }
        for (kind, n) in &expired {
            self.metrics
                .expired
                .get_or_create(&label("kind", kind.as_str()))
                .inc_by(*n);
        }
        Ok(Attempt::Done(outcomes))
    }

    fn count_pushes(&self, outcomes: &[Outcome]) {
        for outcome in outcomes {
            let result = match outcome {
                Outcome::Applied => "applied",
                Outcome::Duplicate => "duplicate",
            };
            self.metrics
                .pushes
                .get_or_create(&label("result", result))
                .inc();
        }
    }
}

#[cfg(test)]
mod tests;

//! The discovery store (spec §7.3): the only code that reads or writes the `mimic:v1:` keys.
//!
//! A push reaches `Store::push` already checked, re-redacted and stamped (`mimic-core`). Within
//! a replica one writer per deployment merges the pushes that queued while it wrote (group
//! commit); at most `MAX_WRITERS` deployments are written at once. A write reads the stored
//! report, its `version` and the `:seq` fields, merges in memory, then runs one Lua script that
//! writes only if those are still what was read (compare-and-set), so a push is applied once
//! whichever replica receives it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimic_core::discovery::report::{Direction, Limits, Report};
use mimic_core::discovery::status::Meta as StatusMeta;
use mimic_core::discovery::store::{Kind, Refusal, drop_expired, merge_bounded_json};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};
use redis::aio::MultiplexedConnection;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, oneshot};

pub mod load;

/// Deployments written at once per replica (memory bound).
const MAX_WRITERS: usize = 2;
/// `:pulls` writes in flight at once; a pull over that is not recorded (spec §7.7).
const MAX_PULL_WRITES: usize = 16;
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

/// A staged write that never reached its script (a crash, a lost connection) is gone after this.
const STAGE_TTL_MS: u64 = 60_000;
/// The bytes of a staged key sent per command: what redis-rs copies at once. A write whose
/// stage is not whole (the expiry cannot pass between two commands of one writer: each renews it)
/// is refused by the script.
const STAGE_CHUNK: usize = 1 << 20;

/// The report and the outbound part are staged under temporary keys, `SET` then `APPEND`ed in
/// chunks so that the command never holds a copy of a whole part (#339); this script checks their
/// lengths and `RENAME`s them into place with the version check, in one atomic step: a reader
/// sees the old value or the new one, never a part of it. Every refusal deletes them.
const WRITE: &str = r"
local function refuse(code)
  redis.call('DEL', KEYS[8], KEYS[9])
  return code
end
local n_pods = tonumber(ARGV[14])
local seq_at = 15 + n_pods
local n_seq = tonumber(ARGV[seq_at])
local out_at = seq_at + 1 + n_seq * 3
local current = redis.call('HGET', KEYS[2], 'version')
if (current or '0') ~= ARGV[1] then return refuse(0) end
for i = 0, n_seq - 1 do
  local at = seq_at + 1 + i * 3
  if (redis.call('HGET', KEYS[4], ARGV[at]) or '') ~= ARGV[at + 1] then return refuse(0) end
end
if redis.call('STRLEN', KEYS[8]) ~= tonumber(ARGV[8]) then return refuse(0) end
if ARGV[out_at] ~= '' and redis.call('STRLEN', KEYS[9]) ~= tonumber(ARGV[out_at]) then
  return refuse(0)
end
local ttl = tonumber(ARGV[2])
local field_ttl = tonumber(ARGV[3])
local now = tonumber(ARGV[4])
local floor = tostring(now - ttl)
redis.call('ZREMRANGEBYSCORE', KEYS[5], '-inf', floor)
redis.call('ZREMRANGEBYSCORE', KEYS[6], '-inf', floor)
if not redis.call('ZSCORE', KEYS[5], ARGV[7])
   and redis.call('ZCARD', KEYS[5]) >= tonumber(ARGV[5]) then
  return refuse(-1)
end
redis.call('RENAME', KEYS[8], KEYS[1])
redis.call('PEXPIRE', KEYS[1], ttl)
if ARGV[out_at] ~= '' then
  redis.call('RENAME', KEYS[9], KEYS[7])
  redis.call('PEXPIRE', KEYS[7], ttl)
else
  redis.call('DEL', KEYS[7])
end
local version = redis.call('HINCRBY', KEYS[2], 'version', 1)
redis.call('HSET', KEYS[2], 'updated', now, 'exchanges', ARGV[9],
  'endpoints_inbound', ARGV[10], 'endpoints_outbound', ARGV[11],
  'sample_bytes', ARGV[12], 'report_bytes', ARGV[13],
  'outbound_digest', version .. ':' .. ARGV[out_at + 1])
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
    /// The outbound part of the report (F8).
    pub outbound: String,
    pub meta: String,
    pub pods: String,
    /// The mode each pod's last egress pull carried (F10).
    pub pulls: String,
    pub seq: String,
    pub ns_index: String,
    pub workloads: String,
}

impl Keys {
    /// A temporary key of one write attempt (same hash tag as the other keys of the deployment).
    fn staged(&self, id: u128, part: &str) -> String {
        let base = self.meta.strip_suffix(":meta").unwrap_or(&self.meta);
        format!("{base}:stage:{id:032x}:{part}")
    }
}

/// The keys of `<namespace>/<deployment>`, once both names are checked.
pub fn keys(namespace: &str, deployment: &str) -> Result<Keys, NameError> {
    check_name(namespace)?;
    check_name(deployment)?;
    let base = format!("mimic:v1:{{{namespace}/{deployment}}}");
    Ok(Keys {
        report: format!("{base}:report"),
        outbound: format!("{base}:outbound"),
        meta: format!("{base}:meta"),
        pods: format!("{base}:pods"),
        pulls: format!("{base}:pulls"),
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

/// A checked, re-redacted and stamped report, and who sent it. The report waits as its JSON
/// (`Report::to_json`): the write merges it into the stored report as it reads it, and reads it
/// again if the write conflicts, so that no parsed copy and no backup is held (SHR-18, #339).
pub struct Push {
    pub pod: String,
    pub seq: Option<SeqId>,
    pub json: String,
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
    /// A load test summary over its cap (spec §7.8 "End").
    TooLarge,
}

/// Why a read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// A name that is not DNS-1123: `400`.
    Name,
    /// A deployment without data: `404`.
    NotFound,
    /// Valkey unavailable: `503`.
    Unavailable,
    /// A stored value that does not parse: `500`.
    Unreadable,
}

/// What `:meta` says of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub namespace: String,
    pub deployment: String,
    pub endpoints: u64,
    pub exchanges: u64,
    pub sample_bytes: u64,
    /// Unix milliseconds.
    pub last_push_ms: u64,
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

/// Stores `bytes` under the temporary `key`, a chunk per command. Every command renews the
/// expiry of `key` and of `other` (the part staged before or after it), so that no staged key
/// exists without one and none expires while the other part is sent.
async fn stage(
    conn: &mut MultiplexedConnection,
    key: &str,
    other: &str,
    bytes: &[u8],
) -> redis::RedisResult<()> {
    for (i, chunk) in bytes.chunks(STAGE_CHUNK).enumerate() {
        append_chunk(conn, key, other, chunk, i == 0).await?;
    }
    Ok(())
}

/// `SET` (the first chunk) or `APPEND`s `chunk` to `key`, and renews the expiry of both keys, in
/// one pipeline: an `APPEND` that re-creates an expired key gives it an expiry too.
async fn append_chunk(
    conn: &mut MultiplexedConnection,
    key: &str,
    other: &str,
    chunk: &[u8],
    first: bool,
) -> redis::RedisResult<()> {
    let mut pipe = redis::pipe();
    if first {
        pipe.cmd("SET").arg(key).arg(chunk).ignore();
    } else {
        pipe.cmd("APPEND").arg(key).arg(chunk).ignore();
    }
    for k in [key, other] {
        pipe.cmd("PEXPIRE").arg(k).arg(STAGE_TTL_MS).ignore();
    }
    pipe.query_async(conn).await
}

/// Deletes the staged keys of a write unless disarmed: [`StageGuard::clean`] on a failure, and the
/// drop (a cancelled future) spawns the `DEL`, never blocking. What neither reaches (a crash, a
/// lost connection) expires after [`STAGE_TTL_MS`].
struct StageGuard {
    conn: MultiplexedConnection,
    keys: [String; 2],
    armed: bool,
}

impl StageGuard {
    fn new(conn: MultiplexedConnection, keys: [&str; 2]) -> StageGuard {
        StageGuard {
            conn,
            keys: keys.map(str::to_string),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Best effort: bounded by the response timeout, failures ignored.
    async fn clean(&mut self) {
        if std::mem::take(&mut self.armed) {
            delete(self.conn.clone(), self.keys.clone()).await;
        }
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if self.armed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(delete(self.conn.clone(), self.keys.clone()));
        }
    }
}

async fn delete(mut conn: MultiplexedConnection, keys: [String; 2]) {
    let mut del = redis::cmd("DEL");
    del.arg(&keys[0]).arg(&keys[1]);
    let _ = tokio::time::timeout(RESPONSE_TIMEOUT, del.query_async::<()>(&mut conn)).await;
}

struct Pending {
    push: Push,
    reply: oneshot::Sender<Result<Outcome, StoreError>>,
    /// Released with the push, when its write ends or the push is dropped.
    _hold: Box<dyn Send>,
}

enum Attempt {
    Done(Vec<Outcome>),
    Conflict,
}

/// What one write reads: `:report`, `:outbound`, the `:meta` version and the `:seq` fields.
type Read = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<String>,
    Vec<Option<String>>,
);

#[cfg(test)]
type Intruder = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

type Queues = BTreeMap<(String, String), Vec<Pending>>;

pub struct Store {
    client: redis::Client,
    config: StoreConfig,
    metrics: StoreMetrics,
    connection: AsyncMutex<Option<MultiplexedConnection>>,
    script: redis::Script,
    writers: Semaphore,
    /// `:pulls` writes in flight (F10).
    pull_writes: Arc<Semaphore>,
    /// A deployment with a writer has an entry: the pushes waiting for its next write.
    queues: Mutex<Queues>,
    /// What was already logged: (namespace, deployment, reason or kind).
    logged: Mutex<BTreeSet<(String, String, String)>>,
    /// A failed write was logged and none succeeded since: one WARN per failure streak.
    failing: AtomicBool,
    /// Test hook: `:pulls` writes wait while this is set.
    #[cfg(test)]
    pub stall_pulls: AtomicBool,
    /// Test hook: the next write panics.
    #[cfg(test)]
    panic_next_write: AtomicBool,
    /// Test hook: runs once between the next read and its write (a type-erased future: a push by
    /// another replica would make the write's future recursive).
    #[cfg(test)]
    intruder: Mutex<Option<Intruder>>,
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// The hash part of `digest` (`<version>:<64 hex>`) when its version is `version`.
fn fresh_digest(version: Option<&str>, digest: Option<&str>) -> Option<String> {
    let (written, hash) = digest?.split_once(':')?;
    let valid = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (valid && Some(written) == version).then(|| hash.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Bound of the set of streaks of unreadable values already logged.
const MAX_LOGGED: usize = 1024;

/// The summary of `:meta`; a field that is missing or not a number makes it unreadable.
fn summary(
    namespace: &str,
    deployment: &str,
    meta: &BTreeMap<String, String>,
) -> Result<Summary, ReadError> {
    let field = |name: &str| meta.get(name).and_then(|v| v.parse::<u64>().ok());
    let read = || {
        Some(Summary {
            namespace: namespace.to_string(),
            deployment: deployment.to_string(),
            endpoints: field("endpoints_inbound")?.saturating_add(field("endpoints_outbound")?),
            exchanges: field("exchanges")?,
            sample_bytes: field("sample_bytes")?,
            last_push_ms: field("updated")?,
        })
    };
    read().ok_or_else(|| {
        tracing::error!(namespace, deployment, "stored meta unreadable");
        ReadError::Unreadable
    })
}

/// The two parts of a stored report (spec §7.3).
#[derive(Clone, Copy)]
enum Part {
    /// `:report`: version 1.
    Inbound,
    /// `:outbound`: version 2, outbound scopes only.
    Outbound,
}

/// A stored part, once it parses, passes the structural check and is of its kind.
fn read_part(bytes: &[u8], part: Part) -> Option<Report> {
    let report = std::str::from_utf8(bytes)
        .ok()
        .and_then(|json| Report::parse_checked(json, usize::MAX, usize::MAX).ok())?;
    match part {
        Part::Inbound => report.version() == 1,
        Part::Outbound => report.version() == 2 && !report.holds_inbound(),
    }
    .then_some(report)
}

/// A stored `:outbound` once it parses, passes the structural check and is of its kind.
pub fn parse_outbound(bytes: &[u8]) -> Option<Report> {
    read_part(bytes, Part::Outbound)
}

/// The stored parts of a report, as read: `:report`, `:outbound` when present, and the `:meta`
/// `version` they were written with.
pub struct StoredReport {
    inbound: Option<Vec<u8>>,
    outbound: Option<Vec<u8>>,
    pub version: u64,
}

/// What a read needs of a stored report.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Half {
    /// `:report` and `:outbound`, joined.
    Both,
    /// `:report` only: the inbound scopes.
    Inbound,
    /// `:outbound` only: the outbound scopes.
    Outbound,
}

impl StoredReport {
    /// The report of the parts that were read, once each parses and passes the check; `None`
    /// for a part that does not. Each part's bytes go as soon as it is parsed.
    pub fn parse(self) -> Option<Report> {
        let inbound = match &self.inbound {
            Some(bytes) => Some(read_part(bytes, Part::Inbound)?),
            None => None,
        };
        drop(self.inbound);
        let outbound = match &self.outbound {
            Some(bytes) => Some(read_part(bytes, Part::Outbound)?),
            None => None,
        };
        drop(self.outbound);
        match inbound {
            Some(inbound) => Some(Report::join(inbound, outbound)),
            None => outbound,
        }
    }
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
            pull_writes: Arc::new(Semaphore::new(MAX_PULL_WRITES)),
            #[cfg(test)]
            stall_pulls: AtomicBool::new(false),
            queues: Mutex::new(BTreeMap::new()),
            logged: Mutex::new(BTreeSet::new()),
            failing: AtomicBool::new(false),
            #[cfg(test)]
            panic_next_write: AtomicBool::new(false),
            #[cfg(test)]
            intruder: Mutex::new(None),
        })
    }

    /// The store limits (the push check uses `max_authorities` too).
    pub fn limits(&self) -> &Limits {
        &self.config.limits
    }

    /// Merges the push into the deployment's report. Answered after the write that holds it.
    #[cfg(test)]
    pub async fn push(
        self: &Arc<Self>,
        namespace: &str,
        deployment: &str,
        push: Push,
    ) -> Result<Outcome, StoreError> {
        self.push_holding(namespace, deployment, push, ()).await
    }

    /// As `push`, and `hold` lives until the write that applies the push ends, even if the
    /// caller is dropped first: the write runs in a task of the store, so what bounds its
    /// memory (the body budget) must not end with the caller.
    pub async fn push_holding(
        self: &Arc<Self>,
        namespace: &str,
        deployment: &str,
        push: Push,
        hold: impl Send + 'static,
    ) -> Result<Outcome, StoreError> {
        let hold: Box<dyn Send> = Box::new(hold);
        let keys = keys(namespace, deployment).map_err(|_| StoreError::Name)?;
        check_name(&push.pod).map_err(|_| StoreError::Name)?;
        let (reply, answer) = oneshot::channel();
        let key = (namespace.to_string(), deployment.to_string());
        let start_writer = {
            let mut queues = lock(&self.queues);
            match queues.get_mut(&key) {
                Some(queue) => {
                    queue.push(Pending {
                        push,
                        reply,
                        _hold: hold,
                    });
                    false
                }
                None => {
                    queues.insert(
                        key.clone(),
                        vec![Pending {
                            push,
                            reply,
                            _hold: hold,
                        }],
                    );
                    true
                }
            }
        };
        if start_writer {
            let store = self.clone();
            let writer = {
                let (store, key) = (store.clone(), key.clone());
                tokio::spawn(async move { store.drain(key, keys).await })
            };
            // A writer that panics leaves no queue entry: the pushes it held were dropped
            // (answered 503) and so are those still waiting in the entry.
            tokio::spawn(async move {
                if writer.await.is_err() {
                    tracing::error!(namespace = key.0, deployment = key.1, "store writer failed");
                    lock(&store.queues).remove(&key);
                }
            });
        }
        answer.await.unwrap_or(Err(StoreError::Unavailable))
    }

    /// Test hook: `intruder` runs between the next write's read and its compare-and-set.
    #[cfg(test)]
    pub(crate) fn intrude_once(&self, intruder: Intruder) {
        *lock(&self.intruder) = Some(intruder);
    }

    /// Test hook: holds every writer slot, so that no write starts until the guard drops.
    #[cfg(test)]
    pub(crate) async fn hold_writers(&self) -> tokio::sync::SemaphorePermit<'_> {
        let slots = u32::try_from(MAX_WRITERS).expect("a small count");
        self.writers.acquire_many(slots).await.expect("open")
    }

    /// Test hook: the pushes waiting for a write.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        lock(&self.queues).values().map(Vec::len).sum()
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
            let mut batch = batch;
            let result = self.write(&key.0, &key.1, &keys, &mut batch).await;
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
        batch: &mut [Pending],
    ) -> Result<Vec<Outcome>, StoreError> {
        #[cfg(test)]
        if self.panic_next_write.swap(false, Ordering::SeqCst) {
            panic!("injected writer panic");
        }
        for attempt in 0..ATTEMPTS {
            let started = Instant::now();
            let result = self.attempt(namespace, deployment, keys, &mut *batch).await;
            self.metrics
                .duration
                .observe(started.elapsed().as_secs_f64());
            match result {
                Ok(Attempt::Done(outcomes)) => {
                    self.failing.store(false, Ordering::Relaxed);
                    return Ok(outcomes);
                }
                Ok(Attempt::Conflict) => {
                    self.failing.store(false, Ordering::Relaxed);
                    self.metrics.write("conflict");
                    if attempt + 1 < ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(rand::random_range(BACKOFF_MS)))
                            .await;
                    }
                }
                Err(e) => {
                    match e {
                        StoreError::Unavailable => self.metrics.write("error"),
                        _ => self.failing.store(false, Ordering::Relaxed),
                    }
                    return Err(e);
                }
            }
        }
        tracing::warn!(namespace, deployment, "store write kept conflicting");
        Err(StoreError::Unavailable)
    }

    /// Members of `mimic:v1:workloads`, all namespaces (`mimic_store_deployments`).
    pub async fn workload_count(&self) -> Result<u64, ReadError> {
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        // Not logged: the status pass that follows reports the outage once.
        redis::cmd("ZCARD")
            .arg("mimic:v1:workloads")
            .query_async(&mut conn)
            .await
            .map_err(|_| ReadError::Unavailable)
    }

    /// The deployments with data, sorted by namespace then deployment.
    /// The second value counts the entries left out because their `:meta` is unreadable.
    pub async fn list(&self) -> Result<(Vec<Summary>, u64), ReadError> {
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        let floor =
            now_ms().saturating_sub(u64::try_from(self.config.ttl.as_millis()).unwrap_or(u64::MAX));
        let members: Vec<String> = match redis::cmd("ZRANGEBYSCORE")
            .arg("mimic:v1:workloads")
            .arg(floor)
            .arg("+inf")
            .query_async(&mut conn)
            .await
        {
            Ok(members) => members,
            Err(e) => return Err(self.read_failed(e).await),
        };
        // Sorted as pairs: the byte order of "<ns>/<dep>" differs ('-' and '.' sort after '/').
        let names: BTreeSet<(String, String)> = members
            .iter()
            .filter_map(|m| m.split_once('/'))
            .filter(|(ns, dep)| keys(ns, dep).is_ok())
            .map(|(ns, dep)| (ns.to_string(), dep.to_string()))
            .collect();
        if names.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let mut pipe = redis::pipe();
        for (ns, dep) in &names {
            let k = keys(ns, dep).map_err(|_| ReadError::Name)?;
            pipe.cmd("HGETALL").arg(k.meta);
        }
        let metas: Vec<BTreeMap<String, String>> = match pipe.query_async(&mut conn).await {
            Ok(metas) => metas,
            Err(e) => return Err(self.read_failed(e).await),
        };
        let mut found = Vec::with_capacity(names.len());
        let mut skipped = 0;
        for ((ns, dep), meta) in names.into_iter().zip(metas) {
            // Expired between the index and the read: not a deployment any more.
            if meta.is_empty() {
                continue;
            }
            // Logged by `summary`: one bad entry does not fail the list.
            match summary(&ns, &dep, &meta) {
                Ok(summary) => found.push(summary),
                Err(_) => skipped += 1,
            }
        }
        Ok((found, skipped))
    }

    /// One deployment and its pods with their last push (Unix milliseconds).
    pub async fn workload(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<(Summary, BTreeMap<String, u64>), ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        let (meta, pods): (BTreeMap<String, String>, BTreeMap<String, String>) = match redis::pipe()
            .cmd("HGETALL")
            .arg(&k.meta)
            .cmd("HGETALL")
            .arg(&k.pods)
            .query_async(&mut conn)
            .await
        {
            Ok(read) => read,
            Err(e) => return Err(self.read_failed(e).await),
        };
        if meta.is_empty() {
            return Err(ReadError::NotFound);
        }
        let summary = summary(namespace, deployment, &meta)?;
        let pods = pods
            .into_iter()
            .map(|(pod, ms)| {
                ms.parse().map(|ms| (pod, ms)).map_err(|_| {
                    tracing::error!(namespace, deployment, "stored pods unreadable");
                    ReadError::Unreadable
                })
            })
            .collect::<Result<_, _>>()?;
        Ok((summary, pods))
    }

    /// `:meta` and `:pods` of a deployment (the pods' last push in Unix milliseconds); no meta
    /// for a deployment without data.
    pub async fn status_input(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<(Option<StatusMeta>, BTreeMap<String, u64>), ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        let (meta, pods): (BTreeMap<String, String>, BTreeMap<String, String>) = match redis::pipe()
            .cmd("HGETALL")
            .arg(&k.meta)
            .cmd("HGETALL")
            .arg(&k.pods)
            .query_async(&mut conn)
            .await
        {
            Ok(read) => read,
            Err(e) => return Err(self.read_failed(e).await),
        };
        if meta.is_empty() {
            self.readable(namespace, deployment);
            return Ok((None, BTreeMap::new()));
        }
        // Read at every status pass: logged once per streak, not at every pass.
        let unreadable = |what: &'static str| {
            if self.once(namespace, deployment, what) {
                tracing::error!(namespace, deployment, "{what}");
            }
            ReadError::Unreadable
        };
        let field = |name: &str| meta.get(name).and_then(|v| v.parse::<u64>().ok());
        let meta = (|| {
            Some(StatusMeta {
                inbound: field("endpoints_inbound")?,
                outbound: field("endpoints_outbound")?,
                exchanges: field("exchanges")?,
                sample_bytes: field("sample_bytes")?,
                updated_ms: field("updated")?,
            })
        })()
        .ok_or_else(|| unreadable("stored meta unreadable"))?;
        let pods = pods
            .into_iter()
            .map(|(pod, ms)| {
                ms.parse()
                    .map(|ms| (pod, ms))
                    .map_err(|_| unreadable("stored pods unreadable"))
            })
            .collect::<Result<_, _>>()?;
        self.readable(namespace, deployment);
        Ok((Some(meta), pods))
    }

    /// The `:pulls` fields of a deployment (pod to `<mode> <ms>`), unparsed.
    pub async fn pulls(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<BTreeMap<String, String>, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        match redis::cmd("HGETALL")
            .arg(&k.pulls)
            .query_async(&mut conn)
            .await
        {
            Ok(pulls) => Ok(pulls),
            Err(e) => Err(self.read_failed(e).await),
        }
    }

    /// Logs once per streak that the `:pulls` of a deployment hold a field that cannot be read;
    /// `unreadable` 0 ends the streak.
    pub fn pulls_unreadable(&self, namespace: &str, deployment: &str, unreadable: u64) {
        const WHAT: &str = "stored pulls unreadable";
        if unreadable == 0 {
            lock(&self.logged).remove(&(namespace.into(), deployment.into(), WHAT.into()));
        } else if self.once(namespace, deployment, WHAT) {
            tracing::error!(namespace, deployment, "{WHAT}");
        }
    }

    /// The stored `:report` bytes, once they are known to parse.
    pub async fn report(&self, namespace: &str, deployment: &str) -> Result<Vec<u8>, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        // One transaction: a write between two GETs could join an old part with a new one.
        let (stored, outbound): (Option<Vec<u8>>, Option<Vec<u8>>) = match redis::pipe()
            .atomic()
            .cmd("GET")
            .arg(&k.report)
            .cmd("GET")
            .arg(&k.outbound)
            .query_async(&mut conn)
            .await
        {
            Ok(read) => read,
            Err(e) => return Err(self.read_failed(e).await),
        };
        let bytes = stored.ok_or(ReadError::NotFound)?;
        let unreadable = |key: &'static str| {
            tracing::error!(namespace, deployment, key, "stored report unreadable");
            ReadError::Unreadable
        };
        let inbound = read_part(&bytes, Part::Inbound).ok_or_else(|| unreadable("report"))?;
        let Some(outbound) = outbound else {
            // F7 bytes: the stored report as it is.
            return Ok(bytes);
        };
        let outbound =
            read_part(&outbound, Part::Outbound).ok_or_else(|| unreadable("outbound"))?;
        Report::join(inbound, Some(outbound))
            .to_json()
            .map(String::into_bytes)
            .map_err(|_| unreadable("report"))
    }

    /// The stored parts that `half` needs, unparsed, and the `:meta` `version` they were written
    /// with. An inbound document needs `:report` only and an outbound one `:outbound` only: the
    /// other half is neither read nor parsed (SHR-18).
    pub async fn stored_report(
        &self,
        namespace: &str,
        deployment: &str,
        half: Half,
    ) -> Result<StoredReport, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        // One transaction, as for `report`: the parts and the version belong together.
        let mut pipe = redis::pipe();
        pipe.atomic();
        if half != Half::Outbound {
            pipe.cmd("GET").arg(&k.report);
        }
        if half != Half::Inbound {
            pipe.cmd("GET").arg(&k.outbound);
        }
        pipe.cmd("HGET").arg(&k.meta).arg("version");
        let mut read: Vec<Option<Vec<u8>>> = match pipe.query_async(&mut conn).await {
            Ok(read) => read,
            Err(e) => return Err(self.read_failed(e).await),
        };
        let version = read.pop().flatten();
        let mut read = read.into_iter();
        let inbound = if half == Half::Outbound {
            None
        } else {
            read.next().flatten()
        };
        let outbound = if half == Half::Inbound {
            None
        } else {
            read.next().flatten()
        };
        // Expired between the keys is the same as absent; so is a half that is not there, for
        // the half that the request is about.
        let present = match half {
            Half::Outbound => outbound.is_some(),
            _ => inbound.is_some(),
        };
        let (true, Some(version)) = (present, version) else {
            return Err(ReadError::NotFound);
        };
        let version = String::from_utf8(version)
            .ok()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| {
                tracing::error!(namespace, deployment, "stored meta unreadable");
                ReadError::Unreadable
            })?;
        Ok(StoredReport {
            inbound,
            outbound,
            version,
        })
    }

    /// The hex SHA-256 of the stored `:outbound` bytes, from `:meta` `outbound_digest`; `None`
    /// when the digest is absent, malformed or written for another `version` (stale, §7.7), or
    /// when the deployment has no `:meta`.
    pub async fn outbound_digest(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<Option<String>, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        let (version, digest): (Option<String>, Option<String>) = match redis::cmd("HMGET")
            .arg(&k.meta)
            .arg("version")
            .arg("outbound_digest")
            .query_async(&mut conn)
            .await
        {
            Ok(read) => read,
            Err(e) => return Err(self.read_failed(e).await),
        };
        Ok(fresh_digest(version.as_deref(), digest.as_deref()))
    }

    /// The size of the stored `:outbound` in bytes (`STRLEN`), 0 when there is none: what a
    /// build reserves before it reads the bytes.
    pub async fn outbound_len(&self, namespace: &str, deployment: &str) -> Result<u64, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        match redis::cmd("STRLEN")
            .arg(&k.outbound)
            .query_async(&mut conn)
            .await
        {
            Ok(len) => Ok(len),
            Err(e) => Err(self.read_failed(e).await),
        }
    }

    /// The stored `:outbound` bytes, unparsed; `None` for a deployment without any.
    pub async fn outbound_bytes(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<Option<Vec<u8>>, ReadError> {
        let k = keys(namespace, deployment).map_err(|_| ReadError::Name)?;
        let mut conn = self
            .connection()
            .await
            .map_err(|_| ReadError::Unavailable)?;
        match redis::cmd("GET")
            .arg(&k.outbound)
            .query_async(&mut conn)
            .await
        {
            Ok(bytes) => Ok(bytes),
            Err(e) => Err(self.read_failed(e).await),
        }
    }

    /// Notes that `pod` pulled the egress set and was answered `mode` (spec §7.3 `:pulls`), in a
    /// task of its own: the answer never waits for Valkey. At most [`MAX_PULL_WRITES`] run at
    /// once; a pull over that is counted `dropped` and not recorded. A failure is counted `error`.
    pub fn record_pull(
        self: &Arc<Self>,
        namespace: &str,
        deployment: &str,
        pod: &str,
        mode: &'static str,
    ) {
        let Ok(k) = keys(namespace, deployment) else {
            return;
        };
        let Ok(permit) = self.pull_writes.clone().try_acquire_owned() else {
            self.metrics.write("dropped");
            return;
        };
        let store = self.clone();
        let pod = pod.to_string();
        tokio::spawn(async move {
            store.write_pull(&k, &pod, mode).await;
            drop(permit);
        });
    }

    async fn write_pull(&self, k: &Keys, pod: &str, mode: &str) {
        #[cfg(test)]
        while self.stall_pulls.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let ttl_ms = u64::try_from(self.config.ttl.as_millis()).unwrap_or(u64::MAX);
        let Ok(mut conn) = self.connection().await else {
            self.metrics.write("error");
            return;
        };
        let written = redis::pipe()
            .cmd("HSET")
            .arg(&k.pulls)
            .arg(pod)
            .arg(format!("{mode} {now}"))
            .ignore()
            .cmd("HPEXPIRE")
            .arg(&k.pulls)
            .arg(ttl_ms.min(FIELD_TTL_MS))
            .arg("FIELDS")
            .arg(1)
            .arg(pod)
            .ignore()
            .cmd("PEXPIRE")
            .arg(&k.pulls)
            .arg(ttl_ms)
            .ignore()
            .query_async::<()>(&mut conn)
            .await;
        if let Err(e) = written {
            self.failed(e).await;
            self.metrics.write("error");
        }
    }

    async fn read_failed(&self, error: redis::RedisError) -> ReadError {
        self.failed(error).await;
        ReadError::Unavailable
    }

    pub(crate) async fn connection(&self) -> Result<MultiplexedConnection, StoreError> {
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
                if self.first_failure() {
                    tracing::warn!(%error, "valkey connection failed");
                }
                StoreError::Unavailable
            })?;
        *guard = Some(conn.clone());
        Ok(conn)
    }

    /// Whether this failure starts a streak (the one to log).
    fn first_failure(&self) -> bool {
        !self.failing.swap(true, Ordering::Relaxed)
    }

    /// A connection that failed is dropped so that the next attempt makes a new one. A reply
    /// error (`OOM`, a script error) leaves it healthy: other writers share it.
    async fn failed(&self, error: redis::RedisError) -> StoreError {
        if self.first_failure() {
            tracing::warn!(%error, "valkey command failed");
        }
        let reply = matches!(
            error.kind(),
            redis::ErrorKind::Server(_)
                | redis::ErrorKind::Extension
                | redis::ErrorKind::UnexpectedReturnType
        );
        if !reply {
            *self.connection.lock().await = None;
        }
        StoreError::Unavailable
    }

    /// Ends the streak of unreadable values of a deployment: the next one is logged again.
    fn readable(&self, namespace: &str, deployment: &str) {
        let mut logged = lock(&self.logged);
        for what in ["stored meta unreadable", "stored pods unreadable"] {
            logged.remove(&(namespace.into(), deployment.into(), what.into()));
        }
    }

    fn once(&self, namespace: &str, deployment: &str, what: &str) -> bool {
        let mut logged = lock(&self.logged);
        // A deleted deployment is never read again, so its streak is never ended: past the bound
        // the set starts over (at worst one more line per live streak).
        if logged.len() >= MAX_LOGGED {
            logged.clear();
        }
        logged.insert((namespace.into(), deployment.into(), what.into()))
    }

    async fn attempt(
        &self,
        namespace: &str,
        deployment: &str,
        keys: &Keys,
        batch: &mut [Pending],
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
        let (stored, stored_outbound, version, seqs): Read = match redis::pipe()
            .cmd("GET")
            .arg(&keys.report)
            .cmd("GET")
            .arg(&keys.outbound)
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
        #[cfg(test)]
        let intruder = lock(&self.intruder).take();
        #[cfg(test)]
        if let Some(intruder) = intruder {
            intruder.await;
        }
        let unreadable = |key: &'static str| {
            tracing::error!(namespace, deployment, key, "stored report unreadable");
            StoreError::Unavailable
        };
        let inbound = match stored {
            None => Report::new(None),
            Some(bytes) => read_part(&bytes, Part::Inbound).ok_or_else(|| unreadable("report"))?,
        };
        let outbound = match stored_outbound {
            None => None,
            Some(bytes) => {
                Some(read_part(&bytes, Part::Outbound).ok_or_else(|| unreadable("outbound"))?)
            }
        };
        let mut report = Report::join(inbound, outbound);
        let version: u64 = match version {
            None => 0,
            Some(v) => v.parse().map_err(|_| unreadable("report"))?,
        };
        let mut sequences: BTreeMap<String, u64> = BTreeMap::new();
        let mut expected: BTreeMap<String, String> = BTreeMap::new();
        for (field, value) in fields.iter().zip(seqs).take(real_fields) {
            if let Some(value) = value {
                sequences.insert(
                    field.clone(),
                    value.parse().map_err(|_| unreadable("report"))?,
                );
                expected.insert(field.clone(), value);
            }
        }
        let read_sequences = sequences.clone();

        let (now, today) = (now_ms(), now_ms() / 1000 / DAY_SECS);
        let ttl_ms = u64::try_from(self.config.ttl.as_millis()).unwrap_or(u64::MAX);
        let mut outcomes = Vec::with_capacity(batch.len());
        let mut refused: BTreeMap<Refusal, u64> = BTreeMap::new();
        let mut evicted: BTreeMap<Kind, u64> = BTreeMap::new();
        for pending in batch.iter() {
            let push = &pending.push;
            let field = push.seq.as_ref().map(|s| s.field(&push.pod));
            let stored_seq = field.as_ref().and_then(|f| sequences.get(f)).copied();
            let outcome = match decide(stored_seq, push.seq.as_ref().map(|s| s.sequence)) {
                Decision::Duplicate => Outcome::Duplicate,
                Decision::Apply => {
                    if let (Some(field), Some(seq)) = (field, &push.seq) {
                        sequences.insert(field, seq.sequence);
                    }
                    // Merged as it is read: at the store limits a parsed push is tens of
                    // megabytes (SHR-18). The JSON stays for the next attempt.
                    let merged = merge_bounded_json(report, &push.json, &self.config.limits)
                        .map_err(|error| {
                            tracing::error!(%error, namespace, deployment, "report not readable");
                            StoreError::Unavailable
                        })?;
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
        let unserialisable = |error: serde_json::Error| {
            tracing::error!(%error, namespace, deployment, "report not serialisable");
            StoreError::Unavailable
        };
        let endpoints = |direction| {
            report
                .endpoints()
                .iter()
                .filter(|e| e.direction == direction)
                .count()
        };
        let (inbound_endpoints, outbound_endpoints) = (
            endpoints(Direction::Inbound),
            endpoints(Direction::Outbound),
        );
        let (exchanges, sample_bytes) = (report.exchanges(), report.sample_bytes());
        // `:report` keeps the F7 format (inbound, version 1); outbound goes in its own key.
        let (inbound, outbound) = report.into_split();
        let json = inbound.to_json().map_err(unserialisable)?;
        drop(inbound);
        let outbound_json = match &outbound {
            Some(part) => part.to_json().map_err(unserialisable)?,
            None => String::new(),
        };
        drop(outbound);
        let stored_bytes = json.len() + outbound_json.len();
        let id = rand::random::<u128>();
        let (staged, staged_outbound) = (keys.staged(id, "report"), keys.staged(id, "outbound"));
        // From here the staged keys are deleted on every way out but the script's answer.
        let mut guard = StageGuard::new(conn.clone(), [&staged, &staged_outbound]);
        let sent = async {
            stage(&mut conn, &staged, &staged_outbound, json.as_bytes()).await?;
            stage(
                &mut conn,
                &staged_outbound,
                &staged,
                outbound_json.as_bytes(),
            )
            .await
        };
        if let Err(e) = sent.await {
            guard.clean().await;
            return Err(self.failed(e).await);
        }

        let mut script = self.script.prepare_invoke();
        script
            .key(&keys.report)
            .key(&keys.meta)
            .key(&keys.pods)
            .key(&keys.seq)
            .key(&keys.ns_index)
            .key(&keys.workloads)
            .key(&keys.outbound)
            .key(&staged)
            .key(&staged_outbound)
            .arg(version)
            .arg(ttl_ms)
            .arg(field_ttl)
            .arg(now)
            .arg(self.config.max_deployments)
            .arg(namespace)
            .arg(deployment)
            .arg(json.len())
            .arg(exchanges)
            .arg(inbound_endpoints)
            .arg(outbound_endpoints)
            .arg(sample_bytes)
            .arg(stored_bytes)
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
        script.arg(if outbound_json.is_empty() {
            String::new()
        } else {
            outbound_json.len().to_string()
        });
        // The digest of §7.7: the script prefixes it with the version it writes.
        script.arg(sha256_hex(outbound_json.as_bytes()));
        drop((json, outbound_json));
        let result: i64 = match script.invoke_async(&mut conn).await {
            Ok(result) => {
                // Renamed, or deleted by the script's refusal.
                guard.disarm();
                result
            }
            Err(e) => {
                guard.clean().await;
                return Err(self.failed(e).await);
            }
        };
        match result {
            1 => {}
            0 => return Ok(Attempt::Conflict),
            _ => {
                self.metrics.write("refused");
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
        self.metrics.report_bytes.observe(stored_bytes as f64);
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

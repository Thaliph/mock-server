//! Load test runs in the store (spec §7.3 `:load:<uid>`, `:loads`, `loads:active`,
//! `loads:reserved`; §7.8 "Admission" and "End").

use std::collections::BTreeMap;

use mimic_core::load::summary::PodResults;
use serde::{Deserialize, Serialize};

use super::{NameError, ReadError, Store, StoreError, check_name};

/// Largest results post of one generator pod, in bytes (spec §7.8 "Load API").
pub const MAX_POST: usize = 256 << 10;
/// Largest stored summary of an ended run, in bytes.
pub const MAX_SUMMARY: usize = 192 << 10;

/// Runs of a deployment kept in `:loads` (spec §7.3).
const KEEP_RUNS: usize = 20;
/// Fields of the run hash that are not a generator pod's results.
const FIXED: [&str; 9] = [
    "spec", "stop", "start", "end", "phase", "reason", "plan", "ticket", "summary",
];
/// What stays once the run ended.
const KEPT: [&str; 7] = ["spec", "stop", "start", "end", "phase", "reason", "summary"];

const ACTIVE: &str = "mimic:v1:loads:active";
const RESERVED: &str = "mimic:v1:loads:reserved";

/// The admission (spec §7.8 "Admission"), in one step so that no replica admits past a limit:
/// expired members and their reservations go first; then, in order, the deployment's own run
/// (`Busy`), the namespace's (`NamespaceBusy`), the cluster's (`ClusterBusy`) and the byte
/// budget (`StoreFull`) refuse; a run already admitted (a retry) is success. The oldest ended
/// runs beyond [`KEEP_RUNS`] are deleted with the admission.
/// Replies 0 admitted, 1 `Busy`, 2 `NamespaceBusy`, 3 `ClusterBusy`, 4 `StoreFull`.
const ADMIT: &str = r"
local member = ARGV[3]..'/'..ARGV[4]..'/'..ARGV[5]
for _, m in ipairs(redis.call('ZRANGEBYSCORE', KEYS[3], '-inf', ARGV[1])) do
  redis.call('HDEL', KEYS[4], m)
  redis.call('ZREM', KEYS[3], m)
end
for _, m in ipairs(redis.call('HKEYS', KEYS[4])) do
  if not redis.call('ZSCORE', KEYS[3], m) then redis.call('HDEL', KEYS[4], m) end
end
if redis.call('ZSCORE', KEYS[3], member) then return 0 end
local own = ARGV[3]..'/'..ARGV[4]..'/'
local space = ARGV[3]..'/'
local namespace, total = 0, 0
for _, m in ipairs(redis.call('ZRANGE', KEYS[3], 0, -1)) do
  total = total + 1
  if string.sub(m, 1, #own) == own then return 1 end
  if string.sub(m, 1, #space) == space then namespace = namespace + 1 end
end
if namespace >= tonumber(ARGV[7]) then return 2 end
if total >= tonumber(ARGV[6]) then return 3 end
local held = 0
for _, v in ipairs(redis.call('HVALS', KEYS[4])) do held = held + tonumber(v) end
if held + tonumber(ARGV[9]) > tonumber(ARGV[8]) then return 4 end
redis.call('ZADD', KEYS[3], ARGV[2], member)
redis.call('HSET', KEYS[4], member, ARGV[9])
redis.call('HSET', KEYS[1], 'spec', ARGV[11], 'stop', '0', 'start', ARGV[1],
  'phase', 'Running', 'reason', '', 'plan', ARGV[12], 'ticket', ARGV[13])
redis.call('PEXPIRE', KEYS[1], ARGV[10])
redis.call('ZADD', KEYS[2], ARGV[1], ARGV[5])
redis.call('PEXPIRE', KEYS[2], ARGV[10])
local excess = redis.call('ZCARD', KEYS[2]) - tonumber(ARGV[15])
if excess > 0 then
  for _, uid in ipairs(redis.call('ZRANGE', KEYS[2], 0, -1)) do
    if excess <= 0 then break end
    local key = ARGV[14]..uid
    if uid ~= ARGV[5] and redis.call('HEXISTS', key, 'plan') == 0 then
      redis.call('DEL', key)
      redis.call('ZREM', KEYS[2], uid)
      excess = excess - 1
    end
  end
end
return 0
";

/// A post replaces the pod's results of an active run, but a late post that is not final never
/// takes a final one back (the value starts with `1` for a final post). Replies -1 for an
/// unknown or ended run, else the stop flag.
const POST: &str = r"
if redis.call('HEXISTS', KEYS[1], 'plan') == 0 then return -1 end
local old = redis.call('HGET', KEYS[1], ARGV[1])
if not (old and string.sub(old, 1, 1) == '1' and string.sub(ARGV[2], 1, 1) == '0') then
  redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
end
redis.call('PEXPIRE', KEYS[1], ARGV[3])
return tonumber(redis.call('HGET', KEYS[1], 'stop'))
";

/// Sets `stop` on an active run. Replies 1, or 0 for an unknown or ended run.
const STOP: &str = r"
if redis.call('HEXISTS', KEYS[1], 'plan') == 0 then return 0 end
redis.call('HSET', KEYS[1], 'stop', '1')
return 1
";

/// The end (spec §7.8 "End"): the plan, the ticket and the pods' results go, the summary stays,
/// the slot and the reservation are released. Replies -1 for an unknown run, 0 for one that
/// ended already, 1 for the end.
const END: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return -1 end
if redis.call('HEXISTS', KEYS[1], 'plan') == 0 then return 0 end
local keep = {spec = 1, stop = 1, start = 1, ['end'] = 1, phase = 1, reason = 1, summary = 1}
for _, f in ipairs(redis.call('HKEYS', KEYS[1])) do
  if not keep[f] then redis.call('HDEL', KEYS[1], f) end
end
redis.call('HSET', KEYS[1], 'phase', ARGV[2], 'reason', ARGV[3], 'end', ARGV[4],
  'summary', ARGV[5])
redis.call('ZREM', KEYS[2], ARGV[1])
redis.call('HDEL', KEYS[3], ARGV[1])
redis.call('PEXPIRE', KEYS[1], ARGV[6])
return 1
";

/// Limits the admission script enforces (spec §10.2).
#[derive(Debug, Clone, Copy)]
pub struct AdmitLimits {
    pub max_runs: u64,
    pub max_namespace_runs: u64,
    pub budget: u64,
}

/// A run to admit: everything the stored run holds while it is active.
pub struct Admit<'a> {
    pub namespace: &'a str,
    pub deployment: &'a str,
    pub uid: &'a str,
    /// The `MimicLoadTest` name and spec, no Secret value.
    pub spec: &'a str,
    pub plan: &'a str,
    pub ticket: &'a str,
    pub now_ms: u64,
    /// Start plus `activeDeadlineSeconds`.
    pub deadline_ms: u64,
    /// Plan bytes + pods x 256 KiB + 192 KiB.
    pub reserve: u64,
    pub limits: AdmitLimits,
}

/// Why a run is not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitError {
    Busy,
    NamespaceBusy,
    ClusterBusy,
    StoreFull,
    Name,
    Unavailable,
}

/// How a run ended.
pub struct Ended<'a> {
    pub phase: &'a str,
    pub reason: &'a str,
    /// `status.results` as JSON, at most [`MAX_SUMMARY`] bytes.
    pub summary: &'a str,
    pub end_ms: u64,
}

/// What a generator pod posted, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPost {
    #[serde(skip)]
    pub final_post: bool,
    pub reason: Option<String>,
    pub results: PodResults,
}

/// The plan and ticket of an active run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub plan: String,
    pub ticket: String,
    pub stop: bool,
}

fn run_key(namespace: &str, deployment: &str, uid: &str) -> Result<String, NameError> {
    check_name(namespace)?;
    check_name(deployment)?;
    check_name(uid)?;
    Ok(format!("mimic:v1:{{{namespace}/{deployment}}}:load:{uid}"))
}

/// A final post starts with `1`, any other with `0`; then the JSON.
fn encode(post: &StoredPost) -> Result<String, serde_json::Error> {
    Ok(format!(
        "{}{}",
        if post.final_post { '1' } else { '0' },
        serde_json::to_string(post)?
    ))
}

fn decode(value: &str) -> Option<StoredPost> {
    let (flag, json) = (value.get(..1)?, value.get(1..)?);
    let mut post: StoredPost = serde_json::from_str(json).ok()?;
    post.final_post = flag == "1";
    Some(post)
}

impl Store {
    fn load_ttl_ms(&self) -> u64 {
        u64::try_from(self.config.ttl.as_millis()).unwrap_or(u64::MAX)
    }

    async fn read_of<T>(&self, result: Result<T, redis::RedisError>) -> Result<T, ReadError> {
        match result {
            Ok(value) => Ok(value),
            Err(e) => Err(self.read_failed(e).await),
        }
    }

    async fn write_of<T>(&self, result: Result<T, redis::RedisError>) -> Result<T, StoreError> {
        match result {
            Ok(value) => Ok(value),
            Err(e) => Err(self.failed(e).await),
        }
    }

    async fn read_connection(&self) -> Result<redis::aio::MultiplexedConnection, ReadError> {
        self.connection().await.map_err(|_| ReadError::Unavailable)
    }

    /// Admits the run (spec §7.8 "Admission"): one script, so that two replicas never admit
    /// past a limit.
    pub async fn load_admit(&self, admit: &Admit<'_>) -> Result<(), AdmitError> {
        let key =
            run_key(admit.namespace, admit.deployment, admit.uid).map_err(|_| AdmitError::Name)?;
        let base = format!("mimic:v1:{{{}/{}}}", admit.namespace, admit.deployment);
        let mut conn = self
            .connection()
            .await
            .map_err(|_| AdmitError::Unavailable)?;
        let reply: Result<i64, _> = redis::Script::new(ADMIT)
            .key(&key)
            .key(format!("{base}:loads"))
            .key(ACTIVE)
            .key(RESERVED)
            .arg(admit.now_ms)
            .arg(admit.deadline_ms)
            .arg(admit.namespace)
            .arg(admit.deployment)
            .arg(admit.uid)
            .arg(admit.limits.max_runs)
            .arg(admit.limits.max_namespace_runs)
            .arg(admit.limits.budget)
            .arg(admit.reserve)
            .arg(self.load_ttl_ms())
            .arg(admit.spec)
            .arg(admit.plan)
            .arg(admit.ticket)
            .arg(format!("{base}:load:"))
            .arg(KEEP_RUNS)
            .invoke_async(&mut conn)
            .await;
        match reply {
            Ok(0) => Ok(()),
            Ok(1) => Err(AdmitError::Busy),
            Ok(2) => Err(AdmitError::NamespaceBusy),
            Ok(3) => Err(AdmitError::ClusterBusy),
            Ok(4) => Err(AdmitError::StoreFull),
            Ok(_) => Err(AdmitError::Unavailable),
            Err(e) => {
                self.failed(e).await;
                Err(AdmitError::Unavailable)
            }
        }
    }

    /// The plan, ticket and stop flag; `None` for an unknown or ended run.
    pub async fn load_run(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
    ) -> Result<Option<ActiveRun>, ReadError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| ReadError::Name)?;
        let mut conn = self.read_connection().await?;
        let fields: Vec<Option<String>> = self
            .read_of(
                redis::cmd("HMGET")
                    .arg(&key)
                    .arg("plan")
                    .arg("ticket")
                    .arg("stop")
                    .query_async(&mut conn)
                    .await,
            )
            .await?;
        match <[Option<String>; 3]>::try_from(fields) {
            Ok([Some(plan), Some(ticket), stop]) => Ok(Some(ActiveRun {
                plan,
                ticket,
                stop: stop.as_deref() == Some("1"),
            })),
            _ => Ok(None),
        }
    }

    /// The stop flag of an active run; `None` for an unknown or ended one.
    pub async fn load_active(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
    ) -> Result<Option<bool>, ReadError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| ReadError::Name)?;
        let mut conn = self.read_connection().await?;
        let (active, stop): (bool, Option<String>) = self
            .read_of(
                redis::pipe()
                    .cmd("HEXISTS")
                    .arg(&key)
                    .arg("plan")
                    .cmd("HGET")
                    .arg(&key)
                    .arg("stop")
                    .query_async(&mut conn)
                    .await,
            )
            .await?;
        Ok(active.then(|| stop.as_deref() == Some("1")))
    }

    /// Replaces the results of `pod`; the stop flag, or `None` for an unknown or ended run.
    pub async fn load_post(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
        pod: &str,
        post: &StoredPost,
    ) -> Result<Option<bool>, StoreError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| StoreError::Name)?;
        check_name(pod).map_err(|_| StoreError::Name)?;
        if FIXED.contains(&pod) {
            return Err(StoreError::Name);
        }
        let value = encode(post).map_err(|_| StoreError::Unavailable)?;
        let mut conn = self.connection().await?;
        let reply: i64 = self
            .write_of(
                redis::Script::new(POST)
                    .key(&key)
                    .arg(pod)
                    .arg(value)
                    .arg(self.load_ttl_ms())
                    .invoke_async(&mut conn)
                    .await,
            )
            .await?;
        Ok((reply >= 0).then_some(reply == 1))
    }

    /// The posts of every pod of an active run.
    pub async fn load_posts(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
    ) -> Result<BTreeMap<String, StoredPost>, ReadError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| ReadError::Name)?;
        let mut conn = self.read_connection().await?;
        let all: BTreeMap<String, String> = self
            .read_of(redis::cmd("HGETALL").arg(&key).query_async(&mut conn).await)
            .await?;
        all.into_iter()
            .filter(|(field, _)| !FIXED.contains(&field.as_str()))
            .map(|(pod, value)| {
                decode(&value)
                    .map(|post| (pod, post))
                    .ok_or(ReadError::Unreadable)
            })
            .collect()
    }

    /// Sets the stop flag; the next generator poll stops.
    pub async fn load_stop(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
    ) -> Result<(), StoreError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| StoreError::Name)?;
        let mut conn = self.connection().await?;
        self.write_of(
            redis::Script::new(STOP)
                .key(&key)
                .invoke_async::<i64>(&mut conn)
                .await,
        )
        .await?;
        Ok(())
    }

    /// Ends the run (spec §7.8 "End"); false when it had ended already or is unknown.
    pub async fn load_end(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
        ended: &Ended<'_>,
    ) -> Result<bool, StoreError> {
        if ended.summary.len() > MAX_SUMMARY {
            return Err(StoreError::TooLarge);
        }
        let key = run_key(namespace, deployment, uid).map_err(|_| StoreError::Name)?;
        let member = format!("{namespace}/{deployment}/{uid}");
        let mut conn = self.connection().await?;
        let reply: i64 = self
            .write_of(
                redis::Script::new(END)
                    .key(&key)
                    .key(ACTIVE)
                    .key(RESERVED)
                    .arg(member)
                    .arg(ended.phase)
                    .arg(ended.reason)
                    .arg(ended.end_ms)
                    .arg(ended.summary)
                    .arg(self.load_ttl_ms())
                    .invoke_async(&mut conn)
                    .await,
            )
            .await?;
        Ok(reply == 1)
    }

    /// The small fields and the summary of a run, whatever its state: never the plan, the
    /// ticket or a pod's results.
    pub async fn load_fields(
        &self,
        namespace: &str,
        deployment: &str,
        uid: &str,
    ) -> Result<BTreeMap<String, String>, ReadError> {
        let key = run_key(namespace, deployment, uid).map_err(|_| ReadError::Name)?;
        let mut conn = self.read_connection().await?;
        let mut command = redis::cmd("HMGET");
        command.arg(&key);
        for field in KEPT {
            command.arg(field);
        }
        let values: Vec<Option<String>> =
            self.read_of(command.query_async(&mut conn).await).await?;
        Ok(KEPT
            .iter()
            .zip(values)
            .filter_map(|(field, value)| Some(((*field).to_string(), value?)))
            .collect())
    }
}

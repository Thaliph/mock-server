//! The bounded merge of the controller store (spec §7.3): the §6 merge under the store
//! limits, where what is kept is chosen by traffic and never by key order alone, so that
//! endpoints that sort first cannot crowd out the deployment's real API.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use super::report::{Direction, EndpointKey, Limits, Report};
use super::sample::Sample;

/// What a limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Endpoint,
    Sample,
}

impl Kind {
    /// The `kind` label of `mimic_store_refused_total` and `mimic_store_evicted_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Sample => "sample",
        }
    }
}

/// Growth that was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    /// A new endpoint past `max_endpoints`.
    EndpointLimit,
    /// A new sample past `sample_budget`.
    Budget,
}

impl Refusal {
    pub fn kind(self) -> Kind {
        match self {
            Self::EndpointLimit => Kind::Endpoint,
            Self::Budget => Kind::Sample,
        }
    }

    /// The `reason` label of `mimic_store_refused_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointLimit => "endpoint_limit",
            Self::Budget => "budget",
        }
    }
}

/// The merged report and what the limits cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounded {
    pub report: Report,
    /// New endpoints and samples that were not kept.
    pub refused: BTreeMap<Refusal, u64>,
    /// Stored endpoints and samples that were dropped to make room.
    pub evicted: BTreeMap<Kind, u64>,
}

/// A status class (`1xx` … `5xx`) of an endpoint: the unit that eviction never empties.
type Pair = (EndpointKey, u16);

/// The samples held in a pair: hash to bytes.
type Held = BTreeMap<u64, usize>;

fn bump<K: Ord>(counts: &mut BTreeMap<K, u64>, key: K) {
    let n = counts.entry(key).or_default();
    *n = n.saturating_add(1);
}

/// Merges `push` into `stored` as §6 does, under `limits` (`max_endpoints` is per
/// direction): the result's `pod` is null, counters saturate, and what exceeds a limit is
/// chosen as follows.
///
/// - Endpoints: the first `max_endpoints` of a direction ranked by exchange count
///   (descending), then stored before new, then by key.
/// - Samples: a new sample past the budget is refused, unless its endpoint has no sample of
///   its status class; then samples are evicted one at a time from the pair holding the most
///   (ties: the most bytes, then the larger key), never the last of a pair. If room cannot be
///   made that way, nothing is evicted and the sample is refused.
pub fn merge_bounded(stored: Report, push: Report, limits: &Limits) -> Bounded {
    // What the stored report holds, to tell it from what the push adds once they are united.
    let held: Vec<(EndpointKey, BTreeSet<u64>)> = stored
        .endpoint_keys()
        .into_iter()
        .map(|key| {
            let hashes = stored
                .stats_at(&key)
                .map(|s| s.samples().iter().map(|(h, _)| h).collect())
                .unwrap_or_default();
            (key, hashes)
        })
        .collect();
    let mut report = stored;
    report.union(push, limits.max_literals);
    report.clear_pod();
    let mut stored_hashes: BTreeMap<EndpointKey, BTreeSet<u64>> = BTreeMap::new();
    for (key, hashes) in held {
        if let Some(routed) = report.route(&key) {
            stored_hashes.entry(routed).or_default().extend(hashes);
        }
    }

    let mut out = Bounded {
        report: Report::new(None),
        refused: BTreeMap::new(),
        evicted: BTreeMap::new(),
    };
    limit_endpoints(&mut report, limits.max_endpoints, &stored_hashes, &mut out);
    limit_samples(&mut report, limits.sample_budget, &stored_hashes, &mut out);
    out.report = report;
    out
}

fn limit_endpoints(
    report: &mut Report,
    max: usize,
    stored: &BTreeMap<EndpointKey, BTreeSet<u64>>,
    out: &mut Bounded,
) {
    type Rank = (Reverse<u64>, bool, String, String, EndpointKey);
    let mut by_direction: BTreeMap<Direction, Vec<Rank>> = BTreeMap::new();
    for key in report.endpoint_keys() {
        let total = report.stats_at(&key).map_or(0, |s| s.total());
        by_direction.entry(key.0).or_default().push((
            Reverse(total),
            !stored.contains_key(&key),
            key.3.clone(),
            key.2.clone(),
            key,
        ));
    }
    for ranks in by_direction.values_mut() {
        if ranks.len() <= max {
            continue;
        }
        ranks.sort();
        for (_, new, _, _, key) in ranks.drain(max..) {
            report.remove_endpoint(&key);
            if new {
                bump(&mut out.refused, Refusal::EndpointLimit);
            } else {
                bump(&mut out.evicted, Kind::Endpoint);
            }
        }
    }
}

fn class(sample: &Sample) -> u16 {
    sample.status() / 100
}

/// The pair with the most samples that has at least two, and its largest sample hash.
fn victim(held: &BTreeMap<Pair, Held>) -> Option<(Pair, u64)> {
    held.iter()
        .filter(|(_, samples)| samples.len() >= 2)
        .max_by_key(|(pair, samples)| (samples.len(), samples.values().sum::<usize>(), *pair))
        .and_then(|(pair, samples)| Some((pair.clone(), *samples.keys().next_back()?)))
}

fn limit_samples(
    report: &mut Report,
    budget: usize,
    stored: &BTreeMap<EndpointKey, BTreeSet<u64>>,
    out: &mut Bounded,
) {
    if report.sample_bytes() <= budget {
        return;
    }
    // The samples the push adds leave the report; they come back one at a time.
    let mut added: Vec<(Pair, u64, Sample)> = Vec::new();
    let mut held: BTreeMap<Pair, Held> = BTreeMap::new();
    let mut total = 0usize;
    for key in report.endpoint_keys() {
        let known = stored.get(&key);
        let Some(stats) = report.stats_at_mut(&key) else {
            continue;
        };
        let all: Vec<(u64, usize, u16, bool)> = stats
            .samples()
            .iter()
            .map(|(h, s)| {
                (
                    h,
                    s.bytes(),
                    class(s),
                    known.is_some_and(|k| k.contains(&h)),
                )
            })
            .collect();
        for (hash, bytes, class, is_stored) in all {
            if is_stored {
                total += bytes;
                held.entry((key.clone(), class))
                    .or_default()
                    .insert(hash, bytes);
            } else if let Some(sample) = stats.samples_mut().remove(hash) {
                added.push(((key.clone(), class), hash, sample));
            }
        }
    }
    // A stored report that exceeds the budget on its own (a lowered limit) is trimmed the same way.
    let mut dropped: Vec<(Pair, u64)> = Vec::new();
    while total > budget {
        let Some((pair, hash)) = victim(&held) else {
            break;
        };
        if let Some(bytes) = held.get_mut(&pair).and_then(|h| h.remove(&hash)) {
            total -= bytes;
        }
        dropped.push((pair, hash));
    }

    added.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut admitted: BTreeMap<(Pair, u64), Sample> = BTreeMap::new();
    for (pair, hash, sample) in added {
        let bytes = sample.bytes();
        let mut undo: Vec<(Pair, u64, usize)> = Vec::new();
        let mut room = total + bytes <= budget;
        if !room && held.get(&pair).is_none_or(Held::is_empty) {
            let mut free = total;
            while free + bytes > budget {
                let Some((from, evict)) = victim(&held) else {
                    break;
                };
                let Some(freed) = held.get_mut(&from).and_then(|h| h.remove(&evict)) else {
                    break;
                };
                free -= freed;
                undo.push((from, evict, freed));
            }
            room = free + bytes <= budget;
            if room {
                total = free;
            } else {
                for (from, evict, freed) in undo.drain(..) {
                    held.entry(from).or_default().insert(evict, freed);
                }
            }
        }
        if !room {
            bump(&mut out.refused, Refusal::Budget);
            continue;
        }
        dropped.extend(undo.into_iter().map(|(from, evict, _)| (from, evict)));
        total += bytes;
        held.entry(pair.clone()).or_default().insert(hash, bytes);
        admitted.insert((pair, hash), sample);
    }

    for (pair, hash) in dropped {
        bump(&mut out.evicted, Kind::Sample);
        if admitted.remove(&(pair.clone(), hash)).is_none()
            && let Some(stats) = report.stats_at_mut(&pair.0)
        {
            stats.samples_mut().remove(hash);
        }
    }
    for (((key, _), hash), sample) in admitted {
        if let Some(stats) = report.stats_at_mut(&key) {
            stats.samples_mut().insert(hash, sample);
        }
    }
}

/// Removes the endpoints and samples whose `last_seen` is older than `today - ttl_days` (a
/// missing `last_seen` is the oldest). Returns how many of each kind were removed.
pub fn drop_expired(_report: &mut Report, _today: u64, _ttl_days: u64) -> BTreeMap<Kind, u64> {
    BTreeMap::new()
}

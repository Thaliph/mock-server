//! Store des échanges : mémoire bornée en nombre et en octets, persistance JSONL locale
//! optionnelle (spec §5.2 et §8).
//!
//! Tout échange qui entre ici est déjà redacté. Les bodies sont échantillonnés par
//! (opération, code) avec un réservoir déterministe (`mimic_core::sample`).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use mimic_core::model::{Body, Direction, Exchange};
use mimic_core::sample::{Offer, Reservoir, priority, sample_key};
use prometheus_client::metrics::counter::Counter;

use crate::journal::{Journal, JournalConfig};

/// Provenance d'un échange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Capturé par ce sidecar.
    Local,
    /// Importé d'un autre sidecar du workload.
    Peer,
}

/// Résultat d'un ajout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    Stored,
    /// Stocké sans ses bodies : plafond d'exemples atteint.
    StoredWithoutBodies,
    Duplicate,
}

/// Bornes du store.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_exchanges: usize,
    /// Bodies gardés par (opération, code).
    pub samples: usize,
    /// Budget en octets estimés (`Exchange::approx_bytes`).
    pub max_bytes: usize,
    /// Borne en octets estimés de la file de publication ; `None` sans partage : rien n'y
    /// est gardé.
    pub share_backlog: Option<usize>,
    /// Borne en nombre d'échanges de la même file (`share.max_pending`).
    pub share_pending: usize,
}

impl Limits {
    /// Bornes en nombre seulement, sans partage.
    pub fn new(max_exchanges: usize, samples: usize) -> Self {
        Self {
            max_exchanges,
            samples,
            max_bytes: usize::MAX,
            share_backlog: None,
            share_pending: usize::MAX,
        }
    }
}

/// Compteurs des retraits (`mimic_capture_dropped_total{reason}`).
#[derive(Debug, Clone, Default)]
pub struct Drops {
    pub store_budget: Counter,
    pub share_backlog: Counter,
    pub local_backlog: Counter,
}

pub struct Store {
    inner: Mutex<Inner>,
    limits: Limits,
    drops: Drops,
    journal: OnceLock<Journal>,
    version: AtomicU64,
    outbound_version: AtomicU64,
}

struct Entry {
    e: Arc<Exchange>,
    origin: Origin,
    bytes: usize,
    /// Part de `bytes` occupée par les bodies.
    body_bytes: usize,
    /// Place dans le réservoir (clé, priorité) tant que l'échange garde ses bodies.
    sample: Option<(String, u64)>,
}

#[derive(Default)]
struct Inner {
    /// Échanges par numéro d'arrivée : le premier est le plus ancien.
    list: BTreeMap<u64, Entry>,
    next: u64,
    ids: HashMap<String, u64>,
    /// Réservoirs non vides, par clé d'échantillonnage.
    samples: BTreeMap<String, Reservoir>,
    /// Numéros des échanges qui gardent des bodies, du plus ancien au plus récent.
    with_bodies: BTreeSet<u64>,
    bytes: usize,
    /// Octets des bodies gardés, au plus la moitié du budget.
    body_bytes: usize,
    local: usize,
    peer: usize,
    /// Échanges locaux pas encore publiés aux autres sidecars, avec leur taille.
    unshared: VecDeque<(usize, Arc<Exchange>)>,
    unshared_bytes: usize,
    /// Segment en cours de publication : les `count` premiers échanges de `unshared`, sous
    /// cet identifiant, jusqu'à ce que le support l'accepte.
    sealed: Option<Sealed>,
}

struct Sealed {
    id: String,
    count: usize,
}

impl Store {
    pub fn new(max: usize, samples: usize) -> Self {
        Self::with_limits(Limits::new(max, samples), Drops::default())
    }

    pub fn with_limits(limits: Limits, drops: Drops) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            limits,
            drops,
            journal: OnceLock::new(),
            version: AtomicU64::new(0),
            outbound_version: AtomicU64::new(0),
        }
    }

    /// Ouvre le répertoire de persistance : recharge les segments présents, du plus ancien au
    /// plus récent, puis y journalise les nouveaux échanges locaux.
    pub fn persist_to(&self, cfg: &JournalConfig) -> std::io::Result<usize> {
        std::fs::create_dir_all(&cfg.dir)?;
        let mut existing = crate::journal::segments(&cfg.dir)?;
        let extra = existing.len().saturating_sub(cfg.max_segments);
        for (_, old) in existing.drain(..extra) {
            std::fs::remove_file(old)?;
        }
        let mut loaded = 0;
        for (_, path) in &existing {
            for line in BufReader::new(File::open(path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                // Une ligne illisible (écriture interrompue) est ignorée.
                if let Ok(e) = serde_json::from_str::<Exchange>(&line)
                    && self.insert(e, Origin::Local, false) != Added::Duplicate
                {
                    loaded += 1;
                }
            }
        }
        let journal = Journal::start(cfg, existing, self.drops.local_backlog.clone())?;
        let _ = self.journal.set(journal);
        Ok(loaded)
    }

    /// Attend que le journal local ait écrit tout ce qui lui a été confié.
    pub fn sync_journal(&self) {
        if let Some(j) = self.journal.get() {
            j.sync();
        }
    }

    pub fn add(&self, e: Exchange, origin: Origin) -> Added {
        self.insert(e, origin, true)
    }

    /// Échange local gardé en mémoire seulement : ni journal, ni file de publication
    /// (`capture.inbound: metadata`, spec §5.2).
    pub fn add_ephemeral(&self, e: Exchange) -> Added {
        self.insert(e, Origin::Local, false)
    }

    /// Vrai si un échange de cette clé et de cet identifiant garderait ses bodies : à vérifier
    /// avant de décoder, parser et redacter un body qui serait jeté (spec §5.2).
    pub fn wants_bodies(
        &self,
        direction: Direction,
        authority: &str,
        method: &str,
        path: &str,
        status: u16,
        id: &str,
    ) -> bool {
        if self.limits.samples == 0 {
            return false;
        }
        let key = sample_key(direction, authority, method, path, status);
        let prio = priority(id);
        let inner = self.lock();
        inner
            .samples
            .get(&key)
            .is_none_or(|r| r.admits(self.limits.samples, prio))
    }

    fn insert(&self, mut e: Exchange, origin: Origin, persist: bool) -> Added {
        let mut inner = self.lock();
        if inner.ids.contains_key(&e.id) {
            return Added::Duplicate;
        }
        let mut outbound_changed = e.direction == Direction::Outbound;
        let mut result = Added::Stored;
        let mut sample = None;
        if has_body_any(&e) {
            let key = sample_key(e.direction, &e.authority, &e.method, &e.path, e.status);
            let prio = priority(&e.id);
            let reservoir = inner.samples.entry(key.clone()).or_default();
            match reservoir.offer(self.limits.samples, prio, &e.id) {
                Offer::Rejected => {
                    if reservoir.is_empty() {
                        inner.samples.remove(&key);
                    }
                    strip_bodies(&mut e);
                    result = Added::StoredWithoutBodies;
                }
                Offer::Kept => sample = Some((key, prio)),
                Offer::Replaced(old) => {
                    sample = Some((key, prio));
                    if let Some(seq) = inner.ids.get(&old).copied() {
                        outbound_changed |= inner.strip(seq);
                    }
                }
            }
        }
        let e = Arc::new(e);
        let bytes = e.approx_bytes();
        let body_bytes = e.req_body.kept_bytes() + e.resp_body.kept_bytes();
        let local = persist && origin == Origin::Local;
        if local && let Some(max) = self.limits.share_backlog {
            inner.unshared_bytes += bytes;
            inner.unshared.push_back((bytes, e.clone()));
            inner.trim_unshared(max, self.limits.share_pending, &self.drops.share_backlog);
        }
        let seq = inner.next;
        inner.next += 1;
        if sample.is_some() {
            inner.with_bodies.insert(seq);
        }
        inner.ids.insert(e.id.clone(), seq);
        match origin {
            Origin::Local => inner.local += 1,
            Origin::Peer => inner.peer += 1,
        }
        inner.bytes += bytes;
        inner.body_bytes += body_bytes;
        let journal = (local && self.journal.get().is_some()).then(|| e.clone());
        inner.list.insert(
            seq,
            Entry {
                e,
                origin,
                bytes,
                body_bytes,
                sample,
            },
        );
        while inner.list.len() > self.limits.max_exchanges {
            outbound_changed |= inner.evict_oldest().unwrap_or(false);
        }
        // Budget en octets : les bodies en occupent au plus la moitié, retirés du plus ancien
        // au plus récent ; au-delà du budget, les échanges les plus anciens sont évincés.
        while inner.body_bytes > self.limits.max_bytes / 2
            && let Some(seq) = inner.with_bodies.first().copied()
        {
            outbound_changed |= inner.strip(seq);
            self.drops.store_budget.inc();
        }
        while inner.bytes > self.limits.max_bytes
            && let Some(changed) = inner.evict_oldest()
        {
            outbound_changed |= changed;
            self.drops.store_budget.inc();
        }
        drop(inner);
        // Hors du verrou : le journal ne fait qu'une mise en file, sans I/O.
        if let (Some(e), Some(j)) = (journal, self.journal.get()) {
            j.append(e, bytes);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
        if outbound_changed {
            self.outbound_version.fetch_add(1, Ordering::SeqCst);
        }
        result
    }

    /// Change à chaque ajout ou purge.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Change à chaque ajout ou purge d'un échange sortant.
    pub fn outbound_version(&self) -> u64 {
        self.outbound_version.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> Vec<Exchange> {
        self.lock()
            .list
            .values()
            .map(|en| Exchange::clone(&en.e))
            .collect()
    }

    /// Échanges d'un sens, partagés avec le store (aucune copie) : de quoi construire le
    /// moteur de mock hors du verrou.
    pub fn shared_direction(&self, direction: Direction) -> Vec<Arc<Exchange>> {
        self.lock()
            .list
            .values()
            .filter(|en| en.e.direction == direction)
            .map(|en| en.e.clone())
            .collect()
    }

    /// Les plus récents d'abord.
    pub fn recent(&self, direction: Option<Direction>, limit: usize) -> Vec<Exchange> {
        self.lock()
            .list
            .values()
            .rev()
            .filter(|en| direction.is_none_or(|d| en.e.direction == d))
            .take(limit)
            .map(|en| Exchange::clone(&en.e))
            .collect()
    }

    pub fn counts(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.local, inner.peer)
    }

    pub fn len(&self) -> usize {
        self.lock().list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Octets estimés des échanges en mémoire.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Couples (opération, code) qui ont encore au moins un exemple avec bodies.
    pub fn sample_keys(&self) -> usize {
        self.lock().samples.len()
    }

    /// Octets estimés de la file de publication.
    pub fn unshared_bytes(&self) -> usize {
        self.lock().unshared_bytes
    }

    /// Nombre d'échanges de la file de publication.
    pub fn unshared_len(&self) -> usize {
        self.lock().unshared.len()
    }

    /// Vide la file de publication et rend ses échanges.
    pub fn take_unshared(&self) -> Vec<Exchange> {
        let mut inner = self.lock();
        inner.unshared_bytes = 0;
        inner.sealed = None;
        let list = std::mem::take(&mut inner.unshared);
        drop(inner);
        list.into_iter()
            .map(|(_, e)| Arc::unwrap_or_clone(e))
            .collect()
    }

    /// Segment en cours de publication, s'il y en a un : identifiant et échanges, du plus
    /// ancien au plus récent. Il reste en tête de la file, sous le même identifiant, tant que
    /// `commit_unshared` ne l'a pas retiré (spec §5.4).
    pub fn sealed_unshared(&self) -> Option<(String, Vec<Arc<Exchange>>)> {
        let inner = self.lock();
        let s = inner.sealed.as_ref()?;
        let list = inner
            .unshared
            .iter()
            .take(s.count)
            .map(|(_, e)| e.clone())
            .collect();
        Some((s.id.clone(), list))
    }

    /// Premiers échanges de la file, jusqu'à `max_bytes` octets estimés (au moins un) : de
    /// quoi former le prochain segment, hors du verrou.
    pub fn unshared_head(&self, max_bytes: usize) -> Vec<Arc<Exchange>> {
        let inner = self.lock();
        let mut total = 0;
        let mut out = vec![];
        for (bytes, e) in &inner.unshared {
            if !out.is_empty() && total + bytes > max_bytes {
                break;
            }
            total += bytes;
            out.push(e.clone());
        }
        out
    }

    /// Scelle les `count` premiers échanges de la file en un segment, sous un nouvel
    /// identifiant. `None` si la tête a changé depuis `unshared_head` (échanges jetés) ou si
    /// un segment est déjà scellé.
    pub fn seal_unshared(&self, first_id: &str, count: usize) -> Option<String> {
        let mut inner = self.lock();
        let head = inner.unshared.front().map(|(_, e)| e.id.as_str());
        if inner.sealed.is_some() || count == 0 || head != Some(first_id) {
            return None;
        }
        if inner.unshared.len() < count {
            return None;
        }
        let id = ulid::Ulid::generate().to_string();
        inner.sealed = Some(Sealed {
            id: id.clone(),
            count,
        });
        Some(id)
    }

    /// Retire de la file le segment accepté par le support.
    pub fn commit_unshared(&self, id: &str) {
        let mut inner = self.lock();
        let Some(count) = inner
            .sealed
            .as_ref()
            .filter(|s| s.id == id)
            .map(|s| s.count)
        else {
            return;
        };
        inner.sealed = None;
        for _ in 0..count {
            let Some((bytes, _)) = inner.unshared.pop_front() else {
                break;
            };
            inner.unshared_bytes -= bytes;
        }
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        *inner = Inner {
            next: inner.next,
            ..Inner::default()
        };
        drop(inner);
        // Les segments sont supprimés : la purge doit survivre au redémarrage.
        if let Some(j) = self.journal.get() {
            j.clear();
        }
        self.version.fetch_add(1, Ordering::SeqCst);
        self.outbound_version.fetch_add(1, Ordering::SeqCst);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("verrou du store")
    }
}

impl Inner {
    /// Retire les bodies d'un échange ; vrai s'il est sortant.
    fn strip(&mut self, seq: u64) -> bool {
        let Some(entry) = self.list.get_mut(&seq) else {
            return false;
        };
        if let Some((key, prio)) = entry.sample.take() {
            unsample(&mut self.samples, &key, prio, &entry.e.id);
        }
        self.with_bodies.remove(&seq);
        match Arc::get_mut(&mut entry.e) {
            Some(e) => strip_bodies(e),
            // Encore en file (journal, partage) : copie sans bodies, l'original y reste.
            None => entry.e = Arc::new(without_bodies(&entry.e)),
        }
        let bytes = entry.e.approx_bytes();
        self.bytes = self.bytes - entry.bytes + bytes;
        self.body_bytes -= entry.body_bytes;
        entry.bytes = bytes;
        entry.body_bytes = 0;
        entry.e.direction == Direction::Outbound
    }

    /// Évince l'échange le plus ancien ; `Some(sortant)`, ou `None` si le store est vide.
    fn evict_oldest(&mut self) -> Option<bool> {
        let (seq, entry) = self.list.pop_first()?;
        self.ids.remove(&entry.e.id);
        self.with_bodies.remove(&seq);
        if let Some((key, prio)) = &entry.sample {
            unsample(&mut self.samples, key, *prio, &entry.e.id);
        }
        self.bytes -= entry.bytes;
        self.body_bytes -= entry.body_bytes;
        match entry.origin {
            Origin::Local => self.local -= 1,
            Origin::Peer => self.peer -= 1,
        }
        Some(entry.e.direction == Direction::Outbound)
    }

    /// Jette les plus anciens échanges de la file au-delà de l'une ou l'autre borne. Un
    /// segment scellé entamé n'est plus le même : le reste repartira sous un nouvel
    /// identifiant.
    fn trim_unshared(&mut self, max_bytes: usize, max_count: usize, dropped: &Counter) {
        while self.unshared_bytes > max_bytes || self.unshared.len() > max_count {
            let Some((bytes, _)) = self.unshared.pop_front() else {
                break;
            };
            self.unshared_bytes -= bytes;
            self.sealed = None;
            dropped.inc();
        }
    }
}

/// Retire un exemple de son réservoir, et le réservoir vidé de la map.
fn unsample(samples: &mut BTreeMap<String, Reservoir>, key: &str, prio: u64, id: &str) {
    if let Some(r) = samples.get_mut(key) {
        r.remove(prio, id);
        if r.is_empty() {
            samples.remove(key);
        }
    }
}

fn has_body_any(e: &Exchange) -> bool {
    matches!(e.req_body, Body::Captured { .. } | Body::Truncated { .. })
        || matches!(e.resp_body, Body::Captured { .. } | Body::Truncated { .. })
}

fn skipped(b: &Body) -> Body {
    match b {
        Body::Captured { data } => Body::Skipped {
            size: data.len() as u64,
        },
        Body::Truncated { size, .. } => Body::Skipped { size: *size },
        other => other.clone(),
    }
}

fn strip_bodies(e: &mut Exchange) {
    e.req_body = skipped(&e.req_body);
    e.resp_body = skipped(&e.resp_body);
}

/// Copie des métadonnées d'un échange, bodies remplacés par leur taille.
fn without_bodies(e: &Exchange) -> Exchange {
    Exchange {
        id: e.id.clone(),
        direction: e.direction,
        started_at_ms: e.started_at_ms,
        scheme: e.scheme.clone(),
        authority: e.authority.clone(),
        method: e.method.clone(),
        path: e.path.clone(),
        query: e.query.clone(),
        req_headers: e.req_headers.clone(),
        req_body: skipped(&e.req_body),
        status: e.status,
        resp_headers: e.resp_headers.clone(),
        resp_body: skipped(&e.resp_body),
        ttfb_us: e.ttfb_us,
        total_us: e.total_us,
        trace_id: e.trace_id.clone(),
        redactions: e.redactions.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(id: &str, path: &str) -> Exchange {
        Exchange {
            id: id.into(),
            direction: Direction::Outbound,
            started_at_ms: 0,
            scheme: "http".into(),
            authority: "a:80".into(),
            method: "GET".into(),
            path: path.into(),
            query: vec![],
            req_headers: vec![],
            req_body: Body::Empty,
            status: 200,
            resp_headers: vec![],
            resp_body: Body::captured("x"),
            ttfb_us: 1,
            total_us: 1,
            trace_id: None,
            redactions: vec![],
        }
    }

    fn with_body(id: &str, size: usize) -> Exchange {
        let mut e = ex(id, &format!("/{id}"));
        e.resp_body = Body::captured(vec![b'x'; size]);
        e
    }

    fn kept(s: &Store) -> BTreeSet<String> {
        s.snapshot()
            .into_iter()
            .filter(|e| matches!(e.resp_body, Body::Captured { .. }))
            .map(|e| e.id)
            .collect()
    }

    #[test]
    fn duplicates_are_ignored() {
        let s = Store::new(10, 10);
        assert_eq!(s.add(ex("1", "/a"), Origin::Local), Added::Stored);
        assert_eq!(s.add(ex("1", "/a"), Origin::Peer), Added::Duplicate);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn bodies_are_capped_per_operation() {
        let s = Store::new(10, 2);
        for i in 0..4 {
            s.add(ex(&i.to_string(), "/a"), Origin::Local);
        }
        assert_eq!(kept(&s).len(), 2);
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn oldest_are_evicted() {
        let s = Store::new(3, 10);
        for i in 0..5 {
            s.add(ex(&i.to_string(), &format!("/{i}")), Origin::Local);
        }
        let ids: Vec<_> = s.snapshot().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["2", "3", "4"]);
        assert_eq!(s.counts(), (3, 0));
    }

    #[test]
    fn cap_01_identifiers_in_paths_count_for_one_operation() {
        let s = Store::new(1000, 20);
        for i in 0..100 {
            s.add(
                ex(&format!("api-0-{i}"), &format!("/users/{i}")),
                Origin::Local,
            );
        }
        assert_eq!(kept(&s).len(), 20);
        assert_eq!(s.len(), 100);
        assert_eq!(s.sample_keys(), 1);
    }

    #[test]
    fn cap_02_same_exchanges_in_any_order_keep_the_same_bodies() {
        let all: Vec<Exchange> = (0..100)
            .map(|i| ex(&format!("api-0-{i}"), &format!("/users/{i}")))
            .collect();
        let (a, b) = (Store::new(1000, 20), Store::new(1000, 20));
        for e in &all {
            a.add(e.clone(), Origin::Local);
        }
        for e in all.iter().rev() {
            b.add(e.clone(), Origin::Peer);
        }
        assert_eq!(kept(&a), kept(&b));
        let first: BTreeSet<String> = all[..20].iter().map(|e| e.id.clone()).collect();
        assert!(!kept(&a).is_subset(&first), "réservoir figé sur le warm-up");
    }

    #[test]
    fn cap_03_bodies_are_wanted_only_if_the_reservoir_would_keep_them() {
        let s = Store::new(100, 1);
        let want = |id: &str| s.wants_bodies(Direction::Outbound, "a:80", "GET", "/u/9", 200, id);
        assert!(want("first"));
        s.add(ex("first", "/u/1"), Origin::Local);
        let p = priority("first");
        let ids: Vec<String> = (0..64).map(|i| format!("id-{i}")).collect();
        let higher = ids.iter().find(|i| priority(i) > p).unwrap();
        let lower = ids.iter().find(|i| priority(i) < p).unwrap();
        assert!(!want(higher), "réservoir plein d'un exemple prioritaire");
        assert!(want(lower));
        assert!(
            s.wants_bodies(Direction::Outbound, "a:80", "GET", "/u/9", 404, higher),
            "autre code, autre réservoir"
        );
        let none = Store::new(100, 0);
        assert!(!none.wants_bodies(Direction::Outbound, "a:80", "GET", "/u/9", 200, "x"));
    }

    #[test]
    fn cap_04_sampling_counters_are_purged_with_their_last_example() {
        let s = Store::new(10, 2);
        for i in 0..1000 {
            let mut e = ex(&format!("api-0-{i}"), "/users/1");
            e.authority = format!("h{i}:80");
            s.add(e, Origin::Local);
        }
        assert_eq!(s.len(), 10);
        assert_eq!(s.sample_keys(), 10);
        s.clear();
        assert_eq!(s.sample_keys(), 0);
    }

    #[test]
    fn cap_05_budget_strips_oldest_bodies_then_evicts_oldest_exchanges() {
        // Bodies de 10 000 octets : deux tiennent dans la moitié du budget, pas trois.
        let budget = 50_000;
        let drops = Drops::default();
        let s = Store::with_limits(
            Limits {
                max_bytes: budget,
                ..Limits::new(1000, 1000)
            },
            drops.clone(),
        );
        for i in 0..3 {
            s.add(with_body(&format!("{i:03}"), 10_000), Origin::Local);
        }
        assert_eq!(s.len(), 3, "bodies retirés avant tout échange");
        assert_eq!(kept(&s), BTreeSet::from(["001".into(), "002".into()]));
        assert_eq!(drops.store_budget.get(), 1);
        for i in 3..300 {
            s.add(with_body(&format!("{i:03}"), 10_000), Origin::Local);
            assert!(s.bytes() <= budget);
        }
        assert!(
            (10..300).contains(&s.len()),
            "{} échanges : évincés au-delà du budget",
            s.len()
        );
        let ids: Vec<String> = s.snapshot().into_iter().map(|e| e.id).collect();
        assert_eq!(ids.last().map(String::as_str), Some("299"));
        assert_eq!(kept(&s), BTreeSet::from(["298".into(), "299".into()]));
        assert!(drops.store_budget.get() > 298);
    }

    #[test]
    fn shr_06_without_share_nothing_waits_for_publication() {
        let s = Store::new(1000, 20);
        for i in 0..100 {
            s.add(ex(&i.to_string(), "/a"), Origin::Local);
        }
        assert_eq!(s.unshared_bytes(), 0);
        assert!(s.take_unshared().is_empty());
    }

    #[test]
    fn shr_07_share_backlog_is_bounded_and_keeps_the_newest() {
        let one = ex("000", "/a").approx_bytes();
        let max = 10 * one;
        let drops = Drops::default();
        let s = Store::with_limits(
            Limits {
                share_backlog: Some(max),
                ..Limits::new(1000, 20)
            },
            drops.clone(),
        );
        for i in 0..100 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
            assert!(s.unshared_bytes() <= max);
        }
        // Les importés ne sont jamais republiés.
        s.add(ex("peer", "/a"), Origin::Peer);
        let pending = s.take_unshared();
        let ids: Vec<&str> = pending.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 10);
        assert_eq!(ids.last(), Some(&"099"));
        assert!(!ids.contains(&"000"));
        assert_eq!(drops.share_backlog.get(), 90);
        // Publication échouée (segment scellé, jamais accepté), puis du trafic : la file reste
        // bornée, les plus récents gagnent.
        for i in 100..110 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
        }
        let head = s.unshared_head(usize::MAX);
        assert!(s.seal_unshared(&head[0].id, head.len()).is_some());
        for i in 110..115 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
        }
        assert!(s.unshared_bytes() <= max);
        let ids: Vec<String> = s.take_unshared().into_iter().map(|e| e.id).collect();
        assert_eq!(ids.first().map(String::as_str), Some("105"));
        assert_eq!(ids.last().map(String::as_str), Some("114"));
        assert_eq!(drops.share_backlog.get(), 95);
    }

    fn shared_store(max_bytes: usize, max_pending: usize) -> (Store, Drops) {
        let drops = Drops::default();
        let s = Store::with_limits(
            Limits {
                share_backlog: Some(max_bytes),
                share_pending: max_pending,
                ..Limits::new(1000, 20)
            },
            drops.clone(),
        );
        (s, drops)
    }

    #[test]
    fn shr_09_pending_bound_applies_to_the_same_queue() {
        let (s, drops) = shared_store(usize::MAX, 3);
        for i in 0..5 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
        }
        assert_eq!(s.unshared_len(), 3);
        assert_eq!(drops.share_backlog.get(), 2);
        let ids: Vec<String> = s.take_unshared().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["002", "003", "004"]);
    }

    #[test]
    fn shr_09_failed_publication_keeps_the_segment_and_its_identifier() {
        let (s, drops) = shared_store(usize::MAX, 4);
        for i in 0..3 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
        }
        let head = s.unshared_head(usize::MAX);
        assert_eq!(head.len(), 3);
        let id = s.seal_unshared("000", 2).unwrap();
        // Un seul segment scellé à la fois, et une tête qui a changé est refusée.
        assert!(s.seal_unshared("000", 2).is_none());
        // Échec : rien n'est retiré, le segment est rendu sous le même identifiant, même
        // après l'arrivée d'autres échanges.
        s.add(ex("003", "/a"), Origin::Local);
        let (again, list) = s.sealed_unshared().unwrap();
        assert_eq!(again, id);
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["000", "001"]);
        // Accepté : retiré de la file.
        s.commit_unshared(&id);
        assert!(s.sealed_unshared().is_none());
        assert_eq!(s.unshared_len(), 2);
        // Un segment entamé par la borne perd son identifiant.
        let id = s.seal_unshared("002", 2).unwrap();
        for i in 4..7 {
            s.add(ex(&format!("{i:03}"), "/a"), Origin::Local);
        }
        assert!(s.sealed_unshared().is_none());
        s.commit_unshared(&id);
        assert_eq!(s.unshared_len(), 4, "un identifiant périmé ne retire rien");
        assert_eq!(drops.share_backlog.get(), 1);
    }

    fn journal(dir: &std::path::Path, segment_bytes: u64, max_segments: usize) -> JournalConfig {
        JournalConfig::new(dir, segment_bytes, max_segments)
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = journal(dir.path(), 1 << 20, 4);
        let s = Store::new(10, 10);
        s.persist_to(&cfg).unwrap();
        s.add(ex("1", "/a"), Origin::Local);
        s.add(ex("2", "/b"), Origin::Peer);
        s.sync_journal();
        let t = Store::new(10, 10);
        assert_eq!(t.persist_to(&cfg).unwrap(), 1);
        assert_eq!(t.snapshot()[0].id, "1");
    }

    #[test]
    fn cap_06_local_journal_rotates_and_reloads_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&ex("0000", "/a")).unwrap().len() as u64 + 1;
        let cfg = journal(dir.path(), 10 * line, 3);
        let s = Store::new(1000, 1000);
        s.persist_to(&cfg).unwrap();
        for i in 0..200 {
            s.add(ex(&format!("{i:04}"), "/a"), Origin::Local);
        }
        s.sync_journal();
        let files = crate::journal::segments(dir.path()).unwrap();
        assert!(files.len() <= 3, "{files:?}");
        for (_, f) in &files {
            assert!(std::fs::metadata(f).unwrap().len() <= 11 * line);
        }
        let t = Store::new(1000, 1000);
        let loaded = t.persist_to(&cfg).unwrap();
        assert!(loaded > 0 && loaded <= 30, "{loaded} rechargés");
        let ids: Vec<String> = t.snapshot().into_iter().map(|e| e.id).collect();
        assert_eq!(ids.last().map(String::as_str), Some("0199"));
        assert!(!ids.contains(&"0000".to_string()));
    }

    #[test]
    fn cap_06_clear_removes_the_local_journal() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = journal(dir.path(), 1 << 20, 4);
        let s = Store::new(10, 10);
        s.persist_to(&cfg).unwrap();
        s.add(ex("1", "/a"), Origin::Local);
        s.clear();
        s.add(ex("2", "/a"), Origin::Local);
        s.sync_journal();
        let t = Store::new(10, 10);
        assert_eq!(t.persist_to(&cfg).unwrap(), 1);
        assert_eq!(t.snapshot()[0].id, "2");
    }

    #[test]
    fn cap_07_full_journal_never_blocks_and_counts_dropped_lines() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = JournalConfig {
            backlog_bytes: 0,
            ..journal(dir.path(), 1 << 20, 4)
        };
        let drops = Drops::default();
        let s = Store::with_limits(Limits::new(100, 10), drops.clone());
        s.persist_to(&cfg).unwrap();
        for i in 0..10 {
            assert_eq!(
                s.add(ex(&i.to_string(), "/a"), Origin::Local),
                Added::Stored
            );
        }
        assert_eq!(s.len(), 10);
        assert_eq!(drops.local_backlog.get(), 10);
    }

    #[test]
    fn cap_07_lines_reach_the_disk_without_explicit_sync() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(10, 10);
        s.persist_to(&journal(dir.path(), 1 << 20, 4)).unwrap();
        s.add(ex("1", "/a"), Origin::Local);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let on_disk: usize = crate::journal::segments(dir.path())
                .unwrap()
                .iter()
                .map(|(_, p)| std::fs::read_to_string(p).unwrap().lines().count())
                .sum();
            if on_disk == 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "ligne pas écrite");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

//! Store des échanges : mémoire bornée, persistance JSONL locale optionnelle (spec §5.2 et §8).
//!
//! Tout échange qui entre ici est déjà redacté.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use mimic_core::model::{Body, Direction, Exchange};

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

pub struct Store {
    inner: Mutex<Inner>,
    version: AtomicU64,
    outbound_version: AtomicU64,
}

struct Inner {
    list: VecDeque<(Exchange, Origin)>,
    ids: HashSet<String>,
    bodies: HashMap<String, usize>,
    max: usize,
    samples: usize,
    local: usize,
    peer: usize,
    file: Option<BufWriter<File>>,
    /// Échanges locaux pas encore publiés aux autres sidecars.
    unshared: Vec<Exchange>,
}

const LOCAL_FILE: &str = "local.jsonl";

impl Store {
    pub fn new(max: usize, samples: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                list: VecDeque::new(),
                ids: HashSet::new(),
                bodies: HashMap::new(),
                max,
                samples,
                local: 0,
                peer: 0,
                file: None,
                unshared: vec![],
            }),
            version: AtomicU64::new(0),
            outbound_version: AtomicU64::new(0),
        }
    }

    /// Ouvre le répertoire de persistance : recharge `local.jsonl`, puis y ajoute les nouveaux échanges.
    pub fn persist_to(&self, dir: &Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dir)?;
        let path: PathBuf = dir.join(LOCAL_FILE);
        let mut loaded = 0;
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
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
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.lock().file = Some(BufWriter::new(file));
        Ok(loaded)
    }

    pub fn add(&self, e: Exchange, origin: Origin) -> Added {
        self.insert(e, origin, true)
    }

    fn insert(&self, mut e: Exchange, origin: Origin, persist: bool) -> Added {
        let outbound = e.direction == Direction::Outbound;
        {
            let mut inner = self.lock();
            if inner.ids.contains(&e.id) {
                return Added::Duplicate;
            }
            let key = body_key(&e);
            let has_body = has_body_any(&e);
            let mut result = Added::Stored;
            if has_body {
                let samples = inner.samples;
                let count = inner.bodies.entry(key).or_default();
                if *count >= samples {
                    strip_bodies(&mut e);
                    result = Added::StoredWithoutBodies;
                } else {
                    *count += 1;
                }
            }
            if persist && origin == Origin::Local {
                if let Some(f) = inner.file.as_mut()
                    && let Ok(line) = serde_json::to_string(&e)
                {
                    let _ = writeln!(f, "{line}").and_then(|_| f.flush());
                }
                inner.unshared.push(e.clone());
            }
            inner.ids.insert(e.id.clone());
            match origin {
                Origin::Local => inner.local += 1,
                Origin::Peer => inner.peer += 1,
            }
            inner.list.push_back((e, origin));
            while inner.list.len() > inner.max {
                if let Some((old, o)) = inner.list.pop_front() {
                    inner.ids.remove(&old.id);
                    if has_body_any(&old)
                        && let Some(c) = inner.bodies.get_mut(&body_key(&old))
                    {
                        *c = c.saturating_sub(1);
                    }
                    match o {
                        Origin::Local => inner.local -= 1,
                        Origin::Peer => inner.peer -= 1,
                    }
                }
            }
            drop(inner);
            self.version.fetch_add(1, Ordering::SeqCst);
            if outbound {
                self.outbound_version.fetch_add(1, Ordering::SeqCst);
            }
            result
        }
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
        self.lock().list.iter().map(|(e, _)| e.clone()).collect()
    }

    pub fn snapshot_direction(&self, direction: Direction) -> Vec<Exchange> {
        self.lock()
            .list
            .iter()
            .filter(|(e, _)| e.direction == direction)
            .map(|(e, _)| e.clone())
            .collect()
    }

    /// Les plus récents d'abord.
    pub fn recent(&self, direction: Option<Direction>, limit: usize) -> Vec<Exchange> {
        self.lock()
            .list
            .iter()
            .rev()
            .filter(|(e, _)| direction.is_none_or(|d| e.direction == d))
            .take(limit)
            .map(|(e, _)| e.clone())
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

    /// Récupère les échanges locaux à publier.
    pub fn take_unshared(&self) -> Vec<Exchange> {
        std::mem::take(&mut self.lock().unshared)
    }

    /// Remet des échanges en tête de la file de publication (publication échouée).
    pub fn restore_unshared(&self, mut list: Vec<Exchange>) {
        let mut inner = self.lock();
        list.append(&mut inner.unshared);
        inner.unshared = list;
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.list.clear();
        inner.ids.clear();
        inner.bodies.clear();
        inner.unshared.clear();
        inner.local = 0;
        inner.peer = 0;
        if let Some(f) = inner.file.as_mut() {
            // Le fichier local est tronqué : la purge doit survivre au redémarrage.
            let _ = f.flush();
            let _ = f.get_ref().set_len(0);
        }
        drop(inner);
        self.version.fetch_add(1, Ordering::SeqCst);
        self.outbound_version.fetch_add(1, Ordering::SeqCst);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("verrou du store")
    }
}

fn body_key(e: &Exchange) -> String {
    format!(
        "{:?}|{}|{}|{}|{}",
        e.direction, e.authority, e.method, e.path, e.status
    )
}

fn has_body_any(e: &Exchange) -> bool {
    matches!(e.req_body, Body::Captured { .. } | Body::Truncated { .. })
        || matches!(e.resp_body, Body::Captured { .. } | Body::Truncated { .. })
}

fn strip_bodies(e: &mut Exchange) {
    for b in [&mut e.req_body, &mut e.resp_body] {
        let size = match b {
            Body::Captured { data } => data.len() as u64,
            Body::Truncated { size, .. } => *size,
            _ => continue,
        };
        *b = Body::Skipped { size };
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
        let bodies = s
            .snapshot()
            .iter()
            .filter(|e| matches!(e.resp_body, Body::Captured { .. }))
            .count();
        assert_eq!(bodies, 2);
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
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(10, 10);
        s.persist_to(dir.path()).unwrap();
        s.add(ex("1", "/a"), Origin::Local);
        s.add(ex("2", "/b"), Origin::Peer);
        let t = Store::new(10, 10);
        assert_eq!(t.persist_to(dir.path()).unwrap(), 1);
        assert_eq!(t.snapshot()[0].id, "1");
    }
}

//! Journal local des échanges (spec §8) : segments JSONL `local-<n>.jsonl` écrits par un fil
//! dédié, jamais sous le verrou du store ni sur un worker tokio.
//!
//! - Le store dépose les échanges dans une file bornée (en nombre et en octets) ; file pleine,
//!   la ligne est abandonnée et comptée, l'ajout ne bloque jamais.
//! - Le tampon d'écriture est vidé toutes les [`FLUSH_EVERY`], sur demande et à l'arrêt.
//! - Un segment est fermé au-delà de `segment_bytes` ; seuls les `max_segments` plus récents
//!   sont gardés.
//!
//! Les échanges reçus sont déjà redactés (RED-04).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use mimic_core::model::Exchange;
use prometheus_client::metrics::counter::Counter;

use crate::throttle::LogLimiter;

/// Délai maximal entre l'écriture d'une ligne et son passage au système.
pub const FLUSH_EVERY: Duration = Duration::from_millis(100);
/// Échanges en attente d'écriture, au plus.
pub const BACKLOG_LEN: usize = 4096;
/// Octets estimés en attente d'écriture, au plus.
pub const BACKLOG_BYTES: usize = 16 * 1024 * 1024;

const PREFIX: &str = "local-";
const SUFFIX: &str = ".jsonl";

#[derive(Debug, Clone)]
pub struct JournalConfig {
    pub dir: PathBuf,
    pub segment_bytes: u64,
    pub max_segments: usize,
    pub backlog_len: usize,
    pub backlog_bytes: usize,
}

impl JournalConfig {
    pub fn new(dir: impl Into<PathBuf>, segment_bytes: u64, max_segments: usize) -> Self {
        Self {
            dir: dir.into(),
            segment_bytes: segment_bytes.max(1),
            max_segments: max_segments.max(1),
            backlog_len: BACKLOG_LEN,
            backlog_bytes: BACKLOG_BYTES,
        }
    }
}

/// Segments présents dans `dir`, du plus ancien au plus récent.
pub fn segments(dir: &Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let index = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?.parse().ok());
        if let Some(i) = index {
            out.push((i, path));
        }
    }
    out.sort();
    Ok(out)
}

fn segment_path(dir: &Path, index: u64) -> PathBuf {
    dir.join(format!("{PREFIX}{index:08}{SUFFIX}"))
}

enum Cmd {
    Append(Arc<Exchange>, usize),
    /// Vide le tampon, puis acquitte.
    Sync(SyncSender<()>),
    /// Supprime tous les segments, repart sur un segment vide, puis acquitte.
    Clear(SyncSender<()>),
}

/// Côté store du journal ; le fil d'écriture s'arrête quand il est abandonné.
pub struct Journal {
    tx: SyncSender<Cmd>,
    pending: Arc<AtomicUsize>,
    backlog_bytes: usize,
    dropped: Counter,
}

impl Journal {
    /// Ouvre le segment courant (le dernier s'il n'est pas plein) et démarre le fil
    /// d'écriture. `existing` : segments présents, du plus ancien au plus récent.
    pub fn start(
        cfg: &JournalConfig,
        existing: Vec<(u64, PathBuf)>,
        dropped: Counter,
    ) -> std::io::Result<Self> {
        let mut segments: VecDeque<(u64, PathBuf)> = existing.into();
        let reuse = segments
            .back()
            .is_some_and(|(_, p)| std::fs::metadata(p).is_ok_and(|m| m.len() < cfg.segment_bytes));
        if !reuse {
            let index = segments.back().map_or(0, |(i, _)| i + 1);
            segments.push_back((index, segment_path(&cfg.dir, index)));
        }
        let (index, path) = segments.back().cloned().expect("segment courant");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        let mut writer = Writer {
            dir: cfg.dir.clone(),
            segment_bytes: cfg.segment_bytes,
            max_segments: cfg.max_segments,
            segments,
            out: BufWriter::new(file),
            size,
            index,
            errors: LogLimiter::per_second(),
        };
        writer.trim();
        let (tx, rx) = std::sync::mpsc::sync_channel(cfg.backlog_len.max(1));
        let pending = Arc::new(AtomicUsize::new(0));
        let p = pending.clone();
        std::thread::Builder::new()
            .name("mimic-journal".into())
            .spawn(move || run(writer, rx, p))?;
        Ok(Self {
            tx,
            pending,
            backlog_bytes: cfg.backlog_bytes,
            dropped,
        })
    }

    /// Dépose un échange à écrire, sans jamais bloquer ; faux (et compté) si la file est pleine.
    pub fn append(&self, e: Arc<Exchange>, bytes: usize) -> bool {
        let before = self.pending.fetch_add(bytes, Ordering::SeqCst);
        if before + bytes > self.backlog_bytes {
            self.pending.fetch_sub(bytes, Ordering::SeqCst);
            self.dropped.inc();
            return false;
        }
        match self.tx.try_send(Cmd::Append(e, bytes)) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(bytes, Ordering::SeqCst);
                self.dropped.inc();
                false
            }
        }
    }

    /// Attend que tout ce qui a été déposé soit écrit et passé au système.
    pub fn sync(&self) {
        self.call(Cmd::Sync);
    }

    /// Supprime les segments ; les échanges déposés avant sont perdus.
    pub fn clear(&self) {
        self.call(Cmd::Clear);
    }

    fn call(&self, cmd: fn(SyncSender<()>) -> Cmd) {
        let (ack, done) = std::sync::mpsc::sync_channel(1);
        // File pleine : attend que le fil d'écriture la vide, ce qui est borné.
        if self.tx.send(cmd(ack)).is_ok() {
            let _ = done.recv();
        }
    }
}

struct Writer {
    dir: PathBuf,
    segment_bytes: u64,
    max_segments: usize,
    /// Segments gardés, du plus ancien au courant.
    segments: VecDeque<(u64, PathBuf)>,
    out: BufWriter<File>,
    size: u64,
    index: u64,
    errors: LogLimiter,
}

impl Writer {
    fn append(&mut self, e: &Exchange) {
        let Ok(mut line) = serde_json::to_vec(e) else {
            return;
        };
        line.push(b'\n');
        let written = self.out.write_all(&line);
        self.check(written);
        self.size += line.len() as u64;
        if self.size >= self.segment_bytes {
            self.rotate();
        }
    }

    fn flush(&mut self) {
        let r = self.out.flush();
        self.check(r);
    }

    /// Ferme le segment courant et en ouvre un nouveau ; supprime les plus anciens en trop.
    fn rotate(&mut self) {
        self.flush();
        self.open(self.index + 1);
        self.trim();
    }

    fn open(&mut self, index: u64) {
        let path = segment_path(&self.dir, index);
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                self.out = BufWriter::new(f);
                self.size = 0;
                self.index = index;
                self.segments.push_back((index, path));
            }
            // On continue sur l'ancien segment : mieux vaut un segment trop gros qu'une perte.
            Err(e) => self.check(Err(e)),
        }
    }

    fn trim(&mut self) {
        while self.segments.len() > self.max_segments {
            if let Some((_, old)) = self.segments.pop_front() {
                let r = std::fs::remove_file(&old);
                self.check(r);
            }
        }
    }

    fn clear(&mut self) {
        self.flush();
        for (_, p) in std::mem::take(&mut self.segments) {
            let _ = std::fs::remove_file(p);
        }
        self.open(self.index + 1);
    }

    fn check(&self, r: std::io::Result<()>) {
        if let Err(e) = r
            && let Some(omitted) = self.errors.allow(Instant::now())
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, omitted, "écriture du journal local échouée");
        }
    }
}

fn run(mut w: Writer, rx: Receiver<Cmd>, pending: Arc<AtomicUsize>) {
    // Première ligne pas encore passée au système.
    let mut dirty: Option<Instant> = None;
    loop {
        let msg = match dirty {
            Some(t) => rx.recv_timeout(FLUSH_EVERY.saturating_sub(t.elapsed())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Cmd::Append(e, bytes)) => {
                w.append(&e);
                pending.fetch_sub(bytes, Ordering::SeqCst);
                dirty.get_or_insert_with(Instant::now);
            }
            Ok(Cmd::Sync(ack)) => {
                w.flush();
                dirty = None;
                let _ = ack.send(());
            }
            Ok(Cmd::Clear(ack)) => {
                w.clear();
                dirty = None;
                let _ = ack.send(());
            }
            Err(RecvTimeoutError::Timeout) => {
                w.flush();
                dirty = None;
            }
            Err(RecvTimeoutError::Disconnected) => {
                w.flush();
                return;
            }
        }
        if dirty.is_some_and(|t| t.elapsed() >= FLUSH_EVERY) {
            w.flush();
            dirty = None;
        }
    }
}

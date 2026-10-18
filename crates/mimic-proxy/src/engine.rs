//! Moteur de mock en service (spec §5.3).
//!
//! - Le moteur est une vue immuable (`MockSet`), publiée d'un bloc dans un `ArcSwap` : une
//!   requête la lit sans verrou. Les constructions, rares, sont sérialisées par un verrou que
//!   les requêtes avec correspondance ne prennent jamais.
//! - Il est construit à la bascule (mode ou snapshot). Sans snapshot, le store change encore :
//!   un moteur périmé continue de servir et un moteur à jour est reconstruit en tâche de fond,
//!   au plus une fois par seconde ; une requête sans correspondance dans un moteur périmé attend
//!   un moteur à jour avant de conclure.
//! - La réponse de chaque enregistrement (headers, body réencodé) est préparée une seule fois.
//! - L'attente de la latence rejouée est faite à la microseconde (timerfd).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use http::StatusCode;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use mimic_core::mock::{MockConfig, MockEngine, MockResponse};
use mimic_core::model::{Body, Direction};
use mimic_core::snapshot::Snapshot;

use crate::capture::encode;
use crate::config::Mode;
use crate::metrics::TriggerLabel;
use crate::state::AppState;

/// Intervalle minimal entre deux reconstructions en tâche de fond.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Ce qui a déclenché une construction (`mimic_mock_engine_builds_total{trigger}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Bascule de mode ou de snapshot.
    Switch,
    /// Reconstruction en tâche de fond après un changement du store.
    Refresh,
    /// Requête sans correspondance dans un moteur périmé.
    Unmatched,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Switch => "switch",
            Trigger::Refresh => "refresh",
            Trigger::Unmatched => "unmatched",
        }
    }
}

/// Données dont un moteur a été construit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Snapshot imposé par le contrôleur : figé, jamais périmé.
    Snapshot(String),
    /// Échanges sortants du store, à cette `outbound_version`.
    Store(u64),
}

/// Réponse prête à servir : seuls le header `x-mimic-match` et la latence varient d'une
/// requête à l'autre.
#[derive(Debug)]
pub struct Prepared {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Moteur publié, avec la réponse préparée de chaque enregistrement rejouable.
pub struct MockSet {
    pub source: Source,
    pub engine: MockEngine,
    prepared: Vec<OnceLock<Prepared>>,
}

impl MockSet {
    pub fn new(source: Source, engine: MockEngine) -> Self {
        let prepared = (0..engine.replayable()).map(|_| OnceLock::new()).collect();
        Self {
            source,
            engine,
            prepared,
        }
    }

    /// Réponse préparée de l'enregistrement choisi, construite à son premier rejeu.
    pub fn prepared(&self, r: &MockResponse) -> &Prepared {
        self.prepared[r.record].get_or_init(|| prepare(r))
    }
}

fn prepare(r: &MockResponse) -> Prepared {
    let mut headers = HeaderMap::new();
    for h in r.headers() {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(h.name.as_bytes()),
            HeaderValue::from_str(&h.value),
        ) {
            headers.append(n, v);
        }
    }
    let data: &[u8] = match r.body() {
        Body::Captured { data } => data,
        _ => &[],
    };
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // Body réencodé dans son encodage d'origine (spec §5.2), une fois pour toutes.
    let body = match encoding {
        Some(enc) if !data.is_empty() => match encode(&enc, data) {
            Some(encoded) => Bytes::from(encoded),
            None => {
                headers.remove(http::header::CONTENT_ENCODING);
                Bytes::copy_from_slice(data)
            }
        },
        _ => Bytes::copy_from_slice(data),
    };
    Prepared {
        status: StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK),
        headers,
        body,
    }
}

/// Moteur courant et état de ses constructions.
pub struct Engines {
    current: ArcSwapOption<MockSet>,
    /// Sérialise les constructions et leur publication.
    build: Mutex<()>,
    /// Une reconstruction en tâche de fond est en cours.
    refreshing: AtomicBool,
    /// Début de la dernière reconstruction en tâche de fond, en ms depuis `epoch`.
    last_refresh_ms: AtomicU64,
    epoch: Instant,
}

impl Default for Engines {
    fn default() -> Self {
        Self {
            current: ArcSwapOption::empty(),
            build: Mutex::new(()),
            refreshing: AtomicBool::new(false),
            last_refresh_ms: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }
}

pub(crate) fn mock_mode(m: Mode) -> bool {
    matches!(m, Mode::Mock | Mode::MockPartial)
}

/// Exécute un calcul long sans bloquer les autres tâches d'un worker tokio.
fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

impl AppState {
    fn mock_config(&self) -> MockConfig {
        let m = &self.config.mock;
        MockConfig {
            strategy: m.strategy.into(),
            latency_factor: m.latency_factor,
            match_headers: m.match_headers.clone(),
            seed: m.seed,
        }
    }

    /// Moteur courant, sans verrou ; `None` hors du mock.
    pub fn mock_engine(&self) -> Option<Arc<MockSet>> {
        self.engines.current.load_full()
    }

    /// Vrai si ce moteur a été construit avant la version `seen` du store sortant (lue à
    /// l'arrivée d'une requête) : des enregistrements arrivés depuis lui manquent.
    pub fn is_stale(&self, set: &MockSet, seen: u64) -> bool {
        matches!(set.source, Source::Store(v) if v < seen)
    }

    /// Construit un moteur sur `snapshot`, ou sur le store sans snapshot ; à appeler sous le
    /// verrou de construction, qui sérialise aussi la publication.
    fn build_locked(&self, snapshot: Option<&Snapshot>, trigger: Trigger) -> Arc<MockSet> {
        let set = match snapshot {
            Some(s) => MockSet::new(
                Source::Snapshot(s.id.clone()),
                MockEngine::new(s.exchanges.iter(), self.mock_config()),
            ),
            None => {
                // Version lue avant les échanges : un ajout concurrent rend le moteur périmé.
                let version = self.store.outbound_version();
                let exchanges = self.store.shared_direction(Direction::Outbound);
                MockSet::new(
                    Source::Store(version),
                    MockEngine::new(exchanges, self.mock_config()),
                )
            }
        };
        let set = Arc::new(set);
        self.metrics
            .mock_engine_builds
            .get_or_create(&TriggerLabel {
                trigger: trigger.as_str().into(),
            })
            .inc();
        set
    }

    /// Moteur à jour des données courantes : celui en service s'il l'est déjà (une autre
    /// requête vient de le reconstruire), sinon un nouveau.
    ///
    /// Sans snapshot, « à jour » veut dire construit sur la version `seen` du store ou une
    /// suivante : des requêtes qui attendent ensemble sont servies par la même construction.
    fn fresh_locked(&self, trigger: Trigger, seen: u64) -> Arc<MockSet> {
        let snapshot = self.snapshot();
        if let Some(set) = self.mock_engine() {
            let fresh = match (&set.source, &snapshot) {
                (Source::Snapshot(id), Some(s)) => *id == s.id,
                (Source::Store(_), None) => !self.is_stale(&set, seen),
                _ => false,
            };
            if fresh {
                return set;
            }
        }
        let set = self.build_locked(snapshot.as_deref(), trigger);
        // Une requête encore en vol après la sortie du mock est servie sans republier de
        // moteur : il resterait en mémoire hors du mock.
        if mock_mode(self.mode()) {
            self.engines.current.store(Some(set.clone()));
        }
        set
    }

    /// Moteur à jour, construit hors des workers tokio si besoin : premier moteur, ou requête
    /// sans correspondance dans un moteur périmé.
    pub async fn fresh_engine(self: &Arc<Self>, trigger: Trigger, seen: u64) -> Arc<MockSet> {
        let state = self.clone();
        let build = move || {
            let _guard = state
                .engines
                .build
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.fresh_locked(trigger, seen)
        };
        match tokio::task::spawn_blocking(build.clone()).await {
            Ok(set) => set,
            // Pool bloquant indisponible (arrêt du runtime) : construction sur place.
            Err(_) => build(),
        }
    }

    /// Moteur périmé en service : reconstruction en tâche de fond, au plus une à la fois et
    /// une par `REFRESH_INTERVAL`.
    pub fn schedule_refresh(self: &Arc<Self>) {
        let e = &self.engines;
        let now = e.epoch.elapsed().as_millis() as u64;
        let last = e.last_refresh_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < REFRESH_INTERVAL.as_millis() as u64 && last != 0 {
            return;
        }
        if e.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        e.last_refresh_ms.store(now.max(1), Ordering::Relaxed);
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            {
                let _guard = state
                    .engines
                    .build
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                // Rien à publier si le mock a été quitté entre-temps.
                if mock_mode(state.mode()) && state.mock_engine().is_some() {
                    state.fresh_locked(Trigger::Refresh, state.store.outbound_version());
                }
            }
            state.engines.refreshing.store(false, Ordering::Release);
        });
    }

    /// Avant d'entrer en mock : moteur construit s'il n'y en a pas, pour que la première
    /// requête mockée le trouve prêt.
    pub(crate) fn prepare_mock(&self) {
        let _guard = self.engines.build.lock().unwrap_or_else(|e| e.into_inner());
        if self.mock_engine().is_none() {
            let snapshot = self.snapshot();
            let set = blocking(|| self.build_locked(snapshot.as_deref(), Trigger::Switch));
            self.engines.current.store(Some(set));
        }
    }

    /// Après la sortie du mock : moteur libéré. Sous le verrou de construction, pour qu'une
    /// reconstruction en cours ne le republie pas.
    pub(crate) fn release_mock(&self) {
        let _guard = self.engines.build.lock().unwrap_or_else(|e| e.into_inner());
        if !mock_mode(self.mode()) {
            self.engines.current.store(None);
        }
    }

    /// Publie `snap` (ou son retrait) ; en mock, avec le moteur construit dessus.
    pub(crate) fn publish_snapshot(&self, snap: Option<Arc<Snapshot>>) {
        let _guard = self.engines.build.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged = {
            let current = self.snapshot.lock().expect("verrou du snapshot");
            current.as_ref().map(|s| &s.id) == snap.as_ref().map(|s| &s.id)
        };
        *self.snapshot.lock().expect("verrou du snapshot") = snap.clone();
        if mock_mode(self.mode()) && !unchanged {
            let set = blocking(|| self.build_locked(snap.as_deref(), Trigger::Switch));
            self.engines.current.store(Some(set));
        }
    }

    /// Mode de démarrage déjà en mock : moteur construit avant la première requête.
    pub fn ensure_mock_engine(&self) {
        if mock_mode(self.mode()) {
            self.prepare_mock();
        }
    }
}

/// Descripteur timerfd utilisable par `AsyncFd`.
struct Timer(nix::sys::timerfd::TimerFd);

impl std::os::fd::AsRawFd for Timer {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsFd;
        self.0.as_fd().as_raw_fd()
    }
}

/// Retard moyen du réveil après l'échéance d'un timerfd, en microsecondes (moyenne mobile).
/// Propre à la machine et à sa charge : l'échéance est armée d'autant plus tôt, pour que les
/// réponses partent en moyenne à l'heure.
static WAKE_LAG_US: AtomicU64 = AtomicU64::new(0);

/// Correction plafonnée : au-delà, le retard vient de la charge, pas du minuteur.
const MAX_WAKE_LAG_US: u64 = 1_000;

/// Attend jusqu'à `deadline`, à la microseconde près : le temporisateur de tokio arrondit à
/// la milliseconde supérieure, même pour une attente nulle. Échéance passée : aucune attente.
/// Repli sur tokio si le timerfd n'est pas disponible.
pub async fn sleep_until(deadline: Instant) {
    use nix::sys::time::TimeSpec;
    use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
    let lag = Duration::from_micros(WAKE_LAG_US.load(Ordering::Relaxed));
    let armed = Instant::now();
    let remaining = deadline.saturating_duration_since(armed);
    let wait = remaining.saturating_sub(lag);
    if wait.is_zero() {
        return;
    }
    let timer = TimerFd::new(
        ClockId::CLOCK_MONOTONIC,
        TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC,
    )
    .ok()
    .filter(|t| {
        t.set(
            Expiration::OneShot(TimeSpec::from_duration(wait)),
            TimerSetTimeFlags::empty(),
        )
        .is_ok()
    })
    .and_then(|t| {
        // Seul constructeur sans `unsafe` (interdit dans le dépôt), déprécié parce qu'il ne
        // peut pas vérifier que le descripteur reste ouvert : ici `Timer` en est propriétaire
        // (`TimerFd` possède son `OwnedFd`) pendant toute la vie de l'`AsyncFd`.
        #[allow(deprecated)]
        let fd = tokio::io::unix::AsyncFd::with_interest(Timer(t), tokio::io::Interest::READABLE);
        fd.ok()
    });
    match timer {
        Some(fd) => {
            let _ = fd.readable().await;
            // Retard observé de ce réveil, intégré à la moyenne (poids 1/8).
            let late = armed.elapsed().saturating_sub(wait).as_micros() as u64;
            let late = late.min(MAX_WAKE_LAG_US);
            let old = WAKE_LAG_US.load(Ordering::Relaxed);
            let new = (old * 7 + late) / 8;
            WAKE_LAG_US.store(new, Ordering::Relaxed);
        }
        None => tokio::time::sleep_until(deadline.into()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimic_core::model::{Exchange, Header};

    fn gzip_record() -> Exchange {
        Exchange {
            id: "gz-1".into(),
            direction: Direction::Outbound,
            started_at_ms: 1,
            scheme: "http".into(),
            authority: "dep:80".into(),
            method: "GET".into(),
            path: "/gz".into(),
            query: vec![],
            req_headers: vec![],
            req_body: Body::Empty,
            status: 200,
            resp_headers: vec![
                Header::new("content-type", "application/json"),
                Header::new("content-encoding", "gzip"),
                Header::new("transfer-encoding", "chunked"),
            ],
            resp_body: Body::captured(br#"{"compressed":true}"#.to_vec()),
            ttfb_us: 10,
            total_us: 10,
            trace_id: None,
            redactions: vec![],
        }
    }

    #[test]
    fn mck_18_encoded_body_is_prepared_once() {
        let engine = MockEngine::new(vec![gzip_record()], MockConfig::default());
        let set = MockSet::new(Source::Store(0), engine);
        let req = mimic_core::mock::MockRequest {
            authority: "dep:80".into(),
            method: "GET".into(),
            path: "/gz".into(),
            ..Default::default()
        };
        let respond = || match set.engine.respond(&req) {
            mimic_core::mock::MockDecision::Matched(r) => r,
            _ => panic!("aucune correspondance"),
        };
        let (a, b) = (respond(), respond());
        let (pa, pb) = (set.prepared(&a), set.prepared(&b));
        // Même réponse préparée, mêmes octets partagés : aucun nouvel encodage.
        assert!(std::ptr::eq(pa, pb));
        assert_eq!(pa.body.as_ptr(), pb.body.as_ptr());
        assert_eq!(pa.headers["content-encoding"], "gzip");
        assert!(!pa.headers.contains_key("transfer-encoding"));
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&pa.body[..]), &mut out)
            .unwrap();
        assert_eq!(out, r#"{"compressed":true}"#);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sleep_until_is_precise_and_skips_past_deadlines() {
        let t = Instant::now();
        sleep_until(t).await;
        assert!(
            t.elapsed() < Duration::from_micros(500),
            "{:?}",
            t.elapsed()
        );
        let mut late = vec![];
        for _ in 0..50 {
            let t = Instant::now();
            sleep_until(t + Duration::from_micros(1_500)).await;
            late.push(t.elapsed().as_micros() as u64);
        }
        late.sort_unstable();
        // tokio arrondirait à 2 ms et au-delà.
        assert!(late[25] < 1_900, "{late:?}");
    }
}

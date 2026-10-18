//! Métriques du contrôleur (spec §9.2b).

use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabel {
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabel {
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StateLabel {
    pub state: String,
}

pub struct Metrics {
    registry: Registry,
    pub segments: Family<ResultLabel, Counter>,
    pub auth_failures: Family<ReasonLabel, Counter>,
    pub workloads: Gauge,
    pub pods: Family<StateLabel, Gauge>,
    pub snapshots: Counter,
    pub exchanges: Gauge,
    /// Exemples par endpoint gardés, tous workloads (spec §5.5.3).
    pub exemplars: Gauge,
    /// Taille sérialisée des exemples gardés, tous workloads.
    pub exemplars_bytes: Gauge,
    /// Exemples refusés : `count` (`max_exemplars_per_workload`), `bytes` (bornes en octets).
    pub exemplars_refused: Family<ReasonLabel, Counter>,
    /// 1 pour la réplique active, 0 pour la passive (spec §5.5.4).
    pub leader: Gauge,
    /// Durée de la dernière prise de relais, en secondes.
    pub takeover_seconds: Gauge<f64, AtomicU64>,
}

impl Default for Metrics {
    fn default() -> Self {
        let mut registry = Registry::default();
        let segments = Family::<ResultLabel, Counter>::default();
        let auth_failures = Family::<ReasonLabel, Counter>::default();
        let workloads = Gauge::default();
        let pods = Family::<StateLabel, Gauge>::default();
        let snapshots = Counter::default();
        let exchanges = Gauge::default();
        let exemplars = Gauge::default();
        let exemplars_bytes = Gauge::default();
        let exemplars_refused = Family::<ReasonLabel, Counter>::default();
        let leader = Gauge::default();
        let takeover_seconds = Gauge::<f64, AtomicU64>::default();
        // Les deux raisons sont exposées dès le départ, à zéro.
        for reason in ["count", "bytes"] {
            let _ = exemplars_refused.get_or_create(&ReasonLabel {
                reason: reason.into(),
            });
        }
        registry.register(
            "mimic_controller_segments",
            "Segments reçus des sidecars",
            segments.clone(),
        );
        registry.register(
            "mimic_controller_auth_failures",
            "Appels de sidecars refusés",
            auth_failures.clone(),
        );
        registry.register(
            "mimic_controller_workloads",
            "Workloads connus",
            workloads.clone(),
        );
        registry.register(
            "mimic_controller_pods",
            "Pods par état d'alignement",
            pods.clone(),
        );
        registry.register(
            "mimic_controller_snapshots",
            "Snapshots construits",
            snapshots.clone(),
        );
        registry.register(
            "mimic_controller_exchanges",
            "Échanges en mémoire, tous workloads",
            exchanges.clone(),
        );
        registry.register(
            "mimic_controller_exemplars",
            "Exemples par endpoint gardés, tous workloads",
            exemplars.clone(),
        );
        registry.register(
            "mimic_controller_exemplars_refused",
            "Exemples refusés : plafond d'endpoints (count) ou bornes en octets (bytes)",
            exemplars_refused.clone(),
        );
        registry.register(
            "mimic_controller_exemplars_bytes",
            "Taille sérialisée des exemples gardés, tous workloads",
            exemplars_bytes.clone(),
        );
        registry.register(
            "mimic_controller_leader",
            "Réplique active (1) ou passive (0)",
            leader.clone(),
        );
        registry.register(
            "mimic_controller_takeover_seconds",
            "Durée de la dernière prise de relais : rechargement de l'état persisté jusqu'à la marque",
            takeover_seconds.clone(),
        );
        Self {
            registry,
            segments,
            auth_failures,
            workloads,
            pods,
            snapshots,
            exchanges,
            exemplars,
            exemplars_bytes,
            exemplars_refused,
            leader,
            takeover_seconds,
        }
    }
}

impl Metrics {
    /// Compte des exemples refusés, en nombre puis en octets.
    pub fn refuse_exemplars(&self, (count, bytes): (u64, u64)) {
        for (reason, n) in [("count", count), ("bytes", bytes)] {
            if n > 0 {
                self.exemplars_refused
                    .get_or_create(&ReasonLabel {
                        reason: reason.into(),
                    })
                    .inc_by(n);
            }
        }
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        let _ = prometheus_client::encoding::text::encode(&mut out, &self.registry);
        out
    }
}

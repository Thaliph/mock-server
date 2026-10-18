//! Métriques du contrôleur (spec §9.2b).

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
    /// Candidats sans exemple faute de place (`max_exemplars_per_workload`).
    pub exemplars_refused: Counter,
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
        let exemplars_refused = Counter::default();
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
            "Candidats sans exemple : plafond d'endpoints du workload atteint",
            exemplars_refused.clone(),
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
            exemplars_refused,
        }
    }
}

impl Metrics {
    pub fn encode(&self) -> String {
        let mut out = String::new();
        let _ = prometheus_client::encoding::text::encode(&mut out, &self.registry);
        out
    }
}

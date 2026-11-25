//! `mimic_gateway_*` (spec §9, §7.10 "Metrics and logs"). No label holds a Gateway name, a
//! namespace, a hostname or any other `spec` value (GW-17).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::mesh::metrics::{Labels, label};

/// The `result` of one Gateway reconcile (§7.10 "Metrics and logs").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Objects and status in line, after writes or none.
    Ok,
    /// Not accepted: no object.
    Invalid,
    /// An object of a generated name is not managed.
    Conflict,
    /// A request failed.
    Error,
}

impl Outcome {
    pub const ALL: [Outcome; 4] = [
        Outcome::Ok,
        Outcome::Invalid,
        Outcome::Conflict,
        Outcome::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Invalid => "invalid",
            Outcome::Conflict => "conflict",
            Outcome::Error => "error",
        }
    }
}

/// The `state` of a Gateway of a mimic class at the last pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayState {
    Programmed,
    Pending,
    NotAccepted,
}

impl GatewayState {
    pub const ALL: [GatewayState; 3] = [
        GatewayState::Programmed,
        GatewayState::Pending,
        GatewayState::NotAccepted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            GatewayState::Programmed => "programmed",
            GatewayState::Pending => "pending",
            GatewayState::NotAccepted => "not_accepted",
        }
    }
}

#[derive(Clone, Default)]
pub struct GatewayMetrics {
    pub(crate) reconciles: Family<Labels, Counter>,
    pub(crate) gateways: Family<Labels, Gauge>,
}

impl GatewayMetrics {
    /// Registers the two families, every series at 0.
    pub fn register(registry: &mut Registry) -> GatewayMetrics {
        let metrics = GatewayMetrics::default();
        registry.register(
            "gateway_reconciles",
            "Gateway reconciles, by result (ok, invalid, conflict or error)",
            metrics.reconciles.clone(),
        );
        registry.register(
            "gateway_gateways",
            "Gateways of mimic classes at the last pass, by state (programmed, pending or not_accepted)",
            metrics.gateways.clone(),
        );
        for result in Outcome::ALL {
            let _ = metrics
                .reconciles
                .get_or_create(&label("result", result.as_str()));
        }
        for state in GatewayState::ALL {
            let _ = metrics
                .gateways
                .get_or_create(&label("state", state.as_str()));
        }
        metrics
    }

    pub(crate) fn count(&self, result: Outcome) {
        self.reconciles
            .get_or_create(&label("result", result.as_str()))
            .inc();
    }

    pub(crate) fn set(&self, state: GatewayState, n: usize) {
        self.gateways
            .get_or_create(&label("state", state.as_str()))
            .set(i64::try_from(n).unwrap_or(i64::MAX));
    }
}

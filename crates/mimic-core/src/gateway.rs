//! The mimic Gateway, pure part (spec §7.10; design `docs/design/mimic-mesh.md` §5, §15): the
//! plain input structs the controller fills from a `Gateway` and its `GatewayClass`, their
//! acceptance and the names of the generated objects.

use std::collections::BTreeSet;

pub mod accept;
pub mod input;
pub mod objects;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests_accept;
#[cfg(test)]
mod tests_names;
#[cfg(test)]
mod tests_prop;

/// The `controllerName` of the mimic GatewayClasses.
pub const CONTROLLER_NAME: &str = "mimic.platform.io/gateway-controller";
/// The gateway's admin port (`/readyz`, `/healthz`, `/metrics`): never a listener port.
pub const ADMIN_PORT: u16 = 15100;
/// The only `spec.infrastructure.annotations` key a Gateway may set.
pub const REPLICAS_ANNOTATION: &str = "mimic.platform.io/replicas";
/// Replicas of a Gateway without the annotation (capped by `Settings::max_replicas`).
pub const DEFAULT_REPLICAS: u32 = 2;
/// Longest Gateway, GatewayClass and generated object name.
pub const MAX_NAME: usize = 63;
/// Most listeners of a Gateway (the CRD's `maxItems`; `MIMIC_GATEWAY_PORTS` takes at most 64).
pub const MAX_LISTENERS: usize = 64;

/// The controller configuration the pure functions read (§10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `MIMIC_GATEWAY_NAMESPACES`.
    pub namespaces: BTreeSet<String>,
    /// `MIMIC_GATEWAY_MAX_REPLICAS`, 1 to 100.
    pub max_replicas: u32,
}

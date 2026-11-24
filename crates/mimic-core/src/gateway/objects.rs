//! The objects of an accepted Gateway (spec §7.10 "Generated objects").

/// The name of the Deployment, Service and PodDisruptionBudget of the Gateway `gateway` (GW-01).
pub fn name(gateway: &str) -> String {
    let _ = gateway;
    String::new()
}

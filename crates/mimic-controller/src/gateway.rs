//! The Gateway controller (spec §7.10; design `docs/design/mimic-mesh.md` §5, §15): reflectors on
//! `gatewayclasses` and `gateways`, their conversion into `mimic_core::gateway::input`, and the
//! reconciler that writes their status. What a status or an object holds is decided by
//! `mimic_core::gateway`, pure functions; this module is the Kubernetes I/O around them.

pub mod cluster;
pub mod config;
pub mod convert;
pub mod metrics;
pub mod reconcile;
pub mod watch;

pub use cluster::KubeCluster;
pub use config::from_env;
pub use metrics::GatewayMetrics;
pub use reconcile::Reconciler;
pub use watch::start;

#[cfg(test)]
mod tests_config;
#[cfg(test)]
mod tests_convert;
#[cfg(test)]
mod tests_reconcile;
#[cfg(test)]
mod tests_watch;

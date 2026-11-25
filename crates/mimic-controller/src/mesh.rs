//! The mesh view of the controller (spec §7.2, §7.9; design `docs/design/mimic-mesh.md` §4):
//! reflectors on the objects a view is built from, the hub that coalesces their changes and
//! serves the views, and the status of the `MimicConfig`s. What a view holds is decided by
//! `mimic_core::mesh`, a pure function; this module is the process plumbing around it.

pub(crate) mod config;
pub(crate) mod convert;
mod hub;
pub(crate) mod metrics;
pub mod status;
pub(crate) mod table;
pub(crate) mod watch;

pub use config::MeshConfig;
pub use hub::{Answer, Entry, Event, Hub};
pub use metrics::MeshMetrics;
pub use watch::spawn;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod tests_config;
#[cfg(test)]
mod tests_convert;
#[cfg(test)]
mod tests_route;
#[cfg(test)]
mod tests_route_status;
#[cfg(test)]
pub(crate) mod tests_watch;

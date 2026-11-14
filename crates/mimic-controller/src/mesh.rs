//! The mesh view of the controller (spec §7.2, §7.9; design `docs/design/mimic-mesh.md` §4):
//! reflectors on the objects a view is built from, the hub that coalesces their changes and
//! serves the views, and the status of the `MimicConfig`s. What a view holds is decided by
//! `mimic_core::mesh`, a pure function; this module is the process plumbing around it.

mod config;
mod convert;
mod hub;
mod metrics;
pub mod status;
mod watch;

pub use config::MeshConfig;
#[cfg(test)]
pub use hub::Event;
pub use hub::{Answer, Entry, Hub};
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
mod tests_watch;

//! `mimic-sidecar` internals, split from the binary so that tests can drive them in process.

pub mod probes;
pub mod connection;
pub mod listener;
pub mod opaque;

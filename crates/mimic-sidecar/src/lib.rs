//! `mimic-sidecar` internals, split from the binary so that tests can drive them in process.

pub mod connection;
pub mod http;
pub mod listener;
pub mod opaque;
pub mod probes;

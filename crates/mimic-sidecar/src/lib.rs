//! `mimic-sidecar` internals, split from the binary so that tests can drive them in process.

pub mod access;
pub mod connection;
pub mod exchange;
pub mod http;
pub mod http1;
pub mod listener;
pub mod opaque;
pub mod probes;
pub mod red;

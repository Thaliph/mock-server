//! The gateway role of `mimic-sidecar` (spec §7.10, `MIMIC_ROLE=gateway`): listeners that answer
//! every request `404` `no-route` until routing exists (#457), bounded for untrusted clients.

pub mod config;
pub mod serve;

use std::process::ExitCode;

use mimic_runtime::config::Env;

/// The whole process of the gateway role.
pub fn main(_env: &Env) -> ExitCode {
    todo!()
}

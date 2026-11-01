//! Endpoint discovery (spec §6): the pure part. Path templating, latency histogram and the
//! mergeable report. Redaction, schemas, volatility and samples are added by later steps.

pub mod histogram;
pub mod report;
pub mod template;

#[cfg(test)]
mod tests;

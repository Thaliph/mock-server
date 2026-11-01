//! Endpoint discovery (spec §6): the pure part. Path templating, redaction, latency histogram and the
//! mergeable report. Schemas, volatility and samples are added by later steps.

pub mod histogram;
pub mod redact;
pub mod report;
pub mod template;

#[cfg(test)]
mod tests;

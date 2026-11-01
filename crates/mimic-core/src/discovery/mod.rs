//! Endpoint discovery (spec §6): the pure part. Path templating, redaction, latency histogram and the
//! mergeable report. Schemas and volatility are mergeable; samples are added by a later step.

pub mod histogram;
pub mod observed;
pub mod redact;
pub mod report;
pub mod schema;
pub mod template;
pub mod volatile;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_samples;
#[cfg(test)]
mod tests_schema;

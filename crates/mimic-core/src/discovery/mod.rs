//! Endpoint discovery (spec §6): the pure part. Path templating, redaction, latency histogram and the
//! mergeable report, with schemas, volatility and samples.

pub mod histogram;
pub mod observed;
pub mod redact;
pub mod report;
pub mod sample;
pub mod schema;
pub mod template;
pub mod volatile;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_followup;
#[cfg(test)]
mod tests_samples;
#[cfg(test)]
mod tests_schema;

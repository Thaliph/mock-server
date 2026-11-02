//! Endpoint discovery (spec §6): the pure part. Path templating, redaction, latency histogram and the
//! mergeable report, with schemas, volatility and samples.

pub mod check;
pub mod histogram;
pub mod observed;
pub mod redact;
pub mod report;
pub mod sample;
pub mod schema;
pub mod status;
pub mod store;
pub mod template;
pub mod volatile;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_check;
#[cfg(test)]
mod tests_ctl;
#[cfg(test)]
mod tests_followup;
#[cfg(test)]
mod tests_samples;
#[cfg(test)]
mod tests_schema;
#[cfg(test)]
mod tests_status;
#[cfg(test)]
mod tests_store;

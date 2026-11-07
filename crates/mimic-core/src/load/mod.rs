//! Load test planning (spec §7.8): the pure part. A plan is built from the stored inbound
//! report and the test's spec; a request is built from the plan, a random seed and the time.
//! No I/O, no clock and no random source: the caller hands them in.

pub mod plan;
pub mod request;

#[cfg(test)]
mod tests;

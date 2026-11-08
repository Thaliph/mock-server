//! Load test planning (spec §7.8): the pure part. A plan is built from the stored inbound
//! report and the test's spec; a request is built from the plan, a random seed and the time;
//! the schedule, the results, the gate and the Job are functions of numbers and names.
//! No I/O, no clock and no random source: the caller hands them in.

pub mod gate;
pub mod histogram;
pub mod job;
pub mod plan;
pub mod request;
pub mod schedule;
pub mod summary;
pub mod ticket;
pub mod validate;

#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;

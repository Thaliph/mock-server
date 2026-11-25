//! The status of a Gateway and of a GatewayClass (spec §7.10 "Status").

use serde_json::Value;

use super::accept::Acceptance;
use super::input::{Gateway, GatewayClass, Observed};
use crate::mesh::policy::Condition;

/// What to send for a status.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The cluster holds it already.
    Keep,
    /// The whole server-side apply body.
    Write(Value),
}

pub fn programmed(acceptance: &Acceptance, observed: &Observed) -> Condition {
    let _ = (acceptance, observed);
    Condition::ok("Programmed")
}

pub fn gateway(gw: &Gateway, acceptance: &Acceptance, observed: &Observed, now: &str) -> Plan {
    let _ = (gw, acceptance, observed, now);
    Plan::Keep
}

pub fn class(class: &GatewayClass, now: &str) -> Plan {
    let _ = (class, now);
    Plan::Keep
}

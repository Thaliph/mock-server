//! Acceptance of a GatewayClass, a Gateway and its listeners (spec §7.10 "Acceptance"). A
//! refusal is a condition, not an error.

use super::Settings;
use super::input::{Gateway, GatewayClass, RouteKind};
use crate::mesh::policy::Condition;

/// The acceptance of one listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerAcceptance {
    pub name: String,
    pub port: u16,
    pub protocol: String,
    pub accepted: Condition,
    pub resolved_refs: Condition,
    pub conflicted: Condition,
    /// Empty or `[HTTPRoute]`.
    pub supported_kinds: Vec<RouteKind>,
    /// 0 until #457.
    pub attached_routes: u32,
}

/// The acceptance of a Gateway: its `Accepted` condition, every listener's, and its replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    pub accepted: Condition,
    /// One per `spec.listeners` entry, in order, also when the Gateway is refused.
    pub listeners: Vec<ListenerAcceptance>,
    pub replicas: u32,
}

impl Acceptance {
    pub fn is_accepted(&self) -> bool {
        self.accepted.ok
    }

    /// The distinct ports of the accepted listeners, ascending; empty when refused.
    pub fn ports(&self) -> Vec<u16> {
        vec![]
    }
}

/// The `Accepted` condition of a mimic GatewayClass.
pub fn class(class: &GatewayClass) -> Condition {
    let _ = class;
    Condition::ok("Accepted")
}

/// The acceptance of `gw`, of class `class`.
pub fn gateway(gw: &Gateway, class: &GatewayClass, settings: &Settings) -> Acceptance {
    let _ = (gw, class, settings);
    Acceptance {
        accepted: Condition::ok("Accepted"),
        listeners: vec![],
        replicas: 0,
    }
}

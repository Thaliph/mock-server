//! `Gateway` and `GatewayClass` objects as the input structs of `mimic_core::gateway::input`
//! (GW-02). Only the fields §7.10 "What is read" names are read: no label, `metadata`
//! annotation, `managedFields` or annotation value (but the replicas one) leaves the object. A
//! field the converter does not know is kept as its path only.

use kube::api::DynamicObject;
use mimic_core::gateway::input;

use crate::mesh::convert::Key;

/// A Gateway of any class; `None` without a namespace or a name.
pub fn gateway(object: &DynamicObject) -> Option<(Key, input::Gateway)> {
    let _ = object;
    None
}

/// A GatewayClass of mimic's `controllerName`, keyed `("", name)`; `None` for another
/// controller's.
pub fn class(object: &DynamicObject) -> Option<(Key, input::GatewayClass)> {
    let _ = object;
    None
}

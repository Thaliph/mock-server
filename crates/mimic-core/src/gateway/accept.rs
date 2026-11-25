//! Acceptance of a GatewayClass, a Gateway and its listeners (spec §7.10 "Acceptance"). A
//! refusal is a condition, not an error. No message holds a listener hostname or an annotation
//! value: only field paths, annotation keys, protocols, class and listener names.

use std::collections::BTreeSet;

use super::input::{Gateway, GatewayClass, Infrastructure, Listener, RouteKind};
use super::{ADMIN_PORT, DEFAULT_REPLICAS, MAX_LISTENERS, MAX_NAME, REPLICAS_ANNOTATION, Settings};
use crate::mesh::input::GATEWAY_GROUP;
use crate::mesh::policy::Condition;
use crate::mesh::route::bounded;

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
        if !self.is_accepted() {
            return vec![];
        }
        let ports: BTreeSet<u16> = self
            .listeners
            .iter()
            .filter(|l| l.accepted.ok)
            .map(|l| l.port)
            .collect();
        ports.into_iter().collect()
    }
}

/// The `Accepted` condition of a mimic GatewayClass.
pub fn class(class: &GatewayClass) -> Condition {
    if class.name.chars().count() > MAX_NAME {
        // Its name is a label value of every generated object.
        return Condition::refused("Unsupported", "name longer than 63 characters".into());
    }
    if class.has_parameters_ref {
        return Condition::refused("InvalidParameters", "parametersRef is not supported".into());
    }
    Condition::ok("Accepted")
}

/// The acceptance of `gw`, of class `class`: the first case of §7.10 that applies.
pub fn gateway(gw: &Gateway, class: &GatewayClass, settings: &Settings) -> Acceptance {
    let listeners: Vec<ListenerAcceptance> = gw
        .listeners
        .iter()
        .enumerate()
        .map(|(i, l)| listener(i, l))
        .collect();
    let replicas = replicas(&gw.infrastructure, settings.max_replicas);
    let accepted = if !settings.namespaces.contains(&gw.namespace) {
        Condition::refused(
            "NamespaceNotAllowed",
            bounded(&format!(
                "namespace {} is not in gateway.namespaces",
                gw.namespace
            )),
        )
    } else if !self::class(class).ok {
        Condition::refused(
            "InvalidParameters",
            bounded(&format!("GatewayClass {} is not accepted", class.name)),
        )
    } else if let Some(path) = first_unsupported(gw) {
        Condition::refused("UnsupportedValue", bounded(&format!("unsupported: {path}")))
    } else if gw.has_addresses {
        Condition::refused(
            "UnsupportedAddress",
            "spec.addresses is not supported".into(),
        )
    } else if let Some(message) = invalid_parameters(&gw.infrastructure, settings.max_replicas) {
        Condition::refused("InvalidParameters", message)
    } else {
        let refused: Vec<&str> = listeners
            .iter()
            .filter(|l| !l.accepted.ok)
            .map(|l| l.name.as_str())
            .collect();
        if refused.len() == listeners.len() {
            Condition::refused("ListenersNotValid", "no valid listener".into())
        } else if refused.is_empty() {
            Condition::ok("Accepted")
        } else {
            Condition {
                ok: true,
                reason: "ListenersNotValid".into(),
                message: bounded(&format!("listeners not valid: {}", refused.join(", "))),
            }
        }
    };
    Acceptance {
        accepted,
        listeners,
        replicas: replicas.unwrap_or(DEFAULT_REPLICAS.min(settings.max_replicas)),
    }
}

/// Case 3: the first field the gateway cannot serve, in document order.
fn first_unsupported(gw: &Gateway) -> Option<String> {
    if gw.name.chars().count() > MAX_NAME {
        return Some("name longer than 63 characters".into());
    }
    if gw.oversize {
        return Some("gateway larger than 64 KiB".into());
    }
    let mut paths: Vec<String> = gw.unknown_fields.clone();
    if gw.has_tls {
        paths.push("spec.tls".into());
    }
    if gw
        .allowed_listeners_from
        .as_deref()
        .is_some_and(|f| f != "None")
    {
        paths.push("spec.allowedListeners.namespaces.from".into());
    }
    if gw.infrastructure.has_labels {
        paths.push("spec.infrastructure.labels".into());
    }
    paths.extend(
        gw.infrastructure
            .annotation_keys
            .iter()
            .filter(|k| *k != REPLICAS_ANNOTATION)
            .map(|k| format!("spec.infrastructure.annotations[{k}]")),
    );
    if gw.listeners.len() > MAX_LISTENERS {
        paths.push("spec.listeners".into());
    }
    first_in_order(paths)
}

/// Case 5: `parametersRef`, or a replicas value out of range. The annotation comes first in
/// document order.
fn invalid_parameters(infrastructure: &Infrastructure, max: u32) -> Option<String> {
    if infrastructure.replicas.is_some() && replicas(infrastructure, max).is_none() {
        return Some(format!(
            "spec.infrastructure.annotations[{REPLICAS_ANNOTATION}] must be 1 to {max}"
        ));
    }
    infrastructure
        .has_parameters_ref
        .then(|| "spec.infrastructure.parametersRef is not supported".into())
}

/// The replicas annotation when it is a decimal integer from 1 to `max`, without sign or
/// leading zero.
fn replicas(infrastructure: &Infrastructure, max: u32) -> Option<u32> {
    let value = infrastructure.replicas.as_deref()?;
    // Checked before the parse: `u32::from_str` takes `+3` and `03`.
    let digits = value.len() <= 3
        && value.starts_with(|c: char| ('1'..='9').contains(&c))
        && value.bytes().all(|b| b.is_ascii_digit());
    if !digits {
        return None;
    }
    value.parse::<u32>().ok().filter(|n| *n <= max)
}

fn listener(i: usize, l: &Listener) -> ListenerAcceptance {
    let accepted = if l.protocol != "HTTP" {
        Condition::refused(
            "UnsupportedProtocol",
            bounded(&format!("protocol {} is not supported", l.protocol)),
        )
    } else if l.port == ADMIN_PORT {
        Condition::refused(
            "PortUnavailable",
            format!("port {ADMIN_PORT} is the gateway admin port"),
        )
    } else if let Some(path) = listener_unsupported(i, l) {
        Condition::refused("UnsupportedValue", bounded(&format!("unsupported: {path}")))
    } else {
        Condition::ok("Accepted")
    };
    let http_route = RouteKind {
        group: GATEWAY_GROUP.into(),
        kind: "HTTPRoute".into(),
    };
    let (supported_kinds, resolved_refs) = match l.kinds.as_deref() {
        // The Gateway API reads an empty list as unset.
        None | Some([]) => (vec![http_route], Condition::ok("ResolvedRefs")),
        Some(kinds) => {
            let supported = if kinds.contains(&http_route) {
                vec![http_route.clone()]
            } else {
                vec![]
            };
            let refs = if kinds.iter().all(|k| *k == http_route) {
                Condition::ok("ResolvedRefs")
            } else {
                Condition::refused("InvalidRouteKinds", "only HTTPRoute is supported".into())
            };
            (supported, refs)
        }
    };
    ListenerAcceptance {
        name: l.name.clone(),
        port: l.port,
        protocol: l.protocol.clone(),
        accepted,
        resolved_refs,
        conflicted: Condition {
            ok: false,
            reason: "NoConflicts".into(),
            message: String::new(),
        },
        supported_kinds,
        attached_routes: 0,
    }
}

/// The first listener field the gateway cannot serve, in document order.
fn listener_unsupported(i: usize, l: &Listener) -> Option<String> {
    let mut paths = l.unknown_fields.clone();
    if l.allowed_from
        .as_deref()
        .is_some_and(|f| f != "Same" && f != "All")
    {
        paths.push(format!("spec.listeners[{i}].allowedRoutes.namespaces.from"));
    }
    if l.port == 0 {
        paths.push(format!("spec.listeners[{i}].port"));
    }
    first_in_order(paths)
}

/// One step of a field path: `[n]` is an index, a `.`-separated name or `[key]` a key.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Segment<'a> {
    Index(usize),
    Key(&'a str),
}

/// The segments of `spec.listeners[1].filters` or `spec.infrastructure.annotations[a/b]`, so
/// that paths compare in document order (the converter walks sorted keys).
fn order(path: &str) -> Vec<Segment<'_>> {
    let mut out = vec![];
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(inner) = rest.strip_prefix('[') {
            let end = inner.find(']').unwrap_or(inner.len());
            let key = &inner[..end];
            out.push(match key.parse::<usize>() {
                Ok(n) => Segment::Index(n),
                Err(_) => Segment::Key(key),
            });
            rest = inner.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            if end > 0 {
                out.push(Segment::Key(&rest[..end]));
            }
            rest = &rest[end..];
        }
        rest = rest.strip_prefix('.').unwrap_or(rest);
    }
    out
}

fn first_in_order(paths: Vec<String>) -> Option<String> {
    paths.into_iter().min_by(|a, b| order(a).cmp(&order(b)))
}

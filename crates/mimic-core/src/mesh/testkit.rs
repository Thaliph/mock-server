//! Builders for the mesh tests.

use super::input::*;
use super::view::{Built, Limits, build};
use crate::Mode;

pub fn svc(ns: &str, name: &str, ip: &str, ports: &[(u16, Option<&str>, TargetPort)]) -> Service {
    Service {
        namespace: ns.into(),
        name: name.into(),
        cluster_ips: if ip.is_empty() {
            vec![]
        } else {
            vec![ip.into()]
        },
        ports: ports
            .iter()
            .map(|(port, name, target_port)| ServicePort {
                port: *port,
                name: name.map(Into::into),
                target_port: target_port.clone(),
            })
            .collect(),
    }
}

/// A Service with one port 80 named `http` (target port `http`).
pub fn http_svc(ns: &str, name: &str, ip: &str) -> Service {
    svc(
        ns,
        name,
        ip,
        &[(80, Some("http"), TargetPort::Name("http".into()))],
    )
}

pub fn ep(ip: &str, ready: bool) -> Endpoint {
    Endpoint {
        ip: ip.into(),
        ready,
        serving: ready,
        terminating: false,
        zone: None,
    }
}

pub fn zoned(ip: &str, zone: &str) -> Endpoint {
    Endpoint {
        zone: Some(zone.into()),
        ..ep(ip, true)
    }
}

/// A slice of `service` with port `http` -> `port`.
pub fn slice(
    ns: &str,
    name: &str,
    service: &str,
    generation: i64,
    port: u16,
    eps: Vec<Endpoint>,
) -> EndpointSlice {
    EndpointSlice {
        namespace: ns.into(),
        name: name.into(),
        service: service.into(),
        generation,
        ports: vec![SlicePort {
            name: Some("http".into()),
            port,
        }],
        endpoints: eps,
    }
}

pub fn cfg(ns: &str, name: &str, created: i64, targets: &[(TargetKind, &str)]) -> Config {
    Config {
        namespace: ns.into(),
        name: name.into(),
        created,
        generation: 1,
        targets: targets
            .iter()
            .map(|(kind, name)| Target {
                kind: *kind,
                name: (*name).into(),
            })
            .collect(),
        destinations: None,
        mesh_enabled: None,
        slow_start: None,
    }
}

/// A config on Deployment `d` declaring `dests` (`ns/name`).
pub fn dep_cfg(ns: &str, name: &str, created: i64, d: &str, dests: &[&str]) -> Config {
    Config {
        destinations: Some(
            dests
                .iter()
                .map(|d| {
                    let (namespace, name) = d.split_once('/').expect("ns/name");
                    Destination {
                        namespace: namespace.into(),
                        name: name.into(),
                    }
                })
                .collect(),
        ),
        ..cfg(ns, name, created, &[(TargetKind::Deployment, d)])
    }
}

/// A grant in `ns` letting `MimicConfig`s of `from_ns` reach any Service.
pub fn grant(ns: &str, from_ns: &str) -> ReferenceGrant {
    ReferenceGrant {
        namespace: ns.into(),
        from: vec![GrantFrom {
            group: MIMIC_GROUP.into(),
            kind: "MimicConfig".into(),
            namespace: from_ns.into(),
        }],
        to: vec![GrantTo {
            group: String::new(),
            kind: "Service".into(),
            name: None,
        }],
    }
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        cluster: "local".into(),
        grants: Some(vec![]),
        ..Snapshot::default()
    }
}

pub fn view_of(s: &Snapshot, ns: &str, d: &str) -> Built {
    build(s, ns, d, Mode::None, &Limits::default())
}

pub fn keys(b: &Built) -> Vec<&str> {
    b.view.services.keys().map(String::as_str).collect()
}

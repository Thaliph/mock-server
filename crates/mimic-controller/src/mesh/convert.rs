//! The watched objects as the plain input structs of `mimic_core::mesh::input` (MSH-27).
//!
//! Only the fields the view uses are read: no label, annotation or Secret value ever leaves
//! the Kubernetes object, so none can reach a view (MSH-30). An object that cannot be keyed
//! (no namespace or name) is skipped; one the CRD would refuse is converted as far as it goes and
//! left for `mesh::policy` to call `Invalid`.

use k8s_openapi::api::core::v1::Service as KubeService;
use k8s_openapi::api::discovery::v1::EndpointSlice as KubeSlice;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::DynamicObject;
use mimic_core::mesh::input::{
    Config, Destination, Endpoint, EndpointSlice, GrantFrom, GrantTo, ReferenceGrant,
    RetryBudgetSpec, Service, ServicePort, SlicePort, Target, TargetKind, TargetPort,
};
use serde_json::Value;

/// `(namespace, name)`.
pub type Key = (String, String);

/// Label of an EndpointSlice naming its Service.
const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

fn key(meta: &ObjectMeta) -> Option<Key> {
    let (namespace, name) = (meta.namespace.as_deref()?, meta.name.as_deref()?);
    (!namespace.is_empty() && !name.is_empty()).then(|| (namespace.to_string(), name.to_string()))
}

pub fn service(object: &KubeService) -> Option<(Key, Service)> {
    let key = key(&object.metadata)?;
    let spec = object.spec.as_ref();
    let cluster_ips = spec
        .and_then(|s| s.cluster_ips.as_ref())
        .into_iter()
        .flatten()
        // `None` is how a headless Service says it has no address.
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .cloned()
        .collect();
    let ports = spec
        .and_then(|s| s.ports.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let port = u16::try_from(p.port).ok().filter(|p| *p != 0)?;
            let target_port = match &p.target_port {
                None => TargetPort::Number(port),
                Some(IntOrString::Int(n)) => TargetPort::Number(u16::try_from(*n).ok()?),
                Some(IntOrString::String(name)) => TargetPort::Name(name.clone()),
            };
            Some(ServicePort {
                port,
                name: p.name.clone().filter(|n| !n.is_empty()),
                target_port,
            })
        })
        .collect();
    let service = Service {
        namespace: key.0.clone(),
        name: key.1.clone(),
        cluster_ips,
        ports,
    };
    Some((key, service))
}

pub fn slice(object: &KubeSlice) -> Option<(Key, EndpointSlice)> {
    let key = key(&object.metadata)?;
    // FQDN slices (ExternalName) hold names, not pod addresses.
    if object.address_type != "IPv4" && object.address_type != "IPv6" {
        return None;
    }
    let service = object
        .metadata
        .labels
        .as_ref()?
        .get(SERVICE_NAME_LABEL)
        .filter(|s| !s.is_empty())?
        .clone();
    let ports = object
        .ports
        .iter()
        .flatten()
        .filter_map(|p| {
            Some(SlicePort {
                name: p.name.clone().filter(|n| !n.is_empty()),
                port: u16::try_from(p.port?).ok().filter(|p| *p != 0)?,
            })
        })
        .collect();
    let endpoints = object
        .endpoints
        .iter()
        .flat_map(|e| {
            let conditions = e.conditions.as_ref();
            let flag =
                |f: fn(&k8s_openapi::api::discovery::v1::EndpointConditions) -> Option<bool>,
                 missing: bool| conditions.and_then(f).unwrap_or(missing);
            let (ready, serving, terminating) = (
                flag(|c| c.ready, true),
                flag(|c| c.serving, true),
                flag(|c| c.terminating, false),
            );
            e.addresses.iter().map(move |ip| Endpoint {
                ip: ip.clone(),
                ready,
                serving,
                terminating,
                zone: e.zone.clone().filter(|z| !z.is_empty()),
            })
        })
        .collect();
    let slice = EndpointSlice {
        namespace: key.0.clone(),
        name: key.1.clone(),
        service,
        uid: object.metadata.uid.clone().unwrap_or_default(),
        generation: object.metadata.generation.unwrap_or(0),
        ports,
        endpoints,
    };
    Some((key, slice))
}

fn text(value: &Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_string)
}

/// `<n>s` as seconds; anything else is not set.
fn seconds(value: &str) -> Option<u32> {
    value.strip_suffix('s')?.parse().ok()
}

/// An integer field the object sets: a value that is not a `u32` (a string, a negative
/// number) becomes `u32::MAX`, over every bound, so that `validate` refuses the object.
fn bounded(value: &Value) -> u32 {
    value
        .as_u64()
        .map_or(u32::MAX, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

pub fn config(object: &DynamicObject) -> Option<(Key, Config)> {
    let key = key(&object.metadata)?;
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    let targets = spec
        .get("targetRefs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let kind = match t.get("kind")?.as_str()? {
                "Deployment" => TargetKind::Deployment,
                "Service" => TargetKind::Service,
                _ => return None,
            };
            Some(Target {
                kind,
                name: text(t, "name")?,
            })
        })
        .collect();
    let destinations = spec
        .get("destinations")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|d| Destination {
                    namespace: text(d, "namespace").unwrap_or_default(),
                    name: text(d, "name").unwrap_or_default(),
                })
                .collect()
        });
    let config = Config {
        namespace: key.0.clone(),
        name: key.1.clone(),
        created: object
            .metadata
            .creation_timestamp
            .as_ref()
            .map_or(0, |t| t.0.as_second()),
        uid: object.metadata.uid.clone().unwrap_or_default(),
        generation: object.metadata.generation.unwrap_or(0),
        targets,
        destinations,
        mesh_enabled: spec
            .get("mesh")
            .and_then(|m| m.get("enabled"))
            .and_then(Value::as_bool),
        slow_start: spec
            .get("loadBalancer")
            .and_then(|l| l.get("slowStart"))
            .and_then(Value::as_str)
            .and_then(seconds),
        retries: spec.get("retries").map(bounded),
        retry_budget: spec.get("retryBudget").map(|b| RetryBudgetSpec {
            percent: b.get("percent").map(bounded),
            min_per_second: b.get("minPerSecond").map(bounded),
            // Not `<n>s`: 0, which `validate` refuses (RTY-46).
            window_seconds: b
                .get("window")
                .map(|w| w.as_str().and_then(seconds).unwrap_or(0)),
        }),
    };
    Some((key, config))
}

pub fn grant(object: &DynamicObject) -> Option<(Key, ReferenceGrant)> {
    let key = key(&object.metadata)?;
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    let list = |field: &str| {
        spec.get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
    };
    let grant = ReferenceGrant {
        namespace: key.0.clone(),
        from: list("from")
            .iter()
            .map(|f| GrantFrom {
                group: text(f, "group").unwrap_or_default(),
                kind: text(f, "kind").unwrap_or_default(),
                namespace: text(f, "namespace").unwrap_or_default(),
            })
            .collect(),
        to: list("to")
            .iter()
            .map(|t| GrantTo {
                group: text(t, "group").unwrap_or_default(),
                kind: text(t, "kind").unwrap_or_default(),
                name: text(t, "name"),
            })
            .collect(),
    };
    Some((key, grant))
}

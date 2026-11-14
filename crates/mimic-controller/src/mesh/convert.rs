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
    BackendRef, Config, Destination, Endpoint, EndpointSlice, GATEWAY_GROUP, GrantFrom, GrantTo,
    HeaderModifier, Mirror, NameMatch, ParentRef, PathMatch, ReferenceGrant, RetryBudgetSpec,
    Route, RouteFilter, RouteMatch, RouteRule, Service, ServicePort, SlicePort, Target, TargetKind,
    TargetPort,
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

/// The longest `spec` of an `HTTPRoute` that is read (RTE-13); a larger one is kept as its key,
/// generation and parents.
const MAX_SPEC_BYTES: usize = 64 * 1024;

/// The fields of each level of an `HTTPRoute` the converter reads (or, for the filter types of
/// the standard channel, that `mesh::route` refuses by `type`). Any other is an unknown field.
const SPEC_FIELDS: [&str; 3] = ["parentRefs", "hostnames", "rules"];
const PARENT_FIELDS: [&str; 6] = ["group", "kind", "namespace", "name", "sectionName", "port"];
const RULE_FIELDS: [&str; 6] = [
    "name",
    "matches",
    "filters",
    "backendRefs",
    "timeouts",
    "retry",
];
const MATCH_FIELDS: [&str; 4] = ["path", "headers", "queryParams", "method"];
const BACKEND_FIELDS: [&str; 7] = [
    "group",
    "kind",
    "namespace",
    "name",
    "port",
    "weight",
    "filters",
];
const FILTER_FIELDS: [&str; 7] = [
    "type",
    "requestHeaderModifier",
    "responseHeaderModifier",
    "requestMirror",
    "requestRedirect",
    "urlRewrite",
    "extensionRef",
];

/// The paths of the fields an object has besides `known`, under `at`.
fn unknown(value: &Value, at: &str, known: &[&str], out: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for field in object.keys().filter(|f| !known.contains(&f.as_str())) {
        out.push(if at.is_empty() {
            field.clone()
        } else {
            format!("{at}.{field}")
        });
    }
}

fn list<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn port(value: &Value) -> Option<u16> {
    u16::try_from(value.get("port")?.as_u64()?).ok()
}

fn names(value: &Value, field: &str) -> Vec<NameMatch> {
    list(value, field)
        .iter()
        .map(|n| NameMatch {
            kind: text(n, "type").unwrap_or_else(|| "Exact".into()),
            name: text(n, "name").unwrap_or_default(),
            value: text(n, "value").unwrap_or_default(),
        })
        .collect()
}

fn pairs(value: &Value, field: &str) -> Vec<(String, String)> {
    list(value, field)
        .iter()
        .map(|p| {
            (
                text(p, "name").unwrap_or_default(),
                text(p, "value").unwrap_or_default(),
            )
        })
        .collect()
}

fn modifier(value: &Value) -> HeaderModifier {
    HeaderModifier {
        set: pairs(value, "set"),
        add: pairs(value, "add"),
        remove: list(value, "remove")
            .iter()
            .map(|n| n.as_str().unwrap_or_default().to_string())
            .collect(),
    }
}

fn mirror(value: &Value) -> Mirror {
    let backend = value.get("backendRef").unwrap_or(&Value::Null);
    Mirror {
        group: text(backend, "group").unwrap_or_default(),
        kind: text(backend, "kind").unwrap_or_else(|| "Service".into()),
        namespace: text(backend, "namespace"),
        name: text(backend, "name").unwrap_or_default(),
        port: port(backend),
        percent: value.get("percent").map(bounded),
        fraction: value.get("fraction").map(|f| {
            (
                f.get("numerator").map_or(0, bounded),
                f.get("denominator").map(bounded),
            )
        }),
    }
}

fn filter(value: &Value, at: &str, out: &mut Vec<String>) -> RouteFilter {
    unknown(value, at, &FILTER_FIELDS, out);
    let member = |field: &str| value.get(field);
    if let Some(m) = member("requestMirror") {
        unknown(
            m,
            &format!("{at}.requestMirror"),
            &["backendRef", "percent", "fraction"],
            out,
        );
    }
    for field in ["requestHeaderModifier", "responseHeaderModifier"] {
        if let Some(m) = member(field) {
            unknown(m, &format!("{at}.{field}"), &["set", "add", "remove"], out);
        }
    }
    RouteFilter {
        kind: text(value, "type").unwrap_or_default(),
        request_header_modifier: member("requestHeaderModifier").map(modifier),
        response_header_modifier: member("responseHeaderModifier").map(modifier),
        request_mirror: member("requestMirror").map(mirror),
    }
}

fn rule(value: &Value, at: &str, out: &mut Vec<String>) -> RouteRule {
    unknown(value, at, &RULE_FIELDS, out);
    let matches = list(value, "matches")
        .iter()
        .enumerate()
        .map(|(j, m)| {
            let at = format!("{at}.matches[{j}]");
            unknown(m, &at, &MATCH_FIELDS, out);
            RouteMatch {
                path: m.get("path").map(|p| PathMatch {
                    kind: text(p, "type").unwrap_or_else(|| "PathPrefix".into()),
                    value: text(p, "value").unwrap_or_else(|| "/".into()),
                }),
                headers: names(m, "headers"),
                query_params: names(m, "queryParams"),
                method: text(m, "method"),
            }
        })
        .collect();
    let filters = list(value, "filters")
        .iter()
        .enumerate()
        .map(|(j, f)| filter(f, &format!("{at}.filters[{j}]"), out))
        .collect();
    let backend_refs = list(value, "backendRefs")
        .iter()
        .enumerate()
        .map(|(j, b)| {
            unknown(b, &format!("{at}.backendRefs[{j}]"), &BACKEND_FIELDS, out);
            BackendRef {
                group: text(b, "group").unwrap_or_default(),
                kind: text(b, "kind").unwrap_or_else(|| "Service".into()),
                namespace: text(b, "namespace"),
                name: text(b, "name").unwrap_or_default(),
                port: port(b),
                weight: b.get("weight").map(bounded),
                has_filters: b.get("filters").is_some(),
            }
        })
        .collect();
    let timeouts = value.get("timeouts");
    if let Some(t) = timeouts {
        unknown(
            t,
            &format!("{at}.timeouts"),
            &["request", "backendRequest"],
            out,
        );
    }
    RouteRule {
        name: text(value, "name"),
        matches,
        backend_refs,
        filters,
        request_timeout: timeouts.and_then(|t| text(t, "request")),
        backend_request_timeout: timeouts.and_then(|t| text(t, "backendRequest")),
        has_retry: value.get("retry").is_some(),
    }
}

/// An `HTTPRoute` (RTE-13). Labels, annotations and `status` are not read, so a change of only
/// those gives an equal [`Route`] and no new view.
pub fn route(object: &DynamicObject) -> Option<(Key, Route)> {
    let key = key(&object.metadata)?;
    let spec = object.data.get("spec").unwrap_or(&Value::Null);
    let parents = list(spec, "parentRefs")
        .iter()
        .map(|p| ParentRef {
            group: text(p, "group").unwrap_or_else(|| GATEWAY_GROUP.into()),
            kind: text(p, "kind").unwrap_or_else(|| "Gateway".into()),
            namespace: text(p, "namespace"),
            name: text(p, "name").unwrap_or_default(),
            section_name: text(p, "sectionName"),
            port: port(p),
        })
        .collect();
    let oversize = spec.to_string().len() > MAX_SPEC_BYTES;
    let mut unknown_fields = Vec::new();
    let mut rules = Vec::new();
    if !oversize {
        unknown(spec, "", &SPEC_FIELDS, &mut unknown_fields);
        for (i, p) in list(spec, "parentRefs").iter().enumerate() {
            unknown(
                p,
                &format!("parentRefs[{i}]"),
                &PARENT_FIELDS,
                &mut unknown_fields,
            );
        }
        for (i, r) in list(spec, "rules").iter().enumerate() {
            rules.push(rule(r, &format!("rules[{i}]"), &mut unknown_fields));
        }
    }
    let route = Route {
        namespace: key.0.clone(),
        name: key.1.clone(),
        uid: object.metadata.uid.clone().unwrap_or_default(),
        generation: object.metadata.generation.unwrap_or(0),
        created: object
            .metadata
            .creation_timestamp
            .as_ref()
            .map_or(0, |t| t.0.as_second()),
        parents,
        rules,
        unknown_fields,
        oversize,
    };
    Some((key, route))
}

/// What the cluster holds of the status of an `HTTPRoute`: the `resourceVersion` the next patch
/// is conditioned on, the generation the entries can be compared with, and `status.parents`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteObserved {
    pub generation: i64,
    pub resource_version: String,
    pub parents: Vec<Value>,
}

pub fn route_observed(object: &DynamicObject) -> Option<(Key, RouteObserved)> {
    let key = key(&object.metadata)?;
    let status = object.data.get("status").unwrap_or(&Value::Null);
    let observed = RouteObserved {
        generation: object.metadata.generation.unwrap_or(0),
        resource_version: object.metadata.resource_version.clone().unwrap_or_default(),
        parents: list(status, "parents").to_vec(),
    };
    Some((key, observed))
}

/// A route and what the cluster holds of its status, from the one object (a single watch feeds
/// both, so that they are never out of step).
pub fn route_and_status(object: &DynamicObject) -> Option<(Key, (Route, RouteObserved))> {
    let (key, route) = route(object)?;
    let (_, observed) = route_observed(object)?;
    Some((key, (route, observed)))
}

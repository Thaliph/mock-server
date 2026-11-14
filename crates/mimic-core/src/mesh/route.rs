//! HTTPRoute compilation (spec §7.9 "Routes", #447): a [`Route`] becomes the route of the view
//! and the conditions of its parent entries. Pure: the Services and grants come in as arguments.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::input::{
    BackendRef, GATEWAY_GROUP, HeaderModifier, Mirror, NameMatch, ReferenceGrant, Route,
    RouteFilter, RouteRule, Service,
};
use super::policy::Condition;
use super::view::{fraction, header_name, header_value, method, path_value};

/// Field order is alphabetical in every struct below: the JSON is canonical (sorted keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathView {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MatchView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<NameValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_params: Vec<NameValue>,
}

/// A valid backend (`service` and `port`) or an invalid one (`invalid`, nothing else).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModifierView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<NameValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<NameValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorView {
    pub denominator: u32,
    pub numerator: u32,
    pub port: u16,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FiltersView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<MirrorView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<ModifierView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<ModifierView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimeoutsView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_request_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleView {
    pub backends: Vec<BackendView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<FiltersView>,
    pub matches: Vec<MatchView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<TimeoutsView>,
}

/// A route as one Service's view holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteView {
    /// `<namespace>/<name>` of the route.
    pub name: String,
    /// Absent: every port of the Service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
    pub rules: Vec<RuleView>,
}

/// The conditions of one `status.parents[]` entry of mimic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentStatus {
    /// Index in `Route::parents`.
    pub index: usize,
    pub accepted: Condition,
    pub resolved_refs: Condition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// `false`: the route is in no view (`Accepted: False`, `UnsupportedValue`).
    pub programmed: bool,
    /// One per Service parent that exists, in `parentRefs` order.
    pub parents: Vec<ParentStatus>,
    /// `<namespace>/<name>` of each Service the route is attached to, with the ports it applies
    /// to (`None`: every port). Empty when not programmed.
    pub attachments: BTreeMap<String, Option<Vec<u16>>>,
    name: String,
    rules: Vec<RuleView>,
}

/// The longest `ResolvedRefs` message.
const MAX_MESSAGE: usize = 1024;

/// Header names a modifier may not touch (besides the `x-mimic-` prefix).
const REFUSED_HEADERS: [&str; 9] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

impl Compiled {
    /// The route as the view of Service `service` holds it.
    pub fn view_for(&self, service: &str) -> Option<RouteView> {
        let ports = self.attachments.get(service)?;
        Some(RouteView {
            name: self.name.clone(),
            ports: ports.clone(),
            rules: self.rules.clone(),
        })
    }

    /// Every `(service, port)` the rules name as valid backend or mirror, in key order.
    pub fn targets(&self) -> Vec<(String, u16)> {
        let mut out = BTreeSet::new();
        for rule in &self.rules {
            for b in &rule.backends {
                if let (Some(service), Some(port)) = (&b.service, b.port) {
                    out.insert((service.clone(), port));
                }
            }
            for m in rule.filters.iter().flat_map(|f| &f.mirrors) {
                out.insert((m.service.clone(), m.port));
            }
        }
        out.into_iter().collect()
    }
}

/// Why a backend or mirror is invalid; the order is the order of the `ResolvedRefs` reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    NotPermitted,
    Kind,
    NotFound,
}

impl Why {
    fn reason(self) -> &'static str {
        match self {
            Why::NotPermitted => "RefNotPermitted",
            Why::Kind => "InvalidKind",
            Why::NotFound => "BackendNotFound",
        }
    }
}

struct Resolver<'a> {
    route: &'a Route,
    services: &'a [Service],
    grants: Option<&'a [ReferenceGrant]>,
    invalid: Vec<(Why, String)>,
}

impl Resolver<'_> {
    /// `<namespace>/<name>` and port of a valid reference; else records why it is not.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &mut self,
        group: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
        port: Option<u16>,
    ) -> Option<(String, u16)> {
        let ns = namespace.unwrap_or(&self.route.namespace);
        let key = format!("{ns}/{name}");
        let verdict = if !group.is_empty() || kind != "Service" {
            Err(Why::Kind)
        } else if ns != self.route.namespace && !self.permitted(ns, name) {
            Err(Why::NotPermitted)
        } else {
            self.services
                .iter()
                .find(|s| s.namespace == ns && s.name == name)
                .and_then(|s| port.filter(|p| s.ports.iter().any(|sp| sp.port == *p)))
                .ok_or(Why::NotFound)
        };
        match verdict {
            Ok(port) => Some((key, port)),
            Err(why) => {
                if !self.invalid.iter().any(|(_, k)| *k == key) {
                    self.invalid.push((why, key));
                }
                None
            }
        }
    }

    /// A `ReferenceGrant` of the Service's namespace for `HTTPRoute`s of the route's namespace.
    fn permitted(&self, ns: &str, name: &str) -> bool {
        self.grants.is_some_and(|grants| {
            grants.iter().any(|g| {
                g.namespace == ns
                    && g.from.iter().any(|f| {
                        f.group == GATEWAY_GROUP
                            && f.kind == "HTTPRoute"
                            && f.namespace == self.route.namespace
                    })
                    && g.to.iter().any(|t| {
                        t.group.is_empty()
                            && t.kind == "Service"
                            && t.name.as_deref().is_none_or(|n| n.is_empty() || n == name)
                    })
            })
        })
    }
}

/// Compiles `route` against the Services of the cluster and its `ReferenceGrant`s (`None`: the
/// CRD is not installed).
pub fn compile(route: &Route, services: &[Service], grants: Option<&[ReferenceGrant]>) -> Compiled {
    let unsupported = first_unsupported(route);
    let mut resolver = Resolver {
        route,
        services,
        grants,
        invalid: vec![],
    };
    let rules: Vec<RuleView> = route
        .rules
        .iter()
        .map(|rule| compile_rule(rule, &mut resolver))
        .collect();
    let resolved_refs = match resolver.invalid.iter().map(|(why, _)| *why).min() {
        None => Condition::ok("ResolvedRefs"),
        Some(why) => {
            let names: Vec<&str> = resolver.invalid.iter().map(|(_, k)| k.as_str()).collect();
            Condition::refused(why.reason(), bounded(&names.join(", ")))
        }
    };
    let retry = route.rules.iter().any(|r| r.has_retry);

    let mut parents = vec![];
    let mut attachments: BTreeMap<String, Option<BTreeSet<u16>>> = BTreeMap::new();
    for (index, p) in route.parents.iter().enumerate() {
        if !p.group.is_empty() || p.kind != "Service" {
            continue;
        }
        let ns = p.namespace.as_deref().unwrap_or(&route.namespace);
        let Some(service) = services
            .iter()
            .find(|s| s.namespace == ns && s.name == p.name)
        else {
            continue;
        };
        let accepted = if let Some(path) = &unsupported {
            Condition::refused("UnsupportedValue", format!("unsupported: {path}"))
        } else if p.section_name.is_some() {
            Condition::refused(
                "NoMatchingParent",
                "sectionName is not supported for a Service parent".into(),
            )
        } else if p
            .port
            .is_some_and(|port| !service.ports.iter().any(|sp| sp.port == port))
        {
            Condition::refused(
                "NoMatchingParent",
                "port is not a port of the Service".into(),
            )
        } else {
            let slot = attachments
                .entry(format!("{ns}/{}", p.name))
                .or_insert_with(|| Some(BTreeSet::new()));
            match (p.port, slot.as_mut()) {
                (Some(port), Some(set)) => {
                    set.insert(port);
                }
                _ => *slot = None,
            }
            let mut ok = Condition::ok("Accepted");
            if retry {
                ok.message = "retry is configured by MimicConfig".into();
            }
            ok
        };
        parents.push(ParentStatus {
            index,
            accepted,
            resolved_refs: resolved_refs.clone(),
        });
    }
    // An entry created above for a parent whose later entry was refused keeps its ports; one
    // that only ever had `None` applies to every port.
    let attachments = attachments
        .into_iter()
        .map(|(k, v)| (k, v.map(|s| s.into_iter().collect())))
        .collect();
    Compiled {
        programmed: unsupported.is_none(),
        parents,
        attachments,
        name: format!("{}/{}", route.namespace, route.name),
        rules: if unsupported.is_none() { rules } else { vec![] },
    }
}

fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_MESSAGE);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn compile_rule(rule: &RouteRule, resolver: &mut Resolver) -> RuleView {
    let matches = if rule.matches.is_empty() {
        vec![MatchView {
            path: Some(PathView {
                kind: "PathPrefix".into(),
                value: "/".into(),
            }),
            ..MatchView::default()
        }]
    } else {
        rule.matches
            .iter()
            .map(|m| MatchView {
                headers: name_values(&m.headers, true),
                method: m.method.clone(),
                path: m.path.as_ref().map(|p| PathView {
                    kind: p.kind.clone(),
                    value: p.value.clone(),
                }),
                query_params: name_values(&m.query_params, false),
            })
            .collect()
    };
    let backends = rule
        .backend_refs
        .iter()
        .map(|b| {
            let weight = b.weight.unwrap_or(1);
            match resolve_backend(b, resolver) {
                Some((service, port)) => BackendView {
                    invalid: None,
                    port: Some(port),
                    service: Some(service),
                    weight,
                },
                None => BackendView {
                    invalid: Some(true),
                    port: None,
                    service: None,
                    weight,
                },
            }
        })
        .collect();

    let mut filters = FiltersView::default();
    for f in &rule.filters {
        match f.kind.as_str() {
            "RequestHeaderModifier" => {
                merge(
                    &mut filters.request_headers,
                    f.request_header_modifier.as_ref(),
                );
            }
            "ResponseHeaderModifier" => {
                merge(
                    &mut filters.response_headers,
                    f.response_header_modifier.as_ref(),
                );
            }
            "RequestMirror" => {
                let Some(m) = &f.request_mirror else { continue };
                let target =
                    resolver.resolve(&m.group, &m.kind, m.namespace.as_deref(), &m.name, m.port);
                if let Some((service, port)) = target {
                    let (numerator, denominator) = mirror_fraction(m);
                    filters.mirrors.push(MirrorView {
                        denominator,
                        numerator,
                        port,
                        service,
                    });
                }
            }
            _ => {}
        }
    }
    let timeouts = TimeoutsView {
        backend_request_ms: rule
            .backend_request_timeout
            .as_deref()
            .and_then(duration_ms),
        request_ms: rule.request_timeout.as_deref().and_then(duration_ms),
    };
    RuleView {
        backends,
        filters: (filters != FiltersView::default()).then_some(filters),
        matches,
        name: rule.name.clone(),
        timeouts: (timeouts != TimeoutsView::default()).then_some(timeouts),
    }
}

/// The numerator and denominator of a mirror: `percent`, else `fraction`, else everything.
fn mirror_fraction(m: &Mirror) -> (u32, u32) {
    match (m.percent, m.fraction) {
        (Some(p), _) => (p, 100),
        (None, Some((n, d))) => (n, d.unwrap_or(100)),
        (None, None) => (100, 100),
    }
}

fn resolve_backend(b: &BackendRef, resolver: &mut Resolver) -> Option<(String, u16)> {
    resolver.resolve(&b.group, &b.kind, b.namespace.as_deref(), &b.name, b.port)
}

fn name_values(matches: &[NameMatch], lower: bool) -> Vec<NameValue> {
    matches
        .iter()
        .map(|m| NameValue {
            name: if lower {
                m.name.to_ascii_lowercase()
            } else {
                m.name.clone()
            },
            value: m.value.clone(),
        })
        .collect()
}

/// Adds `m` to the modifier of the rule (a rule has one filter of each type in practice; a
/// second one appends).
fn merge(into: &mut Option<ModifierView>, m: Option<&HeaderModifier>) {
    let Some(m) = m else { return };
    let pairs = |l: &[(String, String)]| {
        l.iter()
            .map(|(n, v)| NameValue {
                name: n.to_ascii_lowercase(),
                value: v.clone(),
            })
            .collect::<Vec<_>>()
    };
    let view = into.get_or_insert_with(ModifierView::default);
    view.set.extend(pairs(&m.set));
    view.add.extend(pairs(&m.add));
    view.remove
        .extend(m.remove.iter().map(|n| n.to_ascii_lowercase()));
    if *view == ModifierView::default() {
        *into = None;
    }
}

/// A Gateway API duration (`1h30m`, `500ms`): one to four `<1 to 5 digits><h|m|s|ms>` parts.
fn duration_ms(text: &str) -> Option<u64> {
    let mut rest = text;
    let mut total = 0u64;
    let mut parts = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 5 {
            return None;
        }
        let n: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, len) = if rest.starts_with("ms") {
            (1, 2)
        } else if rest.starts_with('s') {
            (1000, 1)
        } else if rest.starts_with('m') {
            (60_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000, 1)
        } else {
            return None;
        };
        rest = &rest[len..];
        total += n * unit;
        parts += 1;
        if parts > 4 {
            return None;
        }
    }
    (parts > 0).then_some(total)
}

/// A header name a modifier may not touch.
pub(super) fn refused_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    REFUSED_HEADERS.contains(&lower.as_str()) || lower.starts_with("x-mimic-")
}

/// The path of the first field the controller cannot program, `None` when it can program all.
fn first_unsupported(route: &Route) -> Option<String> {
    if route.oversize {
        return Some("route larger than 64 KiB".into());
    }
    // The first unknown field of the converter wins over a known item of a later rule, and
    // loses to one of an earlier rule.
    let unknown = route.unknown_fields.first().map(|path| {
        let rule = path
            .strip_prefix("rules[")
            .and_then(|r| r.split_once(']'))
            .and_then(|(i, _)| i.parse::<usize>().ok())
            .unwrap_or(0);
        (rule, path)
    });
    for (i, rule) in route.rules.iter().enumerate() {
        if let Some((at, path)) = unknown
            && at <= i
        {
            return Some(path.clone());
        }
        if let Some(path) = rule_unsupported(i, rule) {
            return Some(path);
        }
    }
    unknown.map(|(_, path)| path.clone())
}

fn rule_unsupported(i: usize, rule: &RouteRule) -> Option<String> {
    let at = format!("rules[{i}]");
    if let Some(j) = rule.backend_refs.iter().position(|b| b.has_filters) {
        return Some(format!("{at}.backendRefs[{j}].filters"));
    }
    for (j, f) in rule.filters.iter().enumerate() {
        if let Some(path) = filter_unsupported(&format!("{at}.filters[{j}]"), f) {
            return Some(path);
        }
    }
    for (j, m) in rule.matches.iter().enumerate() {
        let at = format!("{at}.matches[{j}]");
        if let Some(p) = &m.path
            && p.kind != "Exact"
            && p.kind != "PathPrefix"
        {
            return Some(format!("{at}.path.type: {}", p.kind));
        }
        for (list, items) in [("headers", &m.headers), ("queryParams", &m.query_params)] {
            if let Some(k) = items.iter().position(|n| n.kind != "Exact") {
                return Some(format!("{at}.{list}[{k}].type: {}", items[k].kind));
            }
        }
    }
    if let Some(path) = value_unsupported(&at, rule) {
        return Some(path);
    }
    for (field, value) in [
        ("request", &rule.request_timeout),
        ("backendRequest", &rule.backend_request_timeout),
    ] {
        if value.as_deref().is_some_and(|v| duration_ms(v).is_none()) {
            return Some(format!("{at}.timeouts.{field}"));
        }
    }
    None
}

/// The path of the first value the view would refuse (`View::from_json`, RTE-12), found by the
/// predicates it uses: a route holding one is in no view, so no sidecar rejects the whole view.
fn value_unsupported(at: &str, rule: &RouteRule) -> Option<String> {
    for (j, m) in rule.matches.iter().enumerate() {
        let at = format!("{at}.matches[{j}]");
        if let Some(p) = &m.path
            && !path_value(&p.value)
        {
            return Some(format!("{at}.path.value"));
        }
        if m.method.as_deref().is_some_and(|v| !method(v)) {
            return Some(format!("{at}.method"));
        }
        for (list, items, lower) in [
            ("headers", &m.headers, true),
            ("queryParams", &m.query_params, false),
        ] {
            for (k, n) in items.iter().enumerate() {
                if lower && !header_name(&n.name.to_ascii_lowercase()) {
                    return Some(format!("{at}.{list}[{k}].name"));
                }
                if !header_value(&n.value) {
                    return Some(format!("{at}.{list}[{k}].value"));
                }
            }
        }
    }
    for (j, f) in rule.filters.iter().enumerate() {
        let at = format!("{at}.filters[{j}]");
        if let Some(m) = &f.request_mirror {
            let (n, d) = mirror_fraction(m);
            if !fraction(n, d) {
                return Some(format!("{at}.requestMirror"));
            }
        }
        for (field, modifier) in [
            ("requestHeaderModifier", &f.request_header_modifier),
            ("responseHeaderModifier", &f.response_header_modifier),
        ] {
            let Some(m) = modifier else { continue };
            for (list, pairs) in [("set", &m.set), ("add", &m.add)] {
                for (k, (name, value)) in pairs.iter().enumerate() {
                    if !header_name(&name.to_ascii_lowercase()) {
                        return Some(format!("{at}.{field}.{list}[{k}].name"));
                    }
                    if !header_value(value) {
                        return Some(format!("{at}.{field}.{list}[{k}].value"));
                    }
                }
            }
            if let Some(k) = m
                .remove
                .iter()
                .position(|n| !header_name(&n.to_ascii_lowercase()))
            {
                return Some(format!("{at}.{field}.remove[{k}]"));
            }
        }
    }
    None
}

fn filter_unsupported(at: &str, f: &RouteFilter) -> Option<String> {
    let (field, modifier) = match f.kind.as_str() {
        "RequestMirror" => return None,
        "RequestHeaderModifier" => ("requestHeaderModifier", &f.request_header_modifier),
        "ResponseHeaderModifier" => ("responseHeaderModifier", &f.response_header_modifier),
        other => return Some(format!("{at}.type: {other}")),
    };
    let m = modifier.as_ref()?;
    for (list, pairs) in [("set", &m.set), ("add", &m.add)] {
        if let Some(k) = pairs.iter().position(|(n, _)| refused_header(n)) {
            return Some(format!("{at}.{field}.{list}[{k}].name"));
        }
    }
    let k = m.remove.iter().position(|n| refused_header(n))?;
    Some(format!("{at}.{field}.remove[{k}]"))
}

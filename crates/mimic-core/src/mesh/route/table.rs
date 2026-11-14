//! Route matching (spec §7.9 "Routing"): the matches of the routes of one Service port, sorted by
//! the Gateway API precedence, so the first that holds wins.

use std::cmp::Reverse;

use super::{MatchView, RouteView};

/// What a match looks at. The path and the query come from [`split_target`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestParts<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The raw query, without `?`.
    pub query: Option<&'a str>,
    /// Field lines in the order received, names in any case.
    pub headers: &'a [(&'a str, &'a [u8])],
}

/// The path (without query) and the raw query of a request target: origin-form, absolute-form
/// (the path after the authority; empty when there is none) or `*`.
pub fn split_target(target: &str) -> (&str, Option<&str>) {
    let (head, query) = match target.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (target, None),
    };
    if head.starts_with('/') {
        return (head, query);
    }
    match head.find("://") {
        Some(i) => {
            let authority_and_path = &head[i + 3..];
            let path = authority_and_path
                .find('/')
                .map_or("", |k| &authority_and_path[k..]);
            (path, query)
        }
        None => (head, query),
    }
}

enum PathItem {
    Exact(String),
    /// The prefix without its one trailing `/`.
    Prefix(String),
}

/// One match of one rule, with the items it holds once per name.
struct Entry {
    route: usize,
    rule: usize,
    path: Option<PathItem>,
    method: Option<String>,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
}

pub struct Table {
    /// Best match first.
    entries: Vec<Entry>,
    routed: bool,
}

impl Table {
    /// The routes of one Service port: those whose `ports` is absent or holds `port`.
    pub fn new(routes: &[RouteView], port: u16) -> Self {
        let mut keyed = vec![];
        let mut routed = false;
        for (route, view) in routes.iter().enumerate() {
            if view.ports.as_ref().is_some_and(|p| !p.contains(&port)) {
                continue;
            }
            routed = true;
            for (rule, r) in view.rules.iter().enumerate() {
                for (index, m) in r.matches.iter().enumerate() {
                    let entry = entry(route, rule, m);
                    let (exact, len) = match &m.path {
                        Some(p) if p.kind == "Exact" => (true, 0),
                        Some(p) => (false, p.value.len()),
                        // No path item is the default prefix `/`.
                        None => (false, 1),
                    };
                    let key = (
                        Reverse(exact),
                        Reverse(len),
                        Reverse(entry.method.is_some()),
                        Reverse(entry.headers.len()),
                        Reverse(entry.query.len()),
                        route,
                        rule,
                        index,
                    );
                    keyed.push((key, entry));
                }
            }
        }
        keyed.sort_by_key(|(key, _)| *key);
        Table {
            entries: keyed.into_iter().map(|(_, e)| e).collect(),
            routed,
        }
    }

    /// No route applies to the port: the Service is balanced, not routed.
    pub fn is_empty(&self) -> bool {
        !self.routed
    }

    /// `(route, rule)` indexes in the `routes` given to [`Table::new`] of the winning match.
    pub fn select(&self, req: &RequestParts) -> Option<(usize, usize)> {
        self.entries
            .iter()
            .find(|e| e.holds(req))
            .map(|e| (e.route, e.rule))
    }
}

fn entry(route: usize, rule: usize, m: &MatchView) -> Entry {
    let path = m.path.as_ref().map(|p| {
        if p.kind == "Exact" {
            PathItem::Exact(p.value.clone())
        } else {
            let value = p.value.strip_suffix('/').unwrap_or(&p.value);
            PathItem::Prefix(value.to_string())
        }
    });
    // Only the first item of a name counts (the Gateway API ignores the next ones).
    let mut headers: Vec<(String, String)> = vec![];
    for h in &m.headers {
        if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(&h.name)) {
            headers.push((h.name.clone(), h.value.clone()));
        }
    }
    let mut query: Vec<(String, String)> = vec![];
    for q in &m.query_params {
        if !query.iter().any(|(n, _)| *n == q.name) {
            query.push((q.name.clone(), q.value.clone()));
        }
    }
    Entry {
        route,
        rule,
        path,
        method: m.method.clone(),
        headers,
        query,
    }
}

impl Entry {
    fn holds(&self, req: &RequestParts) -> bool {
        if let Some(item) = &self.path {
            // `*` and an empty path match no path item.
            if !req.path.starts_with('/') {
                return false;
            }
            let ok = match item {
                PathItem::Exact(v) => req.path == v,
                PathItem::Prefix(v) => req
                    .path
                    .strip_prefix(v.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/')),
            };
            if !ok {
                return false;
            }
        }
        if self.method.as_deref().is_some_and(|m| m != req.method) {
            return false;
        }
        self.headers
            .iter()
            .all(|(name, value)| header_equals(req.headers, name, value.as_bytes()))
            && self
                .query
                .iter()
                .all(|(name, value)| query_equals(req.query, name, value))
    }
}

/// The lines of `name` in the order received, joined by `,`, equal `want`.
fn header_equals(headers: &[(&str, &[u8])], name: &str, want: &[u8]) -> bool {
    let mut at = 0;
    let mut seen = false;
    for (_, value) in headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)) {
        if seen {
            if want.get(at) != Some(&b',') {
                return false;
            }
            at += 1;
        }
        seen = true;
        if !want[at..].starts_with(value) {
            return false;
        }
        at += value.len();
    }
    seen && at == want.len()
}

/// The first valid pair named `name` has the decoded value `want`.
fn query_equals(query: Option<&str>, name: &str, want: &str) -> bool {
    for pair in query.unwrap_or("").split('&') {
        let (n, v) = pair.split_once('=').unwrap_or((pair, ""));
        if decoded_equals(n.as_bytes(), name.as_bytes()) == Some(true)
            && let Some(equal) = decoded_equals(v.as_bytes(), want.as_bytes())
        {
            return equal;
        }
    }
    false
}

/// Whether `raw` percent-decoded equals `want` (`+` stays `+`); `None`: an invalid escape.
/// The whole of `raw` is read, so an invalid escape is found after a mismatch too.
fn decoded_equals(raw: &[u8], want: &[u8]) -> Option<bool> {
    let hex = |b: Option<&u8>| b.and_then(|b| char::from(*b).to_digit(16)).map(|d| d as u8);
    let (mut i, mut j, mut equal) = (0, 0, true);
    while i < raw.len() {
        let byte = if raw[i] == b'%' {
            let decoded = hex(raw.get(i + 1))? << 4 | hex(raw.get(i + 2))?;
            i += 2;
            decoded
        } else {
            raw[i]
        };
        i += 1;
        equal &= want.get(j) == Some(&byte);
        j += 1;
    }
    Some(equal && j == want.len())
}

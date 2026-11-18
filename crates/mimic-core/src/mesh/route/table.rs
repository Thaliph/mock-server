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

/// Whether `m` holds for `req` as the match of a route: the oracle of the fault tests (FLT-07).
#[cfg(test)]
pub(crate) fn route_holds(m: &MatchView, req: &RequestParts) -> bool {
    entry(0, 0, m).holds(req)
}

/// Matches tried in their order, each holding as it does in a route, made once (when a view
/// is built) so that a request looks each header name up once and compares each header item
/// once, however many matches hold it: a policy's faults (spec §7.9 "Fault injection", FLT-10).
pub struct Matches {
    /// `None`: holds for every request.
    entries: Vec<Option<Interned>>,
    /// The header names of all items, once each, lower-cased.
    names: Vec<String>,
    /// `(name_key, index in names)` at the slot of the key, the next free one on a collision:
    /// a power of two of at least twice the names.
    slots: Vec<Option<(u64, usize)>>,
    /// The header items of all matches, once each: `(index in names, value)`.
    items: Vec<(usize, String)>,
}

/// A match whose header items are their index in [`Matches::items`].
struct Interned {
    /// Its `headers` are empty.
    entry: Entry,
    headers: Vec<usize>,
}

/// What one request has shown of the header items, filled as they are needed.
struct Seen {
    /// For each name, its first line and whether another follows.
    lines: Vec<Option<(usize, bool)>>,
    /// For each item, whether it holds, once compared.
    items: Vec<Option<bool>>,
}

impl Matches {
    pub fn new<'m>(matches: impl IntoIterator<Item = Option<&'m MatchView>>) -> Self {
        let mut names: Vec<String> = vec![];
        let mut items: Vec<(usize, String)> = vec![];
        let entries = matches
            .into_iter()
            .map(|m| {
                let mut entry = entry(0, 0, m?);
                let headers = std::mem::take(&mut entry.headers)
                    .into_iter()
                    .map(|(name, value)| {
                        let name = index_of(&mut names, name.to_ascii_lowercase());
                        index_of(&mut items, (name, value))
                    })
                    .collect();
                Some(Interned { entry, headers })
            })
            .collect();
        let mut slots = vec![None; (names.len() * 2).next_power_of_two()];
        for (id, n) in names.iter().enumerate() {
            let key = name_key(n);
            let mut at = slot(key, slots.len());
            while slots[at].is_some() {
                at = (at + 1) & (slots.len() - 1);
            }
            slots[at] = Some((key, id));
        }
        Matches {
            entries,
            names,
            slots,
            items,
        }
    }

    /// The index of the first match that holds for `req`.
    pub fn first(&self, req: &RequestParts) -> Option<usize> {
        // Made at the first header item and kept for the next ones.
        let mut seen: Option<Seen> = None;
        self.entries.iter().position(|m| {
            let Some(Interned { entry, headers }) = m else {
                return true;
            };
            entry.path_and_method_hold(req)
                && (headers.is_empty() || {
                    let seen = seen.get_or_insert_with(|| Seen {
                        lines: self.lines(req.headers),
                        items: vec![None; self.items.len()],
                    });
                    headers.iter().all(|i| {
                        *seen.items[*i].get_or_insert_with(|| {
                            let (name, want) = &self.items[*i];
                            match seen.lines[*name] {
                                None => false,
                                Some((first, false)) => req.headers[first].1 == want.as_bytes(),
                                Some((first, true)) => header_equals(
                                    &req.headers[first..],
                                    &self.names[*name],
                                    want.as_bytes(),
                                ),
                            }
                        })
                    })
                })
                && entry.query_holds(req)
        })
    }

    /// For each of `names`, the first of `headers` of that name and whether another follows.
    fn lines(&self, headers: &[(&str, &[u8])]) -> Vec<Option<(usize, bool)>> {
        let mut lines = vec![None; self.names.len()];
        for (i, (name, _)) in headers.iter().enumerate() {
            if let Some(id) = self.name_index(name) {
                match &mut lines[id] {
                    Some((_, more)) => *more = true,
                    none => *none = Some((i, false)),
                }
            }
        }
        lines
    }

    /// The index in `names` of `name`, in any case.
    fn name_index(&self, name: &str) -> Option<usize> {
        let key = name_key(name);
        let mut at = slot(key, self.slots.len());
        // The table is never full: a free slot ends the probe.
        while let Some((k, id)) = *self.slots.get(at)? {
            if k == key && self.names[id].eq_ignore_ascii_case(name) {
                return Some(id);
            }
            at = (at + 1) & (self.slots.len() - 1);
        }
        None
    }
}

/// The slot of `key` in a table of `len` slots, a power of two.
fn slot(key: u64, len: usize) -> usize {
    ((key ^ key >> 32).wrapping_mul(0xFF51_AFD7_ED55_8CCD) >> 32) as usize & (len - 1)
}

/// The index of `item` in `list`, pushed when it is not there.
fn index_of<T: PartialEq>(list: &mut Vec<T>, item: T) -> usize {
    list.iter().position(|e| *e == item).unwrap_or_else(|| {
        list.push(item);
        list.len() - 1
    })
}

/// A key of a header name, one for the names equal in any case: its length and its first and
/// last 8 bytes (4 when shorter), each byte with bit 5 set (which folds the ASCII letters).
/// Names of two keys differ; names of one key are compared.
fn name_key(name: &str) -> u64 {
    const FOLD: u64 = 0x2020_2020_2020_2020;
    let name = name.as_bytes();
    let (head, tail) = if let (Some(h), Some(t)) = (name.first_chunk(), name.last_chunk()) {
        (u64::from_le_bytes(*h), u64::from_le_bytes(*t))
    } else if let (Some(h), Some(t)) = (name.first_chunk(), name.last_chunk()) {
        (
            u64::from(u32::from_le_bytes(*h)),
            u64::from(u32::from_le_bytes(*t)),
        )
    } else {
        (name.iter().fold(0, |w, b| w << 8 | u64::from(*b)), 0)
    };
    (head | FOLD).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (tail | FOLD) ^ name.len() as u64
}

impl Entry {
    fn holds(&self, req: &RequestParts) -> bool {
        self.path_and_method_hold(req)
            && self
                .headers
                .iter()
                .all(|(name, value)| header_equals(req.headers, name, value.as_bytes()))
            && self.query_holds(req)
    }

    fn path_and_method_hold(&self, req: &RequestParts) -> bool {
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
        self.method.as_deref().is_none_or(|m| m == req.method)
    }

    fn query_holds(&self, req: &RequestParts) -> bool {
        self.query
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

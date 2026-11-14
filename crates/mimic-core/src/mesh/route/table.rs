//! Route matching (spec §7.9 "Routing"): the matches of the routes of one Service port, sorted by
//! the Gateway API precedence, so the first that holds wins.

use super::RouteView;

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
/// (the path after the authority) or `*`.
pub fn split_target(target: &str) -> (&str, Option<&str>) {
    todo!("{target}")
}

pub struct Table {}

impl Table {
    /// The routes of one Service port: those whose `ports` is absent or holds `port`.
    pub fn new(routes: &[RouteView], port: u16) -> Self {
        todo!("{} {port}", routes.len())
    }

    /// No route applies to the port: the Service is balanced, not routed.
    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// `(route, rule)` indexes in the `routes` given to [`Table::new`] of the winning match.
    pub fn select(&self, req: &RequestParts) -> Option<(usize, usize)> {
        todo!("{}", req.method)
    }
}

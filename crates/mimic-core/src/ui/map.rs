//! The Map tab (spec §7.5, *Map tab*): the graph of which deployment calls which, built from
//! the stored outbound counters and the watched Services, laid out in layers and rendered as
//! SVG. Pure and deterministic: the same inputs in any order give the same bytes.

use crate::mesh::input::Snapshot;

/// What one deployment sent to one authority: the stored counters, summed over its statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub authority: String,
    pub exchanges: u64,
    /// The exchanges whose recorded status is 5xx.
    pub errors: u64,
}

/// A deployment with stored data and what it called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub namespace: String,
    pub deployment: String,
    pub calls: Vec<Call>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_deployments: usize,
    pub max_others: usize,
    pub max_edges: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_deployments: 1000,
            max_others: 200,
            max_edges: 2000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deployment,
    Service,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    pub namespace: String,
    pub name: String,
    pub label: String,
    pub layer: usize,
    pub x: i64,
    pub y: i64,
    pub href: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub exchanges: u64,
    pub errors: u64,
    pub error_permille: u64,
    pub declared: bool,
    pub href: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    pub nodes: usize,
    pub edges: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Map {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub dropped: Dropped,
    pub width: i64,
    pub height: i64,
}

/// The height and width of a node's box in the layout.
pub const NODE_H: i64 = 36;
pub const NODE_W: i64 = 200;

/// The map of `sources`; `mesh` is the hub's snapshot once its lists are complete (`None`: the
/// mesh is off or not listed yet, nothing is flagged and only names resolve).
pub fn build(sources: &[Source], mesh: Option<&Snapshot>, limits: &Limits) -> Map {
    let _ = (sources, mesh, limits);
    Map::default()
}

impl Map {
    pub fn to_json(&self) -> String {
        String::new()
    }

    pub fn to_svg(&self) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_prop;

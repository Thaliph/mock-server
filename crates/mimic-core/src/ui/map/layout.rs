//! The layered layout of the map: layers by repeated removal of the nodes whose callers are all
//! placed, rows by barycentre sweeps, integer coordinates. No clock, no randomness: nodes are
//! indexed in id order and every tie is broken by that index.

use std::collections::BTreeMap;

use super::{Map, NODE_H, NODE_W};

const MARGIN: i64 = 20;
const GAP_X: i64 = 140;
const GAP_Y: i64 = 24;
/// The line under the nodes that says what was left out.
pub(super) const NOTE_H: i64 = 28;
const SWEEPS: usize = 4;

/// Sets `layer`, `x` and `y` of every node and the size of the map.
pub(super) fn place(map: &mut Map) {
    let index: BTreeMap<&str, usize> = map
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let n = map.nodes.len();
    let mut preds = vec![Vec::new(); n];
    let mut succs = vec![Vec::new(); n];
    for e in &map.edges {
        if let (Some(&from), Some(&to)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            preds[to].push(from);
            succs[from].push(to);
        }
    }
    let layer = layers(&preds);
    let rows = rows(&layer, &preds, &succs);
    let depth = layer.iter().max().map_or(0, |l| l + 1);
    let tallest = rows.iter().map(Vec::len).max().unwrap_or(0);
    for (l, row) in rows.iter().enumerate() {
        for (r, &v) in row.iter().enumerate() {
            let node = &mut map.nodes[v];
            node.layer = l;
            node.x = MARGIN + as_i64(l) * (NODE_W + GAP_X);
            node.y = MARGIN + as_i64(r) * (NODE_H + GAP_Y);
        }
    }
    let columns = as_i64(depth.max(1));
    map.width = 2 * MARGIN + columns * NODE_W + (columns - 1) * (GAP_X);
    let lines = as_i64(tallest.max(1));
    map.height = 2 * MARGIN + lines * NODE_H + (lines - 1) * GAP_Y;
    if map.dropped.nodes + map.dropped.edges > 0 {
        map.height += NOTE_H;
    }
}

fn as_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Longest path from the nodes nobody calls; a cycle is broken at the node with the lowest id.
fn layers(preds: &[Vec<usize>]) -> Vec<usize> {
    let n = preds.len();
    let mut layer = vec![usize::MAX; n];
    let mut left = n;
    while left > 0 {
        let mut batch: Vec<usize> = (0..n)
            .filter(|&v| layer[v] == usize::MAX && preds[v].iter().all(|&p| layer[p] != usize::MAX))
            .collect();
        if batch.is_empty() {
            batch.extend((0..n).find(|&v| layer[v] == usize::MAX));
        }
        // Every layer of the batch is read from what was placed before it.
        let placed: Vec<usize> = batch
            .iter()
            .map(|&v| {
                preds[v]
                    .iter()
                    .filter(|&&p| layer[p] != usize::MAX)
                    .map(|&p| layer[p] + 1)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for (&v, l) in batch.iter().zip(placed) {
            layer[v] = l;
        }
        left -= batch.len();
    }
    layer
}

/// The nodes of each layer in row order: barycentre sweeps down then up, ties by index.
fn rows(layer: &[usize], preds: &[Vec<usize>], succs: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let depth = layer.iter().max().map_or(0, |l| l + 1);
    let mut rows = vec![Vec::new(); depth];
    for (v, &l) in layer.iter().enumerate() {
        rows[l].push(v);
    }
    let mut row_of = vec![0usize; layer.len()];
    let renumber = |rows: &[Vec<usize>], row_of: &mut [usize]| {
        for row in rows {
            for (r, &v) in row.iter().enumerate() {
                row_of[v] = r;
            }
        }
    };
    renumber(&rows, &mut row_of);
    for _ in 0..SWEEPS {
        for l in 1..depth {
            order(&mut rows[l], preds, layer, l - 1, &row_of);
            renumber(&rows, &mut row_of);
        }
        for l in (0..depth.saturating_sub(1)).rev() {
            order(&mut rows[l], succs, layer, l + 1, &row_of);
            renumber(&rows, &mut row_of);
        }
    }
    rows
}

/// Sorts one layer by the mean row of its neighbours in layer `from`; a node with none keeps its
/// row.
fn order(
    row: &mut [usize],
    neighbours: &[Vec<usize>],
    layer: &[usize],
    from: usize,
    row_of: &[usize],
) {
    let key = |v: usize| -> (usize, usize) {
        let near: Vec<usize> = neighbours[v]
            .iter()
            .filter(|&&u| layer[u] == from)
            .map(|&u| row_of[u])
            .collect();
        if near.is_empty() {
            (row_of[v], 1)
        } else {
            (near.iter().sum(), near.len())
        }
    };
    row.sort_by(|&a, &b| {
        let (ka, kb) = (key(a), key(b));
        (ka.0 * kb.1).cmp(&(kb.0 * ka.1)).then(a.cmp(&b))
    });
}

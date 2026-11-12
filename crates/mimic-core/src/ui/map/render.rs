//! The JSON and the SVG of a laid-out map. Every label is escaped for XML and bounded; every part
//! of a link is percent-encoded.

use serde_json::json;

use super::layout::NOTE_H;
use super::{Edge, Kind, Map, NODE_H, NODE_W, Node, ellipsize};

/// The longest label the SVG shows (spec §7.5); the JSON has the whole name.
const LABEL_CHARS: usize = 48;

/// Percent-encodes every byte outside the RFC 3986 unreserved characters.
pub(super) fn percent(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `text` as XML character data or attribute value: the five predefined entities, and U+FFFD
/// for what XML 1.0 cannot hold (line breaks become spaces).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(' '),
            '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}' => {
                out.push(c);
            }
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

pub(super) fn json(map: &Map) -> String {
    let nodes: Vec<_> = map
        .nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.id, "kind": n.kind.as_str(), "namespace": n.namespace,
                "name": n.name, "label": n.label, "layer": n.layer,
                "x": n.x, "y": n.y, "href": n.href,
            })
        })
        .collect();
    let edges: Vec<_> = map
        .edges
        .iter()
        .map(|e| {
            json!({
                "from": e.from, "to": e.to, "exchanges": e.exchanges, "errors": e.errors,
                "error_permille": e.error_permille, "declared": e.declared,
                "href": (!e.href.is_empty()).then_some(&e.href),
            })
        })
        .collect();
    json!({
        "nodes": nodes,
        "edges": edges,
        "dropped": {"nodes": map.dropped.nodes, "edges": map.dropped.edges},
        "width": map.width,
        "height": map.height,
    })
    .to_string()
}

fn edge_label(e: &Edge) -> String {
    let mut label = format!("{} calls", e.exchanges);
    if e.errors > 0 {
        label.push_str(&format!(
            ", {}.{}% 5xx",
            e.error_permille / 10,
            e.error_permille % 10
        ));
    }
    if !e.declared {
        label.push_str(", not declared");
    }
    label
}

fn edge_svg(out: &mut String, e: &Edge, from: &Node, to: &Node) {
    let (y1, y2) = (from.y + NODE_H / 2, to.y + NODE_H / 2);
    let (path, mx, my) = if to.x > from.x {
        let x1 = from.x + NODE_W;
        (
            format!("M{x1} {y1}L{} {y2}", to.x),
            (x1 + to.x) / 2,
            (y1 + y2) / 2,
        )
    } else if to.x == from.x {
        let x1 = from.x + NODE_W;
        (
            format!("M{x1} {y1}C{} {y1} {} {y2} {x1} {y2}", x1 + 60, x1 + 60),
            x1 + 45,
            (y1 + y2) / 2,
        )
    } else {
        let x2 = to.x + NODE_W;
        (
            format!("M{} {y1}L{x2} {y2}", from.x),
            (from.x + x2) / 2,
            (y1 + y2) / 2,
        )
    };
    let (stroke, dash) = if e.declared {
        ("#555555", "")
    } else {
        ("#a40000", " stroke-dasharray=\"6 4\"")
    };
    let link = !e.href.is_empty();
    if link {
        out.push_str(&format!("<a href=\"{}\">", escape(&e.href)));
    } else {
        out.push_str("<g>");
    }
    out.push_str(&format!(
        "<path d=\"{path}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"1.5\"{dash} marker-end=\"url(#arrow)\"/>"
    ));
    out.push_str(&format!(
        "<text x=\"{mx}\" y=\"{}\" text-anchor=\"middle\" font-size=\"11\" fill=\"{stroke}\">{}</text>",
        my - 4,
        escape(&edge_label(e))
    ));
    out.push_str(if link { "</a>" } else { "</g>" });
}

fn node_svg(out: &mut String, n: &Node) {
    let (fill, stroke, dash) = match n.kind {
        Kind::Deployment => ("#eef4fb", "#0b5cad", ""),
        Kind::Service => ("#f3f5f7", "#555555", ""),
        Kind::External => ("#ffffff", "#888888", " stroke-dasharray=\"4 3\""),
    };
    match &n.href {
        Some(href) => out.push_str(&format!("<a href=\"{}\">", escape(href))),
        None => out.push_str("<g>"),
    }
    out.push_str(&format!(
        "<rect x=\"{}\" y=\"{}\" width=\"{NODE_W}\" height=\"{NODE_H}\" rx=\"4\" fill=\"{fill}\" stroke=\"{stroke}\"{dash}/>",
        n.x, n.y
    ));
    out.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"#1b1b1b\">{}</text>",
        n.x + 10,
        n.y + NODE_H / 2 + 4,
        escape(&ellipsize(&n.label, LABEL_CHARS))
    ));
    out.push_str(if n.href.is_some() { "</a>" } else { "</g>" });
}

pub(super) fn svg(map: &Map) -> String {
    let (w, h) = (map.width, map.height);
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" \
         role=\"img\" aria-label=\"Calls between deployments\" font-family=\"system-ui, sans-serif\">"
    );
    out.push_str(
        "<defs><marker id=\"arrow\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"4\" orient=\"auto\">\
         <path d=\"M0 0L8 4L0 8z\" fill=\"#555555\"/></marker></defs>",
    );
    let find = |id: &str| map.nodes.iter().find(|n| n.id == id);
    for e in &map.edges {
        if let (Some(from), Some(to)) = (find(&e.from), find(&e.to)) {
            edge_svg(&mut out, e, from, to);
        }
    }
    for n in &map.nodes {
        node_svg(&mut out, n);
    }
    if map.dropped.nodes + map.dropped.edges > 0 {
        out.push_str(&format!(
            "<text x=\"20\" y=\"{}\" font-size=\"12\" fill=\"#a40000\">{} nodes and {} edges left out</text>",
            h - NOTE_H / 2 + 4,
            map.dropped.nodes,
            map.dropped.edges
        ));
    }
    out.push_str("</svg>");
    out
}

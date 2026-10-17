//! Différence entre deux documents OpenAPI, pour suivre la découverte d'un tir à
//! l'autre (spec §9.5).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    OperationRemoved,
    OperationAdded,
    ParameterRequired,
    ParameterAdded,
    ParameterRemoved,
    ResponseRemoved,
    ResponseAdded,
}

impl ChangeKind {
    fn label(self) -> &'static str {
        match self {
            ChangeKind::OperationRemoved => "opération supprimée",
            ChangeKind::OperationAdded => "opération ajoutée",
            ChangeKind::ParameterRequired => "paramètre devenu requis",
            ChangeKind::ParameterAdded => "paramètre ajouté",
            ChangeKind::ParameterRemoved => "paramètre supprimé",
            ChangeKind::ResponseRemoved => "réponse supprimée",
            ChangeKind::ResponseAdded => "réponse ajoutée",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Change {
    pub path: String,
    pub method: String,
    pub kind: ChangeKind,
    /// `query limit` pour un paramètre, `404` pour une réponse, vide pour une opération.
    pub detail: String,
    pub breaking: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SpecDiff {
    pub changes: Vec<Change>,
}

impl SpecDiff {
    pub fn is_breaking(&self) -> bool {
        self.changes.iter().any(|c| c.breaking)
    }

    /// Une ligne par changement, triée par chemin, méthode puis nature.
    pub fn to_text(&self) -> String {
        self.changes
            .iter()
            .map(|c| {
                let flag = if c.breaking { "CASSANT" } else { "       " };
                let detail = if c.detail.is_empty() {
                    String::new()
                } else {
                    format!(" : {}", c.detail)
                };
                format!(
                    "{flag}  {} {}  {}{detail}\n",
                    c.method,
                    c.path,
                    c.kind.label()
                )
            })
            .collect()
    }
}

/// Une opération, indexée par méthode et chemin normalisé (`/pets/{}`) pour qu'un
/// paramètre de chemin renommé ne passe pas pour une suppression.
struct Operation<'a> {
    path: &'a str,
    params: BTreeMap<(String, String), bool>,
    responses: BTreeSet<String>,
}

fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_param = false;
    for c in path.chars() {
        match c {
            '{' => {
                in_param = true;
                out.push_str("{}");
            }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(c),
        }
    }
    out
}

fn operations(doc: &Value) -> BTreeMap<(String, String), Operation<'_>> {
    let mut ops = BTreeMap::new();
    let Some(paths) = doc["paths"].as_object() else {
        return ops;
    };
    for (path, item) in paths {
        let shared = item["parameters"].as_array();
        for method in METHODS {
            let op = &item[*method];
            if !op.is_object() {
                continue;
            }
            let mut params = BTreeMap::new();
            for p in shared
                .into_iter()
                .flatten()
                .chain(op["parameters"].as_array().into_iter().flatten())
            {
                let (Some(loc), Some(name)) = (p["in"].as_str(), p["name"].as_str()) else {
                    continue;
                };
                // Les paramètres de chemin suivent le template : leur nom ne compte pas.
                let name = if loc == "path" {
                    String::new()
                } else {
                    name.to_string()
                };
                let required = p["required"].as_bool().unwrap_or(loc == "path");
                params.insert((loc.to_string(), name), required);
            }
            let responses = op["responses"]
                .as_object()
                .map(|r| r.keys().cloned().collect())
                .unwrap_or_default();
            ops.insert(
                (normalize(path), method.to_uppercase()),
                Operation {
                    path,
                    params,
                    responses,
                },
            );
        }
    }
    ops
}

fn param_label((loc, name): &(String, String)) -> String {
    if name.is_empty() {
        loc.clone()
    } else {
        format!("{loc} {name}")
    }
}

/// Compare `old` et `new`. Cassant : opération supprimée, paramètre devenu requis ou
/// ajouté requis, code 2xx supprimé.
pub fn diff(old: &Value, new: &Value) -> SpecDiff {
    let old_ops = operations(old);
    let new_ops = operations(new);
    let mut changes = Vec::new();
    let mut push = |op: &Operation<'_>, method: &str, kind, detail: String, breaking| {
        changes.push(Change {
            path: op.path.to_string(),
            method: method.to_string(),
            kind,
            detail,
            breaking,
        });
    };
    for ((key, method), o) in &old_ops {
        let Some(n) = new_ops.get(&(key.clone(), method.clone())) else {
            push(o, method, ChangeKind::OperationRemoved, String::new(), true);
            continue;
        };
        for (p, &was_required) in &o.params {
            match n.params.get(p) {
                None => push(
                    n,
                    method,
                    ChangeKind::ParameterRemoved,
                    param_label(p),
                    false,
                ),
                Some(true) if !was_required => push(
                    n,
                    method,
                    ChangeKind::ParameterRequired,
                    param_label(p),
                    true,
                ),
                Some(_) => {}
            }
        }
        for (p, &required) in &n.params {
            if !o.params.contains_key(p) {
                push(
                    n,
                    method,
                    ChangeKind::ParameterAdded,
                    param_label(p),
                    required,
                );
            }
        }
        for code in o.responses.difference(&n.responses) {
            push(
                n,
                method,
                ChangeKind::ResponseRemoved,
                code.clone(),
                code.starts_with('2'),
            );
        }
        for code in n.responses.difference(&o.responses) {
            push(n, method, ChangeKind::ResponseAdded, code.clone(), false);
        }
    }
    for ((key, method), n) in &new_ops {
        if !old_ops.contains_key(&(key.clone(), method.clone())) {
            push(n, method, ChangeKind::OperationAdded, String::new(), false);
        }
    }
    changes.sort();
    SpecDiff { changes }
}

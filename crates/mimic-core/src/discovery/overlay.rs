//! The gRPC overlay (spec §7.3, F14): the full samples (message and trailers) of the gRPC
//! endpoints of a report, stored apart from the parts that a controller up to F13 reads. Pure.

use crate::compact::{Name, SortedMap};
use serde::{Deserialize, Serialize};

use super::report::Direction;
use super::sample::{MAX_SAMPLES, Samples};
use super::scope::ScopeKey;

/// Samples by direction, scope, gRPC path and method.
type Entries = SortedMap<Direction, SortedMap<ScopeKey, SortedMap<Name, SortedMap<Name, Samples>>>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcOverlay {
    version: u32,
    scopes: Entries,
}

impl GrpcOverlay {
    pub(super) fn new() -> Self {
        GrpcOverlay {
            version: 1,
            scopes: Entries::new(),
        }
    }

    pub(super) fn insert(
        &mut self,
        direction: Direction,
        key: ScopeKey,
        path: Name,
        method: Name,
        samples: Samples,
    ) {
        self.scopes
            .entry(direction)
            .or_default()
            .entry(key)
            .or_default()
            .entry(path)
            .or_default()
            .insert(method, samples);
    }

    pub(super) fn entries(
        self,
    ) -> impl Iterator<Item = (Direction, ScopeKey, Name, Name, Samples)> {
        self.scopes.into_iter().flat_map(|(d, scopes)| {
            scopes.into_iter().flat_map(move |(k, paths)| {
                let k2 = k.clone();
                paths.into_iter().flat_map(move |(p, methods)| {
                    let k3 = k2.clone();
                    methods
                        .into_iter()
                        .map(move |(m, s)| (d, k3.clone(), p.clone(), m, s))
                })
            })
        })
    }

    /// No sample at all: nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Reads an overlay and checks it: version 1, at most 20 samples per endpoint, each within
    /// the bounds of a sample (spec §7.3).
    pub fn from_json(json: &str) -> Result<Self, String> {
        let overlay: GrpcOverlay =
            serde_json::from_str(json).map_err(|e| format!("invalid gRPC overlay: {e}"))?;
        if overlay.version != 1 {
            return Err(format!("unsupported overlay version {}", overlay.version));
        }
        let samples = overlay
            .scopes
            .values()
            .flat_map(|s| s.values())
            .flat_map(|p| p.values())
            .flat_map(|m| m.values());
        for s in samples {
            if s.len() > MAX_SAMPLES || s.check().is_err() {
                return Err("gRPC overlay out of bounds".to_string());
            }
        }
        Ok(overlay)
    }
}

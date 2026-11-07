//! The effective mode of a deployment (spec §7): what its `MimicWorkload`s say, combined.

use std::collections::BTreeSet;

use super::latency::format_factor;
use crate::Mode;
use crate::proxy::tls::MAX_EXCLUDES;

/// The latency factor, in hundredths, of a `MimicWorkload` that sets none: 1.
pub const DEFAULT_FACTOR: u32 = 100;

/// `spec.mode`, `spec.mock.latencyFactor` and `spec.mock.passthrough` of a `MimicWorkload`
/// (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockSettings {
    /// `spec.mode` is `mock`.
    pub mock: bool,
    /// `spec.mock.latencyFactor` in hundredths, rounded half up (0 to 10 000).
    pub factor: u32,
    pub passthrough: Vec<String>,
}

impl Default for MockSettings {
    fn default() -> MockSettings {
        MockSettings {
            mock: false,
            factor: DEFAULT_FACTOR,
            passthrough: Vec::new(),
        }
    }
}

/// The mode of a deployment, from all its `MimicWorkload`s (spec §7 "Effective mode").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub mock: bool,
    /// Latency factor in hundredths.
    pub factor: u32,
    /// Sorted, deduplicated, at most [`MAX_EXCLUDES`] entries.
    pub passthrough: Vec<String>,
}

impl Effective {
    pub fn mode(&self) -> Mode {
        if self.mock {
            Mode::Mock
        } else {
            Mode::Discovery
        }
    }

    /// `Mimic-Latency-Factor` and `MIMIC_MOCK_LATENCY_FACTOR`: the shortest decimal that reads
    /// back to the same value (`1`, `0.5`, `0.05`, `100`).
    pub fn factor_text(&self) -> String {
        format_factor(self.factor)
    }
}

/// The effective mode of the `MimicWorkload`s of one deployment, given in name order:
/// `mock` when any of them says so, with the latency factor of the first of those and the
/// union of all their passthrough lists. No workload gives `discovery`, factor 1.
pub fn effective<'a>(workloads: impl IntoIterator<Item = &'a MockSettings>) -> Effective {
    let mut found = Effective {
        mock: false,
        factor: DEFAULT_FACTOR,
        passthrough: Vec::new(),
    };
    let mut union = BTreeSet::new();
    for settings in workloads {
        if settings.mock && !found.mock {
            found.mock = true;
            found.factor = settings.factor;
        }
        union.extend(settings.passthrough.iter().cloned());
    }
    found.passthrough = union.into_iter().take(MAX_EXCLUDES).collect();
    found
}

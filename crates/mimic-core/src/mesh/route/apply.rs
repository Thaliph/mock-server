//! The weighted backend draw and the header modifiers of a matched rule (spec §7.9 "Routing").

use super::{BackendView, ModifierView};

/// The outcome of [`draw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Index of the drawn valid backend.
    Backend(usize),
    /// No backend, or every weight is 0.
    NoBackend,
    /// The drawn backend is invalid.
    Invalid,
}

/// Draws a backend of `backends` with the uniform 64-bit random value `r`.
pub fn draw(backends: &[BackendView], r: u64) -> Pick {
    let total: u64 = backends.iter().map(|b| u64::from(b.weight)).sum();
    if total == 0 {
        return Pick::NoBackend;
    }
    // floor(r * total / 2^64) < total: exact, with no bias and no float.
    let target = ((u128::from(r) * u128::from(total)) >> 64) as u64;
    let mut running = 0u64;
    for (i, b) in backends.iter().enumerate() {
        running += u64::from(b.weight);
        // A weight of 0 never makes the running sum exceed the target.
        if running > target {
            return if b.invalid == Some(true) {
                Pick::Invalid
            } else {
                Pick::Backend(i)
            };
        }
    }
    Pick::NoBackend
}

/// The field lines of a head, in order, as a modifier needs them.
pub trait FieldList {
    /// Removes every line whose name equals `name` in any case.
    fn remove_all(&mut self, name: &str);
    /// Appends one line.
    fn append(&mut self, name: &str, value: &str);
}

impl FieldList for Vec<(String, String)> {
    fn remove_all(&mut self, name: &str) {
        self.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    fn append(&mut self, name: &str, value: &str) {
        self.push((name.to_ascii_lowercase(), value.to_string()));
    }
}

impl ModifierView {
    /// `set`, then `add`, then `remove`, each list in its order.
    pub fn apply<H: FieldList>(&self, fields: &mut H) {
        for nv in &self.set {
            fields.remove_all(&nv.name);
            fields.append(&nv.name, &nv.value);
        }
        for nv in &self.add {
            fields.append(&nv.name, &nv.value);
        }
        for name in &self.remove {
            fields.remove_all(name);
        }
    }
}

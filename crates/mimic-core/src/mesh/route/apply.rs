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
    todo!("{} {r}", backends.len())
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
        todo!("{name}")
    }

    fn append(&mut self, name: &str, value: &str) {
        todo!("{name} {value}")
    }
}

impl ModifierView {
    /// `set`, then `add`, then `remove`, each list in its order.
    pub fn apply<H: FieldList>(&self, _fields: &mut H) {
        todo!("{}", self.set.len())
    }
}

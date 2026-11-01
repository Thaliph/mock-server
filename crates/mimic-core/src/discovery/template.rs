//! Path templating (spec §6): segment classes and template rendering.

/// One segment of a template.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Seg {
    Literal(String),
    Param,
}

/// True when the segment is id-like and becomes a parameter (raw bytes, nothing is decoded).
pub fn is_param(_segment: &str) -> bool {
    todo!()
}

/// Splits a path (without query) into raw segments; a trailing slash is an empty last segment.
pub fn split_path(_path: &str) -> Vec<&str> {
    todo!()
}

/// Renders a template, naming parameters `{<literal>_id}` or `{param<n>}`.
pub fn render(_segs: &[Seg]) -> String {
    todo!()
}

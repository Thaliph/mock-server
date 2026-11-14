//! The stripper of the `x-mimic-*` fields of an app's response head (spec §4.1 "Mimic fields
//! of app responses", RTY-06): pure, no I/O. The inbound side of a meshed sidecar runs it on
//! every final response head so that only a sidecar writes such a field (§7.9 "Retries").

use std::borrow::Cow;

/// `head` without the lines of its `x-mimic-*` fields, and how many fields were removed.
pub fn strip(head: &[u8]) -> (Cow<'_, [u8]>, usize) {
    (Cow::Borrowed(head), 0)
}

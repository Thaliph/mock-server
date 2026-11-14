//! The stripper of the `x-mimic-*` fields of an app's response head (spec §4.1 "Mimic fields
//! of app responses", RTY-06): pure, no I/O. The inbound side of a meshed sidecar runs it on
//! every final response head so that only a sidecar writes such a field (§7.9 "Retries").

use std::borrow::Cow;

const PREFIX: &[u8] = b"x-mimic-";

/// `head` without the lines of its `x-mimic-*` fields (name in any case, with the `obs-fold`
/// lines that continue them), and how many fields were removed. Only the lines up to the blank
/// line that ends the head are looked at, and the first one (the status line) is kept: every
/// other byte is unchanged. A head with no such field is returned as it is, not copied.
pub fn strip(head: &[u8]) -> (Cow<'_, [u8]>, usize) {
    let mut out: Option<Vec<u8>> = None;
    let mut removed = 0;
    // Inside a removed field: its continuation lines go too.
    let mut dropping = false;
    let mut at = 0;
    for (index, line) in head.split_inclusive(|&b| b == b'\n').enumerate() {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if index > 0 && text.is_empty() {
            break;
        }
        let fold = matches!(line.first(), Some(b' ' | b'\t'));
        let named = line.len() >= PREFIX.len() && line[..PREFIX.len()].eq_ignore_ascii_case(PREFIX);
        let drop = index > 0 && (named || (dropping && fold));
        dropping = drop;
        removed += usize::from(index > 0 && named);
        if drop && out.is_none() {
            out = Some(head[..at].to_vec());
        }
        at += line.len();
        if !drop && let Some(out) = out.as_mut() {
            out.extend_from_slice(line);
        }
    }
    match out {
        Some(mut out) => {
            out.extend_from_slice(&head[at..]);
            (Cow::Owned(out), removed)
        }
        None => (Cow::Borrowed(head), 0),
    }
}

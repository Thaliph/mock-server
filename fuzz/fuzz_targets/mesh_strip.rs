//! Fuzz target for the stripper of the `x-mimic-*` fields of a response head (spec RTY-07,
//! §4.1 "Mimic fields of app responses").
//!
//! Blank lines before the status line are skipped, as the HTTP/1 codec skips them. The stripper
//! never panics; its output is the input with whole lines removed, never longer;
//! no line of the head after it starts with `x-mimic-` in any case (the status line apart);
//! the count is the number of lines removed that start a field; and stripping again changes
//! nothing.

#![no_main]

use std::borrow::Cow;

use libfuzzer_sys::fuzz_target;
use mimic_core::mesh::strip::strip;

const PREFIX: &[u8] = b"x-mimic-";

fn named(line: &[u8]) -> bool {
    line.len() >= PREFIX.len() && line[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

/// The lines of the head, up to its blank line, and the rest.
fn split(data: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut at = 0;
    let mut lines = Vec::new();
    for line in data.split_inclusive(|&b| b == b'\n') {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.is_empty() {
            // Blank lines before the status line are skipped, as the codec skips them.
            if !lines.is_empty() {
                break;
            }
        } else {
            lines.push(line);
        }
        at += line.len();
    }
    (lines, &data[at..])
}

fuzz_target!(|data: &[u8]| {
    let (out, count) = strip(data);
    assert!(out.len() <= data.len(), "never longer");
    if let Cow::Borrowed(same) = &out {
        assert_eq!(*same, data);
        assert_eq!(count, 0, "unchanged means nothing removed");
    }
    // The output is the input with whole lines removed: every output line is, in order, a
    // line of the input.
    let mut rest = data;
    for line in out.split_inclusive(|&b| b == b'\n') {
        loop {
            let end = rest
                .iter()
                .position(|&b| b == b'\n')
                .map_or(rest.len(), |p| p + 1);
            assert!(end > 0, "an output line is an input line");
            let (head, tail) = rest.split_at(end);
            rest = tail;
            if head == line {
                break;
            }
        }
    }
    let (lines, _) = split(&out);
    for line in lines.iter().skip(1) {
        assert!(!named(line), "no x-mimic- field is left");
    }
    let (before, _) = split(data);
    let fields = before.iter().skip(1).filter(|line| named(line)).count();
    assert_eq!(count, fields, "one count per field");
    let (again, n) = strip(&out);
    assert_eq!(n, 0, "stripping twice changes nothing");
    assert_eq!(&*again, &*out);
});

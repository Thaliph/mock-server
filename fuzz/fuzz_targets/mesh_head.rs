//! Fuzz target for the head reader of a mesh-routed HTTP/1 connection (spec MSH-66, §4.1
//! limits 1 and 8).
//!
//! The first byte says whether `Upgrade` heads leave (bit 0), the second seeds the sizes of the
//! reads, the rest is what the client sends. The reader never panics; what it passes on is a
//! prefix of the input; with what it holds back (a stop) or still buffers (an incomplete head)
//! nothing is lost; and the verdict is the same whether the input arrives whole or in reads of
//! any sizes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::mesh::head::{Reader, Stop};

struct Outcome {
    stop: Option<Stop>,
    pass: Vec<u8>,
    held: Vec<u8>,
    pending: usize,
}

fn run(input: &[u8], upgrades: bool, mut seed: u8, cut: bool) -> Outcome {
    let mut reader = Reader::new();
    let mut pass = Vec::new();
    let mut stop = None;
    let mut rest = input;
    while !rest.is_empty() {
        let size = if cut {
            seed = seed.wrapping_mul(97).wrapping_add(13);
            usize::from(seed % 23) + 1
        } else {
            rest.len()
        };
        let (chunk, tail) = rest.split_at(size.min(rest.len()));
        rest = tail;
        stop = stop.or(reader.feed(chunk, upgrades, &mut pass));
    }
    Outcome {
        stop,
        pass,
        held: reader.held().to_vec(),
        pending: reader.pending(),
    }
}

fuzz_target!(|data: &[u8]| {
    let [flags, seed, input @ ..] = data else {
        return;
    };
    let upgrades = flags & 1 == 1;
    let whole = run(input, upgrades, *seed, false);
    assert!(input.starts_with(&whole.pass), "what passes is a prefix");
    if whole.stop.is_some() {
        assert_eq!(whole.pending, 0);
        assert_eq!(
            &input[whole.pass.len()..],
            &whole.held[..],
            "nothing is lost"
        );
    } else {
        assert_eq!(
            whole.pass.len() + whole.pending,
            input.len(),
            "nothing is lost"
        );
    }
    let cut = run(input, upgrades, *seed, true);
    assert_eq!(
        cut.stop, whole.stop,
        "the verdict does not depend on the reads"
    );
    assert_eq!(cut.pass, whole.pass);
    assert_eq!(cut.held, whole.held);
    assert_eq!(cut.pending, whole.pending);
});

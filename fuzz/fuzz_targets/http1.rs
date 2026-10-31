//! Fuzz target for the HTTP/1 framer (spec PRX-28, codec properties of PRX-27 and PRX-30).
//!
//! The input is a sequence of steps, each one a kind byte, then (for data steps) a length byte
//! and that many bytes:
//!
//! - kind `% 3 == 0`: bytes read from the client;
//! - kind `% 3 == 1`: bytes read from the destination;
//! - kind `% 3 == 2`: the destination closed, no length byte.
//!
//! A truncated step takes the bytes that are left. Three runs are made over the same steps:
//!
//! 1. plain: client reads go to `client_bytes`, once as one read and once byte by byte. Both
//!    must report the same events in the same order whatever the split (PRX-27, split
//!    invariance);
//! 2. limited: client reads go to `client_bytes_limited` like a relay does. The bytes it did not
//!    use are held and offered again after every destination read (PRX-30). Used bytes never
//!    exceed the offered ones.
//!
//! In every run, after every feed, the retained state stays within `RETAINED_LIMIT` (PRX-27).
//! A panic, a hang or an out-of-memory fails the run.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mimic_core::proxy::http1::{Codec, Event, RETAINED_LIMIT};

#[derive(Clone, Copy)]
enum Step<'a> {
    Client(&'a [u8]),
    Server(&'a [u8]),
    ServerEof,
}

fn steps(mut data: &[u8]) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    while let Some((&kind, rest)) = data.split_first() {
        if kind % 3 == 2 {
            steps.push(Step::ServerEof);
            data = rest;
            continue;
        }
        let Some((&len, rest)) = rest.split_first() else {
            break;
        };
        let (chunk, rest) = rest.split_at((len as usize).min(rest.len()));
        steps.push(if kind % 3 == 0 {
            Step::Client(chunk)
        } else {
            Step::Server(chunk)
        });
        data = rest;
    }
    steps
}

fn pieces(bytes: &[u8], bytewise: bool) -> impl Iterator<Item = &[u8]> {
    bytes.chunks(if bytewise { 1 } else { bytes.len().max(1) })
}

fn check_retained(codec: &Codec) {
    let retained = codec.retained_bytes();
    assert!(
        retained <= RETAINED_LIMIT,
        "retained state {retained} over the limit {RETAINED_LIMIT}"
    );
}

/// Plain reads: every client read goes to `client_bytes`.
fn run_plain(steps: &[Step<'_>], bytewise: bool) -> Vec<Event> {
    let mut codec = Codec::new();
    let mut events = Vec::new();
    for step in steps {
        match *step {
            Step::Client(bytes) => {
                for piece in pieces(bytes, bytewise) {
                    events.extend(codec.client_bytes(piece));
                    check_retained(&codec);
                }
            }
            Step::Server(bytes) => {
                for piece in pieces(bytes, bytewise) {
                    events.extend(codec.server_bytes(piece));
                    check_retained(&codec);
                }
            }
            Step::ServerEof => {
                events.extend(codec.server_eof());
                check_retained(&codec);
            }
        }
    }
    events
}

/// Relay-style reads: client bytes go to `client_bytes_limited`; the unused ones are held and
/// offered again after every destination read until the codec stops using them.
fn run_limited(steps: &[Step<'_>]) -> Vec<Event> {
    let mut codec = Codec::new();
    let mut events = Vec::new();
    let mut held: Vec<u8> = Vec::new();
    for step in steps {
        match *step {
            Step::Client(bytes) => held.extend_from_slice(bytes),
            Step::Server(bytes) => {
                events.extend(codec.server_bytes(bytes));
                check_retained(&codec);
            }
            Step::ServerEof => {
                events.extend(codec.server_eof());
                check_retained(&codec);
            }
        }
        offer(&mut codec, &mut held, &mut events);
    }
    events
}

/// Offers the held client bytes until the codec uses none of them.
fn offer(codec: &mut Codec, held: &mut Vec<u8>, events: &mut Vec<Event>) {
    while !held.is_empty() {
        let (used, new_events) = codec.client_bytes_limited(held);
        check_retained(codec);
        assert!(
            used <= held.len(),
            "codec used {used} of {} offered bytes",
            held.len()
        );
        events.extend(new_events);
        held.drain(..used);
        if used == 0 {
            break;
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let steps = steps(data);
    let whole = run_plain(&steps, false);
    let bytewise = run_plain(&steps, true);
    assert_eq!(whole, bytewise, "events depend on how the reads are split");
    run_limited(&steps);
});
